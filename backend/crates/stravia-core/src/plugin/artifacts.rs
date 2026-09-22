use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::PathBuf;

use anyhow::{Context, ensure};
use bytes::Bytes;
use stravia_runtime_contract::protocol::ir::canonical::{hash_bytes, hash_hex};

pub(super) const MAX_COMPONENT_BYTES: usize = 64 * 1024 * 1024;

/// 安装记录只保存内容摘要，路径始终由实例目录派生，不接受包提供的文件名。
pub(super) struct PluginArtifacts {
    directory: PathBuf,
}

impl PluginArtifacts {
    pub(super) fn new(plugin_directory: PathBuf) -> Self {
        Self {
            directory: plugin_directory.join("artifacts"),
        }
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
