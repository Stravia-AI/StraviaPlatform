mod parsers;

use serde_json::Value;
use stravia_vendor_sdk::wit::types::HttpRequest;
use stravia_vendor_sdk::{
    AllowanceRequest, AllowanceResponse, ErrorKind, GuestHost, PluginError, ProviderSnapshot,
    read_http_body,
};

use parsers::Monitor;

const MAX_RESPONSE_BYTES: usize = 1024 * 1024;

pub(crate) fn supports(vendor_id: &str, channel: &str) -> bool {
    monitor_for(vendor_id, channel).is_some()
}

pub(crate) fn execute(
    vendor_id: &str,
    host: &GuestHost,
    provider: ProviderSnapshot,
    _request: AllowanceRequest,
) -> Result<AllowanceResponse, PluginError> {
    let monitor = monitor_for(vendor_id, provider.channel.as_str()).ok_or_else(|| {
        error(
            ErrorKind::Unsupported,
            "allowance is not supported for this provider channel",
            None,
        )
    })?;
    let credential = credential_for(&provider)?;
    let requests = requests_for(monitor, credential);
    let count = requests.len();
    for (index, request) in requests.into_iter().enumerate() {
        let response = host.http_start(request)?;
        let status = response.status()?;
        if !(200..300).contains(&status) {
            if count > 1 && index == 0 && status == 404 {
                continue;
            }
            return Err(status_error(status));
        }
        let body = read_http_body(&response, MAX_RESPONSE_BYTES)?;
        let parsed = if count > 1 && index > 0 {
            parsers::parse_minimax_fallback(&body)
        } else {
            parsers::parse(monitor, &body)
        };
        match parsed {
            Ok(parsed) => return Ok(parsed),
            Err(()) if count > 1 && index == 0 => continue,
            Err(()) => return Err(invalid_response()),
        }
    }
    Err(invalid_response())
}

fn monitor_for(vendor_id: &str, channel: &str) -> Option<Monitor> {
    match (vendor_id, channel) {
        ("anthropic", "claude-code") => Some(Monitor::AnthropicClaudeCode),
        ("github-copilot", "default") => Some(Monitor::GitHubCopilot),
        ("kimi-for-coding", "default") => Some(Monitor::KimiCoding),
        ("nano-gpt", "default") => Some(Monitor::NanoGpt),
        ("zai-coding-plan", "default") => Some(Monitor::ZaiCodingPlan),
        ("zhipuai-coding-plan", "default") => Some(Monitor::ZhipuAiCodingPlan),
        ("minimax-coding-plan", "default") => Some(Monitor::MiniMaxCodingPlan),
        ("minimax-cn-coding-plan", "default") => Some(Monitor::MiniMaxCnCodingPlan),
        ("wafer.ai", "default") => Some(Monitor::Wafer),
        ("opencode-go", "default") => Some(Monitor::OpenCodeGo),
        ("crof", "default") => Some(Monitor::Crof),
        ("deepseek", "default") => Some(Monitor::DeepSeek),
        ("neuralwatt", "default") => Some(Monitor::NeuralWatt),
        _ => None,
    }
}

fn credential_for(provider: &ProviderSnapshot) -> Result<&str, PluginError> {
    ["access_token", "apiKey", "token"]
        .into_iter()
        .find_map(|key| provider.credentials.get(key).and_then(string_value))
        .ok_or_else(|| error(ErrorKind::Auth, "provider credential is missing", None))
}

fn string_value(value: &Value) -> Option<&str> {
    value
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn requests_for(monitor: Monitor, credential: &str) -> Vec<HttpRequest> {
    let (method, urls, body): (&str, &[&str], Vec<u8>) = match monitor {
        Monitor::AnthropicClaudeCode => (
            "GET",
            &["https://api.anthropic.com/api/oauth/usage"],
            vec![],
        ),
        Monitor::GitHubCopilot => (
            "GET",
            &["https://api.github.com/copilot_internal/user"],
            vec![],
        ),
        Monitor::KimiCoding => ("GET", &["https://api.kimi.com/coding/v1/usages"], vec![]),
        Monitor::NanoGpt => (
            "GET",
            &["https://nano-gpt.com/api/subscription/v1/usage"],
            vec![],
        ),
        Monitor::ZaiCodingPlan => (
            "GET",
            &["https://api.z.ai/api/monitor/usage/quota/limit"],
            vec![],
        ),
        Monitor::ZhipuAiCodingPlan => (
            "GET",
            &["https://open.bigmodel.cn/api/monitor/usage/quota/limit"],
            vec![],
        ),
        Monitor::MiniMaxCodingPlan => (
            "GET",
            &[
                "https://api.minimax.io/v1/token_plan/remains",
                "https://api.minimax.io/v1/api/openplatform/coding_plan/remains",
            ],
            vec![],
        ),
        Monitor::MiniMaxCnCodingPlan => (
            "GET",
            &[
                "https://api.minimaxi.com/v1/token_plan/remains",
                "https://www.minimaxi.com/v1/api/openplatform/coding_plan/remains",
            ],
            vec![],
        ),
        Monitor::Wafer => ("GET", &["https://pass.wafer.ai/v1/inference/quota"], vec![]),
        Monitor::OpenCodeGo => ("GET", &["https://opencode.ai/zen/go/v1/usage"], vec![]),
        Monitor::Crof => ("GET", &["https://crof.ai/usage_api/"], vec![]),
        Monitor::DeepSeek => ("GET", &["https://api.deepseek.com/user/balance"], vec![]),
        Monitor::NeuralWatt => ("GET", &["https://api.neuralwatt.com/v1/quota"], vec![]),
    };

    urls.iter()
        .map(|url| {
            let mut headers = base_headers(monitor, credential);
            match monitor {
                Monitor::AnthropicClaudeCode => {
                    headers.push(("anthropic-beta".into(), "oauth-2025-04-20".into()));
                }
                Monitor::GitHubCopilot => {
                    headers.push(("editor-version".into(), "vscode/1.96.2".into()));
                    headers.push(("x-github-api-version".into(), "2025-04-01".into()));
                }
                Monitor::OpenCodeGo => headers.push(("user-agent".into(), "Stravia".into())),
                _ => {}
            }
            HttpRequest {
                method: method.into(),
                url: (*url).into(),
                headers,
                body: body.clone(),
            }
        })
        .collect()
}

fn base_headers(monitor: Monitor, credential: &str) -> Vec<(String, String)> {
    let authorization = if monitor == Monitor::GitHubCopilot {
        format!("token {credential}")
    } else {
        format!("Bearer {credential}")
    };
    vec![
        ("authorization".into(), authorization),
        ("accept".into(), "application/json".into()),
    ]
}

fn status_error(status: u16) -> PluginError {
    let kind = match status {
        401 | 403 => ErrorKind::Auth,
        _ => ErrorKind::upstream_unknown(),
    };
    error(kind, "upstream allowance request failed", Some(status))
}

fn invalid_response() -> PluginError {
    error(
        ErrorKind::upstream_unknown(),
        "upstream allowance response has an unsupported shape",
        None,
    )
}

fn error(kind: ErrorKind, message: &str, upstream_status: Option<u16>) -> PluginError {
    PluginError {
        kind,
        message: message.into(),
        upstream_status,
    }
}
