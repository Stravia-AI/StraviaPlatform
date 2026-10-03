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
    pub pools: Vec<RpmPool>,
}

impl Default for RpmConfig {
    fn default() -> Self {
        Self {
            preferred_wait_ms: 5_000,
            total_wait_ms: 30_000,
            queue_capacity: 128,
            destinations: Vec::new(),
            pools: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DestinationRpmLimit {
    pub provider_id: String,
    pub model: String,
    pub rpm_limit: Option<i32>,
    #[serde(default)]
    pub rpm_pool_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RpmPool {
    pub id: String,
    pub name: String,
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
        let mut pools = HashSet::new();
        for pool in &self.pools {
            anyhow::ensure!(
                !pool.id.trim().is_empty() && pool.id.trim() == pool.id,
                "pool id must be nonempty and trimmed"
            );
            anyhow::ensure!(
                !pool.name.trim().is_empty() && pool.name.trim() == pool.name,
                "pool name must be nonempty and trimmed"
            );
            anyhow::ensure!(
                pool.rpm_limit.is_none_or(|limit| limit > 0),
                "rpm_limit must be positive or null"
            );
            anyhow::ensure!(pools.insert(&pool.id), "duplicate RPM pool id");
        }
        for destination in &self.destinations {
            if let Some(pool_id) = &destination.rpm_pool_id {
                anyhow::ensure!(
                    destination.rpm_limit.is_none(),
                    "destination must not set both rpm_limit and rpm_pool_id"
                );
                anyhow::ensure!(pools.contains(pool_id), "unknown RPM pool: {pool_id}");
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn destination_pool_membership_requires_one_existing_counting_policy() {
        let mut config = RpmConfig {
            destinations: vec![DestinationRpmLimit {
                provider_id: "provider".into(),
                model: "model".into(),
                rpm_limit: None,
                rpm_pool_id: Some("shared".into()),
            }],
            pools: vec![RpmPool {
                id: "shared".into(),
                name: "Shared".into(),
                rpm_limit: Some(2),
            }],
            ..Default::default()
        };
        config.validate().unwrap();
        config.destinations[0].rpm_limit = Some(1);
        assert!(config.validate().is_err());
        config.destinations[0].rpm_limit = None;
        config.pools.clear();
        assert!(config.validate().is_err());
        config.destinations[0].rpm_pool_id = None;
        config.validate().unwrap();
    }

    #[test]
    fn omitted_destination_pool_deserializes_as_unbound() {
        let config: RpmConfig = serde_json::from_str(
            r#"{"destinations":[{"provider_id":"p","model":"m","rpm_limit":1}]}"#,
        )
        .unwrap();
        assert!(config.destinations[0].rpm_pool_id.is_none());
        config.validate().unwrap();
    }
}
