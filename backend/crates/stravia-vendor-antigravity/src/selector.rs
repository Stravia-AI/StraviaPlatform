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

pub(crate) struct Selection<'a> {
    pub id: &'a str,
    pub thinking_budget: Option<i32>,
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
) -> Result<Selection<'a>, PluginError> {
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
    let mut default_variant = None;
    for variant in variants {
        let id = variant
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.trim().is_empty())
            .ok_or_else(invalid_table)?;
        has_default |= id == default;
        if id == default {
            default_variant = Some(variant);
        }
        if let Some(TargetThinkingControl::Effort { value }) = control
            && variant.get("effort").and_then(Value::as_str) == Some(value.as_str())
        {
            selected = Some((id, variant));
        }
    }
    if !has_default {
        return Err(invalid_table());
    }
    let (id, variant) = if let Some(TargetThinkingControl::Effort { .. }) = control {
        selected.ok_or_else(|| {
            plugin_error(
                ErrorKind::Invalid,
                "The selected Antigravity model does not provide this reasoning effort",
            )
        })?
    } else {
        (default, default_variant.ok_or_else(invalid_table)?)
    };
    // 显式非 Effort 控制优先于目录预算；不能让陈旧元数据覆盖它。
    let use_budget =
        control.is_none() || matches!(control, Some(TargetThinkingControl::Effort { .. }));
    let thinking_budget = if use_budget {
        let value = variant
            .get("metadata")
            .and_then(|metadata| metadata.get("thinkingBudget"));
        match value {
            Some(value) => Some(
                value
                    .as_i64()
                    .and_then(|value| i32::try_from(value).ok())
                    .filter(|value| *value >= -1)
                    .ok_or_else(invalid_table)?,
            ),
            None if control.is_some() => {
                return Err(plugin_error(
                    ErrorKind::Invalid,
                    "The selected Antigravity reasoning effort has no thinking budget; sync models again",
                ));
            }
            None => None,
        }
    } else {
        None
    };
    Ok(Selection {
        id,
        thinking_budget,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn effort_retains_variant_budget_and_explicit_controls_override_metadata() {
        let table = json!({"default":"flash-high","variants":[
            {"id":"flash-high","effort":"high","metadata":{"thinkingBudget":-1}},
            {"id":"flash-medium","effort":"medium","metadata":{"thinkingBudget":4000}}
        ]});
        for (effort, id, budget) in [("medium", "flash-medium", 4000), ("high", "flash-high", -1)] {
            let selection = resolve(
                &table,
                Some(&TargetThinkingControl::Effort {
                    value: effort.into(),
                }),
            )
            .unwrap();
            assert_eq!(selection.id, id);
            assert_eq!(selection.thinking_budget, Some(budget));
        }
        for budget in [json!(null), json!("4000"), json!(-2), json!(2147483648_i64)] {
            let mut invalid_table = table.clone();
            invalid_table["variants"][1]["metadata"]["thinkingBudget"] = budget;
            assert!(
                resolve(
                    &invalid_table,
                    Some(&TargetThinkingControl::Effort {
                        value: "medium".into()
                    })
                )
                .is_err()
            );
        }
        let missing = json!({"default":"flash","variants":[{"id":"flash","effort":"medium"}]});
        assert!(
            resolve(
                &missing,
                Some(&TargetThinkingControl::Effort {
                    value: "medium".into()
                })
            )
            .is_err()
        );
        assert_eq!(resolve(&missing, None).unwrap().thinking_budget, None);
        for control in [
            TargetThinkingControl::Budget { value: 2048 },
            TargetThinkingControl::Disabled,
            TargetThinkingControl::Enabled,
        ] {
            assert_eq!(
                resolve(&table, Some(&control)).unwrap().thinking_budget,
                None
            );
        }
    }

    #[test]
    fn selects_only_registered_request_ids_and_preserves_default() {
        let table = json!({
            "default": "gemini-pro-agent",
            "variants": [
                {"id": "gemini-pro-agent", "effort": "high", "metadata":{"thinkingBudget":-1}},
                {"id": "gemini-3.1-pro-low", "effort": "low", "metadata":{"thinkingBudget":1024}}
            ]
        });
        assert_eq!(resolve(&table, None).unwrap().id, "gemini-pro-agent");
        assert_eq!(
            resolve(
                &table,
                Some(&TargetThinkingControl::Effort {
                    value: "low".into()
                })
            )
            .unwrap()
            .id,
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
