//! 不可变的原生编译镜像；源 Wasm 始终是缓存重建依据。
//!
//! 文件锁覆盖校验、发布和清理，避免跨 Engine／进程观察到镜像与摘要的半次更新。
//! 镜像只通过原子替换发布，绝不原地写入；已映射的旧文件可继续执行。

use std::fs::{self, File, Metadata, OpenOptions};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use sha2::{Digest, Sha256};
use stravia_runtime_contract::protocol::ir::canonical::hash_hex;
use wasmtime::component::Component;
use wasmtime::{CodeBuilder, Engine};

use crate::error::LoadError;

// 沿用原 Wasmtime 缓存的软容量和清理水位，不把派生缓存变成磁盘硬配额。
const SOFT_BYTES: u64 = 512 * 1024 * 1024;
const SOFT_ENTRIES: usize = 65_536;
const TARGET_PERCENT: u64 = 70;

pub(super) struct CompilationCache {
    directory: PathBuf,
    compatibility: u64,
    #[cfg(test)]
    hits: std::sync::atomic::AtomicUsize,
    #[cfg(test)]
    misses: std::sync::atomic::AtomicUsize,
}

impl CompilationCache {
    pub(super) fn new(directory: &Path, engine: &Engine) -> Result<Self, LoadError> {
        let directory = super::prepare_cache_directory(directory)?;
        let mut compatibility = DefaultHasher::new();
        engine
            .precompile_compatibility_hash()
            .hash(&mut compatibility);
        Ok(Self {
            directory,
            compatibility: compatibility.finish(),
            #[cfg(test)]
            hits: Default::default(),
            #[cfg(test)]
            misses: Default::default(),
        })
    }

    pub(super) fn load(
        &self,
        engine: &Engine,
        wasm: &[u8],
        digest: &[u8; 32],
    ) -> Result<Component, LoadError> {
        let image = self.directory.join(format!(
            "v1-{:016x}-{}.cwasm",
            self.compatibility,
            hash_hex(digest)
        ));
        let _lock = match self.lock() {
            Ok(lock) => lock,
            Err(error) => {
                tracing::warn!(%error, "compiled image cache unavailable; compiling without persistence");
                return Component::from_binary(engine, wasm).map_err(invalid_component);
            }
        };
        match self.read(engine, &image) {
            Ok(Some(component)) => {
                #[cfg(test)]
                self.hits.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                return Ok(component);
            }
            Ok(None) => {}
            Err(error) => {
                // 不可信文件不得通过修权限或覆盖恢复信任；只使用已提供的源 Wasm。
                tracing::warn!(%error, "compiled image cache cannot be read safely; compiling without persistence");
                return Component::from_binary(engine, wasm).map_err(invalid_component);
            }
        }
        #[cfg(test)]
        self.misses
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        // 保持 load 的 binary-only 契约，并直接生成镜像，不先装入匿名执行副本。
        let serialized = CodeBuilder::new(engine)
            .wasm_binary(wasm, None)
            .and_then(|builder| builder.compile_component_serialized())
            .map_err(invalid_component)?;
        let mut record = [0_u8; 40];
        record[..8].copy_from_slice(&(serialized.len() as u64).to_le_bytes());
        record[8..].copy_from_slice(&Sha256::digest(&serialized));
        let published = self
            .publish(&image, &serialized, true)
            .and_then(|()| self.publish(&image.with_extension("sha256"), &record, false));
        if let Err(error) = published {
            tracing::warn!(%error, "compiled image cache publication failed; using the compiled bytes without persistence");
            return deserialize_bytes(engine, &serialized);
        }
        // SAFETY: 文件来自本 Engine 的编译结果，位于已验证的私有目录；持有跨进程锁，
        // 所有发布只替换目录项，永不修改已发布文件内容。映射生命周期由 Wasmtime 持有。
        let component = match unsafe { Component::deserialize_file(engine, &image) } {
            Ok(component) => component,
            Err(error) => {
                tracing::warn!(%error, "compiled image file cannot be mapped; using the compiled bytes without persistence");
                return deserialize_bytes(engine, &serialized);
            }
        };
        drop(serialized);
        if let Err(error) = self.prune(&image, SOFT_BYTES, SOFT_ENTRIES) {
            tracing::warn!(%error, "compiled image cache cleanup failed");
        }
        Ok(component)
    }

