use std::collections::{BTreeMap, BTreeSet};

use serde::Deserialize;
use serde_json::{Value, json};
use stravia_vendor_common::connection_check::{self, invalid_response};
use stravia_vendor_sdk::wit::types::HttpRequest;
use stravia_vendor_sdk::{
    ConnectionCheckResponse, ConnectionCheckVerification, DiscoverResponse, DiscoveredModel,
    ErrorKind, GuestHost, MODELS_SOURCE_CATALOG, PluginError, ProviderSnapshot, read_http_body,
};

use crate::allowance::commandcode_request;
use crate::{MAX_BODY, api_key, base_url, error, http_error};

// command-code 1.87.1 的 rankedAccess 使用此顺序；不把额外付费余额当作订阅权益。
const PLANS: &[&str] = &[
    "individual-go",
    "individual-go-v1",
    "individual-goat",
    "individual-pro",
    "individual-pro-v1",
    "individual-provider",
    "individual-max",
    "individual-ultra",
    "teams-pro",
];

#[derive(Deserialize)]
struct Account {
    org: Option<Organization>,
}

#[derive(Deserialize)]
struct Organization {
    id: String,
}

#[derive(Deserialize)]
struct Subscription {
    success: bool,
    data: Option<SubscriptionData>,
}

#[derive(Deserialize)]
struct SubscriptionData {
    #[serde(rename = "planId")]
    plan_id: String,
    status: String,
}

#[derive(Deserialize)]
struct Catalog {
    data: Vec<CatalogModel>,
}

#[derive(Deserialize)]
struct CatalogModel {
    id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    family: Option<String>,
    #[serde(default)]
    selector: Option<String>,
    #[serde(default)]
    capabilities: Vec<String>,
    #[serde(flatten)]
    metadata: BTreeMap<String, Value>,
}

pub(super) fn execute(
    host: &GuestHost,
    provider: &ProviderSnapshot,
) -> Result<DiscoverResponse, PluginError> {
    let selected = selected_models(provider)?;
    let key = api_key(provider)?;
    let base = base_url(provider);
    let account: Account = get(host, commandcode_request(&base, key, "/alpha/whoami", &[]))?;
    let org_param = account.org.as_ref().map(|org| ("orgId", org.id.as_str()));
    let subscription = get(
        host,
        commandcode_request(
            &base,
            key,
            "/alpha/billing/subscriptions",
            org_param.as_slice(),
        ),
    )?;
    let Some(plan) = subscription_plan(subscription)? else {
        return Err(invalid(
            "an active subscription is required for model discovery",
        ));
    };
    let mut request = commandcode_request(&base, key, "/provider/v1/models", &[("view", "client")]);
    if let Some(source) = provider
        .operation_metadata
        .get("models_source")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|source| !source.is_empty() && *source != MODELS_SOURCE_CATALOG)
    {
        request.url = source.to_owned();
    }
    let catalog = get(host, request)?;
    parse_catalog(catalog, plan, selected.as_ref())
}

/// 只读取账号；不要求订阅、不拉取目录，也不初始化 CLI 会话。
pub(super) fn connection_check(
    host: &GuestHost,
    provider: &ProviderSnapshot,
) -> Result<ConnectionCheckResponse, PluginError> {
    let base = base_url(provider);
    let value = connection_check::json(
        host,
        commandcode_request(&base, api_key(provider)?, "/alpha/whoami", &[]),
    )?;
    validate_account(value)?;
    // 自定义端点可能忽略 API key，响应不能证明凭据被接受。
    let verified = if base.trim_end_matches('/') == crate::DEFAULT_BASE_URL {
        ConnectionCheckVerification::EndpointAndCredentials
    } else {
        ConnectionCheckVerification::Endpoint
    };
    Ok(ConnectionCheckResponse { verified })
}

fn validate_account(value: Value) -> Result<(), PluginError> {
    let object = value.as_object().ok_or_else(|| {
        invalid_response("Command Code whoami response must be an account object")
    })?;
    if !object.contains_key("org") && !object.get("user").is_some_and(Value::is_object) {
        return Err(invalid_response(
            "Command Code whoami response has no account fields",
        ));
    }
    if object
        .get("user")
        .is_some_and(|user| !user.is_object() && !user.is_null())
    {
        return Err(invalid_response(
            "Command Code whoami user is not an object",
        ));
    }
    let account: Account = serde_json::from_value(value).map_err(|_| {
        invalid_response("Command Code whoami response has an invalid account shape")
    })?;
    if let Some(org) = account.org
        && org.id.trim().is_empty()
    {
        return Err(invalid_response(
            "Command Code whoami organization id is empty",
        ));
    }
    Ok(())
}

fn get<T: serde::de::DeserializeOwned>(
    host: &GuestHost,
    request: HttpRequest,
) -> Result<T, PluginError> {
    let response = host.http_start(request)?;
    let status = response.status()?;
    if !(200..300).contains(&status) {
        return Err(http_error(
            &response,
            status,
            "Command Code model discovery",
        ));
    }
    let bytes = read_http_body(&response, MAX_BODY)?;
    serde_json::from_slice(&bytes)
        .map_err(|err| invalid(format!("invalid discovery response: {err}")))
}

