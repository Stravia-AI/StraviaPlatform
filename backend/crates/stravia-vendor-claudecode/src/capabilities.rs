//! Anthropic `GET /v1/models` 返回的型号能力：发现阶段翻译成 Stravia 的模型元数据，
//! 推理阶段按同一份数据约束请求。接口只覆盖思考类型、effort 档位、`max_tokens` 与
//! 上下文管理策略；其余型号规则见 `thinking.rs` 的静态回退。

use std::collections::BTreeMap;

use serde_json::{Map, Value, json};
use stravia_vendor_sdk::{DiscoveredModel, ProviderSnapshot};

use crate::thinking;

/// 原样保存在 Provider Model 元数据扩展里的 `capabilities` 对象。推理时从
/// `ProviderSnapshot.model_metadata` 读回，避免每次请求再访问上游。
pub(crate) const METADATA_KEY: &str = "anthropic_capabilities";

const EFFORTS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];
const CONTEXT_EDIT_KEYS: [&str; 3] = [
    "clear_thinking_20251015",
    "clear_tool_uses_20250919",
    "compact_20260112",
];

/// 已知字段为 `None` 表示上游没有给出，调用方回退到静态规则。
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct ModelCapabilities {
    pub thinking: Option<bool>,
    pub adaptive: Option<bool>,
    pub enabled: Option<bool>,
    /// 受支持的 effort 档位，升序；`Some(空)` 表示型号不支持 effort。
    pub efforts: Option<Vec<&'static str>>,
    pub max_tokens: Option<u64>,
    /// `context_management.supported`。
    pub context_management: Option<bool>,
    /// 各上下文编辑策略是否受支持，键为编辑类型。
    pub context_edits: BTreeMap<&'static str, bool>,
}

impl ModelCapabilities {
    pub(crate) fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// 解析 `/v1/models` 条目里的 `capabilities` 对象与顶层 `max_tokens`。
    pub(crate) fn from_wire(capabilities: &Value, max_tokens: Option<u64>) -> Self {
        let supported = |path: &str| {
            capabilities
                .pointer(&format!("{path}/supported"))
                .and_then(Value::as_bool)
        };
        let efforts = match supported("/effort") {
            Some(false) => Some(Vec::new()),
            Some(true) => {
                let levels = EFFORTS
                    .into_iter()
                    .filter(|level| supported(&format!("/effort/{level}")) == Some(true))
                    .collect::<Vec<_>>();
                (!levels.is_empty()).then_some(levels)
            }
            None => None,
        };
        Self {
            thinking: supported("/thinking"),
            adaptive: supported("/thinking/types/adaptive"),
            enabled: supported("/thinking/types/enabled"),
            efforts,
            max_tokens,
            context_management: supported("/context_management"),
            context_edits: CONTEXT_EDIT_KEYS
                .into_iter()
                .filter_map(|key| Some((key, supported(&format!("/context_management/{key}"))?)))
                .collect(),
        }
    }

    /// 读取宿主在推理时传入的型号元数据；没有同步过发现数据时返回空能力。
    pub(crate) fn from_snapshot(provider: &ProviderSnapshot) -> Self {
        let Some(extensions) = provider
            .model_metadata
            .as_ref()
            .map(|metadata| &metadata.extensions)
        else {
            return Self::default();
        };
        let max_tokens = extensions
            .get("limit")
            .and_then(|limit| limit.get("output"))
            .and_then(Value::as_u64);
        match extensions.get(METADATA_KEY) {
            Some(capabilities) => Self::from_wire(capabilities, max_tokens),
            None => Self::default(),
        }
    }

    /// 型号声明不支持的上下文编辑（或整个上下文管理）必须从请求中移除，否则上游 400。
    fn rejects_context_edit(&self, kind: &str) -> bool {
        self.context_management == Some(false)
            || self
                .context_edits
                .iter()
                .any(|(key, supported)| *key == kind && !supported)
    }
}

