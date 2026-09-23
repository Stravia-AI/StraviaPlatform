use super::*;

pub(super) fn load_active_generation(data_dir: &Path) -> anyhow::Result<Option<CatalogSnapshot>> {
    let path = active_manifest_path(data_dir);
    let body = match std::fs::read(&path) {
        Ok(body) => body,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("read active Provider Catalog manifest"),
    };
    let manifest: CatalogManifest = match serde_json::from_slice(&body) {
        Ok(manifest) => manifest,
        Err(_) => {
            let legacy: LegacyCatalogManifest =
                serde_json::from_slice(&body).context("decode active Provider Catalog manifest")?;
            let version = CatalogVersion {
                revision: legacy.revision,
                generated_at: legacy.generated_at,
            };
            CatalogManifest {
                providers: version.clone(),
                canonical_models: version,
            }
        }
    };
    validate_version(&manifest.providers)?;
    validate_version(&manifest.canonical_models)?;
    // The two documents advance independently; either side can still be the
    // embedded bootstrap, which has no generation directory on disk.
    let providers = if manifest.providers.revision == BOOTSTRAP_REVISION {
        BUILTIN_PROVIDERS.as_bytes().to_vec()
    } else {
        std::fs::read(
            generation_directory(data_dir, &manifest.providers.revision).join("providers.json"),
        )?
    };
    let canonical_models = if manifest.canonical_models.revision == BOOTSTRAP_REVISION {
        BUILTIN_CANONICAL_MODELS.as_bytes().to_vec()
    } else {
        std::fs::read(
            generation_directory(data_dir, &manifest.canonical_models.revision).join("models.json"),
        )?
    };
    let mut snapshot = parse_snapshot(&providers, &canonical_models, manifest.canonical_models)
        .context("parse active Provider Catalog generation")?;
    snapshot.providers_version = manifest.providers;
    Ok(Some(snapshot))
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

/// Advance the manifest's provider index revision after writing the body.
/// Called under `persist_lock`; a write failure leaves the previous manifest.
pub(super) fn persist_provider_generation(
    data_dir: &Path,
    providers_raw: &Value,
    version: &CatalogVersion,
) -> anyhow::Result<()> {
    atomic_write(
        &generation_directory(data_dir, &version.revision).join("providers.json"),
        &serde_json::to_vec(providers_raw)?,
    )?;
    update_manifest(data_dir, |manifest| {
        manifest.providers = version.clone();
    })
}

/// Advance the manifest's Canonical Model revision after writing the body.
pub(super) fn persist_canonical_generation(
    data_dir: &Path,
    canonical_models: &BTreeMap<String, Value>,
    version: &CatalogVersion,
) -> anyhow::Result<()> {
    atomic_write(
        &generation_directory(data_dir, &version.revision).join("models.json"),
        &serde_json::to_vec(canonical_models)?,
    )?;
    update_manifest(data_dir, |manifest| {
        manifest.canonical_models = version.clone();
    })
}

fn update_manifest(
    data_dir: &Path,
    update: impl FnOnce(&mut CatalogManifest),
) -> anyhow::Result<()> {
    let path = active_manifest_path(data_dir);
    // A missing manifest is a fresh instance; an unreadable or unparseable
    // one is rewritten from bootstrap revisions so a successful generation
    // write is never blocked by stale pointer state.
    let mut manifest = match std::fs::read(&path) {
        Ok(body) => match serde_json::from_slice::<CatalogManifest>(&body)
            .ok()
            .or_else(|| {
                serde_json::from_slice::<LegacyCatalogManifest>(&body)
                    .ok()
                    .map(|legacy| {
                        let version = CatalogVersion {
                            revision: legacy.revision,
                            generated_at: legacy.generated_at,
                        };
                        CatalogManifest {
                            providers: version.clone(),
                            canonical_models: version,
                        }
                    })
            }) {
            Some(manifest) => manifest,
            None => {
                tracing::warn!(
                    path = %path.display(),
                    "discarding unparseable Provider Catalog manifest"
                );
                bootstrap_manifest()
            }
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => bootstrap_manifest(),
        Err(error) => {
            tracing::warn!(
                error = %error,
                path = %path.display(),
                "discarding unreadable Provider Catalog manifest"
            );
            bootstrap_manifest()
        }
    };
    update(&mut manifest);
    atomic_write(&path, &serde_json::to_vec(&manifest)?)
}

fn bootstrap_manifest() -> CatalogManifest {
    CatalogManifest {
        providers: CatalogVersion {
            revision: BOOTSTRAP_REVISION.to_owned(),
            generated_at: BOOTSTRAP_GENERATED_AT.to_owned(),
        },
        canonical_models: CatalogVersion {
            revision: BOOTSTRAP_REVISION.to_owned(),
            generated_at: BOOTSTRAP_GENERATED_AT.to_owned(),
        },
    }
}

/// Catalog profiles retired by a confirmed snapshot. The guest owns profile
/// derivation, but only the host knows which identities survive restart —
/// the retired set is host-persisted state, not guest data.
pub(super) fn load_retired_profiles(
    data_dir: &Path,
) -> anyhow::Result<Vec<stravia_vendor_sdk::ProviderDescriptor>> {
    let path = retired_profiles_path(data_dir);
    match std::fs::read(&path) {
        Ok(body) => serde_json::from_slice(&body)
            .with_context(|| format!("parse retired Provider Catalog profiles {}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error)
            .with_context(|| format!("read retired Provider Catalog profiles {}", path.display())),
    }
}

pub(super) fn persist_retired_profiles(
    data_dir: &Path,
    profiles: &[stravia_vendor_sdk::ProviderDescriptor],
) -> anyhow::Result<()> {
    let path = retired_profiles_path(data_dir);
    if profiles.is_empty() {
        match std::fs::remove_file(&path) {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("clear retired Provider Catalog profiles {}", path.display())
                });
            }
        }
    }
    atomic_write(&path, &serde_json::to_vec(profiles)?)
}

pub(super) fn retired_profiles_path(data_dir: &Path) -> PathBuf {
    data_dir
        .join(CACHE_DIRECTORY)
        .join("retired-providers.json")
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

/// Favicon cache is keyed by the full website origin (scheme + host + port),
/// sanitized to a single safe file name.
pub(super) fn favicon_path(data_dir: &Path, origin: &str) -> anyhow::Result<PathBuf> {
    let name: String = origin
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    if name.is_empty() || name.len() > 120 {
        anyhow::bail!("provider website origin is invalid");
    }
    Ok(data_dir
        .join(CACHE_DIRECTORY)
        .join(FAVICON_DIRECTORY)
        .join(name))
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
        if let Err(cleanup_error) = std::fs::remove_file(&temporary) {
            tracing::debug!(%cleanup_error, "failed to remove stale provider cache file");
        }
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
