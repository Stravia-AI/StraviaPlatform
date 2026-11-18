use serde_json::Value;
use stravia_vendor_common::common::{plugin_error, upstream_error};
use stravia_vendor_sdk::{
    ErrorKind, Fields, GuestHost, HttpResponse, Options, PluginError, ProviderSnapshot, Request,
    read_http_body,
};
use uuid::Uuid;

use crate::DEFAULT_BASE_URL;

#[derive(Clone, Copy)]
pub(super) enum Action {
    Generate,
    GetUsageLimits,
    ListModels,
}

pub(super) fn rpc(
    provider: &ProviderSnapshot,
    action: Action,
    input: &impl serde::Serialize,
) -> Result<Request, PluginError> {
    let (runtime, management) = match provider.base_url.trim_end_matches('/') {
        "" | DEFAULT_BASE_URL => (DEFAULT_BASE_URL, "https://management.us-east-1.kiro.dev"),
        "https://runtime.eu-central-1.kiro.dev" => (
            "https://runtime.eu-central-1.kiro.dev",
            "https://management.eu-central-1.kiro.dev",
        ),
        _ => {
            return Err(plugin_error(
                ErrorKind::Invalid,
                "use an official Kiro runtime URL in us-east-1 or eu-central-1",
            ));
        }
    };
    let key = provider
        .credentials
        .get("apiKey")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| plugin_error(ErrorKind::Auth, "Kiro API key is missing"))?;
    let (origin, target) = match action {
        Action::Generate => (runtime, "KiroRuntimeService.GenerateAssistantResponse"),
        Action::GetUsageLimits => (management, "KiroControlPlaneBearerService.GetUsageLimits"),
        Action::ListModels => (
            management,
            "KiroControlPlaneBearerService.ListAvailableModels",
        ),
    };
    let mut headers = Fields::new();
    headers.append("content-type", "application/x-amz-json-1.0");
    headers.append("x-amz-target", target);
    // Node 的 HTTP/1 sender 在固定位置填写长度和 authority。值由宿主根据最终请求填写。
    headers.append("content-length", "");
    let inference = matches!(action, Action::Generate);
    if inference {
        headers.append("x-amzn-codewhisperer-optout", "true");
    }
    headers.append("TokenType", "API_KEY");
    if inference {
        headers.append("x-amzn-kiro-client-attribution", "stravia");
        headers.append("x-amzn-kiro-mode-identifier", "vibe");
        headers.append("x-amzn-kiro-fallback", "0;max=0");
    }
    // 保留 SDK 的双 UA 位置与协议版本；不声称当前进程是 Node/Smithy 或官方 CLI 安装。
    const UA: &str = concat!("Stravia/", env!("CARGO_PKG_VERSION"), " Kiro-compat/3.4.0");
    headers.append("x-amz-user-agent", UA);
    headers.append("user-agent", UA);
    headers.append("host", "");
    headers.append("amz-sdk-invocation-id", Uuid::new_v4().to_string());
    // 宿主管理重试；插件只有一个 SDK 级尝试，不广告 CLI 的 max=3。
    headers.append("amz-sdk-request", "attempt=1; max=1");
    if inference {
        headers.append("x-amzn-kiro-attempt", "1;max=1");
    }
    headers.append("Authorization", format!("Bearer {key}"));
    headers.append("Connection", "keep-alive");
    Ok(Request {
        method: "POST".into(),
        url: format!("{origin}/"),
        headers,
        body: serde_json::to_vec(input)
            .map_err(|e| plugin_error(ErrorKind::Invalid, e.to_string()))?,
    })
}

pub(super) fn send(host: &GuestHost, request: Request) -> Result<HttpResponse, PluginError> {
    let mut options = Options::new();
    options
        .shape("plugin:kiro-cli-3-4-0")
        .expect("valid Kiro CLI egress shape selector");
    let response = host.http_start(request, &options)?;
    let status = response.status()?;
    if !(200..300).contains(&status) {
        return Err(upstream_error(
            status,
            &response.headers()?,
            &read_http_body(&response, 1024 * 1024)?,
        ));
    }
    Ok(response)
}