    fn lock(&self) -> io::Result<File> {
        // 缓存可被管理员删除；恢复时仍重新验证目录，不能绕过首次创建的信任检查。
        super::prepare_cache_directory(&self.directory).map_err(io::Error::other)?;
        let path = self.directory.join(".lock");
        inspect_existing(&path)?;
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        let file = options.open(&path)?;
        validate_file(&file.metadata()?)?;
        // 独立 open 的文件描述符也串行化同进程不同 Engine；析构自动释放锁。
        file.lock()?;
        Ok(file)
    }

    fn read(&self, engine: &Engine, image: &Path) -> io::Result<Option<Component>> {
        let record_path = image.with_extension("sha256");
        let Some(mut record_file) = open_trusted(&record_path, true)? else {
            return Ok(None);
        };
        if record_file.metadata()?.len() != 40 {
            tracing::debug!(path = %record_path.display(), "discarding invalid compiled image checksum record");
            return Ok(None);
        }
        let mut record = [0_u8; 40];
        record_file.read_exact(&mut record)?;
        let Some(mut file) = open_trusted(image, false)? else {
            return Ok(None);
        };
        let expected_len = u64::from_le_bytes(record[..8].try_into().expect("fixed length field"));
        if file.metadata()?.len() != expected_len {
            tracing::debug!(path = %image.display(), "rebuilding truncated compiled image");
            return Ok(None);
        }
        // 不把整个镜像读成 Vec，也不为校验留下另一份常驻映像。
        let mut hash = Sha256::new();
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let read = file.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            hash.update(&buffer[..read]);
        }
        if hash.finalize()[..] != record[8..] {
            tracing::debug!(path = %image.display(), "rebuilding damaged compiled image");
            return Ok(None);
        }
        // SAFETY: 私有目录和文件的属主／权限已检查，摘要确认完整性，兼容键由
        // Wasmtime 提供，且文件锁阻止校验与映射之间的发布／清理。摘要不是认证：
        // 与原持久编译缓存一样，不信任能够修改本用户私有文件的攻击者。
        match unsafe { Component::deserialize_file(engine, image) } {
            Ok(component) => {
                if let Err(error) = record_file.set_modified(SystemTime::now()) {
                    tracing::debug!(%error, "cannot update compiled image cache recency");
                }
                Ok(Some(component))
            }
            Err(error) => {
                tracing::warn!(%error, "cached compiled image cannot be loaded; rebuilding from Wasm");
                Ok(None)
            }
        }
    }

    fn publish(&self, destination: &Path, bytes: &[u8], executable_image: bool) -> io::Result<()> {
        // 校验记录缺失或截断时，读取路径尚未检查镜像；发布也不能覆盖不可信目录项。
        inspect_existing(destination)?;
        let mut temporary = tempfile::Builder::new()
            .prefix(".compile-")
            .tempfile_in(&self.directory)?;
        temporary.write_all(bytes)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            temporary
                .as_file()
                .set_permissions(fs::Permissions::from_mode(if executable_image {
                    0o400
                } else {
                    0o600
                }))?;
        }
        #[cfg(not(unix))]
        let _ = executable_image;
        temporary.as_file().sync_all()?;
        temporary
            .persist(destination)
            .map_err(|error| error.error)?;
        Ok(())
    }

    fn prune(&self, current: &Path, soft_bytes: u64, soft_entries: usize) -> io::Result<()> {
        let mut entries = Vec::new();
        let mut bytes = 0_u64;
        for entry in fs::read_dir(&self.directory)? {
            let entry = entry?;
            let path = entry.path();
            let metadata = entry.metadata()?;
            if !metadata.is_file() || metadata.file_type().is_symlink() {
                continue;
            }
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if name.starts_with(".compile-") {
                // 已持有目录锁，没有另一发布者正在写；这些仅可能是崩溃遗留。
                validate_file(&metadata)?;
                fs::remove_file(path)?;
                continue;
            }
            if !is_image_name(name) {
                continue;
            }
            validate_file(&metadata)?;
            let modified = match fs::symlink_metadata(path.with_extension("sha256")) {
                Ok(metadata) => {
                    validate_file(&metadata)?;
                    metadata.modified()?
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => metadata.modified()?,
                Err(error) => return Err(error),
            };
            bytes = bytes.saturating_add(metadata.len());
            entries.push((modified, path, metadata.len()));
        }
        if bytes <= soft_bytes && entries.len() <= soft_entries {
            return Ok(());
        }
        entries.sort_unstable_by_key(|(modified, _, _)| *modified);
        let target_bytes = soft_bytes * TARGET_PERCENT / 100;
        let target_entries = soft_entries * TARGET_PERCENT as usize / 100;
        let mut count = entries.len();
        for (_, path, size) in entries {
            if bytes <= target_bytes && count <= target_entries {
                break;
            }
            // 单个大镜像允许超过软容量；不能刚发布就令它在下一调用重新编译。
            if path == current {
                continue;
            }
            match fs::remove_file(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => {
                    tracing::debug!(%error, path = %path.display(), "cannot evict compiled image; it may still be mapped");
                    continue;
                }
            }
            match fs::remove_file(path.with_extension("sha256")) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
            bytes = bytes.saturating_sub(size);
            count -= 1;
        }
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn cache_hits(&self) -> usize {
        self.hits.load(std::sync::atomic::Ordering::Relaxed)
    }

    #[cfg(test)]
    pub(super) fn cache_misses(&self) -> usize {
        self.misses.load(std::sync::atomic::Ordering::Relaxed)
    }
}

fn is_image_name(name: &str) -> bool {
    let Some(stem) = name
        .strip_prefix("v1-")
        .and_then(|name| name.strip_suffix(".cwasm"))
    else {
        return false;
    };
    let Some((engine, wasm)) = stem.split_once('-') else {
        return false;
    };
    engine.len() == 16
        && wasm.len() == 64
        && engine
            .bytes()
            .chain(wasm.bytes())
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn inspect_existing(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => validate_file(&metadata),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn open_trusted(path: &Path, writable: bool) -> io::Result<Option<File>> {
    inspect_existing(path)?;
    let mut options = OpenOptions::new();
    options.read(true).write(writable);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    match options.open(path) {
        Ok(file) => {
            validate_file(&file.metadata()?)?;
            Ok(Some(file))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn validate_file(metadata: &Metadata) -> io::Result<()> {
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(io::Error::other(
            "compiled cache entry is not a regular file",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        // SAFETY: geteuid has no preconditions and touches no memory.
        if metadata.uid() != unsafe { libc::geteuid() }
            || metadata.permissions().mode() & 0o022 != 0
            || metadata.nlink() != 1
        {
            return Err(io::Error::other(
                "compiled cache entry has an untrusted owner, mode, or hard link",
            ));
        }
    }
    Ok(())
}

fn invalid_component(error: wasmtime::Error) -> LoadError {
    LoadError::InvalidComponent(error.to_string())
}

fn deserialize_bytes(engine: &Engine, bytes: &[u8]) -> Result<Component, LoadError> {
    // SAFETY: bytes were just produced by this Engine's CodeBuilder.
    unsafe { Component::deserialize(engine, bytes) }.map_err(invalid_component)
}
