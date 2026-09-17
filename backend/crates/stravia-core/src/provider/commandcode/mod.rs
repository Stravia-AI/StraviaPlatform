//! Command Code vendor — CLI envelope, device fingerprint, and session headers.

use std::sync::LazyLock;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use dashmap::DashMap;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::error::GatewayError;
use crate::provider::common::pipeline;
use crate::provider::inbound::InboundResponse;
use crate::provider::metadata::{
    AuthMode, CapabilitiesSource, ChannelDef, Label, ProtocolBaseUrl, VendorMetadata,
};
use crate::provider::outbound::OutboundRequest;
use crate::provider::registry::{VendorRegistration, VendorScope};
use crate::provider::vendor::{ProviderCtx, Vendor};
use crate::provider::vendor_ext::{
    ConstructedRequest, RequestContext, RequestPurpose, ResolvedTargetCapabilities, VendorCtx,
};
use stravia_runtime_contract::protocol::ids::COMMAND_CODE_GENERATE_V1;
use stravia_runtime_contract::protocol::ids::ProtocolId;
use stravia_runtime_contract::protocol::ir::AiRequest;
use stravia_runtime_contract::protocol::ir::AiResponse;

const PROTOCOL_VERSION: &str = "1.53.1";
const DEFAULT_BASE_URL: &str = "https://api.commandcode.ai";
const FP_SALT: &str = "command-code:device-fingerprint:v1";
const SESSION_TTL: Duration = Duration::from_secs(12 * 60 * 60);
const SESSION_JITTER: Duration = Duration::from_secs(60 * 60);
const INIT_TTL: Duration = Duration::from_secs(8 * 60 * 60);
const INIT_JITTER: Duration = Duration::from_secs(2 * 60 * 60);

const FINGERPRINT_CPUS: &[(&str, u8)] = &[
    ("12th Gen Intel(R) Core(TM) i7-12650H", 10),
    ("12th Gen Intel(R) Core(TM) i5-12400F", 6),
    ("12th Gen Intel(R) Core(TM) i9-12900K", 16),
    ("13th Gen Intel(R) Core(TM) i7-13700K", 16),
    ("13th Gen Intel(R) Core(TM) i5-13600K", 14),
    ("13th Gen Intel(R) Core(TM) i9-13900K", 24),
    ("Intel(R) Core(TM) Ultra 7 155H", 16),
    ("Intel(R) Core(TM) Ultra 9 285H", 16),
    ("Intel(R) Core(TM) i9-14900K", 24),
    ("Intel(R) Core(TM) i7-14700K", 20),
    ("AMD Ryzen 7 7800X3D", 8),
    ("AMD Ryzen 9 7950X", 16),
    ("AMD Ryzen 5 7600", 6),
    ("AMD Ryzen 9 7900X", 12),
    ("AMD Ryzen 7 5800X3D", 8),
];
const FINGERPRINT_MEMS: &[u8] = &[8, 16, 24, 32, 48, 64];
const FINGERPRINT_TZS: &[&str] = &[
    "America/New_York",
    "America/Chicago",
    "America/Los_Angeles",
    "America/Toronto",
    "Europe/London",
    "Europe/Berlin",
    "Europe/Paris",
    "Europe/Moscow",
    "Asia/Shanghai",
    "Asia/Tokyo",
    "Asia/Singapore",
    "Asia/Seoul",
    "Asia/Hong_Kong",
    "Australia/Sydney",
    "Pacific/Auckland",
];
const FINGERPRINT_MAC_COUNTS: &[usize] = &[2, 3, 4, 5];
const FP_OS_USERS: &[&str] = &["dev", "user", "admin", "coder", "engineer", "work"];
const FP_MAIL_DOMAINS: &[&str] = &["gmail.com", "outlook.com", "qq.com", "163.com"];

