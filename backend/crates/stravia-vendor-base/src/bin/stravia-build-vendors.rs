use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, ensure};
use serde::Serialize;
use sha2::{Digest, Sha256};

const COMPONENTS: &[(&str, &str)] = &[
    ("base", "stravia-vendor-base"),
    ("openai-codex", "stravia-vendor-codex"),
    ("xai-grok", "stravia-vendor-grok"),
    ("command-code", "stravia-vendor-command-code"),
    ("devin", "stravia-vendor-devin"),
];

#[derive(Serialize)]
struct ComponentRecord<'a> {
    vendor_id: &'a str,
    version: &'a str,
    file: String,
    sha256: String,
}

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let all = match args.next().as_deref() {
        None => false,
        Some("--all") => true,
        Some(argument) => anyhow::bail!("unknown argument: {argument}; expected --all"),
    };
    ensure!(args.next().is_none(), "expected at most one --all argument");
    let components = if all { COMPONENTS } else { &COMPONENTS[..1] };
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .context("vendor crate is outside the expected workspace layout")?;
    let output = root.join(if all {
        "target/vendor-plugins-all"
    } else {
        "target/vendor-plugins"
    });
    // 不使用父 cargo run 的 target 锁；guest 构建也不能继承本机链接器参数。
    let guest_target = root.join("target/vendor-guest-build");
    fs::create_dir_all(&output)?;
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let mut command = Command::new(cargo);
    command
        .current_dir(root)
        .args([
            "build",
            "--locked",
            "--release",
            "--lib",
            "--target",
            "wasm32-wasip2",
            "--target-dir",
        ])
        .arg(&guest_target)
        .env_remove("RUSTFLAGS")
        .env_remove("CARGO_ENCODED_RUSTFLAGS");
    for &(_, package) in components {
        command.args(["--package", package]);
    }
    let status = command
        .status()
        .context("cannot start vendor component build")?;
    ensure!(status.success(), "vendor component build failed");

    let mut records = Vec::with_capacity(components.len());
    for &(vendor_id, package) in components {
        let source = guest_target
            .join("wasm32-wasip2/release")
            .join(format!("{}.wasm", package.replace('-', "_")));
        let artifact_digest = digest(&source)?;
        let filename = format!("{vendor_id}-{artifact_digest}.wasm");
        let destination = output.join(&filename);
        if !destination.exists() || digest(&destination)? != artifact_digest {
            let pending = PendingFile::new(&output);
            fs::copy(&source, &pending.path)?;
            OpenOptions::new()
                .write(true)
                .open(&pending.path)?
                .sync_all()?;
            fs::rename(&pending.path, &destination)?;
        }
        records.push(ComponentRecord {
            vendor_id,
            version: env!("CARGO_PKG_VERSION"),
            file: filename,
            sha256: artifact_digest,
        });
    }
    // 全部成功后才发布索引，旧索引始终指向完整、不可变的产物集合。
    let pending = PendingFile::new(&output);
    let mut file = File::create(&pending.path)?;
    serde_json::to_writer_pretty(&mut file, &records)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    drop(file);
    fs::rename(&pending.path, output.join("manifest.json"))?;
    println!(
        "Built {} self-contained vendor components in {}",
        records.len(),
        output.display()
    );
    Ok(())
}

fn digest(path: &Path) -> anyhow::Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let length = file.read(&mut buffer)?;
        if length == 0 {
            break;
        }
        hasher.update(&buffer[..length]);
    }
    Ok(stravia_runtime_contract::protocol::ir::canonical::hash_hex(
        &hasher.finalize().into(),
    ))
}

struct PendingFile {
    path: PathBuf,
}

impl PendingFile {
    fn new(directory: &Path) -> Self {
        Self {
            path: directory.join(format!(".pending-{}", uuid::Uuid::new_v4())),
        }
    }
}

impl Drop for PendingFile {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_file(&self.path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            eprintln!(
                "could not remove pending component file {}: {error}",
                self.path.display()
            );
        }
    }
}
