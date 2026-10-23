use super::*;

pub(super) fn parse(value: Value) -> Result<AllowanceResponse, PluginError> {
    // 官方客户端把该数值直接渲染为 `$x.xx`，余额单位即美元；缺字段说明是错误包或 schema 变化，不能当作零余额。
    let balance = value
        .get("balance")
        .filter(|balance| balance.is_number())
        .ok_or_else(|| {
            fail(
                ErrorKind::Invalid,
                "Kilo balance response missing numeric balance",
            )
        })?;
    Ok(AllowanceResponse {
        allowances: vec![AllowanceItem {
            key: "balance".into(),
            label: "Credit balance".into(),
            kind: "balance".into(),
            used: None,
            remaining: Some(AllowanceAmount {
                value: balance.to_string(),
                unit: "USD".into(),
                currency: Some("USD".into()),
            }),
            limit: None,
            used_percent: None,
            window_seconds: None,
            resets_at_unix_ms: None,
            condition: None,
        }],
        models: Vec::new(),
        plan_label: None,
        account_label: None,
    })
}
pub(super) fn execute(
    host: &GuestHost,
    provider: &ProviderSnapshot,
) -> Result<AllowanceResponse, PluginError> {
    token(provider)?;
    // 已验证的组织由 headers() 附加组织头，此时返回组织共享余额。
    let mut response = parse(http_json(host, provider, "GET", "/api/profile/balance")?)?;
    // 设备码授权和刷新时已从 /api/profile 保存 email、name。
    let stored = |key: &str| provider.credentials.get(key).and_then(Value::as_str);
    response.account_label =
        stravia_vendor_common::account::first_account_label([stored("email"), stored("name")]);
    Ok(response)
}