const METADATA: VendorMetadata = VendorMetadata {
    id: "commandcode",
    label: Label {
        zh: "Command Code",
        en: "Command Code",
    },
    icon: "commandcode",
    default_protocol: "command-code",
    credential_fields: crate::provider::metadata::API_KEY_CREDENTIAL_FIELDS,
    option_fields: &[crate::provider::metadata::OptionFieldDef {
        key: "zdr",
        label: "Zero Data Retention",
        // 默认开启 = 沿用历史行为(始终发送 x-cmd-zdr: 1);关闭后与 CLI
        // 默认形态一致,上游可恢复提示缓存。
        input: crate::provider::metadata::OptionInputKind::Toggle,
        default_on: true,
    }],
    channels: &[ChannelDef {
        id: "default",
        label: Label {
            zh: "默认",
            en: "Default",
        },
        base_urls: &[ProtocolBaseUrl {
            protocol: "command-code",
            base_url: DEFAULT_BASE_URL,
        }],
        api_key: None,
        // 模型清单来自 `/provider/v1/models`(OpenAI 兼容 data[].id);
        // 发现走 HTTP,元数据在同步时经 canonical 模板匹配,能力查询回落到同一端点。
        models_source: Some("https://api.commandcode.ai/provider/v1/models"),
        capabilities_source: CapabilitiesSource::Http(
            "https://api.commandcode.ai/provider/v1/models",
        ),
        static_models: &[],
        auth_mode: AuthMode::ApiKey,
        oauth: None,
        runtime: None,
    }],
};

pub struct CommandCodeVendor;

struct SessionEntry {
    session_id: String,
    expires_at: Instant,
}

struct KeyState {
    fingerprint: Value,
    next_init_at: Instant,
}

fn sessions() -> &'static DashMap<String, SessionEntry> {
    static SESSIONS: LazyLock<DashMap<String, SessionEntry>> = LazyLock::new(DashMap::new);
    &SESSIONS
}

fn key_states() -> &'static DashMap<String, KeyState> {
    static STATES: LazyLock<DashMap<String, KeyState>> = LazyLock::new(DashMap::new);
    &STATES
}

fn http_client() -> &'static reqwest::Client {
    static CLIENT: LazyLock<reqwest::Client> = LazyLock::new(reqwest::Client::new);
    &CLIENT
}

#[async_trait]
impl Vendor for CommandCodeVendor {
    fn scope(&self) -> VendorScope {
        VendorScope::Vendor {
            vendor_id: "commandcode",
        }
    }

