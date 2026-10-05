use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;
use stravia_runtime_contract::thinking::TargetThinkingControl;
use stravia_vendor_common::common::plugin_error;
use stravia_vendor_sdk::{ErrorKind, PluginError};

pub(crate) const EXTENSION_KEY: &str = "antigravity";
pub(crate) const EFFORTS: &[&str] = &["none", "minimal", "low", "medium", "high", "xhigh", "max"];

#[derive(Serialize)]
pub(crate) struct Variant {
    pub id: String,
    pub slug: String,
    pub effort: Option<String>,
    pub metadata: BTreeMap<String, Value>,
}

#[derive(Serialize)]
pub(crate) struct SelectorTable {
    pub default: String,
    pub variants: Vec<Variant>,
}

pub(crate) fn user_facing_slug(id: &str) -> &str {
    // 官方 CLI 1.2.16 的 base/effort 映射；显示别名不能替换请求身份。
    match id {
        "gemini-pro-agent" => "gemini-3.1-pro-high",
        _ => id,
    }
}

pub(crate) fn family_and_effort(slug: &str) -> (&str, Option<&str>) {
    if let Some((family, effort)) = slug.rsplit_once('-')
        && EFFORTS.contains(&effort)
    {
        return (family, Some(effort));
    }
    (slug, None)
}

pub(crate) fn resolve<'a>(
    extension: &'a Value,
    control: Option<&TargetThinkingControl>,
) -> Result<&'a str, PluginError> {
    let invalid_table = || {
        plugin_error(
            ErrorKind::Invalid,
            "Antigravity model selector metadata is invalid; sync models again",
        )
    };
    let default = extension
        .get("default")
        .and_then(Value::as_str)
        .filter(|id| !id.trim().is_empty())
        .ok_or_else(invalid_table)?;
    let variants = extension
        .get("variants")
        .and_then(Value::as_array)
        .ok_or_else(invalid_table)?;
    let mut has_default = false;
    let mut selected = None;
    for variant in variants {
        let id = variant
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.trim().is_empty())
            .ok_or_else(invalid_table)?;
        has_default |= id == default;
        if let Some(TargetThinkingControl::Effort { value }) = control
            && variant.get("effort").and_then(Value::as_str) == Some(value.as_str())
        {
            selected = Some(id);
        }
    }
    if !has_default {
        return Err(invalid_table());
    }
    if let Some(TargetThinkingControl::Effort { .. }) = control {
        selected.ok_or_else(|| {
            plugin_error(
                ErrorKind::Invalid,
                "The selected Antigravity model does not provide this reasoning effort",
            )
        })
    } else {
        Ok(default)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn selects_only_registered_request_ids_and_preserves_default() {
        let table = json!({
            "default": "gemini-pro-agent",
            "variants": [
                {"id": "gemini-pro-agent", "effort": "high"},
                {"id": "gemini-3.1-pro-low", "effort": "low"}
            ]
        });
        assert_eq!(resolve(&table, None).unwrap(), "gemini-pro-agent");
        assert_eq!(
            resolve(
                &table,
                Some(&TargetThinkingControl::Effort {
                    value: "low".into()
                })
            )
            .unwrap(),
            "gemini-3.1-pro-low"
        );
        assert!(
            resolve(
                &table,
                Some(&TargetThinkingControl::Effort {
                    value: "medium".into()
                })
            )
            .is_err()
        );
        assert!(
            resolve(
                &json!({"default": "removed", "variants": [{"id": "live"}]}),
                None
            )
            .is_err()
        );
    }
}
