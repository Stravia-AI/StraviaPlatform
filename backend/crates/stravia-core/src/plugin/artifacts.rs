use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, ensure};
use async_trait::async_trait;
use bytes::Bytes;
use stravia_runtime_contract::protocol::ir::canonical::{hash_bytes, hash_hex};
use stravia_vendor_runtime::{ComponentBytes, ComponentSource, SourceError};

pub(super) const MAX_COMPONENT_BYTES: usize = 64 * 1024 * 1024;

/// 安装记录只保存内容摘要，路径始终由实例目录派生，不接受包提供的文件名。
#[derive(Clone)]
pub(super) struct PluginArtifacts {
    directory: PathBuf,
}

/// 已安装组件的编译产物被释放后，按内容摘要重新读取并校验字节。
struct ArtifactSource {
    artifacts: PluginArtifacts,
    digest: String,
}

#[async_trait]
impl ComponentSource for ArtifactSource {
    async fn read(&self) -> Result<ComponentBytes, SourceError> {
        Ok(Box::new(self.artifacts.read(&self.digest).await?))
    }
}

impl PluginArtifacts {
    pub(super) fn new(plugin_directory: PathBuf) -> Self {
        Self {
            directory: plugin_directory.join("artifacts"),
        }
    }

    pub(super) fn source(&self, digest: &str) -> Arc<dyn ComponentSource> {
        Arc::new(ArtifactSource {
            artifacts: self.clone(),
            digest: digest.to_owned(),
        })
    }

    fn path(&self, digest: &str) -> anyhow::Result<PathBuf> {
        ensure!(
            digest.len() == 64
                && digest
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
            "invalid plugin artifact digest"
        );
        Ok(self.directory.join(format!("{digest}.wasm")))
    }

    /// 只检查文件存在且大小合规，不读取内容：恢复准入记录时用它保持"安装包
    /// 缺失即标记不可用"的启动行为；内容摘要在组件被装入时才校验。
    pub(super) async fn is_available(&self, digest: &str) -> bool {
        let Ok(path) = self.path(digest) else {
            return false;
        };
        tokio::fs::symlink_metadata(path)
            .await
            .is_ok_and(|metadata| {
                metadata.is_file()
                    && metadata.len() > 0
                    && metadata.len() <= MAX_COMPONENT_BYTES as u64
            })
    }

    pub(super) async fn read(&self, digest: &str) -> anyhow::Result<Bytes> {
        let path = self.path(digest)?;
        let digest = digest.to_owned();
        tokio::task::spawn_blocking(move || {
            let metadata = fs::symlink_metadata(&path).context("inspect plugin artifact")?;
            ensure!(metadata.is_file(), "plugin artifact is not a regular file");
            ensure!(
                metadata.len() > 0 && metadata.len() <= MAX_COMPONENT_BYTES as u64,
                "plugin artifact size is invalid"
            );
            let mut bytes = Vec::with_capacity(metadata.len() as usize);
            File::open(&path)
                .context("open plugin artifact")?
                .take(MAX_COMPONENT_BYTES as u64 + 1)
                .read_to_end(&mut bytes)
                .context("read plugin artifact")?;
            ensure!(
                !bytes.is_empty() && bytes.len() <= MAX_COMPONENT_BYTES,
                "plugin artifact size is invalid"
            );
            ensure!(
                hash_hex(&hash_bytes(&bytes)) == digest,
                "plugin artifact digest mismatch"
            );
            Ok(Bytes::from(bytes))
        })
        .await
        .context("plugin artifact reader failed")?
    }

    /// 在提交安装元数据前落盘。相同内容复用文件；显式重装可以修复损坏的同摘要文件。
    pub(super) async fn store(&self, digest: &str, bytes: Bytes) -> anyhow::Result<()> {
        let destination = self.path(digest)?;
        let directory = self.directory.clone();
        let digest = digest.to_owned();
        tokio::task::spawn_blocking(move || {
            ensure!(
                !bytes.is_empty() && bytes.len() <= MAX_COMPONENT_BYTES,
                "plugin artifact size is invalid"
            );
            ensure!(
                hash_hex(&hash_bytes(&bytes)) == digest,
                "plugin artifact digest mismatch"
            );
            fs::create_dir_all(&directory).context("create plugin artifact directory")?;
            match fs::symlink_metadata(&destination) {
                Ok(metadata) if metadata.is_file() && metadata.len() == bytes.len() as u64 => {
                    if matches_file(&destination, &bytes)? {
                        return Ok(());
                    }
                }
                Ok(metadata) => {
                    ensure!(metadata.is_file(), "plugin artifact is not a regular file")
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error).context("inspect installed plugin artifact"),
            }
            let temporary = directory.join(format!(
                ".install-{}.tmp",
                stravia_runtime_contract::identifier::new_id()
            ));
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options
                .open(&temporary)
                .context("create staged plugin artifact")?;
            let staged = StagedArtifact(temporary);
            file.write_all(&bytes)
                .context("write staged plugin artifact")?;
            file.sync_all().context("sync staged plugin artifact")?;
            drop(file);
            fs::rename(&staged.0, &destination).context("publish plugin artifact")?;
            #[cfg(unix)]
            File::open(&directory)
                .and_then(|directory| directory.sync_all())
                .context("sync plugin artifact directory")?;
            Ok(())
        })
        .await
        .context("plugin artifact writer failed")?
    }
}

fn matches_file(path: &std::path::Path, expected: &[u8]) -> anyhow::Result<bool> {
    let mut file = File::open(path).context("open installed plugin artifact")?;
    let mut buffer = [0_u8; 64 * 1024];
    for chunk in expected.chunks(buffer.len()) {
        let actual = &mut buffer[..chunk.len()];
        file.read_exact(actual)
            .context("read installed plugin artifact")?;
        if actual != chunk {
            return Ok(false);
        }
    }
    Ok(file
        .read(&mut buffer[..1])
        .context("finish reading installed plugin artifact")?
        == 0)
}

struct StagedArtifact(PathBuf);

impl Drop for StagedArtifact {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_file(&self.0)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(%error, "failed to remove staged plugin artifact");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn digest_of(bytes: &[u8]) -> String {
        hash_hex(&hash_bytes(bytes))
    }

    #[tokio::test]
    async fn released_plugin_bytes_come_back_from_the_stored_artifact_only_when_intact() {
        let directory = tempfile::tempdir().expect("temp dir");
        let artifacts = PluginArtifacts::new(directory.path().to_owned());
        let component = Bytes::from_static(b"\0asm installed component");
        let digest = digest_of(&component);
        artifacts
            .store(&digest, component.clone())
            .await
            .expect("store artifact");

        let source = artifacts.source(&digest);
        let restored = source.read().await.expect("restore intact artifact");
        assert_eq!((*restored).as_ref(), component.as_ref());

        // 磁盘上的字节被改动后，来源拒绝返回，不会把不同代码交给运行时。
        let path = artifacts.path(&digest).expect("artifact path");
        fs::write(&path, b"\0asm tampered component").expect("tamper artifact");
        assert!(source.read().await.is_err());
    }
}