    fn metadata(&self) -> Option<&'static VendorMetadata> {
        Some(&METADATA)
    }

    fn assemble_base_url(
        &self,
        _credentials: &std::collections::BTreeMap<String, String>,
        configured_base_url: Option<&str>,
    ) -> anyhow::Result<String> {
        crate::provider::vendor::resolve_base_url(self.vendor_id(), configured_base_url, || {
            DEFAULT_BASE_URL.to_string()
        })
    }

    fn target_capabilities(&self, _protocol: ProtocolId) -> ResolvedTargetCapabilities {
        ResolvedTargetCapabilities {
            stream_only: true,
            responses_websocket: false,
        }
    }

    fn construct_request(
        &self,
        ctx: &RequestContext<'_>,
        purpose: RequestPurpose<'_>,
    ) -> anyhow::Result<ConstructedRequest> {
        let url = match purpose {
            RequestPurpose::Compact { .. } => {
                anyhow::bail!("Vendor does not support standalone compaction")
            }
            RequestPurpose::Inference { .. } | RequestPurpose::Models { .. } => purpose.endpoint(),
        };
        let mut headers = HeaderMap::new();
        insert_cli_identity_headers(&mut headers);
        if matches!(purpose, RequestPurpose::Inference { .. }) {
            if zdr_enabled(ctx.provider) {
                insert_zdr_header(&mut headers);
            }
            headers.insert(
                HeaderName::from_static("x-project-slug"),
                HeaderValue::from_str(&slugify_project_path(&device_project_dir()))?,
            );
            headers.insert(
                HeaderName::from_static("x-taste-learning"),
                HeaderValue::from_static("false"),
            );
            headers.insert(
                HeaderName::from_static("x-session-id"),
                HeaderValue::from_str(&session_id(ctx.api_key))?,
            );
            headers.insert(
                HeaderName::from_static("traceparent"),
                HeaderValue::from_str(&traceparent())?,
            );
        }
        if !ctx.disable_default_auth {
            headers.insert(
                reqwest::header::AUTHORIZATION,
                HeaderValue::from_str(&format!("Bearer {}", ctx.api_key))?,
            );
        }
        ConstructedRequest::new(ctx, purpose, url, headers)
    }

    async fn pre_request(
        &self,
        ctx: &VendorCtx<'_>,
        _req: &mut AiRequest,
        _gw: &crate::Gateway,
    ) -> anyhow::Result<()> {
        let trimmed = ctx.provider.base_url.trim();
        let base = if trimmed.is_empty() {
            DEFAULT_BASE_URL
        } else {
            trimmed.trim_end_matches('/')
        };
        ensure_initialized(ctx.api_key, base, zdr_enabled(ctx.provider)).await;
        Ok(())
    }

    async fn post_encode(
        &self,
        ctx: &VendorCtx<'_>,
        body: &mut Value,
        _headers: &mut HeaderMap,
    ) -> anyhow::Result<()> {
        apply_device_envelope(body, &session_id(ctx.api_key))
    }

    fn vendor_id(&self) -> &'static str {
        "commandcode"
    }

    fn supported_protocols(&self) -> &'static [ProtocolId] {
        &[COMMAND_CODE_GENERATE_V1]
    }

    async fn build_request(
        &self,
        req: &mut AiRequest,
        ctx: &ProviderCtx<'_>,
    ) -> Result<OutboundRequest, GatewayError> {
        pipeline::build_request(self, req, ctx).await
    }

    async fn parse_response(
        &self,
        resp: InboundResponse,
        ctx: &ProviderCtx<'_>,
    ) -> Result<AiResponse, GatewayError> {
        pipeline::parse_response(self, resp, ctx).await
    }

    fn map_error(&self, status: u16, body: Value) -> GatewayError {
        let mapped = match status {
            400 | 422 => 400,
            401 | 403 => 401,
            402 | 429 => 429,
            404 => 404,
            503 => 503,
            _ => 502,
        };
        let message = body
            .pointer("/error/message")
            .or_else(|| body.get("message"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| format!("CC API error ({status})"));
        GatewayError::upstream_status("commandcode", mapped, Some(message))
    }
}

inventory::submit! { VendorRegistration { make: || Box::new(CommandCodeVendor) } }

fn session_id(api_key: &str) -> String {
    let now = Instant::now();
    if let Some(entry) = sessions().get(api_key)
        && now < entry.expires_at
    {
        return entry.session_id.clone();
    }
    let jitter = Duration::from_millis(
        u64::from_le_bytes(
            fp_digest(api_key, "session-jitter")[..8]
                .try_into()
                .unwrap(),
        ) % SESSION_JITTER.as_millis() as u64,
    );
    let session_id = Uuid::new_v4().to_string();
    sessions().insert(
        api_key.to_string(),
        SessionEntry {
            session_id: session_id.clone(),
            expires_at: now + SESSION_TTL + jitter,
        },
    );
    session_id
}

async fn ensure_initialized(api_key: &str, base_url: &str, zdr: bool) {
    let now = Instant::now();
    if key_states()
        .get(api_key)
        .is_some_and(|state| now < state.next_init_at)
    {
        return;
    }
    let fingerprint = key_states()
        .entry(api_key.to_string())
        .or_insert_with(|| KeyState {
            fingerprint: generate_fingerprint(api_key),
            next_init_at: now,
        })
        .fingerprint
        .clone();
    let headers = init_headers(api_key, zdr);
    let fingerprint_url = format!("{base_url}/alpha/fingerprint/record");
    let lifecycle_url = format!("{base_url}/alpha/lifecycle-events");
    let lifecycle = json!({
        "eventType": "cli_session_exists",
        "metadata": {
            "sessionId": format!("sess_{}", hex_encode(&fp_digest(api_key, "lifecycle-session")[..8])),
            "cliVersion": PROTOCOL_VERSION,
            "mode": "interactive",
            "os": "win32-x64",
        },
    });
    let client = http_client();
    let _ = tokio::join!(
        client
            .post(&fingerprint_url)
            .headers(headers.clone())
            .json(&fingerprint)
            .send(),
        client
            .post(&lifecycle_url)
            .headers(headers)
            .json(&lifecycle)
            .send(),
    );
    let jitter = Duration::from_millis(
        u64::from_le_bytes(fp_digest(api_key, "init-jitter")[..8].try_into().unwrap())
            % INIT_JITTER.as_millis() as u64,
    );
    if let Some(mut state) = key_states().get_mut(api_key) {
        state.next_init_at = Instant::now() + INIT_TTL + jitter;
    }
}

