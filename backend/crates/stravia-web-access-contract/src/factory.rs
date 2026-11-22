//! Web Provider 扩展注入契约。
//!
//! 开源核心只依赖本模块的 trait 与注册表，不内置具体搜索/抓取适配器实现；
//! 外层宿主（如商业版 Worker 调度层或测试替身）通过 `GatewayBuilder::web_provider_factory`
//! 注册可用类型与适配器工厂。

use std::collections::BTreeMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{ProviderFailure, WebProviderAdapter};

/// 出站 HTTP 请求的代理模式选择。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutboundProxyMode {
    Direct,
    System,
    Explicit { url: String, bypass: String },
}

/// 单个 Web Provider 类型支持的能力集合。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct WebProviderCapabilities {
    pub search: bool,
    pub fetch: bool,
}

/// 已注册 Web Provider 类型的静态描述，供管理面与前端动态发现。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WebProviderKindDescriptor {
    pub kind: String,
    pub capabilities: WebProviderCapabilities,
    pub requires_api_key: bool,
    pub singleton: bool,
    pub allow_delete: bool,
}

/// 持久化 Web Provider 记录传递给适配器工厂的不可变快照。
#[derive(Debug, Clone)]
pub struct WebProviderConfig {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub api_key: Option<String>,
    pub use_proxy: bool,
    pub config_json: Option<Value>,
}

/// 单个 Web Provider 类型的适配器工厂扩展缝。
#[async_trait::async_trait]
pub trait WebProviderFactory: Send + Sync {
    /// 返回该类型的静态描述；`kind` 必须为非空小写标识符。
    fn descriptor(&self) -> WebProviderKindDescriptor;

    /// 创建或更新时校验并可规范化该类型专有配置。
    fn validate_config(
        &self,
        api_key: Option<&str>,
        api_key_supplied: bool,
        _config_supplied: bool,
        _config_json: Option<&mut Value>,
    ) -> Result<(), String> {
        let descriptor = self.descriptor();
        if descriptor.requires_api_key {
            if api_key
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .is_none()
            {
                return Err(format!("API key is required for {}", descriptor.kind));
            }
        } else if api_key_supplied {
            return Err(format!("{} does not accept an API key", descriptor.kind));
        }
        Ok(())
    }

    /// 合并已有配置与新提交的配置；默认整对象替换。
    fn merge_config(&self, _current: Option<&Value>, update: Value) -> Value {
        update
    }

    /// 构造新建记录未提供配置时的默认配置对象；默认 `None`。
    fn default_config(&self) -> Option<Value> {
        None
    }

    /// 导出可安全暴露给管理 API 的非敏感配置视图；默认不暴露原始配置。
    fn public_config_view(&self, _config: Option<&Value>) -> Option<Value> {
        None
    }

    /// 检查运行期前置条件（例如对应 Worker 节点是否在线）；默认可用。
    async fn is_available(&self) -> bool {
        true
    }

    /// 运行期前置条件不满足时返回给管理面的结构化错误码与提示。
    fn unavailable_reason(&self) -> (&'static str, String) {
        (
            "WEB_PROVIDER_UNAVAILABLE",
            format!(
                "Web Provider kind '{}' is currently unavailable",
                self.descriptor().kind
            ),
        )
    }

    /// 根据持久化记录与出站代理配置构造可执行适配器。
    fn build(
        &self,
        config: &WebProviderConfig,
        outbound: OutboundProxyMode,
    ) -> Result<Arc<dyn WebProviderAdapter>, ProviderFailure>;
}

/// 不可变 Web Provider 工厂注册表，在 `GatewayBuilder::build()` 阶段完成装配。
#[derive(Clone, Default)]
pub struct WebProviderRegistry {
    factories: BTreeMap<String, Arc<dyn WebProviderFactory>>,
}

impl WebProviderRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// 注册一个 Web Provider 工厂；拒绝空类型名或重复注册。
    pub fn register(&mut self, factory: Arc<dyn WebProviderFactory>) -> Result<(), String> {
        let descriptor = factory.descriptor();
        let kind = descriptor.kind.trim().to_ascii_lowercase();
        if kind.is_empty() || kind != descriptor.kind {
            return Err(format!(
                "Web Provider kind must be a non-empty lowercase identifier: '{}'",
                descriptor.kind
            ));
        }
        if self.factories.contains_key(&kind) {
            return Err(format!("duplicate Web Provider kind registration: {kind}"));
        }
        self.factories.insert(kind, factory);
        Ok(())
    }

    pub fn get(&self, kind: &str) -> Option<&Arc<dyn WebProviderFactory>> {
        self.factories.get(kind)
    }

    pub fn descriptor(&self, kind: &str) -> Option<WebProviderKindDescriptor> {
        self.factories.get(kind).map(|factory| factory.descriptor())
    }

    pub fn descriptors(&self) -> Vec<WebProviderKindDescriptor> {
        self.factories
            .values()
            .map(|factory| factory.descriptor())
            .collect()
    }

    pub fn capabilities(&self, kind: &str) -> Option<WebProviderCapabilities> {
        self.descriptor(kind)
            .map(|descriptor| descriptor.capabilities)
    }

    pub fn is_empty(&self) -> bool {
        self.factories.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AdapterSuccess, FetchRequest, FetchResult, SearchRequest, SearchResponse};

    struct StubFactory(&'static str);

    #[async_trait::async_trait]
    impl WebProviderFactory for StubFactory {
        fn descriptor(&self) -> WebProviderKindDescriptor {
            WebProviderKindDescriptor {
                kind: self.0.to_string(),
                capabilities: WebProviderCapabilities {
                    search: true,
                    fetch: false,
                },
                requires_api_key: true,
                singleton: false,
                allow_delete: true,
            }
        }

        fn build(
            &self,
            _config: &WebProviderConfig,
            _outbound: OutboundProxyMode,
        ) -> Result<Arc<dyn WebProviderAdapter>, ProviderFailure> {
            struct Noop;
            #[async_trait::async_trait]
            impl WebProviderAdapter for Noop {
                fn supports_search(&self) -> bool {
                    true
                }
                fn supports_fetch(&self) -> bool {
                    false
                }
                async fn search(
                    &self,
                    _request: &SearchRequest,
                ) -> Result<AdapterSuccess<SearchResponse>, ProviderFailure> {
                    Err(ProviderFailure::unavailable("stub"))
                }
                async fn fetch(
                    &self,
                    _request: &FetchRequest,
                ) -> Result<AdapterSuccess<Vec<FetchResult>>, ProviderFailure> {
                    Err(ProviderFailure::unavailable("stub"))
                }
            }
            Ok(Arc::new(Noop))
        }
    }

    #[test]
    fn registry_rejects_empty_non_lowercase_and_duplicate_kinds() {
        let mut registry = WebProviderRegistry::new();
        assert!(registry.is_empty());
        assert!(registry.register(Arc::new(StubFactory(""))).is_err());
        assert!(registry.register(Arc::new(StubFactory("Worker"))).is_err());
        registry.register(Arc::new(StubFactory("worker"))).unwrap();
        assert!(!registry.is_empty());
        assert!(registry.register(Arc::new(StubFactory("worker"))).is_err());
        assert_eq!(registry.descriptors().len(), 1);
        assert_eq!(
            registry.capabilities("worker"),
            Some(WebProviderCapabilities {
                search: true,
                fetch: false,
            })
        );
    }
}