/// 把发现到的能力翻译成 Stravia 元数据：思考类型与 effort 决定档位表，
/// `max_tokens`/`max_input_tokens` 决定上限，输入与结构化输出能力进入能力列表。
pub(crate) fn discovered_model(entry: &Value) -> Option<DiscoveredModel> {
    let id = entry.get("id").and_then(Value::as_str)?.trim();
    if id.is_empty() {
        return None;
    }
    let mut model = DiscoveredModel {
        id: id.to_owned(),
        display_name: entry
            .get("display_name")
            .and_then(Value::as_str)
            .unwrap_or(id)
            .to_owned(),
        family: None,
        selector: None,
        capabilities: Vec::new(),
        metadata: BTreeMap::new(),
    };
    let max_input = entry.get("max_input_tokens").and_then(Value::as_u64);
    let max_output = entry.get("max_tokens").and_then(Value::as_u64);
    if max_input.is_some() || max_output.is_some() {
        let mut limit = Map::new();
        if let Some(context) = max_input {
            limit.insert("context".into(), json!(context));
        }
        if let Some(output) = max_output {
            limit.insert("output".into(), json!(output));
        }
        model.metadata.insert("limit".into(), Value::Object(limit));
    }
    let Some(capabilities) = entry.get("capabilities").filter(|value| value.is_object()) else {
        return Some(model);
    };
    let caps = ModelCapabilities::from_wire(capabilities, max_output);
    let flag = |path: &str| {
        capabilities
            .pointer(&format!("{path}/supported"))
            .and_then(Value::as_bool)
    };
    for (name, path) in [
        ("image_input", "/image_input"),
        ("structured_output", "/structured_outputs"),
        ("reasoning", "/thinking"),
    ] {
        if flag(path) == Some(true) {
            model.capabilities.push(name.to_owned());
        }
    }
    if let Some(reasoning) = caps.thinking {
        model
            .metadata
            .insert("reasoning".into(), Value::Bool(reasoning));
    }
    if let Some(options) = thinking::reasoning_options(id, &caps) {
        model
            .metadata
            .insert("reasoning_options".into(), Value::Array(options));
    }
    model
        .metadata
        .insert(METADATA_KEY.into(), capabilities.clone());
    Some(model)
}

