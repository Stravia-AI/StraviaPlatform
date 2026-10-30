use super::*;
use stravia_core::admin::{ProviderCostPlanInput, provider_cost_plan_value};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CurrencyInput {
    pub symbol: String,
    pub usd_rate: String,
}

pub(super) async fn get_currency(State(gw): State<Gateway>) -> impl IntoResponse {
    match gw.admin().custom_currency().await {
        Ok(custom) => Json(serde_json::json!({ "data": { "custom": custom } })).into_response(),
        Err(e) => err(e),
    }
}

pub(super) async fn put_currency(
    State(gw): State<Gateway>,
    Json(input): Json<CurrencyInput>,
) -> impl IntoResponse {
    match gw
        .admin()
        .save_custom_currency(&input.symbol, &input.usd_rate)
        .await
    {
        Ok(custom) => Json(serde_json::json!({ "data": { "custom": custom } })).into_response(),
        Err(e) => err(e),
    }
}

pub(super) async fn list_cost_plans(
    State(gw): State<Gateway>,
    Path(provider_id): Path<String>,
) -> impl IntoResponse {
    let plans = match gw.admin().provider_cost_plans(&provider_id).await {
        Ok(plans) => plans,
        Err(e) => return err(e),
    };
    let mut data = Vec::with_capacity(plans.len());
    for plan in &plans {
        match provider_cost_plan_value(plan) {
            Ok(value) => data.push(value),
            Err(e) => return err(e),
        }
    }
    Json(serde_json::json!({ "data": data })).into_response()
}

pub(super) async fn append_cost_plan(
    State(gw): State<Gateway>,
    Path(provider_id): Path<String>,
    Json(input): Json<ProviderCostPlanInput>,
) -> impl IntoResponse {
    match gw
        .admin()
        .append_provider_cost_plan(&provider_id, input)
        .await
    {
        Ok(plan) => match provider_cost_plan_value(&plan) {
            Ok(value) => Json(serde_json::json!({ "data": value })).into_response(),
            Err(e) => err(e),
        },
        Err(e) => err(e),
    }
}
