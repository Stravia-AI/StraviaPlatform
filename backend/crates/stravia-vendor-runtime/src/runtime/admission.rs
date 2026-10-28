//! 已通过准入校验的组件记录。
//!
//! 记录只是让启动跳过"为取得 descriptor 而实例化组件"的提示，不是信任锚：
//! 凭记录恢复的插件在首次装入时仍会完整校验 imports，并要求 guest 当场报告的
//! descriptor 与记录一致（见 `Residency::reload_resident`）。所以记录被篡改、
//! 过期或损坏，最坏只是首次使用失败或下次启动重新校验。

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use serde::{Deserialize, Serialize};
use stravia_vendor_sdk::VendorDescriptor;

/// 记录文件的上限，防止异常大文件在启动时被整体读入。
const MAX_RECORD_BYTES: u64 = 1024 * 1024;

#[derive(Serialize, Deserialize)]
struct Record {
    /// 写入记录的宿主构建。宿主更换后校验规则可能不同，旧记录必须作废。
    host: String,
    descriptor: VendorDescriptor,
}

pub(super) struct AdmissionRecords {
    directory: PathBuf,
    host: String,
}

impl AdmissionRecords {
    /// 无法确定当前宿主构建时返回 `None`：没有可靠的失效依据就不使用记录。
    pub(super) fn new(directory: PathBuf) -> Option<Self> {
        Some(Self {
            directory,
            host: host_build()?,
        })
    }

    #[cfg(test)]
    pub(super) fn for_host(directory: PathBuf, host: &str) -> Self {
        Self {
            directory,
            host: host.to_owned(),
        }
    }

    fn path(&self, digest: &[u8; 32]) -> PathBuf {
        self.directory.join(format!(
            "{}.json",
            stravia_runtime_contract::protocol::ir::canonical::hash_hex(digest)
        ))
    }

    /// 缺失、损坏、超限或来自其他宿主构建的记录都视为没有记录。
    pub(super) fn read(&self, digest: &[u8; 32]) -> Option<VendorDescriptor> {
        let path = self.path(digest);
        let file = std::fs::File::open(&path).ok()?;
        let metadata = file.metadata().ok()?;
        if !metadata.is_file() || metadata.len() > MAX_RECORD_BYTES {
            return None;
        }
        let record: Record = serde_json::from_reader(io::BufReader::new(file)).ok()?;
        (record.host == self.host).then_some(record.descriptor)
    }

    pub(super) fn write(&self, digest: &[u8; 32], descriptor: &VendorDescriptor) -> io::Result<()> {
        create_private_directory(&self.directory)?;
        let bytes = serde_json::to_vec(&Record {
            host: self.host.clone(),
            descriptor: descriptor.clone(),
        })
        .map_err(io::Error::other)?;
        let destination = self.path(digest);
        // 先写临时文件再改名：崩溃不会留下被截断的记录。
        let temporary = destination.with_extension("json.tmp");
        let mut file = std::fs::File::create(&temporary)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temporary, &destination).inspect_err(|_| {
            // 改名失败时尽力清理临时文件；清理失败不改变要报告的原始错误。
            let _ = std::fs::remove_file(&temporary);
        })
    }

    /// 作废记录。记录本就不存在不算错误。
    pub(super) fn forget(&self, digest: &[u8; 32]) -> io::Result<()> {
        match std::fs::remove_file(self.path(digest)) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
            _ => Ok(()),
        }
    }
}

/// 标识当前宿主构建：包版本加可执行文件的大小与修改时间，任何重新构建或
/// 升级都会使旧记录失效，不依赖人工维护的校验版本号。
fn host_build() -> Option<String> {
    let metadata = std::fs::metadata(std::env::current_exe().ok()?).ok()?;
    let modified = metadata.modified().ok()?.duration_since(UNIX_EPOCH).ok()?;
    Some(format!(
        "{}:{}:{}",
        env!("CARGO_PKG_VERSION"),
        metadata.len(),
        modified.as_nanos()
    ))
}

fn create_private_directory(directory: &Path) -> io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(directory)
}

/// 64 位小写十六进制摘要（与安装记录和 artifact 文件名一致）转回字节。
pub(super) fn decode_digest(hex: &str) -> Option<[u8; 32]> {
    if hex.len() != 64 {
        return None;
    }
    let mut digest = [0_u8; 32];
    let (pairs, _) = hex.as_bytes().as_chunks::<2>();
    for (byte, [high, low]) in digest.iter_mut().zip(pairs) {
        *byte = (nibble(*high)? << 4) | nibble(*low)?;
    }
    Some(digest)
}

fn nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}
