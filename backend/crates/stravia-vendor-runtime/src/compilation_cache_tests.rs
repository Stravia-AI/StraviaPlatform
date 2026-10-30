use std::error::Error;
use std::path::PathBuf;

use stravia_runtime_contract::protocol::ir::canonical::hash_bytes;
use wasmtime::component::{Component, Linker};
use wasmtime::{Config, Engine, Store};

use super::compilation_cache::CompilationCache;
use super::{LoadError, VendorRuntime};

fn component_bytes() -> Vec<u8> {
    wat::parse_str(
        r#"(component
             (core module
               (func (export "compiled") (result i32)
                 i32.const 7))
             (import "wasi:untrusted/cache-probe@0.2.13" (instance)))"#,
    )
    .expect("valid component")
}

pub(super) fn base_component() -> Vec<u8> {
    let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../..")
        .join("target/vendor-plugins");
    let manifest_path = directory.join("manifest.json");
    let manifest: serde_json::Value = serde_json::from_slice(
        &std::fs::read(&manifest_path)
            .unwrap_or_else(|error| panic!("read {}: {error}", manifest_path.display())),
    )
    .expect("valid vendor manifest");
    let entry = manifest
        .as_array()
        .expect("manifest entries")
        .iter()
        .find(|entry| entry["vendor_id"] == "base")
        .expect("manifest must contain the real base component");
    let path = directory.join(entry["file"].as_str().expect("base component filename"));
    std::fs::read(&path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
}

async fn load_error(runtime: &VendorRuntime) -> LoadError {
    let bytes = component_bytes();
    match runtime.load(&bytes).await {
        Err(error @ LoadError::ForbiddenImport(_)) => error,
        Err(other) => panic!("expected repeated import validation, got {other:?}"),
        Ok(_) => panic!("invalid component was admitted"),
    }
}

pub(super) fn metrics(runtime: &VendorRuntime) -> (usize, usize) {
    runtime
        .cache
        .as_ref()
        .map(|cache| (cache.cache_hits(), cache.cache_misses()))
        .expect("cache enabled")
}

#[tokio::test]
async fn persistent_cache_reuses_compilation_and_rebuilds_after_deletion()
-> Result<(), Box<dyn Error>> {
    let workspace = tempfile::tempdir()?;
    let cache_directory = workspace.path().join("cache/wasmtime");

    let cold = VendorRuntime::with_cache(&cache_directory)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&cache_directory)?.permissions().mode() & 0o777,
            0o700
        );
    }
    load_error(&cold).await;
    let (hits, misses) = metrics(&cold);
    assert_eq!(hits, 0, "first compilation cannot hit the cache");
    assert!(misses > 0, "first compilation must populate the cache");
    drop(cold);

    // A new Engine over the same directory must reuse the serialized artifact.
    // Import validation still runs after the cache hit.
    let warm = VendorRuntime::with_cache(&cache_directory)?;
    load_error(&warm).await;
    let (hits, misses) = metrics(&warm);
    assert!(hits > 0, "second Engine must use the persistent cache");
    assert_eq!(misses, 0, "warm load must not compile again");
    drop(warm);

    // Cache contents are disposable: deleting them forces a correct rebuild.
    std::fs::remove_dir_all(&cache_directory)?;
    let rebuilt = VendorRuntime::with_cache(&cache_directory)?;
    load_error(&rebuilt).await;
    let (hits, misses) = metrics(&rebuilt);
    assert_eq!(hits, 0);
    assert!(misses > 0, "deleted cache must be repopulated");
    Ok(())
}

#[tokio::test]
async fn corrupted_executable_cache_is_rebuilt_before_deserialization() -> Result<(), Box<dyn Error>>
{
    let workspace = tempfile::tempdir()?;
    let directory = workspace.path().join("images");
    let cold = VendorRuntime::with_cache(&directory)?;
    load_error(&cold).await;
    drop(cold);
    let image = std::fs::read_dir(&directory)?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .find(|path| {
            path.extension()
                .is_some_and(|extension| extension == "cwasm")
        })
        .expect("persistent compilation must publish a directly mappable image");
    let original = std::fs::read(&image)?;
    assert_eq!(&original[..4], b"\x7fELF");
    let mut damaged = original.clone();
    damaged[0] ^= 0xff;
    let replacement = image.with_extension("damaged");
    std::fs::write(&replacement, &damaged)?;
    std::fs::rename(&replacement, &image)?;

    let recovered = VendorRuntime::with_cache(&directory)?;
    // 缓存损坏不是插件准入失败；重新编译后仍必须执行原有 import 检查。
    load_error(&recovered).await;
    assert_eq!(std::fs::read(&image)?, original);
    assert_eq!(metrics(&recovered), (0, 1));
    Ok(())
}

