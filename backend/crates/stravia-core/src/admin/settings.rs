use super::*;

impl AdminService {
    // ── Settings ──

    pub async fn get_setting(&self, key: &str) -> anyhow::Result<Option<String>> {
        if key == "artifact_upload_signing_key" {
            anyhow::bail!("reserved internal setting");
        }
        let value = self.gw.storage.settings().get(key).await?;
        if key == "artifact_settings" {
            return value
                .map(|value| {
                    let settings: crate::agent::artifact::ArtifactSettings =
                        serde_json::from_str(&value)?;
                    settings.validate()?;
                    Ok(serde_json::to_string(&settings)?)
                })
                .transpose();
        }
        Ok(value)
    }

    pub async fn set_setting(&self, key: &str, value: &str) -> anyhow::Result<()> {
        if key == "artifact_upload_signing_key" {
            anyhow::bail!("reserved internal setting");
        }
        if key == crate::media_generation::config::SETTINGS_KEY {
            self.update_media_generation_config(serde_json::from_str(value)?)
                .await?;
            return Ok(());
        }
        if key == "artifact_settings" {
            // Serialize the persisted configuration and the live backend cutover together.
            static SAVE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
            let _save = SAVE.lock().await;
            let settings: crate::agent::artifact::ArtifactSettings = serde_json::from_str(value)?;
            settings.validate_for_save()?;
            let previous = self.gw.storage.settings().get(key).await?;
            let previous: crate::agent::artifact::ArtifactSettings = previous
                .as_deref()
                .map(serde_json::from_str)
                .transpose()?
                .unwrap_or_default();
            let store = self
                .gw
                .artifact_store()
                .ok_or_else(|| anyhow::anyhow!("artifact storage unavailable"))?;
            store.configure(&settings).await?;
            if let Err(error) = self
                .gw
                .storage
                .settings()
                .set(key, &serde_json::to_string(&settings)?)
                .await
            {
                store.configure(&previous).await?;
                return Err(error);
            }
            return Ok(());
        }
        if key == "reversible_redaction_enabled" && !matches!(value, "true" | "false") {
            anyhow::bail!("reversible_redaction_enabled must be true or false");
        }
        let retention_days =
            if key == "log_retention_days" {
                Some(value.parse::<u32>().map_err(|_| {
                    anyhow::anyhow!("log_retention_days must be a nonnegative integer")
                })?)
            } else {
                None
            };
        self.gw.storage.settings().set(key, value).await?;
        if let Some(days) = retention_days {
            self.gw.observation.set_retention_days(days).await?;
        }
        Ok(())
    }
}
