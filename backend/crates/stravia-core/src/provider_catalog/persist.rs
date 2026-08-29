use super::*;

pub(super) fn load_active_generation(data_dir: &Path) -> anyhow::Result<Option<CatalogSnapshot>> {
    let path = active_manifest_path(data_dir);
    let body = match std::fs::read(&path) {
        Ok(body) => body,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("read active Provider Catalog manifest"),
    };
    let manifest: CatalogManifest =
        serde_json::from_slice(&body).context("decode active Provider Catalog manifest")?;
    let version = CatalogVersion {
        revision: manifest.revision,
        generated_at: manifest.generated_at,
    };
    validate_version(&version)?;
    let directory = generation_directory(data_dir, &version.revision);
    let providers = std::fs::read(directory.join("providers.json"))?;
    let canonical_models = std::fs::read(directory.join("models.json"))?;
    parse_snapshot(&providers, &canonical_models, version)
        .context("parse active Provider Catalog generation")
        .map(Some)
}

pub(super) fn load_scope(
    data_dir: &Path,
    revision: &str,
    provider_id: &str,
) -> anyhow::Result<Option<CatalogProviderScope>> {
    let path = scope_path(data_dir, revision, provider_id);
    match std::fs::read(&path) {
        Ok(body) => parse_scope(&body, revision, provider_id)
            .map(Some)
            .with_context(|| format!("parse Provider Catalog scope cache {}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error)
            .with_context(|| format!("read Provider Catalog scope cache {}", path.display())),
    }
}

pub(super) fn load_verified_scope(
    data_dir: &Path,
    revision: &str,
    provider_id: &str,
) -> anyhow::Result<Option<CatalogProviderScope>> {
    match load_scope(data_dir, revision, provider_id) {
        Ok(scope) => Ok(scope),
        Err(error) => {
            let path = scope_path(data_dir, revision, provider_id);
            tracing::warn!(
                error = %error,
                path = %path.display(),
                "discarding invalid Provider Catalog scope cache"
            );
            match std::fs::remove_file(&path) {
                Ok(()) => Ok(None),
                Err(remove_error) if remove_error.kind() == std::io::ErrorKind::NotFound => {
                    Ok(None)
                }
                Err(remove_error) => Err(remove_error).with_context(|| {
                    format!(
                        "discard invalid Provider Catalog scope cache {}",
                        path.display()
                    )
                }),
            }
        }
    }
}

pub(super) fn persist_generation(
    data_dir: &Path,
    snapshot: &CatalogSnapshot,
) -> anyhow::Result<()> {
    let directory = generation_directory(data_dir, &snapshot.version.revision);
    atomic_write(
        &directory.join("providers.json"),
        &serde_json::to_vec(&snapshot.providers_raw)?,
    )?;
    atomic_write(
        &directory.join("models.json"),
        &serde_json::to_vec(&snapshot.canonical_models)?,
    )?;
    let manifest = CatalogManifest {
        revision: snapshot.version.revision.clone(),
        generated_at: snapshot.version.generated_at.clone(),
    };
    atomic_write(
        &active_manifest_path(data_dir),
        &serde_json::to_vec(&manifest)?,
    )
}

pub(super) fn persist_scope(data_dir: &Path, scope: &CatalogProviderScope) -> anyhow::Result<()> {
    let raw: BTreeMap<_, _> = scope
        .models
        .iter()
        .map(|source| (model_source_id(source).to_string(), source.metadata.clone()))
        .collect();
    atomic_write(
        &scope_path(data_dir, &scope.revision, &scope.provider_id),
        &serde_json::to_vec(&raw)?,
    )
}

pub(super) fn active_manifest_path(data_dir: &Path) -> PathBuf {
    data_dir.join(CACHE_DIRECTORY).join(ACTIVE_MANIFEST_FILE)
}

pub(super) fn generation_directory(data_dir: &Path, revision: &str) -> PathBuf {
    data_dir
        .join(CACHE_DIRECTORY)
        .join(GENERATIONS_DIRECTORY)
        .join(revision)
}

pub(super) fn scope_path(data_dir: &Path, revision: &str, provider_id: &str) -> PathBuf {
    data_dir
        .join(CACHE_DIRECTORY)
        .join(SCOPES_DIRECTORY)
        .join(revision)
        .join(format!("{provider_id}.json"))
}

pub(super) fn logo_path(data_dir: &Path, provider_id: &str) -> PathBuf {
    data_dir
        .join(CACHE_DIRECTORY)
        .join(LOGO_DIRECTORY)
        .join(format!("{provider_id}.svg"))
}

pub(super) fn atomic_write(path: &Path, body: &[u8]) -> anyhow::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("cache path has no parent"))?;
    std::fs::create_dir_all(parent)?;
    let temporary = path.with_extension(format!(
        "{}.{}.tmp",
        std::process::id(),
        TEMP_FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed),
    ));
    std::fs::write(&temporary, body)?;
    if let Err(error) = replace_file_atomically(&temporary, path) {
        let _ = std::fs::remove_file(&temporary);
        return Err(error).with_context(|| format!("activate cache file {}", path.display()));
    }
    Ok(())
}

pub(super) fn replace_file_atomically(temporary: &Path, path: &Path) -> std::io::Result<()> {
    match std::fs::rename(temporary, path) {
        Ok(()) => Ok(()),
        #[cfg(windows)]
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            replace_existing_file_windows(temporary, path)
        }
        Err(error) => Err(error),
    }
}

#[cfg(windows)]
pub(super) fn replace_existing_file_windows(temporary: &Path, path: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;

    let path: Vec<_> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let temporary: Vec<_> = temporary.as_os_str().encode_wide().chain(Some(0)).collect();
    // ReplaceFileW keeps the manifest path continuously bound to either the
    // previous complete generation or the newly validated one.
    let replaced = unsafe {
        replace_file_w(
            path.as_ptr(),
            temporary.as_ptr(),
            std::ptr::null(),
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if replaced == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(windows)]
#[link(name = "Kernel32")]
unsafe extern "system" {
    #[link_name = "ReplaceFileW"]
    fn replace_file_w(
        replaced_file_name: *const u16,
        replacement_file_name: *const u16,
        backup_file_name: *const u16,
        replace_flags: u32,
        exclude: *mut std::ffi::c_void,
        reserved: *mut std::ffi::c_void,
    ) -> i32;
}

pub(super) fn file_is_fresh(path: &Path, ttl: Duration) -> bool {
    std::fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
        .is_some_and(|age| age <= ttl)
}
