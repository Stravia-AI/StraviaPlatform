//! Format-1 aliases are derived at the plugin boundary, never canonical state.
use std::collections::BTreeMap;

use serde::{
    Serialize, Serializer,
    ser::{SerializeMap, SerializeSeq},
};
use serde_json::Value;
use stravia_runtime_contract::protocol::{
    ids::Protocol,
    ir::{
        self, AiRequest, CacheRetention, Continuation, Modality, NativeTool, ParameterReset,
        ReasoningSummary, ResponseFormat, ServiceTier, ToolChoice, native_tool_to_anthropic,
        native_tool_to_chat_web_search_options, native_tool_to_gemini, native_tool_to_responses,
    },
};

use crate::{bindings::stravia::vendor::types::CanonicalPayload, envelope::encode_payload};

#[derive(Serialize)]
#[serde(untagged)]
enum Field<'a> {
    Json(&'a Value),
    Owned(Value),
    String(&'a str),
    Bool(bool),
    U32(u32),
    I64(i64),
    Float(f64),
    Strings(&'a [String]),
    Audio(&'a ir::AudioOutput),
    Metadata(&'a BTreeMap<String, String>),
    Bias(&'a std::collections::HashMap<String, f64>),
    Modalities(&'a [Modality]),
    GoogleModalities(GoogleModalities<'a>),
    Native(NativeProjection<'a>),
    Compaction(CompactionView<'a>),
    Object(Fields<'a>),
}
type Fields<'a> = BTreeMap<&'a str, Field<'a>>;
macro_rules! optional {
    ($map:expr, $key:expr, $value:expr, $variant:ident) => {
        if let Some(value) = $value {
            $map.insert($key, Field::$variant(value));
        }
    };
}

struct GoogleModalities<'a>(&'a [Modality]);
impl Serialize for GoogleModalities<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut seq = serializer.serialize_seq(Some(self.0.len()))?;
        for modality in self.0 {
            seq.serialize_element(match modality {
                Modality::Text => "TEXT",
                Modality::Audio => "AUDIO",
                Modality::Image => "IMAGE",
            })?;
        }
        seq.end()
    }
}

#[derive(Clone, Copy)]
enum NativeFamily {
    Responses,
    Anthropic,
    Gemini,
}
impl NativeFamily {
    fn name(self) -> &'static str {
        match self {
            Self::Responses => "open-responses",
            Self::Anthropic => "anthropic",
            Self::Gemini => "google",
        }
    }
    fn convert(self, tool: &NativeTool) -> Option<Value> {
        match self {
            Self::Responses => native_tool_to_responses(tool),
            Self::Anthropic => native_tool_to_anthropic(tool),
            Self::Gemini => native_tool_to_gemini(tool),
        }
    }
}
struct NativeProjection<'a> {
    tools: &'a [NativeTool],
    family: NativeFamily,
}
impl Serialize for NativeProjection<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut seq = serializer.serialize_seq(None)?;
        for tool in self.tools {
            if let Some(value) = self.family.convert(tool) {
                seq.serialize_element(&value)?;
            }
        }
        seq.end()
    }
}
struct HostedProjection<'a>(&'a [NativeTool]);
impl Serialize for HostedProjection<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct Hosted {
            family: &'static str,
            definition: Value,
        }
        let mut seq = serializer.serialize_seq(None)?;
        for tool in self.0 {
            for family in [
                NativeFamily::Responses,
                NativeFamily::Anthropic,
                NativeFamily::Gemini,
            ] {
                if let Some(definition) = family.convert(tool) {
                    seq.serialize_element(&Hosted {
                        family: family.name(),
                        definition,
                    })?;
                }
            }
        }
        seq.end()
    }
}
fn retention(value: CacheRetention) -> &'static str {
    match value {
        CacheRetention::Default => "in_memory",
        CacheRetention::Extended => "24h",
    }
}
fn tier(value: ServiceTier, family: Option<Protocol>) -> &'static str {
    match value {
        ServiceTier::Auto => "auto",
        ServiceTier::Default if family == Some(Protocol::AnthropicMessages) => "standard_only",
        ServiceTier::Default => "default",
        ServiceTier::Flex => "flex",
        ServiceTier::Priority => "priority",
    }
}
fn summary(value: ReasoningSummary) -> &'static str {
    match value {
        ReasoningSummary::None => "none",
        ReasoningSummary::Auto => "auto",
        ReasoningSummary::Concise => "concise",
        ReasoningSummary::Detailed => "detailed",
    }
}
fn verbosity(value: ir::Verbosity) -> &'static str {
    match value {
        ir::Verbosity::Low => "low",
        ir::Verbosity::Medium => "medium",
        ir::Verbosity::High => "high",
    }
}
fn truncation(value: ir::Truncation) -> &'static str {
    match value {
        ir::Truncation::Auto => "auto",
        ir::Truncation::Disabled => "disabled",
    }
}
fn container(request: &AiRequest) -> Option<&str> {
    request
        .parameters
        .native_tools
        .iter()
        .find_map(|tool| match tool {
            NativeTool::CodeExecution(tool) => tool.container.as_deref(),
            _ => None,
        })
}