fn subscription_plan(subscription: Subscription) -> Result<Option<usize>, PluginError> {
    if !subscription.success {
        return Err(invalid("subscription lookup was unsuccessful"));
    }
    let Some(data) = subscription.data else {
        return Ok(None);
    };
    if !matches!(data.status.as_str(), "active" | "trialing" | "past_due") {
        return Ok(None);
    }
    PLANS
        .iter()
        .position(|plan| *plan == data.plan_id)
        .map(Some)
        .ok_or_else(|| invalid("subscription plan is not recognized"))
}

fn selected_models(provider: &ProviderSnapshot) -> Result<Option<BTreeSet<&str>>, PluginError> {
    let Some(value) = provider.operation_metadata.get("static_models") else {
        return Ok(None);
    };
    let values = value
        .as_array()
        .ok_or_else(|| invalid("operation_metadata.static_models must be a JSON array"))?;
    let mut selected = BTreeSet::new();
    for value in values {
        let id = value
            .as_str()
            .ok_or_else(|| invalid("operation_metadata.static_models entries must be strings"))?
            .trim();
        if !id.is_empty() {
            selected.insert(id);
        }
    }
    Ok(Some(selected))
}

fn parse_catalog(
    catalog: Catalog,
    plan: usize,
    selected: Option<&BTreeSet<&str>>,
) -> Result<DiscoverResponse, PluginError> {
    if catalog.data.is_empty() {
        return Err(invalid("model catalog returned no models"));
    }
    let mut models = BTreeMap::new();
    for mut model in catalog.data {
        let id = model.id.trim();
        if id.is_empty()
            || selected.is_some_and(|selected| !selected.contains(id))
            || model.metadata.get("hidden").and_then(Value::as_bool) == Some(true)
            || !subscription_allows(plan, &model.metadata)?
        {
            continue;
        }
        if let Some(context) = model.metadata.remove("context_length") {
            model
                .metadata
                .insert("limit".into(), json!({"context":context}));
        }
        if let Some(input) = model.metadata.remove("inputModalities") {
            model.metadata.insert(
                "modalities".into(),
                json!({"input":input,"output":["text"]}),
            );
        }
        if let Some(efforts) = model.metadata.remove("reasoningEfforts") {
            let efforts = match efforts {
                Value::Null => Vec::new(),
                Value::Array(efforts) => efforts,
                _ => return Err(invalid("model reasoningEfforts must be an array or null")),
            };
            let efforts = efforts
                .into_iter()
                .map(|value| match value {
                    Value::String(value) if value == "off" => Ok(json!("none")),
                    Value::String(value) => Ok(Value::String(value)),
                    _ => Err(invalid("model reasoningEfforts entries must be strings")),
                })
                .collect::<Result<Vec<_>, _>>()?;
            model
                .metadata
                .insert("reasoning_efforts".into(), Value::Array(efforts));
        }
        let id = id.to_owned();
        let display_name = model
            .name
            .filter(|name| !name.trim().is_empty())
            .unwrap_or_else(|| id.clone());
        models.insert(
            id.clone(),
            DiscoveredModel {
                id,
                display_name,
                family: model.family,
                selector: model.selector,
                capabilities: model.capabilities,
                metadata: model.metadata,
            },
        );
    }
    if models.is_empty() {
        return Err(invalid(
            "no models are included in the current subscription and selection",
        ));
    }
    Ok(DiscoverResponse {
        models: models.into_values().collect(),
        next_cursor: None,
    })
}

fn subscription_allows(
    plan: usize,
    metadata: &BTreeMap<String, Value>,
) -> Result<bool, PluginError> {
    match metadata.get("minimumPlan") {
        Some(Value::String(required)) => Ok(PLANS
            .iter()
            .position(|plan| *plan == required)
            .is_some_and(|required| plan >= required)),
        Some(Value::Null) => Ok(false),
        Some(_) => Err(invalid("model minimumPlan must be a string or null")),
        None => match metadata.get("category").and_then(Value::as_str) {
            Some("opensource") => Ok(true),
            Some("premium") => Ok(plan >= 3),
            _ => Err(invalid(
                "model catalog is missing subscription requirements",
            )),
        },
    }
}