#[tokio::test]
#[ignore = "requires built base component"]
async fn in_memory_compilation_reuses_live_components_and_recompiles_after_release()
-> Result<(), Box<dyn Error>> {
    let workspace = tempfile::tempdir()?;
    let cache_directory = workspace.path().join("cache/wasmtime");
    let runtime = VendorRuntime::with_cache(&cache_directory)?;
    let bytes = base_component();

    let plugin = runtime.load(&bytes).await?;
    let pinned = plugin.with_descriptor(plugin.descriptor().clone());
    drop(plugin);
    let (hits, misses) = metrics(&runtime);

    // The pinned handle keeps the Engine-local Weak entry upgradeable, so a
    // fresh load of the same bytes must not compile at all.
    let reloaded = runtime.load(&bytes).await?;
    assert_eq!(metrics(&runtime), (hits, misses));

    // Releasing every LoadedPlugin expires the Weak entry; the next load must
    // recompile and reuse the persistent artifact instead of memory.
    drop(pinned);
    drop(reloaded);
    let _recompiled = runtime.load(&bytes).await?;
    let (hits_after, misses_after) = metrics(&runtime);
    assert!(hits_after > hits, "expired in-memory entry must recompile");
    assert_eq!(misses_after, misses);
    Ok(())
}

#[tokio::test]
async fn invalid_cache_directories_are_rejected() -> Result<(), Box<dyn Error>> {
    let workspace = tempfile::tempdir()?;
    let file = workspace.path().join("not-a-directory");
    std::fs::write(&file, b"occupied")?;
    assert!(matches!(
        VendorRuntime::with_cache(&file),
        Err(LoadError::CompilationCache(_))
    ));
    assert!(matches!(
        VendorRuntime::with_cache("relative/wasmtime-cache"),
        Err(LoadError::CompilationCache(_))
    ));

    #[cfg(unix)]
    {
        let target = workspace.path().join("outside");
        std::fs::create_dir(&target)?;
        let link = workspace.path().join("cache-link");
        std::os::unix::fs::symlink(&target, &link)?;
        assert!(matches!(
            VendorRuntime::with_cache(&link),
            Err(LoadError::CompilationCache(_))
        ));
        assert!(std::fs::read_dir(&target)?.next().is_none());
    }
    Ok(())
}

#[tokio::test]
async fn persistent_cache_hits_still_reject_invalid_descriptors() -> Result<(), Box<dyn Error>> {
    let workspace = tempfile::tempdir()?;
    let cache_directory = workspace.path().join("cache/wasmtime");
    let bytes = wat::parse_str(
        r#"(component
             (core module (func (export "not-a-descriptor"))))"#,
    )?;

    let cold = VendorRuntime::with_cache(&cache_directory)?;
    assert!(matches!(
        cold.load(&bytes).await,
        Err(LoadError::DescriptorExecution(_))
    ));
    drop(cold);

    let warm = VendorRuntime::with_cache(&cache_directory)?;
    assert!(matches!(
        warm.load(&bytes).await,
        Err(LoadError::DescriptorExecution(_))
    ));
    assert!(
        metrics(&warm).0 > 0,
        "descriptor validation must run after a cache hit"
    );
    Ok(())
}

#[cfg(unix)]
#[test]
fn cache_writable_by_other_users_is_not_trusted() -> Result<(), Box<dyn Error>> {
    use std::os::unix::fs::PermissionsExt;

    let workspace = tempfile::tempdir()?;
    let cache_directory = workspace.path().join("cache");
    std::fs::create_dir(&cache_directory)?;
    std::fs::set_permissions(&cache_directory, std::fs::Permissions::from_mode(0o777))?;
    assert!(matches!(
        VendorRuntime::with_cache(&cache_directory),
        Err(LoadError::CompilationCache(_))
    ));
    assert_eq!(
        std::fs::metadata(&cache_directory)?.permissions().mode() & 0o777,
        0o777
    );
    Ok(())
}

