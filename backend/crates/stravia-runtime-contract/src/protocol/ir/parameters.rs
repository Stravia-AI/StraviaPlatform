//! 统一控制只保存跨协议意图；旧快照在私有读取边界收敛。
use super::native_tools::*;
use super::{AiRequest, ProtocolExt, ReasoningSummary, ResponseFormat};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, HashMap};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Modality {
    Text,
    Audio,
    Image,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verbosity {
    Low,
    Medium,
    High,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Truncation {
    Auto,
    Disabled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceTier {
    Auto,
    Default,
    Flex,
    Priority,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheRetention {
    Default,
    Extended,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParameterReset {
    Instructions,
    NativeTools,
    Temperature,
    TopP,
    PresencePenalty,
    FrequencyPenalty,
    MaxTokens,
    ParallelToolCalls,
    Background,
    MaxToolCalls,
    Continuation,
    CacheKey,
    CacheRetention,
    Logprobs,
    Truncation,
    Store,
    ServiceTier,
    Reasoning,
    ReasoningLevel,
    ReasoningSummary,
    ResponseFormat,
    ResponseFormatStrict,
    Verbosity,
    EndUser,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AudioOutput {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub voice: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CompactionControl {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub threshold_tokens: Option<u32>,
    /// 摘要专用指令，与会话级 `AiRequest.instructions` 分离。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CacheParameters {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retention: Option<CacheRetention>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionParameters {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forked_from_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_turn_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_turn_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Continuation {
    PreviousResponse(String),
    Conversation(String),
}
impl Continuation {
    pub fn previous_response_id(&self) -> Option<&str> {
        match self {
            Self::PreviousResponse(id) => Some(id),
            _ => None,
        }
    }
    pub fn conversation_id(&self) -> Option<&str> {
        match self {
            Self::Conversation(id) => Some(id),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct RequestParameters {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_k: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_p: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repetition_penalty: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidate_count: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logit_bias: Option<HashMap<String, f64>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logprobs: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modalities: Option<Vec<Modality>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio: Option<AudioOutput>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verbosity: Option<Verbosity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tool_calls: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub store: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generate: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncation: Option<Truncation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compaction: Option<CompactionControl>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<BTreeMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_user_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<ServiceTier>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub continuation: Option<Continuation>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub native_tools: Vec<NativeTool>,
    #[serde(default, skip_serializing_if = "cache_empty")]
    pub cache: CacheParameters,
    #[serde(default)]
    pub session: SessionParameters,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub resets: Vec<ParameterReset>,
    #[serde(skip)]
    legacy: LegacyParameterControls,
}
fn cache_empty(value: &CacheParameters) -> bool {
    value == &CacheParameters::default()
}
#[derive(Debug, Clone, Default)]
struct LegacyParameterControls {
    fields: Map<String, Value>,
    native_tools_present: bool,
}
impl<'de> Deserialize<'de> for RequestParameters {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        let mut result = Self::default();
        if let Value::Object(mut fields) = value {
            result.legacy.native_tools_present = fields.contains_key("native_tools");
            for (name, value) in &fields {
                if value.is_null()
                    && let Some(reset) = reset_name(name)
                {
                    result.reset(reset);
                }
            }
            result.top_k = take(&mut fields, "top_k");
            result.min_p = take(&mut fields, "min_p");
            result.repetition_penalty = take(&mut fields, "repetition_penalty");
            result.candidate_count = take(&mut fields, "candidate_count");
            result.logit_bias = take(&mut fields, "logit_bias");
            result.modalities = take(&mut fields, "modalities");
            result.audio = take(&mut fields, "audio");
            result.verbosity = take(&mut fields, "verbosity");
            result.max_tool_calls = take(&mut fields, "max_tool_calls");
            result.store = take(&mut fields, "store");
            result.background = take(&mut fields, "background");
            result.generate = take(&mut fields, "generate");
            result.truncation = take(&mut fields, "truncation");
            result.compaction = take(&mut fields, "compaction");
            result.end_user_id = take(&mut fields, "end_user_id");
            result.service_tier = take(&mut fields, "service_tier");
            if fields
                .get("native_tools")
                .and_then(Value::as_array)
                .is_some_and(Vec::is_empty)
            {
                result.reset(ParameterReset::NativeTools);
            }
            result.native_tools = take(&mut fields, "native_tools").unwrap_or_default();
            for reset in take::<Vec<ParameterReset>>(&mut fields, "resets").unwrap_or_default() {
                result.reset(reset);
            }
            if let Some(Value::Object(mut cache)) = fields.remove("cache") {
                if cache.get("key").is_some_and(Value::is_null) {
                    result.reset(ParameterReset::CacheKey);
                }
                if cache.get("retention").is_some_and(Value::is_null) {
                    result.reset(ParameterReset::CacheRetention);
                }
                result.cache.key = take(&mut cache, "key");
                result.cache.retention = cache.remove("retention").as_ref().and_then(retention);
            }
            if let Some(session) = fields.remove("session") {
                result.import_session(session);
            }
            if let Some(value) = fields.remove("continuation") {
                result.continuation = serde_json::from_value(value.clone()).ok().or_else(|| {
                    value
                        .get("previous_response_id")
                        .and_then(Value::as_str)
                        .map(|id| Continuation::PreviousResponse(id.into()))
                        .or_else(|| {
                            value
                                .get("conversation")
                                .and_then(conversation_id)
                                .map(Continuation::Conversation)
                        })
                });
            }
            result.logprobs = fields.remove("logprobs").as_ref().and_then(logprobs);
            if let Some(n) = take(&mut fields, "top_logprobs") {
                result.logprobs = Some(n);
            }
            result.metadata = fields.remove("metadata").as_ref().and_then(string_map);
            result.import_fields(&mut fields);
            result.legacy.fields = fields;
        }
        Ok(result)
    }
}
fn take<T: serde::de::DeserializeOwned>(fields: &mut Map<String, Value>, key: &str) -> Option<T> {
    fields
        .remove(key)
        .and_then(|v| serde_json::from_value(v).ok())
}
fn prefer<T>(target: &mut Option<T>, source: Option<T>) {
    if target.is_none() {
        *target = source;
    }
}
fn string_map(value: &Value) -> Option<BTreeMap<String, String>> {
    value.as_object().map(|map| {
        map.iter()
            .filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.into())))
            .collect()
    })
}
fn conversation_id(value: &Value) -> Option<String> {
    value
        .as_str()
        .or_else(|| value.get("id").and_then(Value::as_str))
        .map(str::to_owned)
}
fn logprobs(value: &Value) -> Option<u32> {
    value
        .as_u64()
        .and_then(|n| u32::try_from(n).ok())
        .or_else(|| (value.as_bool() == Some(true)).then_some(0))
}
fn retention(value: &Value) -> Option<CacheRetention> {
    match value.as_str()? {
        "default" | "in_memory" | "5m" => Some(CacheRetention::Default),
        "extended" | "24h" | "1h" => Some(CacheRetention::Extended),
        _ => None,
    }
}
fn service(value: &Value) -> Option<ServiceTier> {
    if value
        .get("family")
        .and_then(Value::as_str)
        .is_some_and(|family| {
            !matches!(
                family,
                "openai" | "open-responses" | "openai-chat" | "anthropic" | "bedrock"
            )
        })
    {
        return None;
    }
    let value = value
        .as_str()
        .or_else(|| value.get("tier").and_then(Value::as_str))?;
    match value {
        "auto" => Some(ServiceTier::Auto),
        "default" | "standard" | "standard_only" => Some(ServiceTier::Default),
        "flex" => Some(ServiceTier::Flex),
        "priority" => Some(ServiceTier::Priority),
        _ => None,
    }
}
fn summary(value: &Value) -> Option<ReasoningSummary> {
    match value.as_str()? {
        "none" | "omitted" => Some(ReasoningSummary::None),
        "auto" | "summarized" => Some(ReasoningSummary::Auto),
        "concise" => Some(ReasoningSummary::Concise),
        "detailed" => Some(ReasoningSummary::Detailed),
        _ => None,
    }
}

impl RequestParameters {
    pub fn resets(&self, reset: ParameterReset) -> bool {
        self.resets.contains(&reset)
    }
    pub fn reset(&mut self, reset: ParameterReset) {
        if !self.resets(reset) {
            self.resets.push(reset);
        }
    }
    fn import_session(&mut self, value: Value) {
        let Value::Object(mut fields) = value else {
            return;
        };
        for name in [
            "id",
            "affinity_id",
            "session_id",
            "thread_id",
            "window_id",
            "x-codex-window-id",
        ] {
            prefer(&mut self.session.id, take(&mut fields, name));
        }
        for name in ["parent_id", "parent_thread_id", "parent_session_id"] {
            prefer(&mut self.session.parent_id, take(&mut fields, name));
        }
        for name in ["forked_from_id", "forked_from_thread_id"] {
            prefer(&mut self.session.forked_from_id, take(&mut fields, name));
        }
        prefer(
            &mut self.session.request_id,
            take(&mut fields, "request_id"),
        );
        prefer(
            &mut self.session.context_window_id,
            take(&mut fields, "context_window_id"),
        );
        prefer(&mut self.session.turn_id, take(&mut fields, "turn_id"));
        prefer(
            &mut self.session.parent_turn_id,
            take(&mut fields, "parent_turn_id"),
        );
        prefer(
            &mut self.session.root_turn_id,
            take(&mut fields, "root_turn_id"),
        );
    }
    pub fn import_client_metadata(&mut self, metadata: Value) {
        self.import_session(metadata);
    }
    pub fn normalize_headers<'a>(&mut self, get: impl Fn(&str) -> Option<&'a str>) {
        fn find<'a>(get: &impl Fn(&str) -> Option<&'a str>, names: &[&str]) -> Option<String> {
            names.iter().find_map(|name| {
                get(name)
                    .map(str::trim)
                    .filter(|v| !v.is_empty())
                    .map(str::to_owned)
            })
        }
        prefer(
            &mut self.session.id,
            find(
                &get,
                &[
                    "x-session-id",
                    "session-id",
                    "session_id",
                    "conversation_id",
                    "x-session-affinity",
                    "x-opencode-session",
                    "x-conversation-id",
                    "x-claude-code-session-id",
                    "thread-id",
                    "x-codex-thread-id",
                    "x-codex-window-id",
                ],
            ),
        );
        prefer(
            &mut self.session.parent_id,
            find(
                &get,
                &[
                    "x-parent-session-id",
                    "parent-thread-id",
                    "x-codex-parent-thread-id",
                ],
            ),
        );
        prefer(
            &mut self.session.request_id,
            find(&get, &["x-client-request-id", "x-request-id"]),
        );
        prefer(
            &mut self.session.forked_from_id,
            find(
                &get,
                &["forked-from-thread-id", "x-codex-forked-from-thread-id"],
            ),
        );
        prefer(
            &mut self.session.context_window_id,
            find(&get, &["x-context-window-id", "x-codex-context-window-id"]),
        );
        prefer(
            &mut self.session.turn_id,
            find(&get, &["x-turn-id", "x-codex-turn-id"]),
        );
        prefer(
            &mut self.session.parent_turn_id,
            find(&get, &["x-parent-turn-id", "x-codex-parent-turn-id"]),
        );
        prefer(
            &mut self.session.root_turn_id,
            find(&get, &["x-root-turn-id", "x-codex-root-turn-id"]),
        );
    }
    fn import_anthropic_session(&mut self) {
        let Some(user) = self.end_user_id.as_ref() else {
            return;
        };
        if let Ok(Value::Object(identity)) = serde_json::from_str::<Value>(user) {
            prefer(
                &mut self.session.parent_id,
                identity
                    .get("parent_session_id")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            );
            if let Some(session) = identity.get("session_id").and_then(Value::as_str) {
                prefer(&mut self.session.id, Some(session.into()));
                self.end_user_id = None;
            }
        } else if let Some((identity, session)) = user.rsplit_once("_session_")
            && identity.starts_with("user_")
            && !session.is_empty()
        {
            prefer(&mut self.session.id, Some(session.into()));
            self.end_user_id = Some(identity.into());
        }
    }
    fn import_tool(&mut self, family: &str, definition: &Value) {
        if self.legacy.native_tools_present {
            return;
        }
        if matches!(family, "google" | "gemini")
            && let Some(fields) = definition.as_object()
        {
            for name in ["googleSearch", "googleSearchRetrieval", "codeExecution"] {
                if let Some(value) = fields.get(name) {
                    let definition = Value::Object(Map::from_iter([(name.into(), value.clone())]));
                    if let Some(tool) = native_tool_from_gemini(&definition)
                        && !self.native_tools.contains(&tool)
                    {
                        self.native_tools.push(tool);
                    }
                }
            }
            return;
        }
        let tool = match family {
            "open-responses" | "responses" | "openai" => native_tool_from_responses(definition),
            "anthropic" => native_tool_from_anthropic(definition),
            "google" | "gemini" => native_tool_from_gemini(definition),
            _ => None,
        };
        if let Some(tool) = tool
            && !self.native_tools.contains(&tool)
        {
            self.native_tools.push(tool);
        }
    }
    fn import_fields(&mut self, fields: &mut Map<String, Value>) {
        prefer(
            &mut self.end_user_id,
            take(fields, "safety_identifier")
                .or_else(|| take(fields, "end_user"))
                .or_else(|| take(fields, "user")),
        );
        prefer(&mut self.generate, take(fields, "responses_generate"));
        prefer(
            &mut self.service_tier,
            fields
                .remove("service")
                .as_ref()
                .and_then(service)
                .or_else(|| fields.remove("serviceTier").as_ref().and_then(service)),
        );
        prefer(
            &mut self.metadata,
            fields
                .remove("bedrock_request_metadata")
                .as_ref()
                .and_then(string_map)
                .or_else(|| {
                    fields
                        .remove("requestMetadata")
                        .as_ref()
                        .and_then(string_map)
                }),
        );
        if fields
            .remove("hosted_tools_specified")
            .and_then(|v| v.as_bool())
            == Some(true)
            && !self.legacy.native_tools_present
            && !fields
                .get("hosted_tools")
                .and_then(Value::as_array)
                .is_some_and(|tools| !tools.is_empty())
        {
            self.reset(ParameterReset::NativeTools);
        }
        if let Some(Value::Array(tools)) = fields.remove("hosted_tools") {
            for tool in tools {
                if let (Some(family), Some(definition)) = (
                    tool.get("family").and_then(Value::as_str),
                    tool.get("definition"),
                ) {
                    self.import_tool(family, definition);
                }
            }
        }
        if let Some(options) = fields.remove("web_search_options")
            && !self.legacy.native_tools_present
            && let Some(tool) = native_tool_from_chat_web_search_options(&options)
            && !self.native_tools.contains(&tool)
        {
            self.native_tools.push(tool);
        }
        if let Some(presence) = fields.remove("response_presence") {
            self.import_presence(&presence);
        }
        if let Some(context) = fields
            .remove("context_management")
            .or_else(|| fields.remove("__anthropic_context_management"))
        {
            self.import_compaction(&context);
        }
        if let Some(cache) = fields.remove("anthropic_cache_control") {
            prefer(
                &mut self.cache.retention,
                cache.get("ttl").and_then(retention),
            );
        }
        if let Some(speech) = fields.remove("google_speech_config") {
            prefer(
                &mut self.audio,
                Some(AudioOutput {
                    voice: speech
                        .pointer("/voiceConfig/prebuiltVoiceConfig/voiceName")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    format: None,
                }),
            );
        }
    }
    fn import_compaction(&mut self, value: &Value) {
        let entries = value
            .as_array()
            .or_else(|| value.get("edits").and_then(Value::as_array));
        if let Some(entries) = entries {
            for entry in entries {
                if entry
                    .get("type")
                    .and_then(Value::as_str)
                    .is_some_and(|s| s == "compaction" || s.starts_with("compact_"))
                {
                    let threshold = entry
                        .get("compact_threshold")
                        .or_else(|| entry.get("threshold_tokens"))
                        .or_else(|| entry.pointer("/trigger/value"))
                        .and_then(Value::as_u64)
                        .and_then(|n| u32::try_from(n).ok());
                    prefer(
                        &mut self.compaction,
                        Some(CompactionControl {
                            threshold_tokens: threshold,
                            instructions: None,
                        }),
                    );
                }
            }
        }
    }
    fn import_presence(&mut self, value: &Value) {
        if let Some(controls) = value.get("null_controls").and_then(Value::as_array) {
            for control in controls {
                if let Some(reset) = control.as_str().and_then(reset_name) {
                    self.reset(reset);
                }
            }
        }
        for (name, reset) in [
            ("text_null", ParameterReset::ResponseFormat),
            ("text_format_null", ParameterReset::ResponseFormat),
            ("text_verbosity_null", ParameterReset::Verbosity),
            (
                "text_format_strict_null",
                ParameterReset::ResponseFormatStrict,
            ),
            ("reasoning_effort_null", ParameterReset::ReasoningLevel),
            ("reasoning_summary_null", ParameterReset::ReasoningSummary),
        ] {
            if value.get(name).and_then(Value::as_bool) == Some(true) {
                self.reset(reset);
            }
        }
    }
}
fn reset_name(name: &str) -> Option<ParameterReset> {
    Some(match name {
        "max_output_tokens" | "max_tokens" => ParameterReset::MaxTokens,
        "previous_response_id" | "conversation" | "continuation" => ParameterReset::Continuation,
        "prompt_cache_key" | "cache_key" => ParameterReset::CacheKey,
        "prompt_cache_retention" | "cache_retention" => ParameterReset::CacheRetention,
        "top_logprobs" | "logprobs" => ParameterReset::Logprobs,
        "safety_identifier" | "end_user" | "end_user_id" => ParameterReset::EndUser,
        "instructions" => ParameterReset::Instructions,
        "tools" | "native_tools" => ParameterReset::NativeTools,
        _ => return serde_json::from_value(Value::String(name.into())).ok(),
    })
}
fn decode_format(mut value: Value) -> Option<ResponseFormat> {
    let fields = value.as_object_mut()?;
    match fields.get("type")?.as_str()? {
        "text" => Some(ResponseFormat::Text),
        "json_object" => Some(ResponseFormat::JsonObject),
        "json_schema" => {
            let mut nested = fields.remove("json_schema");
            let fields = if let Some(nested) = nested.as_mut() {
                nested.as_object_mut()?
            } else {
                fields
            };
            Some(ResponseFormat::JsonSchema {
                name: take(fields, "name").unwrap_or_else(|| "response".into()),
                description: take(fields, "description"),
                schema: fields.remove("schema")?,
                strict: take(fields, "strict"),
            })
        }
        _ => None,
    }
}

impl AiRequest {
    /// 旧存储只依赖 JSON 边界，避免把历史供应商 DTO 泄漏给运行时调用方。
    pub fn import_legacy_responses_extension(&mut self, value: Value) {
        if let Value::Object(mut fields) = value {
            self.import_controls("open-responses", &mut fields);
        }
        self.normalize_parameters();
    }

    pub(super) fn import_legacy_extension(&mut self, extension: ProtocolExt) {
        let (family, value) = match extension {
            ProtocolExt::OpenAiChat(ext) => ("openai-chat", serde_json::to_value(ext)),
            ProtocolExt::OpenResponses(ext) => {
                if ext.instructions_present && self.instructions.is_none() {
                    self.parameters.reset(ParameterReset::Instructions);
                }
                ("open-responses", serde_json::to_value(ext))
            }
            ProtocolExt::Anthropic(ext) => ("anthropic", serde_json::to_value(ext)),
            ProtocolExt::Google(ext) => ("google", serde_json::to_value(ext)),
        };
        // DTO 只含 JSON 可表达字段；序列化失败不会引入参数透传回退。
        if let Ok(Value::Object(mut fields)) = value {
            self.import_controls(family, &mut fields);
        }
    }
    pub fn normalize_parameters(&mut self) {
        if self.reasoning.mode.as_deref() == Some("adaptive") {
            self.reasoning.mode = Some("auto".into());
        }
        let mut fields = std::mem::take(&mut self.parameters.legacy.fields);
        self.import_controls("legacy", &mut fields);
        if let Some(Value::Object(extensions)) = fields.remove("extensions") {
            for (family, controls) in extensions {
                if let Value::Object(mut controls) = controls {
                    self.import_controls(&family, &mut controls);
                }
            }
        }
        if let Some(tools) = self.tools.as_mut() {
            for tool in tools.extract_if(.., |tool| {
                matches!(
                    tool.name.as_str(),
                    "__builtin__google_search"
                        | "__builtin__code_execution"
                        | "__builtin__google_search_retrieval"
                )
            }) {
                let key = match tool.name.as_str() {
                    "__builtin__google_search" => "googleSearch",
                    "__builtin__code_execution" => "codeExecution",
                    _ => "googleSearchRetrieval",
                };
                let definition = if tool.parameters.get(key).is_some() {
                    tool.parameters
                } else {
                    Value::Object(Map::from_iter([(key.into(), tool.parameters)]))
                };
                self.parameters.import_tool("google", &definition);
            }
        }
        let mut bag: Map<String, Value> = std::mem::take(&mut self.meta.vendor.ingress)
            .into_iter()
            .collect();
        self.import_controls("legacy", &mut bag);
        self.meta.vendor.ingress = bag
            .into_iter()
            .filter(|(key, _)| key.starts_with("__"))
            .collect();
        self.parameters.import_anthropic_session();
    }
    fn import_controls(&mut self, family: &str, fields: &mut Map<String, Value>) {
        if fields
            .remove("instructions_specified")
            .and_then(|value| value.as_bool())
            == Some(true)
            && self.instructions.is_none()
        {
            self.parameters.reset(ParameterReset::Instructions);
        }
        if (family == "anthropic" || fields.contains_key("__anthropic_metadata"))
            && let Some(cache) = fields.remove("cache_control")
        {
            prefer(
                &mut self.parameters.cache.retention,
                cache.get("ttl").and_then(retention),
            );
        }
        if let Some(Value::Object(body)) = fields.remove("passthrough_body") {
            for (key, value) in body {
                fields.entry(key).or_insert(value);
            }
        }
        for (old, new) in [
            ("n", "candidate_count"),
            ("response_logprobs", "logprobs"),
            ("response_modalities", "modalities"),
            ("prompt_cache_retention", "retention"),
            ("__anthropic_top_k", "top_k"),
            ("__anthropic_container", "container"),
            ("__anthropic_service_tier", "service_tier"),
            ("__anthropic_output_config", "output_config"),
            ("__anthropic_thinking", "thinking"),
            ("__google_generation_config", "generation_config"),
            ("__google_tool_config", "tool_config"),
            ("__google_raw_tools", "raw_tools"),
            ("__anthropic_metadata", "anthropic_metadata"),
        ] {
            if let Some(value) = fields.remove(old) {
                fields.entry(new).or_insert(value);
            }
        }
        for (old, new) in [
            ("candidateCount", "candidate_count"),
            ("topK", "top_k"),
            ("topP", "top_p"),
            ("maxOutputTokens", "max_tokens"),
            ("responseMimeType", "response_mime_type"),
            ("responseJsonSchema", "response_json_schema"),
            ("responseSchema", "response_json_schema"),
            ("responseModalities", "modalities"),
            ("responseLogprobs", "logprobs"),
            ("thinkingConfig", "thinking_config"),
            ("speechConfig", "google_speech_config"),
            ("requestMetadata", "bedrock_request_metadata"),
            ("serviceTier", "service_tier"),
        ] {
            if let Some(value) = fields.remove(old) {
                fields.entry(new).or_insert(value);
            }
        }
        if family == "google"
            && let Some(config) = fields.remove("speech_config")
        {
            fields.entry("google_speech_config").or_insert(config);
        }
        for (name, value) in fields.iter() {
            if value.is_null()
                && let Some(reset) = reset_name(name)
            {
                self.parameters.reset(reset);
            }
        }
        if let Some(Value::Object(mut generation)) = fields.remove("generation_config") {
            self.import_controls("google", &mut generation);
        }
        prefer(&mut self.parameters.top_k, take(fields, "top_k"));
        prefer(&mut self.parameters.min_p, take(fields, "min_p"));
        prefer(
            &mut self.parameters.repetition_penalty,
            take(fields, "repetition_penalty"),
        );
        prefer(
            &mut self.parameters.candidate_count,
            take(fields, "candidate_count"),
        );
        prefer(&mut self.parameters.logit_bias, take(fields, "logit_bias"));
        let enabled = fields.remove("logprobs").as_ref().and_then(logprobs);
        prefer(
            &mut self.parameters.logprobs,
            take(fields, "top_logprobs").or(enabled),
        );
        prefer(&mut self.parameters.audio, take(fields, "audio"));
        if let Some(Value::Array(values)) = fields.remove("modalities") {
            prefer(
                &mut self.parameters.modalities,
                Some(
                    values
                        .into_iter()
                        .filter_map(|v| {
                            v.as_str().and_then(|s| {
                                serde_json::from_value(Value::String(s.to_ascii_lowercase())).ok()
                            })
                        })
                        .collect(),
                ),
            );
        }
        prefer(&mut self.parameters.verbosity, take(fields, "verbosity"));
        prefer(&mut self.parameters.store, take(fields, "store"));
        prefer(&mut self.parameters.background, take(fields, "background"));
        prefer(&mut self.parameters.generate, take(fields, "generate"));
        prefer(
            &mut self.parameters.max_tool_calls,
            take(fields, "max_tool_calls"),
        );
        prefer(&mut self.parameters.truncation, take(fields, "truncation"));
        prefer(
            &mut self.parameters.metadata,
            fields.remove("metadata").as_ref().and_then(string_map),
        );
        prefer(
            &mut self.parameters.service_tier,
            fields.remove("service_tier").as_ref().and_then(service),
        );
        prefer(
            &mut self.parameters.cache.key,
            take(fields, "prompt_cache_key"),
        );
        prefer(
            &mut self.parameters.cache.retention,
            fields.remove("retention").as_ref().and_then(retention),
        );
        if let Some(id) = take(fields, "previous_response_id") {
            prefer(
                &mut self.parameters.continuation,
                Some(Continuation::PreviousResponse(id)),
            );
        }
        if let Some(id) = fields
            .remove("conversation")
            .as_ref()
            .and_then(conversation_id)
        {
            prefer(
                &mut self.parameters.continuation,
                Some(Continuation::Conversation(id)),
            );
        }
        if let Some(metadata) = fields.remove("client_metadata") {
            self.parameters.import_client_metadata(metadata);
        }
        if let Some(metadata) = fields.remove("anthropic_metadata") {
            prefer(
                &mut self.parameters.end_user_id,
                metadata
                    .get("user_id")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            );
        }
        for key in ["passthrough_tools", "server_tools", "raw_tools"] {
            if let Some(Value::Array(tools)) = fields.remove(key) {
                for tool in tools {
                    self.parameters
                        .import_tool(family_for_tools(family, key), &tool);
                }
            }
        }
        if let Some(tool) = fields.remove("native_web_search") {
            self.parameters.import_tool("open-responses", &tool);
        }
        if let Some(tool_choice) = fields.remove("tool_choice_ext") {
            prefer(
                &mut self.tool_choice,
                serde_json::from_value(tool_choice).ok(),
            );
        }
        if let Some(Value::Object(mut text)) = fields.remove("text") {
            if text.get("format").is_some_and(Value::is_null) {
                self.parameters.reset(ParameterReset::ResponseFormat);
            }
            if text.get("verbosity").is_some_and(Value::is_null) {
                self.parameters.reset(ParameterReset::Verbosity);
            }
            if text
                .get("format")
                .and_then(|v| v.get("strict"))
                .is_some_and(Value::is_null)
            {
                self.parameters.reset(ParameterReset::ResponseFormatStrict);
            }
            prefer(&mut self.parameters.verbosity, take(&mut text, "verbosity"));
            if let Some(format) = text.remove("format") {
                prefer(&mut self.response_format, decode_format(format));
            }
        }
        if let Some(format) = fields.remove("response_format") {
            prefer(&mut self.response_format, decode_format(format));
        }
        if let Some(schema) = fields.remove("response_json_schema") {
            prefer(
                &mut self.response_format,
                Some(ResponseFormat::JsonSchema {
                    name: "response".into(),
                    description: None,
                    schema,
                    strict: None,
                }),
            );
        }
        match fields
            .remove("response_mime_type")
            .and_then(|v| v.as_str().map(str::to_owned))
            .as_deref()
        {
            Some("application/json") => {
                prefer(&mut self.response_format, Some(ResponseFormat::JsonObject));
            }
            Some("text/plain") => {
                prefer(&mut self.response_format, Some(ResponseFormat::Text));
            }
            _ => {}
        }
        if let Some(ResponseFormat::JsonSchema { description, .. }) = self.response_format.as_mut()
        {
            prefer(description, take(fields, "response_format_description"));
        }
        for key in ["output_config", "bedrock_output_config", "outputConfig"] {
            if let Some(mut output) = fields.remove(key) {
                if let Some(effort) = output.get("effort").and_then(Value::as_str) {
                    prefer(
                        &mut self.reasoning.level,
                        crate::thinking::ThinkingLevel::from_wire(effort).ok(),
                    );
                }
                if let Some(format) = output.get_mut("format").map(Value::take) {
                    prefer(&mut self.response_format, decode_format(format));
                }
                if let Some(native) = output.pointer("/textFormat/structure/jsonSchema")
                    && let Some(schema) = native
                        .get("schema")
                        .and_then(Value::as_str)
                        .and_then(|s| serde_json::from_str(s).ok())
                {
                    prefer(
                        &mut self.response_format,
                        Some(ResponseFormat::JsonSchema {
                            name: native
                                .get("name")
                                .and_then(Value::as_str)
                                .unwrap_or("response")
                                .into(),
                            description: native
                                .get("description")
                                .and_then(Value::as_str)
                                .map(str::to_owned),
                            schema,
                            strict: None,
                        }),
                    );
                }
            }
        }
        for key in [
            "reasoning",
            "thinking",
            "thinking_config",
            "__anthropic_thinking",
            "enable_thinking",
            "chat_template_args",
            "chat_template_kwargs",
        ] {
            if let Some(value) = fields.remove(key) {
                self.import_thinking(&value);
            }
        }
        if let Some(effort) = fields
            .remove("reasoning_effort")
            .and_then(|v| v.as_str().map(str::to_owned))
        {
            prefer(
                &mut self.reasoning.level,
                crate::thinking::ThinkingLevel::from_wire(&effort).ok(),
            );
        }
        if let Some(value) = fields
            .remove("stream_reasoning_summary_delivery")
            .or_else(|| {
                fields
                    .get("stream_options")
                    .and_then(|v| v.get("reasoning_summary_delivery"))
                    .cloned()
            })
        {
            prefer(&mut self.reasoning.summary, summary(&value));
        }
        if let Some(config) = fields.remove("tool_config")
            && let Some(config) = config.get("functionCallingConfig")
            && let Some(names) = config.get("allowedFunctionNames").and_then(Value::as_array)
        {
            prefer(
                &mut self.tool_choice,
                Some(super::ToolChoice::Allowed {
                    names: names
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect(),
                    required: config.get("mode").and_then(Value::as_str) == Some("ANY"),
                }),
            );
        }
        prefer(
            &mut self.generation.temperature,
            take(fields, "temperature"),
        );
        prefer(&mut self.generation.top_p, take(fields, "top_p"));
        prefer(&mut self.generation.max_tokens, take(fields, "max_tokens"));
        prefer(
            &mut self.generation.frequency_penalty,
            take(fields, "frequency_penalty"),
        );
        prefer(
            &mut self.generation.presence_penalty,
            take(fields, "presence_penalty"),
        );
        prefer(&mut self.generation.seed, take(fields, "seed"));
        prefer(
            &mut self.generation.stop,
            take(fields, "stop").or_else(|| take(fields, "__anthropic_stop_sequences")),
        );
        prefer(
            &mut self.parallel_tool_calls,
            take(fields, "parallel_tool_calls"),
        );
        if let Some(strict) =
            take(fields, "cohere_strict_tools").or_else(|| take(fields, "strict_tools"))
            && let Some(tools) = self.tools.as_mut()
        {
            for tool in tools {
                prefer(&mut tool.strict, Some(strict));
            }
        }
        self.parameters.import_fields(fields);
        if let Some(container) = fields
            .remove("container")
            .as_ref()
            .and_then(conversation_id)
        {
            for tool in &mut self.parameters.native_tools {
                if let NativeTool::CodeExecution(tool) = tool {
                    prefer(&mut tool.container, Some(container.clone()));
                }
            }
        }
        self.parameters.import_anthropic_session();
        for name in [
            "__google_safety_settings",
            "__google_cached_content",
            "__anthropic_inference_geo",
        ] {
            fields.remove(name);
        }
    }
    fn import_thinking(&mut self, value: &Value) {
        if let Some(enabled) = value
            .as_bool()
            .or_else(|| value.get("enabled").and_then(Value::as_bool))
            .or_else(|| value.get("enable_thinking").and_then(Value::as_bool))
        {
            prefer(
                &mut self.reasoning.mode,
                Some(if enabled { "enabled" } else { "disabled" }.into()),
            );
        }
        if let Some(mode) = value
            .get("type")
            .or_else(|| value.get("thinking_mode"))
            .and_then(Value::as_str)
        {
            prefer(
                &mut self.reasoning.mode,
                Some(if mode == "adaptive" { "auto" } else { mode }.into()),
            );
        }
        for key in ["effort", "thinkingLevel"] {
            if let Some(effort) = value.get(key).and_then(Value::as_str) {
                prefer(
                    &mut self.reasoning.level,
                    crate::thinking::ThinkingLevel::from_wire(effort).ok(),
                );
            }
        }
        for key in ["budget_tokens", "max_tokens", "thinkingBudget"] {
            if let Some(budget) = value.get(key).and_then(Value::as_i64) {
                if budget == -1 {
                    prefer(&mut self.reasoning.mode, Some("auto".into()));
                } else if budget == 0 {
                    prefer(&mut self.reasoning.mode, Some("disabled".into()));
                } else if let Ok(budget) = u32::try_from(budget) {
                    prefer(&mut self.reasoning.budget_tokens, Some(budget));
                    prefer(&mut self.reasoning.mode, Some("enabled".into()));
                }
            }
        }
        if let Some(display) = value.get("summary").or_else(|| value.get("display")) {
            prefer(&mut self.reasoning.summary, summary(display));
        }
        if let Some(enabled) = value.get("includeThoughts").and_then(Value::as_bool) {
            prefer(
                &mut self.reasoning.summary,
                Some(if enabled {
                    ReasoningSummary::Auto
                } else {
                    ReasoningSummary::None
                }),
            );
        }
        if value.get("exclude").and_then(Value::as_bool) == Some(true) {
            prefer(&mut self.reasoning.summary, Some(ReasoningSummary::None));
        }
        if value.get("effort").is_some_and(Value::is_null) {
            self.parameters.reset(ParameterReset::ReasoningLevel);
        }
        if value.get("summary").is_some_and(Value::is_null) {
            self.parameters.reset(ParameterReset::ReasoningSummary);
        }
    }
}
fn family_for_tools<'a>(family: &'a str, key: &str) -> &'a str {
    match key {
        "raw_tools" => "google",
        "server_tools" => "anthropic",
        "passthrough_tools" => "open-responses",
        _ => family,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn legacy_snapshot_imports_unified_controls_and_is_idempotent() {
        let mut request: AiRequest = serde_json::from_value(json!({
            "model":"test", "items":[],
            "reasoning":{"display":"summarized"},
            "tool_choice":{"raw":{"type":"web_search"}},
            "response_format":{"type":"json_schema","name":"answer","schema":{}},
            "parameters":{
                "session":{"affinity_id":"session","thread_id":"thread","window_id":"window",
                    "parent_thread_id":"parent","forked_from_thread_id":"fork",
                    "installation_id":"discard","provider_context":{"private":true}},
                "hosted_tools":[
                    {"family":"open-responses","definition":{"type":"web_search","filters":{"allowed_domains":["example.com"]}}},
                    {"family":"open-responses","definition":{"type":"file_search"}}],
                "service":{"family":"anthropic","tier":"standard_only"},
                "response_presence":{"null_controls":["previous_response_id","top_logprobs"],"reasoning_summary_null":true},
                "cache":{"retention":"24h"},
                "continuation":{"conversation":{"id":"conversation"}},
                "end_user":"old","safety_identifier":"safe","logprobs":true,"top_logprobs":4,
                "response_format_description":"description",
                "metadata":{"keep":"value","drop":2}
            }
        })).unwrap();
        assert_eq!(request.parameters.session.id.as_deref(), Some("session"));
        assert_eq!(
            request.parameters.session.parent_id.as_deref(),
            Some("parent")
        );
        assert_eq!(
            request.parameters.session.forked_from_id.as_deref(),
            Some("fork")
        );
        assert_eq!(request.parameters.service_tier, Some(ServiceTier::Default));
        assert_eq!(
            request.parameters.cache.retention,
            Some(CacheRetention::Extended)
        );
        assert_eq!(
            request
                .parameters
                .continuation
                .as_ref()
                .and_then(Continuation::conversation_id),
            Some("conversation")
        );
        assert_eq!(request.parameters.end_user_id.as_deref(), Some("safe"));
        assert_eq!(request.parameters.logprobs, Some(4));
        assert_eq!(request.parameters.native_tools.len(), 1);
        assert!(request.parameters.resets(ParameterReset::Continuation));
        assert!(request.parameters.resets(ParameterReset::ReasoningSummary));
        assert_eq!(request.reasoning.summary, Some(ReasoningSummary::Auto));
        assert_eq!(request.parameters.metadata.as_ref().unwrap().len(), 1);
        assert!(
            matches!(&request.response_format, Some(ResponseFormat::JsonSchema { description: Some(description), .. }) if description == "description")
        );
        let stored = serde_json::to_value(&request).unwrap();
        request.normalize_parameters();
        assert_eq!(serde_json::to_value(&request).unwrap(), stored);
        let restored: AiRequest = serde_json::from_value(stored.clone()).unwrap();
        assert_eq!(serde_json::to_value(restored).unwrap(), stored);
        assert!(
            stored["parameters"]["session"]
                .get("installation_id")
                .is_none()
        );
    }

    #[test]
    fn session_sources_preserve_first_value_and_drop_provider_identity() {
        let mut parameters = RequestParameters::default();
        parameters.import_client_metadata(json!({"session_id":"first","thread_id":"second","parent_thread_id":"parent","installation_id":"discard"}));
        parameters.normalize_headers(|name| match name {
            "x-session-id" => Some("header"),
            "x-parent-session-id" => Some("header-parent"),
            "x-client-request-id" => Some("request"),
            _ => None,
        });
        assert_eq!(parameters.session.id.as_deref(), Some("first"));
        assert_eq!(parameters.session.parent_id.as_deref(), Some("parent"));
        assert_eq!(parameters.session.request_id.as_deref(), Some("request"));
        parameters.end_user_id = Some(
            r#"{"session_id":"anthropic","parent_session_id":"parent","account_uuid":"discard"}"#
                .into(),
        );
        parameters.import_anthropic_session();
        assert_eq!(parameters.session.id.as_deref(), Some("first"));
        assert!(parameters.end_user_id.is_none());
    }

    #[test]
    fn explicit_null_records_reset_without_serializing_control_nulls() {
        let parameters: RequestParameters = serde_json::from_value(json!({
            "background":null,"continuation":null,"logprobs":null,"service_tier":null,
            "resets":["background"]
        }))
        .unwrap();
        assert_eq!(
            parameters
                .resets
                .iter()
                .filter(|reset| **reset == ParameterReset::Background)
                .count(),
            1
        );
        for reset in [
            ParameterReset::Background,
            ParameterReset::Continuation,
            ParameterReset::Logprobs,
            ParameterReset::ServiceTier,
        ] {
            assert!(parameters.resets(reset));
        }
        let stored = serde_json::to_value(parameters).unwrap();
        assert!(stored.get("background").is_none());
        assert!(stored.get("continuation").is_none());
    }

    #[test]
    fn public_legacy_boundary_imports_responses_and_combined_google_tools_once() {
        let mut request = AiRequest::new("test", vec![]);
        let extension = json!({
            "previous_response_id":"previous",
            "passthrough_tools":[{"type":"web_search"}],
            "passthrough_body":{"client_metadata":{"thread_id":"thread"}}
        });
        request.import_legacy_responses_extension(extension.clone());
        request.import_legacy_responses_extension(extension);
        assert_eq!(request.parameters.native_tools.len(), 1);
        assert_eq!(request.parameters.session.id.as_deref(), Some("thread"));
        assert_eq!(
            request
                .parameters
                .continuation
                .as_ref()
                .and_then(Continuation::previous_response_id),
            Some("previous")
        );
        request
            .parameters
            .import_tool("google", &json!({"googleSearch":{},"codeExecution":{}}));
        assert_eq!(request.parameters.native_tools.len(), 2);
        assert!(
            request
                .parameters
                .native_tools
                .iter()
                .any(|tool| matches!(tool, NativeTool::CodeExecution(_)))
        );
        let cleared: RequestParameters =
            serde_json::from_value(json!({"native_tools":[]})).unwrap();
        assert!(cleared.resets(ParameterReset::NativeTools));
    }

    #[test]
    fn canonical_tools_and_populated_legacy_presence_do_not_create_resets() {
        let native_tools = vec![NativeTool::CodeExecution(CodeExecutionTool {
            container: Some("canonical-container".into()),
        })];
        let request: AiRequest = serde_json::from_value(json!({
            "model":"test","items":[],"instructions":"preserved",
            "parameters":{
                "native_tools":native_tools,
                "instructions_specified":true,
                "hosted_tools_specified":true,
                "hosted_tools":[{"family":"google","definition":{"codeExecution":{},"googleSearch":{}}}]
            }
        })).unwrap();
        assert_eq!(request.parameters.native_tools, native_tools);
        assert!(!request.parameters.resets(ParameterReset::Instructions));
        assert!(!request.parameters.resets(ParameterReset::NativeTools));
        let cleared: AiRequest = serde_json::from_value(json!({
            "model":"test","items":[],
            "parameters":{"instructions_specified":true,"hosted_tools_specified":true,"hosted_tools":[]}
        })).unwrap();
        assert!(cleared.parameters.resets(ParameterReset::Instructions));
        assert!(cleared.parameters.resets(ParameterReset::NativeTools));
    }
}
