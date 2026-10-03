use serde::{Deserialize, Serialize};
use std::collections::HashSet;

pub const SETTINGS_KEY: &str = "rpm_admission";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RpmConfig {
    pub preferred_wait_ms: u64,
    pub total_wait_ms: u64,
    pub queue_capacity: usize,
    pub destinations: Vec<DestinationRpmLimit>,
}

impl Default for RpmConfig {
    fn default() -> Self {
        Self {
            preferred_wait_ms: 5_000,
            total_wait_ms: 30_000,
            queue_capacity: 128,
            destinations: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DestinationRpmLimit {
    pub provider_id: String,
    pub model: String,
    pub rpm_limit: Option<i32>,
}

impl RpmConfig {
    pub(crate) fn from_setting(value: Option<&str>) -> anyhow::Result<Self> {
        let config: Self = value
            .map(serde_json::from_str)
            .transpose()?
            .unwrap_or_default();
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.preferred_wait_ms <= self.total_wait_ms,
            "preferred_wait_ms must not exceed total_wait_ms"
        );
        anyhow::ensure!(
            self.total_wait_ms <= i64::MAX as u64,
            "total_wait_ms exceeds the supported duration"
        );
        anyhow::ensure!(self.queue_capacity > 0, "queue_capacity must be positive");
        let mut destinations = HashSet::new();
        for destination in &self.destinations {
            anyhow::ensure!(
                !destination.provider_id.trim().is_empty()
                    && destination.provider_id.trim() == destination.provider_id,
                "provider_id must be nonempty and trimmed"
            );
            anyhow::ensure!(
                !destination.model.trim().is_empty()
                    && destination.model.trim() == destination.model,
                "model must be nonempty and trimmed"
            );
            anyhow::ensure!(
                destination.rpm_limit.is_none_or(|limit| limit > 0),
                "rpm_limit must be positive or null"
            );
            anyhow::ensure!(
                destinations.insert((&destination.provider_id, &destination.model)),
                "duplicate destination RPM configuration"
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_shared_capacity_fields_are_rejected() {
        for setting in [
            r#"{"pools":[]}"#,
            r#"{"destinations":[{"provider_id":"p","model":"m","rpm_limit":1,"rpm_pool_id":null}]}"#,
        ] {
            assert!(RpmConfig::from_setting(Some(setting)).is_err());
        }
    }
}