#[cfg(unix)]
#[test]
fn cache_below_a_writable_parent_is_not_trusted() -> Result<(), Box<dyn Error>> {
    use std::os::unix::fs::PermissionsExt;

    let workspace = tempfile::tempdir()?;
    let parent = workspace.path().join("shared");
    let cache_directory = parent.join("wasmtime");
    std::fs::create_dir_all(&cache_directory)?;
    std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o777))?;
    std::fs::set_permissions(&cache_directory, std::fs::Permissions::from_mode(0o700))?;
    assert!(matches!(
        VendorRuntime::with_cache(&cache_directory),
        Err(LoadError::CompilationCache(_))
    ));
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn cache_reuses_artifacts_through_a_trusted_parent_symlink() -> Result<(), Box<dyn Error>> {
    let workspace = tempfile::tempdir()?;
    let target = workspace.path().join("target");
    std::fs::create_dir(&target)?;
    let alias = workspace.path().join("alias");
    std::os::unix::fs::symlink(&target, &alias)?;

    let cold = VendorRuntime::with_cache(alias.join("wasmtime"))?;
    load_error(&cold).await;
    drop(cold);

    let warm = VendorRuntime::with_cache(target.join("wasmtime"))?;
    load_error(&warm).await;
    assert!(
        metrics(&warm).0 > 0,
        "trusted aliases must reuse compilation"
    );
    Ok(())
}

#[cfg(unix)]
#[test]
fn existing_cache_directories_are_restricted_to_the_owner() -> Result<(), Box<dyn Error>> {
    use std::os::unix::fs::PermissionsExt;

    let workspace = tempfile::tempdir()?;
    let cache_directory = workspace.path().join("cache");
    std::fs::create_dir(&cache_directory)?;
    std::fs::set_permissions(&cache_directory, std::fs::Permissions::from_mode(0o755))?;
    let _runtime = VendorRuntime::with_cache(&cache_directory)?;
    let mode = std::fs::metadata(&cache_directory)?.permissions().mode() & 0o777;
    assert_eq!(mode, 0o700);
    Ok(())
}

fn executable_component(value: u32) -> Vec<u8> {
    wat::parse_str(format!(
        r#"(component
            (core module $m (func (export "value") (result i32) i32.const {value}))
            (core instance $i (instantiate $m))
            (func (export "value") (result u32) (canon lift (core func $i "value"))))"#,
    ))
    .expect("valid executable component")
}

fn execute_value(
    engine: &Engine,
    component: &Component,
    fuel: Option<u64>,
) -> wasmtime::Result<u32> {
    let mut store = Store::new(engine, ());
    if let Some(fuel) = fuel {
        store.set_fuel(fuel)?;
    }
    let instance = Linker::<()>::new(engine).instantiate(&mut store, component)?;
    let function = instance.get_typed_func::<(), (u32,)>(&mut store, "value")?;
    let (value,) = function.call(&mut store, ())?;
    Ok(value)
}

#[test]
fn cached_code_preserves_wasm_identity_and_engine_fuel_semantics() -> Result<(), Box<dyn Error>> {
    let workspace = tempfile::tempdir()?;
    let directory = workspace.path().join("images");
    let bytes = executable_component(7);
    let digest = hash_bytes(&bytes);
    let plain_engine = Engine::default();
    let plain = CompilationCache::new(&directory, &plain_engine)?;
    let component = plain.load(&plain_engine, &bytes, &digest)?;
    assert_eq!(execute_value(&plain_engine, &component, None)?, 7);

    let mut config = Config::new();
    config.consume_fuel(true);
    let metered_engine = Engine::new(&config)?;
    let metered = CompilationCache::new(&directory, &metered_engine)?;
    let component = metered.load(&metered_engine, &bytes, &digest)?;
    let exhausted = execute_value(&metered_engine, &component, Some(0)).unwrap_err();
    assert!(matches!(
        exhausted.downcast_ref::<wasmtime::Trap>(),
        Some(wasmtime::Trap::OutOfFuel)
    ));
    assert_eq!(execute_value(&metered_engine, &component, Some(100))?, 7);

    let changed = executable_component(19);
    let component = metered.load(&metered_engine, &changed, &hash_bytes(&changed))?;
    assert_eq!(execute_value(&metered_engine, &component, Some(100))?, 19);
    // 恢复旧身份不能被随后写入的另一个组件或 Engine 配置覆盖。
    let reopened = CompilationCache::new(&directory, &plain_engine)?;
    let original = reopened.load(&plain_engine, &bytes, &digest)?;
    assert_eq!(execute_value(&plain_engine, &original, None)?, 7);
    assert_eq!((reopened.cache_hits(), reopened.cache_misses()), (1, 0));
    Ok(())
}

