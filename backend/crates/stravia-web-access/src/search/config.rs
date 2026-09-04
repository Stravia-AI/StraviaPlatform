use std::{
    collections::HashMap,
    fs,
    path::Path,
    sync::{Arc, LazyLock},
};

use serde::Deserialize;
use tracing::info;

use crate::search::engines::Engine;

impl Default for Config {
    fn default() -> Self {
        Config {
            engines: Arc::new(EnginesConfig::default()),
            urls: UrlsConfig {
                replace: vec![(
                    HostAndPath::new("minecraft.fandom.com/wiki/"),
                    HostAndPath::new("minecraft.wiki/w/"),
                )],
                weight: vec![],
            },
        }
    }
}

impl Default for EnginesConfig {
    fn default() -> Self {
        use toml::value::Value;

        let mut map = HashMap::new();
        // engines are enabled by default, so engines that aren't listed here are
        // enabled

        // main search engines
        map.insert(Engine::Google, EngineConfig::new().with_weight(1.05));
        map.insert(Engine::Bing, EngineConfig::new().with_weight(1.0));
        map.insert(Engine::Brave, EngineConfig::new().with_weight(1.25));

        // additional search engines
        map.insert(
            Engine::GoogleScholar,
            EngineConfig::new().with_weight(0.50).disabled(),
        );
        map.insert(
            Engine::Baidu,
            EngineConfig::new().with_weight(1.0).disabled(),
        );
        map.insert(
            Engine::So360,
            EngineConfig::new().with_weight(0.75).disabled(),
        );
        map.insert(
            Engine::SogouWeixin,
            EngineConfig::new().with_weight(0.50).disabled(),
        );
        // calculators (give them a high weight so they're always the first thing in
        // autocomplete)
        map.insert(Engine::Numbat, EngineConfig::new().with_weight(10.0));
        map.insert(
            Engine::Fend,
            EngineConfig::new().with_weight(10.0).disabled(),
        );

        // other engines
        map.insert(
            Engine::Mdn,
            EngineConfig::new().with_extra(
                vec![("max_sections".to_string(), Value::Integer(1))]
                    .into_iter()
                    .collect(),
            ),
        );

        Self { map }
    }
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            weight: 1.0,
            extra: Default::default(),
        }
    }
}
static DEFAULT_ENGINE_CONFIG_REF: LazyLock<EngineConfig> = LazyLock::new(EngineConfig::default);
impl EngineConfig {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn with_weight(self, weight: f64) -> Self {
        Self { weight, ..self }
    }
    pub fn disabled(self) -> Self {
        Self {
            enabled: false,
            ..self
        }
    }
    pub fn with_extra(self, extra: toml::Table) -> Self {
        Self { extra, ..self }
    }
}

//

#[derive(Debug, Clone)]
pub struct Config {
    // wrapped in an arc to make Config cheaper to clone
    pub engines: Arc<EnginesConfig>,
    pub urls: UrlsConfig,
}

#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct PartialConfig {
    pub engines: Option<PartialEnginesConfig>,
    pub urls: Option<PartialUrlsConfig>,
}

impl Config {
    pub fn overlay(&mut self, partial: PartialConfig) {
        if let Some(partial_engines) = partial.engines {
            let mut engines = self.engines.as_ref().clone();
            engines.overlay(partial_engines);
            self.engines = Arc::new(engines);
        }
        self.urls.overlay(partial.urls.unwrap_or_default());
    }
}

#[cfg(test)]
mod tests {
    use super::{Config, PartialConfig};
    use crate::search::engines::Engine;

    #[test]
    fn rejects_removed_image_search_configuration() {
        assert!(toml::from_str::<PartialConfig>("[image_search]\nenabled = false\n").is_err());
    }

    #[test]
    fn rejects_removed_search_engines() {
        for engine in ["rightdao", "stract", "yep"] {
            let config = format!("[engines]\n{engine} = false\n");
            assert!(toml::from_str::<PartialConfig>(&config).is_err());
        }
    }

    #[test]
    fn enables_360_and_sogou_weixin_search() {
        let partial = toml::from_str(
            r#"
                [engines]
                "360" = true
                sogou_weixin = true
            "#,
        )
        .unwrap();
        let mut config = Config::default();

        config.overlay(partial);

        assert!(config.engines.get(Engine::So360).enabled);
        assert!(config.engines.get(Engine::SogouWeixin).enabled);
    }
}