fn invalid(message: impl Into<String>) -> PluginError {
    error(
        ErrorKind::Invalid,
        format!("Command Code {}", message.into()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connection_check_requires_a_recognizable_account_response() {
        assert!(validate_account(json!({"org": {"id": "org-1"}})).is_ok());
        assert!(validate_account(json!({"org": null})).is_ok());
        assert!(validate_account(json!({"user": {"email": "local@example.test"}})).is_ok());
        for malformed in [
            json!({}),
            json!([]),
            json!({"org": {}}),
            json!({"org": {"id": ""}}),
            json!({"user": "invalid"}),
        ] {
            assert!(matches!(
                validate_account(malformed).unwrap_err().kind,
                ErrorKind::InvalidResponse
            ));
        }
    }

    #[test]
    fn only_current_subscription_statuses_grant_a_recognized_plan() {
        for status in ["active", "trialing", "past_due"] {
            let subscription = serde_json::from_value(json!({
                "success":true,"data":{"planId":"individual-goat","status":status}
            }))
            .unwrap();
            let plan = subscription_plan(subscription)
                .unwrap()
                .expect("current subscription");
            let required = |plan: &str| BTreeMap::from([("minimumPlan".into(), json!(plan))]);
            assert!(subscription_allows(plan, &required("individual-goat")).unwrap());
            assert!(!subscription_allows(plan, &required("individual-pro")).unwrap());
        }
        for status in ["canceled", "incomplete", "unpaid", "paused", "unknown"] {
            let subscription = serde_json::from_value(json!({
                "success":true,"data":{"planId":"individual-goat","status":status}
            }))
            .unwrap();
            assert_eq!(subscription_plan(subscription).unwrap(), None);
        }
        let absent = serde_json::from_value(json!({"success":true,"data":null})).unwrap();
        assert_eq!(subscription_plan(absent).unwrap(), None);
        for value in [
            json!({"success":false,"data":null}),
            json!({"success":true,"data":{"planId":"future-plan","status":"active"}}),
        ] {
            assert!(subscription_plan(serde_json::from_value(value).unwrap()).is_err());
        }
    }

    #[test]
    fn model_requirements_bound_each_subscription_without_unlocking_extra_usage() {
        for (plan, expected) in [
            (0, vec!["go"]),
            (2, vec!["go", "goat"]),
            (3, vec!["go", "goat", "pro"]),
            (5, vec!["go", "goat", "pro", "provider"]),
        ] {
            let catalog = serde_json::from_value(json!({"data":[
                {"id":"go","minimumPlan":"individual-go"},
                {"id":"goat","category":"opensource","minimumPlan":"individual-goat"},
                {"id":"pro","minimumPlan":"individual-pro"},
                {"id":"provider","minimumPlan":"individual-provider"},
                {"id":"extra-only","minimumPlan":null},
                {"id":"future","minimumPlan":"future-plan"},
                {"id":"hidden","minimumPlan":"individual-go","hidden":true}
            ]}))
            .unwrap();
            let models = parse_catalog(catalog, plan, None).unwrap().models;
            assert_eq!(
                models
                    .iter()
                    .map(|model| model.id.as_str())
                    .collect::<Vec<_>>(),
                expected
            );
        }
    }

    #[test]
    fn static_selection_cannot_add_unknown_or_subscription_locked_models() {
        let catalog = serde_json::from_value(json!({"data":[
            {"id":"selected","minimumPlan":"individual-go"},
            {"id":"unselected","minimumPlan":"individual-go"},
            {"id":"locked","minimumPlan":"individual-provider"}
        ]}))
        .unwrap();
        let selected = BTreeSet::from(["selected", "locked", "unknown"]);
        let models = parse_catalog(catalog, 0, Some(&selected)).unwrap().models;
        assert_eq!(
            models
                .iter()
                .map(|model| model.id.as_str())
                .collect::<Vec<_>>(),
            ["selected"]
        );
        let locked_only = serde_json::from_value(json!({"data":[
            {"id":"locked","minimumPlan":"individual-provider"}
        ]}))
        .unwrap();
        assert!(parse_catalog(locked_only, 0, None).is_err());
    }

    #[test]
    fn explicit_no_effort_and_unknown_effort_remain_distinct_from_missing_metadata() {
        let catalog = serde_json::from_value(json!({"data":[
            {"id":"disabled","minimumPlan":"individual-go","reasoningEfforts":null},
            {"id":"future","minimumPlan":"individual-go","reasoningEfforts":["low","future-effort"]},
            {"id":"unknown","minimumPlan":"individual-go"}
        ]})).unwrap();
        let models = parse_catalog(catalog, 0, None).unwrap().models;
        assert_eq!(models[0].metadata["reasoning_efforts"], json!([]));
        assert_eq!(
            models[1].metadata["reasoning_efforts"],
            json!(["low", "future-effort"])
        );
        assert!(!models[2].metadata.contains_key("reasoning_efforts"));
    }

    #[test]
    fn missing_minimum_plan_uses_only_the_cli_category_contract() {
        let catalog = serde_json::from_value(json!({"data":[
            {"id":"open","category":"opensource"},
            {"id":"premium","category":"premium"}
        ]}))
        .unwrap();
        let models = parse_catalog(catalog, 0, None).unwrap().models;
        assert_eq!(
            models
                .iter()
                .map(|model| model.id.as_str())
                .collect::<Vec<_>>(),
            ["open"]
        );
        let malformed = serde_json::from_value(json!({"data":[{"id":"unknown"}]})).unwrap();
        assert!(parse_catalog(malformed, 0, None).is_err());
    }
}