fn init_headers(api_key: &str, zdr: bool) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        reqwest::header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    insert_cli_identity_headers(&mut headers);
    if zdr {
        insert_zdr_header(&mut headers);
    }
    if let Ok(value) = HeaderValue::from_str(&format!("Bearer {api_key}")) {
        headers.insert(reqwest::header::AUTHORIZATION, value);
    }
    headers
}

fn insert_cli_identity_headers(headers: &mut HeaderMap) {
    headers.insert(reqwest::header::USER_AGENT, HeaderValue::from_static("cli"));
    headers.insert(
        HeaderName::from_static("x-command-code-version"),
        HeaderValue::from_static(PROTOCOL_VERSION),
    );
    headers.insert(
        HeaderName::from_static("x-cli-environment"),
        HeaderValue::from_static("production"),
    );
}

fn insert_zdr_header(headers: &mut HeaderMap) {
    headers.insert(
        HeaderName::from_static("x-cmd-zdr"),
        HeaderValue::from_static("1"),
    );
}

/// `vendor_options.zdr`;缺省(未设置或 JSON 损坏)保持历史行为:发送
/// `x-cmd-zdr: 1`。显式 `false` 时省略该头,与 CLI 默认形态一致。
fn zdr_enabled(provider: &crate::db::models::Provider) -> bool {
    provider
        .vendor_options()
        .get("zdr")
        .and_then(Value::as_bool)
        .unwrap_or(true)
}

fn device_project_dir() -> String {
    // 固定的伪造目录:与设备指纹的伪造档案(osUser=dev 等)保持自洽,
    // 也不把宿主真实用户名泄给上游(workingDir 与 x-project-slug 同源)。
    // 值与 codec 的 DEFAULT_WORKING_DIR 是同一个 CLI 档案,保持同步。
    crate::protocol::codec::command_code::DEFAULT_WORKING_DIR.to_string()
}

fn apply_device_envelope(body: &mut Value, session_id: &str) -> anyhow::Result<()> {
    let object = body
        .as_object()
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("Command Code request body is not an object"))?;
    let mut ordered = Map::new();
    for key in ["config", "memory", "taste", "skills", "permissionMode"] {
        if let Some(value) = object.get(key) {
            ordered.insert(key.to_string(), value.clone());
        }
    }
    if let Some(config) = ordered.get_mut("config").and_then(Value::as_object_mut) {
        config.insert("workingDir".into(), Value::String(device_project_dir()));
        config.insert("environment".into(), Value::String("win32".into()));
    }
    ordered.insert("threadId".into(), Value::String(session_id.to_string()));
    for key in ["mode", "promptCache", "params"] {
        if let Some(value) = object.get(key) {
            ordered.insert(key.to_string(), value.clone());
        }
    }
    *body = Value::Object(ordered);
    Ok(())
}