#[derive(Debug, Clone)]
pub struct EnginesConfig {
    pub map: HashMap<Engine, EngineConfig>,
}

#[derive(Deserialize, Debug, Default)]
pub struct PartialEnginesConfig {
    #[serde(flatten)]
    pub map: HashMap<Engine, PartialDefaultableEngineConfig>,
}

#[derive(Deserialize, Clone, Debug)]
#[serde(untagged)]
pub enum PartialDefaultableEngineConfig {
    Boolean(bool),
    Full(PartialEngineConfig),
}

impl EnginesConfig {
    pub fn overlay(&mut self, partial: PartialEnginesConfig) {
        for (key, value) in partial.map {
            let full = match value {
                PartialDefaultableEngineConfig::Boolean(enabled) => PartialEngineConfig {
                    enabled: Some(enabled),
                    ..Default::default()
                },
                PartialDefaultableEngineConfig::Full(full) => full,
            };
            if let Some(existing) = self.map.get_mut(&key) {
                existing.overlay(full);
            } else {
                let mut new = EngineConfig::default();
                new.overlay(full);
                self.map.insert(key, new);
            }
        }
    }

    pub fn get(&self, engine: Engine) -> &EngineConfig {
        self.map.get(&engine).unwrap_or(&DEFAULT_ENGINE_CONFIG_REF)
    }
}

#[derive(Debug, Clone)]
pub struct EngineConfig {
    pub enabled: bool,
    /// The priority of this engine relative to the other engines.
    pub weight: f64,
    /// Per-engine configs. These are parsed at request time.
    pub extra: toml::Table,
}

#[derive(Deserialize, Clone, Debug, Default)]
pub struct PartialEngineConfig {
    pub enabled: Option<bool>,
    pub weight: Option<f64>,
    #[serde(flatten)]
    pub extra: toml::Table,
}

impl EngineConfig {
    pub fn overlay(&mut self, partial: PartialEngineConfig) {
        self.enabled = partial.enabled.unwrap_or(self.enabled);
        self.weight = partial.weight.unwrap_or(self.weight);
        self.extra.extend(partial.extra);
    }
}

impl Config {
    pub fn read_or_create(config_path: &Path) -> anyhow::Result<Self> {
        let mut config = Config::default();

        if !config_path.exists() {
            info!("No config found, creating one at {config_path:?}");
            let default_config_str = include_str!("../../config-default.toml");
            if let Some(parent_path) = config_path.parent() {
                let _ = fs::create_dir_all(parent_path);
            }
            fs::write(config_path, default_config_str)?;
        }

        let given_config = toml::from_str::<PartialConfig>(&fs::read_to_string(config_path)?)?;
        config.overlay(given_config);
        Ok(config)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct HostAndPath {
    pub host: String,
    pub path: String,
}
impl HostAndPath {
    pub fn new(s: &str) -> Self {
        let (host, path) = s.split_once('/').unwrap_or((s, ""));
        Self {
            host: host.to_owned(),
            path: path.to_owned(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct UrlsConfig {
    pub replace: Vec<(HostAndPath, HostAndPath)>,
    pub weight: Vec<(HostAndPath, f64)>,
}
#[derive(Deserialize, Debug, Default)]
pub struct PartialUrlsConfig {
    #[serde(default)]
    pub replace: HashMap<String, String>,
    #[serde(default)]
    pub weight: HashMap<String, f64>,
}
impl UrlsConfig {
    pub fn overlay(&mut self, partial: PartialUrlsConfig) {
        for (from, to) in partial.replace {
            let from = HostAndPath::new(&from);
            if to.is_empty() {
                // setting the value to an empty string removes it
                let index = self.replace.iter().position(|(u, _)| u == &from);
                // swap_remove is fine because the order of this vec doesn't matter
                self.replace.swap_remove(index.unwrap());
            } else {
                let to = HostAndPath::new(&to);
                self.replace.push((from, to));
            }
        }

        for (url, weight) in partial.weight {
            let url = HostAndPath::new(&url);
            self.weight.push((url, weight));
        }

        // sort by length so that more specific checks are done first
        self.weight.sort_by(|(a, _), (b, _)| {
            let a_len = a.path.len() + a.host.len();
            let b_len = b.path.len() + b.host.len();
            b_len.cmp(&a_len)
        });
    }
}