struct CompactionView<'a> {
    control: &'a ir::CompactionControl,
    family: Option<Protocol>,
}

impl Serialize for CompactionView<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct Trigger {
            #[serde(rename = "type")]
            kind: &'static str,
            value: u32,
        }
        #[derive(Serialize)]
        struct Edit {
            #[serde(rename = "type")]
            kind: &'static str,
            #[serde(skip_serializing_if = "Option::is_none")]
            trigger: Option<Trigger>,
        }
        #[derive(Serialize)]
        struct Compaction {
            #[serde(rename = "type")]
            kind: &'static str,
            #[serde(skip_serializing_if = "Option::is_none")]
            compact_threshold: Option<u32>,
        }
        if self.family == Some(Protocol::AnthropicMessages) {
            #[derive(Serialize)]
            struct Context {
                edits: [Edit; 1],
            }
            Context {
                edits: [Edit {
                    kind: "compact_20260112",
                    trigger: self.control.threshold_tokens.map(|value| Trigger {
                        kind: "input_tokens",
                        value,
                    }),
                }],
            }
            .serialize(serializer)
        } else {
            [Compaction {
                kind: "compaction",
                compact_threshold: self.control.threshold_tokens,
            }]
            .serialize(serializer)
        }
    }
}

/// Serialize borrowed conversation bodies with aliases understood by installed format-1 plugins.
pub fn encode_request_payload(request: &AiRequest, target_protocol: &str) -> CanonicalPayload {
    let family = Protocol::from_identifier(target_protocol)
        .or_else(|| request.meta.source_protocol.map(|source| source.protocol));
    encode_payload(&RequestView { request, family })
}
struct RequestView<'a> {
    request: &'a AiRequest,
    family: Option<Protocol>,
}
impl Serialize for RequestView<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let r = self.request;
        let (variant, ext, ingress) = projection(r, self.family);
        let extension = variant.map(|name| BTreeMap::from([(name, Field::Object(ext))]));
        let mut map = serializer.serialize_map(None)?;
        macro_rules! field {
            ($name:ident) => {
                map.serialize_entry(stringify!($name), &r.$name)?;
            };
        }
        field!(model);
        field!(items);
        field!(generation);
        field!(embedding);
        field!(stream);
        field!(tools);
        if let Some(instructions) = &r.instructions {
            map.serialize_entry("instructions", instructions)?;
        }
        if let Some(format) = &r.response_format {
            map.serialize_entry("response_format", format)?;
        }
        if let Some(choice) = &r.tool_choice {
            map.serialize_entry("tool_choice", &ToolChoiceView(choice))?;
        }
        if let Some(parallel) = r.parallel_tool_calls {
            map.serialize_entry("parallel_tool_calls", &parallel)?;
            map.serialize_entry("disable_parallel_tool_calls", &!parallel)?;
        }
        map.serialize_entry("reasoning", &ReasoningView(&r.reasoning))?;
        map.serialize_entry(
            "parameters",
            &ParametersView {
                request: r,
                family: self.family,
            },
        )?;
        map.serialize_entry("ext", &extension)?;
        map.serialize_entry(
            "meta",
            &MetaView {
                request: r,
                ingress,
            },
        )?;
        map.end()
    }
}
struct ToolChoiceView<'a>(&'a ToolChoice);
#[derive(Serialize)]
struct AllowedFunction<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    name: &'a str,
}
struct AllowedFunctions<'a>(&'a [String]);
impl Serialize for AllowedFunctions<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut seq = serializer.serialize_seq(Some(self.0.len()))?;
        for name in self.0 {
            seq.serialize_element(&AllowedFunction {
                kind: "function",
                name,
            })?;
        }
        seq.end()
    }
}
impl Serialize for ToolChoiceView<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self.0 {
            ToolChoice::Allowed { names, required } => {
                #[derive(Serialize)]
                struct Allowed<'a> {
                    #[serde(rename = "type")]
                    kind: &'static str,
                    mode: &'static str,
                    tools: AllowedFunctions<'a>,
                }
                BTreeMap::from([(
                    "raw",
                    Allowed {
                        kind: "allowed_tools",
                        mode: if *required { "required" } else { "auto" },
                        tools: AllowedFunctions(names),
                    },
                )])
                .serialize(serializer)
            }
            ToolChoice::Native(kind) => {
                let kind = match kind {
                    ir::NativeToolKind::WebSearch => "web_search",
                    ir::NativeToolKind::CodeExecution => "code_interpreter",
                    ir::NativeToolKind::ImageGeneration => "image_generation",
                    ir::NativeToolKind::ToolSearch => "tool_search",
                };
                BTreeMap::from([("raw", BTreeMap::from([("type", kind)]))]).serialize(serializer)
            }
            choice => choice.serialize(serializer),
        }
    }
}
struct ParametersView<'a> {
    request: &'a AiRequest,
    family: Option<Protocol>,
}
impl Serialize for ParametersView<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let p = &self.request.parameters;
        let mut map = serializer.serialize_map(None)?;
        macro_rules! field { ($($name:ident),* $(,)?) => { $(if let Some(value) = &p.$name { map.serialize_entry(stringify!($name), value)?; })* }; }
        field!(
            top_k,
            min_p,
            repetition_penalty,
            candidate_count,
            logit_bias,
            modalities,
            audio,
            verbosity,
            max_tool_calls,
            store,
            background,
            generate,
            truncation,
            compaction,
            metadata,
            end_user_id,
            service_tier
        );
        if let Some(control) = &p.compaction {
            map.serialize_entry(
                "context_management",
                &CompactionView {
                    control,
                    family: self.family,
                },
            )?;
        }
        if !p.native_tools.is_empty() {
            map.serialize_entry("native_tools", &p.native_tools)?;
        }
        map.serialize_entry("resets", &p.resets)?;
        if let Some(count) = p.logprobs {
            map.serialize_entry("logprobs", &true)?;
            map.serialize_entry("top_logprobs", &count)?;
        }
        map.serialize_entry("hosted_tools", &HostedProjection(&p.native_tools))?;
        map.serialize_entry(
            "instructions_specified",
            &(self.request.instructions.is_some() || p.resets(ParameterReset::Instructions)),
        )?;
        map.serialize_entry(
            "hosted_tools_specified",
            &(!p.native_tools.is_empty() || p.resets(ParameterReset::NativeTools)),
        )?;
        if let Some(user) = &p.end_user_id {
            map.serialize_entry("end_user", user)?;
            map.serialize_entry("safety_identifier", user)?;
        }
        if let Some(generate) = p.generate {
            map.serialize_entry("responses_generate", &generate)?;
        }
        let service = p.service_tier.map(|value| {
            BTreeMap::from([
                (
                    "family",
                    if self.family == Some(Protocol::AnthropicMessages) {
                        "anthropic"
                    } else {
                        "openai"
                    },
                ),
                ("tier", tier(value, self.family)),
            ])
        });
        if let Some(service) = service {
            map.serialize_entry("service", &service)?;
        }
        #[derive(Serialize)]
        struct Cache<'a> {
            #[serde(skip_serializing_if = "Option::is_none")]
            key: &'a Option<String>,
            #[serde(skip_serializing_if = "Option::is_none")]
            retention: Option<&'static str>,
        }
        map.serialize_entry(
            "cache",
            &Cache {
                key: &p.cache.key,
                retention: p.cache.retention.map(retention),
            },
        )?;
        #[derive(Serialize)]
        struct Session<'a> {
            #[serde(flatten)]
            canonical: &'a ir::SessionParameters,
            affinity_id: &'a Option<String>,
            thread_id: &'a Option<String>,
            window_id: &'a Option<String>,
            parent_thread_id: &'a Option<String>,
            forked_from_thread_id: &'a Option<String>,
        }
        map.serialize_entry(
            "session",
            &Session {
                canonical: &p.session,
                affinity_id: &p.session.id,
                thread_id: &p.session.id,
                window_id: &p.session.id,
                parent_thread_id: &p.session.parent_id,
                forked_from_thread_id: &p.session.forked_from_id,
            },
        )?;
        let continuation = p.continuation.as_ref().map(|value| match value {
            Continuation::PreviousResponse(id) => {
                BTreeMap::from([("previous_response_id", id.as_str())])
            }
            Continuation::Conversation(id) => BTreeMap::from([("conversation", id.as_str())]),
        });
        if let Some(continuation) = continuation {
            map.serialize_entry("continuation", &continuation)?;
        }
        map.end()
    }
}
struct ReasoningView<'a>(&'a ir::ReasoningConfig);
#[derive(Serialize)]
#[serde(untagged)]
enum LegacyEffort {
    Level(&'static str),
    Budget { budget: u32 },
}
impl Serialize for ReasoningView<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let r = self.0;
        let effort = r
            .level
            .map(|level| {
                LegacyEffort::Level(
                    if level == stravia_runtime_contract::thinking::ThinkingLevel::Off {
                        "none"
                    } else {
                        level.as_str()
                    },
                )
            })
            .or_else(|| {
                r.budget_tokens
                    .map(|budget| LegacyEffort::Budget { budget })
            });
        let mut map = serializer.serialize_map(None)?;
        map.serialize_entry("mode", &r.mode)?;
        map.serialize_entry("enabled", &r.is_enabled())?;
        map.serialize_entry("budget_tokens", &r.budget_tokens)?;
        map.serialize_entry("effort", &effort)?;
        if let Some(value) = r.summary {
            map.serialize_entry("display", &summary(value))?;
            map.serialize_entry("summary", &value)?;
        }
        map.serialize_entry("level", &r.level)?;
        map.serialize_entry("target_control", &r.target_control)?;
        map.end()
    }
}
#[derive(Serialize)]
struct VendorView<'a> {
    ingress: &'a Fields<'a>,
    egress: &'a std::collections::HashMap<String, Value>,
    passthrough_safe: Fields<'a>,
}
struct MetaView<'a> {
    request: &'a AiRequest,
    ingress: Fields<'a>,
}
impl Serialize for MetaView<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let meta = &self.request.meta;
        let mut map = serializer.serialize_map(None)?;
        map.serialize_entry(
            "source_protocol",
            &meta.source_protocol.map(|p| p.to_string()),
        )?;
        map.serialize_entry(
            "vendor",
            &VendorView {
                ingress: &self.ingress,
                egress: &meta.vendor.egress,
                passthrough_safe: Fields::new(),
            },
        )?;
        map.serialize_entry("media_routing", &meta.media_routing)?;
        map.end()
    }
}
fn format_fields(format: &ResponseFormat, chat: bool) -> Fields<'_> {
    let mut result = Fields::new();
    match format {
        ResponseFormat::Text => {
            result.insert("type", Field::String("text"));
        }
        ResponseFormat::JsonObject => {
            result.insert("type", Field::String("json_object"));
        }
        ResponseFormat::JsonSchema {
            name,
            description,
            schema,
            strict,
        } => {
            result.insert("type", Field::String("json_schema"));
            let mut details = Fields::new();
            details.insert("name", Field::String(name));
            details.insert("schema", Field::Json(schema));
            optional!(details, "description", description.as_deref(), String);
            optional!(details, "strict", *strict, Bool);
            if chat {
                result.insert("json_schema", Field::Object(details));
            } else {
                result.extend(details);
            }
        }
    }
    result
}
fn projection(
    r: &AiRequest,
    family: Option<Protocol>,
) -> (Option<&'static str>, Fields<'_>, Fields<'_>) {
    let p = &r.parameters;
    let mut ext = Fields::new();
    let mut ingress = Fields::new();
    let variant = match family {
        Some(Protocol::OpenAICompatible) => {
            optional!(ext, "audio", p.audio.as_ref(), Audio);
            optional!(ext, "logit_bias", p.logit_bias.as_ref(), Bias);
            optional!(ext, "logprobs", p.logprobs.map(|_| true), Bool);
            optional!(ext, "top_logprobs", p.logprobs, U32);
            optional!(ext, "modalities", p.modalities.as_deref(), Modalities);
            optional!(ext, "n", p.candidate_count, U32);
            optional!(
                ext,
                "prompt_cache_retention",
                p.cache.retention.map(retention),
                String
            );
            optional!(ext, "verbosity", p.verbosity.map(verbosity), String);
            if let Some(options) = p
                .native_tools
                .iter()
                .find_map(native_tool_to_chat_web_search_options)
            {
                ext.insert("web_search_options", Field::Owned(options));
            }
            optional!(ingress, "seed", r.generation.seed, I64);
            optional!(
                ingress,
                "frequency_penalty",
                r.generation.frequency_penalty,
                Float
            );
            optional!(
                ingress,
                "presence_penalty",
                r.generation.presence_penalty,
                Float
            );
            optional!(ingress, "stop", r.generation.stop.as_deref(), Strings);
            optional!(ingress, "parallel_tool_calls", r.parallel_tool_calls, Bool);
            optional!(ingress, "audio", p.audio.as_ref(), Audio);
            optional!(ingress, "modalities", p.modalities.as_deref(), Modalities);
            optional!(ingress, "logit_bias", p.logit_bias.as_ref(), Bias);
            optional!(ingress, "logprobs", p.logprobs.map(|_| true), Bool);
            optional!(ingress, "top_logprobs", p.logprobs, U32);
            optional!(ingress, "top_k", p.top_k, U32);
            optional!(ingress, "min_p", p.min_p, Float);
            optional!(ingress, "repetition_penalty", p.repetition_penalty, Float);
            optional!(ingress, "n", p.candidate_count, U32);
            optional!(ingress, "user", p.end_user_id.as_deref(), String);
            optional!(
                ingress,
                "safety_identifier",
                p.end_user_id.as_deref(),
                String
            );
            optional!(ingress, "prompt_cache_key", p.cache.key.as_deref(), String);
            optional!(
                ingress,
                "prompt_cache_retention",
                p.cache.retention.map(retention),
                String
            );
            optional!(
                ingress,
                "service_tier",
                p.service_tier.map(|v| tier(v, family)),
                String
            );
            optional!(
                ingress,
                "reasoning_effort",
                r.reasoning.level.map(|level| {
                    if level == stravia_runtime_contract::thinking::ThinkingLevel::Off {
                        "none"
                    } else {
                        level.as_str()
                    }
                }),
                String
            );
            if let Some(format) = &r.response_format {
                ingress.insert(
                    "response_format",
                    Field::Object(format_fields(format, true)),
                );
            }
            ext.insert(
                "stream_options",
                Field::Object(BTreeMap::from([(
                    "include_usage",
                    Field::Bool(r.stream.include_usage),
                )])),
            );
            ingress.insert(
                "stream_options",
                Field::Object(BTreeMap::from([(
                    "include_usage",
                    Field::Bool(r.stream.include_usage),
                )])),
            );
            Some("OpenAiChat")
        }
        Some(Protocol::OpenResponses) => {
            optional!(ext, "background", p.background, Bool);
            let mut include = Vec::new();
            if p.store == Some(false) && r.reasoning.is_enabled() {
                include.push(Value::String("reasoning.encrypted_content".into()));
            }
            if p.logprobs.is_some() {
                include.push(Value::String("message.output_text.logprobs".into()));
            }
            if !include.is_empty() {
                ext.insert("include", Field::Owned(Value::Array(include)));
            }
            optional!(
                ext,
                "previous_response_id",
                p.continuation
                    .as_ref()
                    .and_then(Continuation::previous_response_id),
                String
            );
            optional!(ext, "max_tool_calls", p.max_tool_calls, U32);
            optional!(ext, "safety_identifier", p.end_user_id.as_deref(), String);
            optional!(ext, "prompt_cache_key", p.cache.key.as_deref(), String);
            optional!(ext, "top_logprobs", p.logprobs, U32);
            optional!(ext, "truncation", p.truncation.map(truncation), String);
            optional!(ext, "store", p.store, Bool);
            optional!(ext, "metadata", p.metadata.as_ref(), Metadata);
            optional!(
                ext,
                "service_tier",
                p.service_tier.map(|v| tier(v, family)),
                String
            );
            let mut text = Fields::new();
            optional!(text, "verbosity", p.verbosity.map(verbosity), String);
            if let Some(format) = &r.response_format {
                text.insert("format", Field::Object(format_fields(format, false)));
            }
            if !text.is_empty() {
                ext.insert("text", Field::Object(text));
            }
            let mut body = Fields::new();
            if let Some(control) = &p.compaction {
                body.insert(
                    "context_management",
                    Field::Compaction(CompactionView { control, family }),
                );
            }
            optional!(body, "generate", p.generate, Bool);
            optional!(
                body,
                "prompt_cache_retention",
                p.cache.retention.map(retention),
                String
            );
            optional!(
                body,
                "conversation",
                p.continuation
                    .as_ref()
                    .and_then(Continuation::conversation_id),
                String
            );
            let mut reasoning = Fields::new();
            optional!(
                reasoning,
                "effort",
                r.reasoning.level.map(|level| level.as_str()),
                String
            );
            optional!(
                reasoning,
                "summary",
                r.reasoning.summary.map(summary),
                String
            );
            if !reasoning.is_empty() {
                body.insert("reasoning", Field::Object(reasoning));
            }
            ext.insert("passthrough_body", Field::Object(body));
            ext.insert(
                "passthrough_tools",
                Field::Native(NativeProjection {
                    tools: &p.native_tools,
                    family: NativeFamily::Responses,
                }),
            );
            Some("OpenResponses")
        }
        Some(Protocol::AnthropicMessages) => {
            optional!(ext, "top_k", p.top_k, U32);
            optional!(ext, "container", container(r), String);
            optional!(
                ext,
                "service_tier",
                p.service_tier.map(|v| tier(v, family)),
                String
            );
            let mut output = Fields::new();
            if let Some(ResponseFormat::JsonSchema { schema, .. }) = &r.response_format {
                output.insert(
                    "format",
                    Field::Object(BTreeMap::from([
                        ("type", Field::String("json_schema")),
                        ("schema", Field::Json(schema)),
                    ])),
                );
            }
            optional!(
                output,
                "effort",
                r.reasoning.level.map(|level| level.as_str()),
                String
            );
            if !output.is_empty() {
                ext.insert("output_config", Field::Object(output));
            }
            ext.insert(
                "server_tools",
                Field::Native(NativeProjection {
                    tools: &p.native_tools,
                    family: NativeFamily::Anthropic,
                }),
            );
            optional!(ingress, "__anthropic_top_k", p.top_k, U32);
            optional!(ingress, "__anthropic_container", container(r), String);
            optional!(
                ingress,
                "__anthropic_service_tier",
                p.service_tier.map(|v| tier(v, family)),
                String
            );
            if let Some(control) = &p.compaction {
                ingress.insert(
                    "__anthropic_context_management",
                    Field::Compaction(CompactionView { control, family }),
                );
            }
            if let Some(user) = &p.end_user_id {
                ingress.insert(
                    "__anthropic_metadata",
                    Field::Object(BTreeMap::from([("user_id", Field::String(user))])),
                );
            }
            Some("Anthropic")
        }
        Some(Protocol::GoogleGemini) => {
            optional!(ext, "top_k", p.top_k, U32);
            optional!(ext, "candidate_count", p.candidate_count, U32);
            optional!(ext, "response_logprobs", p.logprobs.map(|_| true), Bool);
            optional!(ext, "logprobs", p.logprobs, U32);
            let mime = r.response_format.as_ref().map(|format| match format {
                ResponseFormat::Text => "text/plain",
                _ => "application/json",
            });
            optional!(ext, "response_mime_type", mime, String);
            let mut generation = Fields::new();
            if let Some(ResponseFormat::JsonSchema { schema, .. }) = &r.response_format {
                ext.insert("response_json_schema", Field::Json(schema));
                generation.insert("responseJsonSchema", Field::Json(schema));
            }
            optional!(generation, "topK", p.top_k, U32);
            optional!(generation, "candidateCount", p.candidate_count, U32);
            optional!(
                generation,
                "responseLogprobs",
                p.logprobs.map(|_| true),
                Bool
            );
            optional!(generation, "logprobs", p.logprobs, U32);
            optional!(generation, "responseMimeType", mime, String);
            if let Some(modalities) = p.modalities.as_deref() {
                ext.insert(
                    "response_modalities",
                    Field::GoogleModalities(GoogleModalities(modalities)),
                );
                generation.insert(
                    "responseModalities",
                    Field::GoogleModalities(GoogleModalities(modalities)),
                );
            }
            ingress.insert("__google_generation_config", Field::Object(generation));
            Some("Google")
        }
        _ => None,
    };
    // These envelopes contain expression fidelity, not permission for arbitrary controls.
    if r.meta.source_protocol.map(|source| source.protocol) == family {
        let keys: &[&str] = match family {
            Some(Protocol::GoogleGemini) => {
                &["__google_raw_system_instruction", "__google_raw_tools"]
            }
            Some(Protocol::AnthropicMessages) => &[
                "__anthropic_raw_system",
                "__anthropic_raw_messages",
                "__anthropic_raw_tools",
            ],
            _ => &[],
        };
        for key in keys {
            if let Some(value) = r.meta.vendor.ingress.get(*key) {
                ingress.insert(key, Field::Json(value));
            }
        }
    }
    (variant, ext, ingress)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn value(request: &AiRequest, target: &str) -> Value {
        serde_json::from_slice(&encode_request_payload(request, target).body).unwrap()
    }

    #[test]
    fn installed_parameter_and_extension_readers_receive_legacy_shapes() {
        let mut request = AiRequest::new("model", Vec::new());
        let p = &mut request.parameters;
        p.session.id = Some("session".into());
        p.session.parent_id = Some("parent".into());
        p.logprobs = Some(0);
        p.end_user_id = Some("user".into());
        p.cache.retention = Some(CacheRetention::Extended);
        p.service_tier = Some(ServiceTier::Default);
        p.continuation = Some(Continuation::PreviousResponse("response".into()));
        p.reset(ParameterReset::Instructions);
        p.reset(ParameterReset::NativeTools);
        request.reasoning.summary = Some(ReasoningSummary::Auto);
        let projected = value(&request, "anthropic");
        assert_eq!(projected["parameters"]["session"]["affinity_id"], "session");
        assert_eq!(projected["parameters"]["session"]["thread_id"], "session");
        assert_eq!(
            projected["parameters"]["session"]["parent_thread_id"],
            "parent"
        );
        assert_eq!(projected["parameters"]["logprobs"], true);
        assert_eq!(projected["parameters"]["top_logprobs"], 0);
        assert_eq!(projected["parameters"]["cache"]["retention"], "24h");
        assert_eq!(
            projected["parameters"]["service"],
            json!({"family":"anthropic", "tier":"standard_only"})
        );
        assert_eq!(
            projected["parameters"]["continuation"],
            json!({"previous_response_id":"response"})
        );
        assert_eq!(projected["parameters"]["instructions_specified"], true);
        assert_eq!(projected["parameters"]["hosted_tools_specified"], true);
        assert_eq!(projected["reasoning"]["display"], "auto");
        assert_eq!(
            projected["meta"]["vendor"]["ingress"]["__anthropic_metadata"]["user_id"],
            "user"
        );
    }

    #[test]
    fn native_tools_project_to_each_supported_official_family() {
        let mut request = AiRequest::new("model", Vec::new());
        request.parameters.native_tools = vec![
            NativeTool::WebSearch(ir::WebSearchTool::default()),
            NativeTool::Custom(ir::CustomTool {
                name: "custom".into(),
                description: None,
                defer_loading: None,
                grammar: None,
            }),
        ];
        let projected = value(&request, "open-responses");
        let hosted = projected["parameters"]["hosted_tools"].as_array().unwrap();
        assert_eq!(hosted.len(), 4);
        assert_eq!(hosted[0]["family"], "open-responses");
        assert_eq!(hosted[0]["definition"]["type"], "web_search");
        assert_eq!(hosted[1]["family"], "anthropic");
        assert!(
            hosted[1]["definition"]["type"]
                .as_str()
                .unwrap()
                .starts_with("web_search_")
        );
        assert_eq!(hosted[2]["definition"], json!({"googleSearch":{}}));
        assert_eq!(
            projected["ext"]["OpenResponses"]["passthrough_tools"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        assert_eq!(
            value(&request, "anthropic")["ext"]["Anthropic"]["server_tools"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn extensions_are_whitelisted_and_never_send_reset_nulls() {
        let mut request = AiRequest::new("model", Vec::new());
        request.parameters.reset(ParameterReset::Temperature);
        request.parameters.reset(ParameterReset::ResponseFormat);
        request
            .meta
            .vendor
            .ingress
            .insert("unknown".into(), json!(true));
        request
            .meta
            .vendor
            .ingress
            .insert("__google_cached_content".into(), json!("opaque"));
        let projected = value(&request, "open-responses");
        assert!(
            projected["meta"]["vendor"]["ingress"]
                .as_object()
                .unwrap()
                .is_empty()
        );
        assert!(
            projected["ext"]["OpenResponses"]["passthrough_body"]
                .as_object()
                .unwrap()
                .is_empty()
        );
        assert!(projected["ext"]["OpenResponses"].get("text").is_none());
        assert!(
            projected["parameters"]
                .get("google_cached_content")
                .is_none()
        );
    }

    #[test]
    fn responses_derive_include_and_chat_merges_logprobs() {
        let mut request = AiRequest::new("model", Vec::new());
        request.parameters.logprobs = Some(3);
        request.parameters.candidate_count = Some(2);
        request.parameters.store = Some(false);
        request.reasoning.mode = Some("enabled".into());
        let responses = value(&request, "open-responses");
        assert_eq!(
            responses["ext"]["OpenResponses"]["include"],
            json!([
                "reasoning.encrypted_content",
                "message.output_text.logprobs"
            ])
        );
        let chat = value(&request, "openai-chat");
        assert_eq!(chat["ext"]["OpenAiChat"]["logprobs"], true);
        assert_eq!(chat["ext"]["OpenAiChat"]["top_logprobs"], 3);
        assert_eq!(chat["ext"]["OpenAiChat"]["n"], 2);
    }

    #[test]
    fn installed_plugins_receive_native_compaction_controls() {
        let mut request = AiRequest::new("model", Vec::new());
        for threshold_tokens in [None, Some(2000)] {
            request.parameters.compaction = Some(ir::CompactionControl {
                threshold_tokens,
                instructions: None,
            });
            let responses = value(&request, "open-responses");
            let expected = if threshold_tokens.is_some() {
                json!([{"type": "compaction", "compact_threshold": 2000}])
            } else {
                json!([{"type": "compaction"}])
            };
            assert_eq!(responses["parameters"]["context_management"], expected);
            assert_eq!(
                responses["ext"]["OpenResponses"]["passthrough_body"]["context_management"],
                expected
            );
            let anthropic = value(&request, "anthropic");
            let expected = if threshold_tokens.is_some() {
                json!({"edits": [{"type": "compact_20260112", "trigger": {"type": "input_tokens", "value": 2000}}]})
            } else {
                json!({"edits": [{"type": "compact_20260112"}]})
            };
            assert_eq!(anthropic["parameters"]["context_management"], expected);
            assert_eq!(
                anthropic["meta"]["vendor"]["ingress"]["__anthropic_context_management"],
                expected
            );
            for projected in [responses, anthropic] {
                let mut decoded: AiRequest = serde_json::from_value(projected).unwrap();
                decoded.normalize_parameters();
                assert_eq!(decoded.parameters.compaction, request.parameters.compaction);
            }
        }
    }

    #[test]
    fn all_unified_controls_round_trip_through_format_one() {
        let mut request = AiRequest::new("model", Vec::new());
        request.instructions = Some("instructions".into());
        request.reasoning.mode = Some("enabled".into());
        request.reasoning.summary = Some(ReasoningSummary::Detailed);
        request.reasoning.budget_tokens = Some(1024);
        request.reasoning.level = Some(stravia_runtime_contract::thinking::ThinkingLevel::High);
        request.reasoning.target_control =
            Some(stravia_runtime_contract::thinking::TargetThinkingControl::Budget { value: 1024 });
        request.response_format = Some(ResponseFormat::JsonSchema {
            name: "result".into(),
            description: Some("description".into()),
            schema: json!({"type":"object"}),
            strict: Some(true),
        });
        request.tool_choice = Some(ToolChoice::Allowed {
            names: vec!["function".into()],
            required: true,
        });
        let p = &mut request.parameters;
        p.top_k = Some(4);
        p.min_p = Some(0.1);
        p.repetition_penalty = Some(1.2);
        p.candidate_count = Some(2);
        p.logit_bias = Some([("1".into(), 0.5)].into());
        p.logprobs = Some(3);
        p.modalities = Some(vec![Modality::Text, Modality::Audio, Modality::Image]);
        p.audio = Some(ir::AudioOutput {
            voice: Some("alloy".into()),
            format: Some("pcm16".into()),
        });
        p.verbosity = Some(ir::Verbosity::High);
        p.max_tool_calls = Some(5);
        p.store = Some(false);
        p.background = Some(false);
        p.generate = Some(false);
        p.truncation = Some(ir::Truncation::Auto);
        p.compaction = Some(ir::CompactionControl {
            threshold_tokens: Some(4096),
            instructions: None,
        });
        p.metadata = Some([("key".into(), "value".into())].into());
        p.end_user_id = Some("user".into());
        p.service_tier = Some(ServiceTier::Default);
        p.cache.key = Some("cache".into());
        p.cache.retention = Some(CacheRetention::Extended);
        p.continuation = Some(Continuation::PreviousResponse("response".into()));
        p.session = ir::SessionParameters {
            id: Some("id".into()),
            request_id: Some("request".into()),
            parent_id: Some("parent".into()),
            forked_from_id: Some("fork".into()),
            context_window_id: Some("context".into()),
            turn_id: Some("turn".into()),
            parent_turn_id: Some("parent-turn".into()),
            root_turn_id: Some("root".into()),
        };
        p.native_tools = serde_json::from_value(json!([
            {"type":"web_search", "allowed_domains":["example.com"]},
            {"type":"code_execution", "container":"container"},
            {"type":"image_generation", "quality":"high"},
            {"type":"tool_search", "description":"search", "parameters":{"type":"object"}},
            {"type":"custom", "name":"custom", "grammar":{"syntax":"regex", "definition":".*"}},
            {"type":"namespace", "name":"namespace", "tools":[{"type":"function", "name":"function", "parameters":{"type":"object"}}]}
        ])).unwrap();
        p.reset(ParameterReset::Temperature);
        for target in ["openai-chat", "open-responses", "anthropic", "google"] {
            let mut decoded: AiRequest =
                serde_json::from_slice(&encode_request_payload(&request, target).body).unwrap();
            decoded.normalize_parameters();
            assert_eq!(
                serde_json::to_value(&decoded.parameters).unwrap(),
                serde_json::to_value(&request.parameters).unwrap(),
                "{target}"
            );
            assert_eq!(
                serde_json::to_value(&decoded.reasoning).unwrap(),
                serde_json::to_value(&request.reasoning).unwrap(),
                "{target}"
            );
            assert_eq!(
                serde_json::to_value(&decoded.tool_choice).unwrap(),
                serde_json::to_value(&request.tool_choice).unwrap(),
                "{target}"
            );
            assert_eq!(
                serde_json::to_value(&decoded.response_format).unwrap(),
                serde_json::to_value(&request.response_format).unwrap(),
                "{target}"
            );
        }
        request.parameters.continuation = Some(Continuation::Conversation("conversation".into()));
        request.parameters.cache.retention = Some(CacheRetention::Default);
        request.tool_choice = Some(ToolChoice::Native(ir::NativeToolKind::CodeExecution));
        let mut decoded: AiRequest =
            serde_json::from_slice(&encode_request_payload(&request, "open-responses").body)
                .unwrap();
        decoded.normalize_parameters();
        assert_eq!(
            serde_json::to_value(&decoded.parameters).unwrap(),
            serde_json::to_value(&request.parameters).unwrap()
        );
        assert_eq!(
            serde_json::to_value(&decoded.tool_choice).unwrap(),
            serde_json::to_value(&request.tool_choice).unwrap()
        );
    }

    #[test]
    fn budget_only_reasoning_and_explicit_resets_round_trip() {
        let mut request = AiRequest::new("model", Vec::new());
        request.reasoning.budget_tokens = Some(1024);
        request.parameters.reset(ParameterReset::Instructions);
        request.parameters.reset(ParameterReset::NativeTools);
        request.parameters.reset(ParameterReset::ReasoningSummary);
        let mut decoded: AiRequest =
            serde_json::from_slice(&encode_request_payload(&request, "open-responses").body)
                .unwrap();
        decoded.normalize_parameters();
        assert_eq!(
            serde_json::to_value(&decoded.parameters).unwrap(),
            serde_json::to_value(&request.parameters).unwrap()
        );
        assert_eq!(
            serde_json::to_value(&decoded.reasoning).unwrap(),
            serde_json::to_value(&request.reasoning).unwrap()
        );
    }
}