/// 按型号能力约束请求：`max_tokens` 不超过型号上限，移除型号不支持的上下文编辑。
/// 必须在思考控制改写之后调用，因为手动预算模式会抬高 `max_tokens`。
pub(crate) fn constrain(object: &mut Map<String, Value>, caps: &ModelCapabilities) {
    if let Some(cap) = caps.max_tokens
        && object
            .get("max_tokens")
            .and_then(Value::as_u64)
            .is_some_and(|requested| requested > cap)
    {
        object.insert("max_tokens".into(), json!(cap));
    }
    let Some(management) = object
        .get_mut("context_management")
        .and_then(Value::as_object_mut)
    else {
        return;
    };
    if let Some(edits) = management.get_mut("edits").and_then(Value::as_array_mut) {
        edits.retain(|edit| {
            edit.get("type")
                .and_then(Value::as_str)
                .is_none_or(|kind| !caps.rejects_context_edit(kind))
        });
        if edits.is_empty() {
            object.remove("context_management");
        }
    } else if caps.context_management == Some(false) {
        object.remove("context_management");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opus_5_5() -> Value {
        json!({
            "thinking": {"supported": true, "types": {
                "adaptive": {"supported": true}, "enabled": {"supported": false}}},
            "effort": {
                "supported": true,
                "low": {"supported": true}, "medium": {"supported": true},
                "high": {"supported": true}, "xhigh": {"supported": true},
                "max": {"supported": true}},
            "image_input": {"supported": true},
            "structured_outputs": {"supported": true},
            "context_management": {
                "supported": true,
                "clear_thinking_20251015": {"supported": true},
                "clear_tool_uses_20250919": {"supported": false},
                "compact_20260112": null},
        })
    }

    #[test]
    fn wire_capabilities_are_parsed_and_unlisted_effort_levels_are_dropped() {
        let mut wire = opus_5_5();
        wire["effort"]["xhigh"] = json!(null);
        let caps = ModelCapabilities::from_wire(&wire, Some(128_000));
        assert_eq!(caps.adaptive, Some(true));
        assert_eq!(caps.enabled, Some(false));
        assert_eq!(caps.efforts, Some(vec!["low", "medium", "high", "max"]));
        assert_eq!(caps.max_tokens, Some(128_000));
        assert_eq!(
            caps.context_edits.get("clear_tool_uses_20250919"),
            Some(&false)
        );
        assert!(!caps.context_edits.contains_key("compact_20260112"));

        let unsupported = json!({"effort": {"supported": false}});
        assert_eq!(
            ModelCapabilities::from_wire(&unsupported, None).efforts,
            Some(Vec::new())
        );
        assert!(ModelCapabilities::from_wire(&json!({}), None).is_empty());
    }

    #[test]
    fn discovery_publishes_limits_effort_levels_and_raw_capabilities() {
        let model = discovered_model(&json!({
            "id": "claude-opus-5-5",
            "display_name": "Claude Opus 5.5",
            "max_input_tokens": 1_000_000,
            "max_tokens": 128_000,
            "capabilities": opus_5_5(),
        }))
        .unwrap();
        assert_eq!(
            model.metadata["limit"],
            json!({"context": 1_000_000, "output": 128_000})
        );
        assert_eq!(model.metadata["reasoning"], json!(true));
        // 思考常开的型号不提供 `none`（关闭）档。
        assert_eq!(
            model.metadata["reasoning_options"],
            json!([{"type": "effort", "values": ["low", "medium", "high", "xhigh", "max"]}])
        );
        assert_eq!(model.metadata[METADATA_KEY], opus_5_5());
        assert_eq!(
            model.capabilities,
            ["image_input", "structured_output", "reasoning"]
        );
    }

    #[test]
    fn discovery_offers_off_and_budget_where_the_model_allows_them() {
        let model = discovered_model(&json!({
            "id": "claude-sonnet-4-5",
            "display_name": "Claude Sonnet 4.5",
            "capabilities": {
                "thinking": {"supported": true, "types": {
                    "adaptive": {"supported": false}, "enabled": {"supported": true}}},
                "effort": {"supported": false},
            },
        }))
        .unwrap();
        assert_eq!(
            model.metadata["reasoning_options"],
            json!([{"type": "budget_tokens", "min": 1024, "max": null}])
        );

        let plain = discovered_model(&json!({
            "id": "claude-haiku-3",
            "display_name": "Claude Haiku 3",
            "capabilities": {"thinking": {"supported": false, "types": {
                "adaptive": {"supported": false}, "enabled": {"supported": false}}}},
        }))
        .unwrap();
        assert_eq!(plain.metadata["reasoning"], json!(false));
        assert!(!plain.metadata.contains_key("reasoning_options"));

        let bare = discovered_model(&json!({"id": "x", "capabilities": null})).unwrap();
        assert!(bare.metadata.is_empty() && bare.capabilities.is_empty());
    }

    #[test]
    fn requests_are_constrained_to_the_advertised_limits_and_strategies() {
        let caps = ModelCapabilities::from_wire(&opus_5_5(), Some(64_000));
        let mut body = json!({
            "max_tokens": 200_000,
            "context_management": {"edits": [
                {"type": "clear_thinking_20251015"},
                {"type": "clear_tool_uses_20250919"},
            ]},
        });
        constrain(body.as_object_mut().unwrap(), &caps);
        assert_eq!(body["max_tokens"], 64_000);
        assert_eq!(
            body["context_management"],
            json!({"edits": [{"type": "clear_thinking_20251015"}]})
        );

        let no_management = ModelCapabilities::from_wire(
            &json!({"context_management": {"supported": false}}),
            None,
        );
        constrain(body.as_object_mut().unwrap(), &no_management);
        assert!(body.get("context_management").is_none());
        assert_eq!(body["max_tokens"], 64_000, "未知上限时不改动 max_tokens");
    }
}