fn generate_fingerprint(api_key: &str) -> Value {
    let &(cpu_model, cpu_count) = fp_pick(api_key, "cpu", FINGERPRINT_CPUS, |(model, cores)| {
        format!("{model}|{cores}")
    });
    let &mem_gib = fp_pick(api_key, "mem", FINGERPRINT_MEMS, u8::to_string);
    let &timezone = fp_pick(api_key, "timezone", FINGERPRINT_TZS, |value| *value);
    let &mac_count = fp_pick(
        api_key,
        "macCount",
        FINGERPRINT_MAC_COUNTS,
        usize::to_string,
    );
    let &os_user = fp_pick(api_key, "osUser", FP_OS_USERS, |value| *value);
    let &mail_domain = fp_pick(api_key, "mailDomain", FP_MAIL_DOMAINS, |value| *value);
    let mid = hex_encode(&fp_digest(api_key, "machineId")[..16]);
    let machine_id = format!(
        "{}-{}-{}-{}-{}",
        &mid[0..8],
        &mid[8..12],
        &mid[12..16],
        &mid[16..20],
        &mid[20..32]
    );
    let mut macs = Vec::new();
    for index in 0..mac_count {
        let bytes = &fp_digest(api_key, &format!("mac{index}"))[..6];
        macs.push(
            bytes
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<Vec<_>>()
                .join(":"),
        );
    }
    macs.sort();
    let hostname = format!(
        "DESKTOP-{}",
        hex_encode(&fp_digest(api_key, "hostname")[..4]).to_ascii_uppercase()
    );
    let git_email = format!(
        "{os_user}.{}@{mail_domain}",
        hex_encode(&fp_digest(api_key, "gitEmail")[..3])
    );
    let thumb_seed = format!("{machine_id}|{}", macs.join(","));
    let thumbmark = fingerprint_hash_raw(&format!("machine\0{thumb_seed}"));
    json!({
        "thumbmark": thumbmark,
        "components": {
            "machineIdHash": fingerprint_hash(&machine_id),
            "macHashes": macs.iter().map(|mac| fingerprint_hash(mac)).collect::<Vec<_>>(),
            "osUserHash": fingerprint_hash(os_user),
            "hostnameHash": fingerprint_hash(&hostname),
            "gitEmailHash": fingerprint_hash(&git_email),
            "platform": "win32",
            "arch": "x64",
            "osRelease": "10.0.22631",
            "cpuModel": cpu_model,
            "cpuCount": cpu_count,
            "memGiB": mem_gib,
            "isContainer": false,
            "timezone": timezone,
            "runtime": "cli",
            "collectorVersion": 1,
        },
    })
}

fn fp_digest(api_key: &str, field: &str) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update([0]);
    hasher.update(api_key.as_bytes());
    hasher.update([0]);
    hasher.update(field.as_bytes());
    hasher.finalize().into()
}

/// Deterministically picks one entry by taking the option whose
/// `{field}\0{label}` digest is largest. Strict `>` keeps the first option on
/// a (practically impossible) tie.
fn fp_pick<'a, T, S: AsRef<str>>(
    api_key: &str,
    field: &str,
    options: &'a [T],
    label: impl Fn(&T) -> S,
) -> &'a T {
    let score = |option: &T| fp_digest(api_key, &format!("{field}\0{}", label(option).as_ref()));
    let mut best = options
        .first()
        .expect("fingerprint options must not be empty");
    let mut best_score = score(best);
    for option in &options[1..] {
        let option_score = score(option);
        if option_score > best_score {
            best = option;
            best_score = option_score;
        }
    }
    best
}

fn fingerprint_hash(value: &str) -> String {
    let trimmed = value.trim();
    fingerprint_hash_raw(&trimmed.to_ascii_lowercase())
}