#[test]
fn independent_engines_publish_one_complete_image_concurrently() -> Result<(), Box<dyn Error>> {
    let workspace = tempfile::tempdir()?;
    let directory = workspace.path().join("images");
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(4));
    let threads = (0..4)
        .map(|_| {
            let directory = directory.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let engine = Engine::default();
                let cache = CompilationCache::new(&directory, &engine).unwrap();
                let bytes = executable_component(31);
                barrier.wait();
                let component = cache.load(&engine, &bytes, &hash_bytes(&bytes)).unwrap();
                assert_eq!(execute_value(&engine, &component, None).unwrap(), 31);
                (cache.cache_hits(), cache.cache_misses())
            })
        })
        .collect::<Vec<_>>();
    let (hits, misses) = threads.into_iter().fold((0, 0), |(hits, misses), thread| {
        let (thread_hits, thread_misses) = thread.join().expect("cache publisher panicked");
        (hits + thread_hits, misses + thread_misses)
    });
    assert_eq!((hits, misses), (3, 1));
    Ok(())
}

#[cfg(unix)]
#[test]
fn cache_eviction_does_not_invalidate_an_executing_image() -> Result<(), Box<dyn Error>> {
    use std::time::{Duration, UNIX_EPOCH};

    let workspace = tempfile::tempdir()?;
    let directory = workspace.path().join("images");
    let engine = Engine::default();
    let cache = CompilationCache::new(&directory, &engine)?;
    let bytes = executable_component(41);
    let resident = cache.load(&engine, &bytes, &hash_bytes(&bytes))?;
    let image = std::fs::read_dir(&directory)?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .find(|path| {
            path.extension()
                .is_some_and(|extension| extension == "cwasm")
        })
        .expect("resident cache image");
    std::fs::OpenOptions::new()
        .write(true)
        .open(image.with_extension("sha256"))?
        .set_modified(UNIX_EPOCH + Duration::from_secs(1))?;

    // 稀疏文件施加磁盘逻辑容量压力，不实际分配半 GiB 测试内存或磁盘块。
    let pressure = directory.join(format!("v1-0000000000000000-{}.cwasm", "0".repeat(64)));
    let file = std::fs::File::create(&pressure)?;
    file.set_len(512 * 1024 * 1024 + 1)?;
    file.set_modified(UNIX_EPOCH + Duration::from_secs(2))?;
    drop(file);
    let next = executable_component(43);
    let published = cache.load(&engine, &next, &hash_bytes(&next))?;
    assert!(!image.exists(), "oldest disk image was not evicted");
    assert!(
        !pressure.exists(),
        "cache did not reclaim the oversized old entry"
    );
    assert_eq!(execute_value(&engine, &resident, None)?, 41);
    assert_eq!(execute_value(&engine, &published, None)?, 43);

    let rebuilt = cache.load(&engine, &bytes, &hash_bytes(&bytes))?;
    assert_eq!(execute_value(&engine, &rebuilt, None)?, 41);
    assert_eq!(execute_value(&engine, &resident, None)?, 41);
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn symlinked_native_cache_is_neither_read_nor_overwritten() -> Result<(), Box<dyn Error>> {
    for record_state in ["valid", "missing", "truncated"] {
        let workspace = tempfile::tempdir()?;
        let directory = workspace.path().join("images");
        let cold = VendorRuntime::with_cache(&directory)?;
        load_error(&cold).await;
        drop(cold);
        let image = std::fs::read_dir(&directory)?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .find(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "cwasm")
            })
            .expect("cached native image");
        match record_state {
            "missing" => std::fs::remove_file(image.with_extension("sha256"))?,
            "truncated" => std::fs::write(image.with_extension("sha256"), b"partial")?,
            _ => {}
        }
        let outside = workspace.path().join("outside-cache");
        std::fs::write(&outside, b"not trusted native code")?;
        std::fs::remove_file(&image)?;
        std::os::unix::fs::symlink(&outside, &image)?;

        let runtime = VendorRuntime::with_cache(&directory)?;
        load_error(&runtime).await;
        assert_eq!(std::fs::read(&outside)?, b"not trusted native code");
        assert!(
            std::fs::symlink_metadata(&image)?.file_type().is_symlink(),
            "untrusted image was replaced with a {record_state} checksum record"
        );
    }
    Ok(())
}
