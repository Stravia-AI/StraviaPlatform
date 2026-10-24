use std::error::Error;
use std::path::PathBuf;

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

fn base_component() -> Vec<u8> {
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

fn metrics(runtime: &VendorRuntime) -> (usize, usize) {
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