fn fingerprint_hash_raw(value: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(FP_SALT.as_bytes());
    hasher.update([0]);
    hasher.update(value.as_bytes());
    hex_encode(&hasher.finalize())
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn slugify_project_path(path: &str) -> String {
    let slug = path
        .to_ascii_lowercase()
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
        .collect::<String>();
    let collapsed = slug
        .split('-')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    if collapsed.is_empty() {
        "root".into()
    } else {
        collapsed
    }
}

fn traceparent() -> String {
    format!(
        "00-{}-{}-01",
        hex_encode(&Uuid::new_v4().as_bytes()[..]),
        hex_encode(&Uuid::new_v4().as_bytes()[..8])
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::Provider;

    fn test_provider() -> Provider {
        Provider {
            id: "test".into(),
            name: "test".into(),
            vendor: Some("commandcode".into()),
            protocol: "command-code".into(),
            base_url: DEFAULT_BASE_URL.into(),
            preset_key: None,
            channel: None,
            models_source: None,
            static_models: None,
            api_key: "sk-test".into(),
            adapter_credentials: r#"{"apiKey":"sk-test"}"#.into(),
            vendor_options: "{}".into(),
            auth_mode: "apikey".into(),
            use_proxy: false,
            last_test_success: None,
            last_test_at: None,
            is_enabled: true,
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    #[test]
    fn fingerprint_matches_cli_hash_for_known_key() {
        let fingerprint = generate_fingerprint("user_testkey");
        assert_eq!(
            fingerprint["thumbmark"],
            "eaf626a88c4f03fd4d369295542c2c610f508385d0b4d1934eb0eab9a65f011b"
        );
        assert_eq!(
            fingerprint["components"]["cpuModel"],
            "13th Gen Intel(R) Core(TM) i9-13900K"
        );
        assert_eq!(fingerprint["components"]["cpuCount"], 24);
        assert_eq!(fingerprint["components"]["memGiB"], 48);
        assert_eq!(fingerprint["components"]["timezone"], "America/New_York");
        assert_eq!(fingerprint["components"]["runtime"], "cli");
        assert_eq!(fingerprint["components"]["collectorVersion"], 1);
        assert_eq!(
            fingerprint["components"]["machineIdHash"],
            "4a562686c0f42deccd8176b4476bbd601a194783449543509c82be619e7fbafc"
        );
        assert_ne!(
            fingerprint["thumbmark"],
            generate_fingerprint("user_other")["thumbmark"]
        );
    }

    #[test]
    fn device_project_dir_is_fixed_fake_profile() {
        // 伪造档案必须固定:与指纹的伪造 osUser/平台自洽,不随宿主真实 home 变化。
        let dir = device_project_dir();
        assert_eq!(
            dir,
            crate::protocol::codec::command_code::DEFAULT_WORKING_DIR
        );
    }

    #[test]
    fn slugifies_windows_project_dir() {
        assert_eq!(
            slugify_project_path(r"C:\Users\dev\projects\app"),
            "c-users-dev-projects-app"
        );
    }

    #[test]
    fn inference_sends_zdr_and_fake_profile_slug() {
        let provider = test_provider();
        let request = CommandCodeVendor
            .construct_request(
                &RequestContext {
                    provider: &provider,
                    api_key: "sk-test",
                    credential: None,
                    disable_default_auth: false,
                },
                RequestPurpose::Inference {
                    protocol: COMMAND_CODE_GENERATE_V1,
                    base_url: DEFAULT_BASE_URL,
                    path: "/alpha/generate",
                    actual_model: "claude-sonnet-4-6",
                },
            )
            .unwrap();
        assert_eq!(request.headers["x-cmd-zdr"], "1");
        assert_eq!(
            request.headers["x-project-slug"],
            slugify_project_path(&device_project_dir())
        );
        assert_eq!(request.headers["x-command-code-version"], PROTOCOL_VERSION);
    }

    #[test]
    fn post_encode_overwrites_working_dir() {
        let mut body = json!({
            "config": {
                "workingDir": r"C:\Users\dev\projects\app",
                "environment": "linux",
            },
            "permissionMode": "standard",
            "mode": "agent",
            "params": {},
        });
        apply_device_envelope(&mut body, "sess-1").unwrap();
        assert_eq!(body["config"]["workingDir"], device_project_dir());
        assert_eq!(body["config"]["environment"], "win32");
        assert_eq!(body["threadId"], "sess-1");
    }

    #[test]
    fn init_headers_follow_zdr_option() {
        assert_eq!(init_headers("sk-test", true)["x-cmd-zdr"], "1");
        assert!(init_headers("sk-test", false).get("x-cmd-zdr").is_none());
    }

    #[test]
    fn inference_omits_zdr_header_when_disabled() {
        let mut provider = test_provider();
        provider.vendor_options = r#"{"zdr":false}"#.into();
        let request = CommandCodeVendor
            .construct_request(
                &RequestContext {
                    provider: &provider,
                    api_key: "sk-test",
                    credential: None,
                    disable_default_auth: false,
                },
                RequestPurpose::Inference {
                    protocol: COMMAND_CODE_GENERATE_V1,
                    base_url: DEFAULT_BASE_URL,
                    path: "/alpha/generate",
                    actual_model: "claude-sonnet-4-6",
                },
            )
            .unwrap();
        assert!(request.headers.get("x-cmd-zdr").is_none());
    }

    #[test]
    fn declares_stream_only() {
        let capabilities = CommandCodeVendor.target_capabilities(COMMAND_CODE_GENERATE_V1);
        assert!(capabilities.stream_only);
    }
}
