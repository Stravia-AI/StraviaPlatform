//! `GetChatMessageRequest` protobuf encoder.
//!
//! Field layout recovered from `dwgx/WindsurfAPI` `src/devin-connect.js`,
//! whose tags were calibrated against live `devin.exe` captures:
//!
//! ```text
//! GetChatMessageRequest
//!   #1  ClientMetadata { #1 client_name, #2 client_version, #3 session_token,
//!                        #4 language, #5 platform, #7 client_version,
//!                        #12 client_name, #28 ide_type, #31 fingerprint (732 hex chars) }
//!   #2  system prompt
//!   #3  repeated ChatMessagePrompt { #1 uuid, #2 source, #3 prompt,
//!                              #6 ChatToolCall{#1 id,#2 name,#3 args_json},
//!                              #7 tool_call_id, #10 ImageData{#1 b64,#2 mime},
//!                              #11 thinking, #12 signature, #13 redacted,
//!                              #18 signature_type }
//!   #7  request source enum (5)
//!   #8  CompletionConfig { #1 enabled, #2 max_tokens, #3 max_newlines,
//!                          #5 f64 temperature, #7 top_k, #8 f64 top_p,
//!                          #9 repeated stop_patterns, #10 uint64 seed,
//!                          #16 return_logprob, #17 service_tier }
//!   #10 repeated ToolDef { #1 name, #2 description, #3 parameters JSON }
//!   #11 disable_parallel_tool_calls (inverse canonical parallel_tool_calls)
//!   #12 ChatToolChoice { oneof #1 option_name("auto"), #2 tool_name }
//!   #15 CortexTrajectoryReference { #1 trajectory_id, #2 step_index (>0 only),
//!                                   #3 =4 CASCADE, #4 =14 USER_INPUT }
//!   #16 cascade id
//!   #20 = 1
//!   #21 model uid
//!   #26 assignment jwt — present only when AssignModel resolved the uid
//! ```
//!
//! The first-party Devin desktop distribution's
//! `resources/app/extensions/windsurf/dist/extension.js` confirms
//! `exa.codeium_common_pb.CompletionConfiguration` #16 `return_logprob` as
//! protobuf bool and #17 `service_tier` as string. The string descriptor
//! does not establish supported tier values; only the existing native
//! `priority` mapping is retained, not guessed mappings for other tiers.
//!
//! The #15/#16/#26 shape mirrors current devin CLI captures (3000.10.x): the
//! CLI calls `AssignModel` per session and carries the returned jwt at #26.
//! Calling a router uid directly without an assignment is answered with the
//! canned `unavailable: third-party model provider` trailer — a permanent
//! failure disguised as a transient one.
//!
//! ChatMessagePrompt.source: 1 = user, 2 = assistant, 4 = tool result.
//!
//! Two wire behaviors from the reference are load-bearing and copied on
//! purpose:
//! * Claude-family upstreams reject a request that declares tools with an
//!   absent or empty system prompt, so a minimal fallback system is injected
//!   whenever tools are present without one.
//! * The upstream MCP gate pattern-matches ToolDef descriptions and parameter
//!   schema annotation keys (`description`, `title`, `$comment`, `x-*`)
//!   against known tool signatures and rejects the whole request
//!   (`permission_denied`). The verified workaround emits the tool NAME as
//!   the description and strips schema annotation keys; the stripped prose
//!   descriptions are relocated into the system prompt — the gate scans
//!   ToolDef fields, not prompt text — so the model still sees name, full
//!   parameter structure, and per-tool guidance.
//! * A second gate scans prompt/message text for competitor-identity
//!   fingerprints and policy phrases (`sanitize.rs`); known trigger
//!   sentences are rewritten to neutral equivalents, and tool names must
//!   match `[A-Za-z0-9_-]` or the request fails upstream with a vague
//!   `invalid_argument` — both are handled before encoding.

use std::collections::HashMap;

use anyhow::{Context, bail};
use serde_json::Value;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::proto::{
    ProtoField, parse_fields, write_fixed64_field, write_message_field, write_string_field,
    write_varint_field,
};
use super::sanitize;
use super::{CUSTOM_TOOL_META, ThinkingReplay};
use stravia_runtime_contract::protocol::ir::AiItem;
use stravia_runtime_contract::protocol::ir::AiRequest;
use stravia_runtime_contract::protocol::ir::ContentBlock;
use stravia_runtime_contract::protocol::ir::DocumentSource;
use stravia_runtime_contract::protocol::ir::MediaSource;
use stravia_runtime_contract::protocol::ir::MessageContent;
use stravia_runtime_contract::protocol::ir::Role;
use stravia_runtime_contract::protocol::ir::ServiceTier;

/// Connect-RPC method path on the Devin api-server host.
pub const GET_CHAT_MESSAGE_PATH: &str = "/exa.api_server_pb.ApiServerService/GetChatMessage";

/// ClientMetadata constants observed on live CLI requests. `chisel` is the
/// CLI's internal client name; the version tracks the Devin CLI build —
/// upstream serves a reduced model catalog to older versions, so this must
/// stay near the current release.
const CLIENT_NAME: &str = "chisel";
const CLIENT_VERSION: &str = "3000.11.3";
const CLIENT_LANGUAGE: &str = "en";

/// ClientMetadata #5 platform identity.
///
/// Wasm guests must select this from the host snapshot rather than from
/// `std::env::consts::OS`, which describes the Wasm target instead of the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DevinClientPlatform {
    Mac,
    Windows,
    Linux,
}

impl DevinClientPlatform {
    pub const fn as_wire(self) -> &'static str {
        match self {
            Self::Mac => "mac",
            Self::Windows => "windows",
            Self::Linux => "linux",
        }
    }
}

/// 私有协议沿用 CLI 逆向得到的 CompletionConfiguration 默认值。
/// max_newlines 限制行数，与 Token 上限不同。
const WIRE_DEFAULT_MAX_TOKENS: u32 = 128_000;
const WIRE_MAX_NEWLINES: u64 = 400;
const DEFAULT_TEMPERATURE: f64 = 1.0;
/// Exactly 0 → upstream "internal error" (live-verified); clamp to epsilon.
const MIN_TEMPERATURE: f64 = 0.001;
const DEFAULT_TOP_K: u64 = 40;
const DEFAULT_TOP_P: f64 = 0.95;

const SOURCE_USER: u64 = 1;
const SOURCE_ASSISTANT: u64 = 2;
const SOURCE_TOOL_RESULT: u64 = 4;

/// Injected when tools are declared without a system prompt — Claude-family
/// upstreams reject that combination, verified live by the reference project.
const TOOLS_FALLBACK_SYSTEM: &str =
    "You are a helpful assistant. Use the available tools when appropriate.";

/// Per-session wire shape for the current CLI protocol generation
/// (3000.10.x captures): `#15.1` trajectory id and `#16` cascade id are
/// stable per conversation, `#15.2` step_index increments once per
/// GetChatMessage call (absent at 0; the CLI's title-gen call consumes 1).
/// Both ids derive from the scoped transport identity — AssignModel binds
/// its jwt to the cascade id, so it must be computable before the chat
/// request exists.
#[derive(Debug)]
pub struct SessionShape {
    pub trajectory_id: String,
    pub cascade_id: String,
    pub step_index: u64,
}

/// Only host-scoped Principal+Target transport affinity may reuse a session.
/// Raw request session/cache/correlation ids and prompt contents are not
/// identities. Without scoped affinity each request receives fresh native ids.
/// Persisted production state, not this pure identity function, owns steps.
/// Credentials do not participate: refreshing a token must not rename a
/// host-scoped session. Separate namespaces preserve the two wire meanings.
pub fn session_shape(transport_affinity: Option<&str>) -> SessionShape {
    let (trajectory_id, cascade_id) =
        if let Some(source) = transport_affinity.filter(|value| !value.is_empty()) {
            (
                uuid_from_source(b"stravia:devin:trajectory:v1\0", source),
                uuid_from_source(b"stravia:devin:cascade:v1\0", source),
            )
        } else {
            (Uuid::new_v4().to_string(), Uuid::new_v4().to_string())
        };
    SessionShape {
        cascade_id,
        step_index: 0,
        trajectory_id,
    }
}

fn uuid_from_source(namespace: &[u8], source: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(namespace);
    hasher.update(source.as_bytes());
    let digest = hasher.finalize();
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    uuid::Builder::from_random_bytes(bytes)
        .into_uuid()
        .to_string()
}

/// Serialize the protobuf request body (un-enveloped). `session_token` is
/// embedded SINGLE inside `ClientMetadata` — the doubling is only for the
/// HTTP `Authorization` header, which the vendor owns. `assignment_jwt`
/// comes from a prior `AssignModel` call and rides at #26.
#[cfg(test)]
pub(crate) fn encode_get_chat_message_request_with_platform(
    req: &AiRequest,
    session_token: &str,
    shape: &SessionShape,
    assignment_jwt: Option<&str>,
    platform: DevinClientPlatform,
) -> anyhow::Result<Vec<u8>> {
    let mut encoding_request = req.clone();
    super::prepare_request(&mut encoding_request);

    encode_prepared_request(
        &encoding_request,
        session_token,
        shape,
        assignment_jwt,
        platform,
    )
}

pub(crate) fn encode_prepared_request(
    req: &AiRequest,
    session_token: &str,
    shape: &SessionShape,
    assignment_jwt: Option<&str>,
    platform: DevinClientPlatform,
) -> anyhow::Result<Vec<u8>> {
    if session_token.trim().is_empty() {
        bail!("Devin Connect request is missing the session token");
    }
    let mut hits = sanitize::SanitizeHits::default();
    let mut system_prompt = sanitize::sanitize_prompt_text(&collect_system_prompt(req), &mut hits);
    let mut chat_messages = collect_chat_messages(req)?;
    for message in &mut chat_messages {
        if !message.text.is_empty() {
            message.text = sanitize::sanitize_message_text(&message.text, &mut hits);
        }
    }
    let (tool_defs, tool_descriptions) = encode_tool_defs(req, &mut hits)?;
    if system_prompt.is_empty() && !tool_defs.is_empty() {
        system_prompt = TOOLS_FALLBACK_SYSTEM.to_string();
    }
    // The MCP gate only scans ToolDef fields — relocate the stripped prose
    // into the system prompt so the model keeps per-tool guidance.
    system_prompt = with_tool_descriptions(&system_prompt, &tool_descriptions);
    if !hits.is_empty() {
        tracing::debug!(
            target: "stravia_protocol_codec::devin_connect",
            "devin request text sanitized: {hits:?}"
        );
    }

    let mut out = Vec::new();
    write_message_field(
        &mut out,
        1,
        &client_metadata(session_token, false, platform),
    );
    write_string_field(&mut out, 2, &system_prompt);
    for message in &chat_messages {
        write_message_field(&mut out, 3, &message.encode());
    }
    write_varint_field(&mut out, 7, 5);
    write_message_field(&mut out, 8, &completion_config(req));
    for tool_def in &tool_defs {
        write_message_field(&mut out, 10, tool_def);
    }
    if let Some(parallel) = req.parallel_tool_calls {
        write_varint_field(&mut out, 11, u64::from(!parallel));
    }
    let mut choice = Vec::new();
    match req.tool_choice.as_ref() {
        Some(stravia_runtime_contract::protocol::ir::ToolChoice::Auto) => {
            write_string_field(&mut choice, 1, "auto");
        }
        Some(stravia_runtime_contract::protocol::ir::ToolChoice::Named { name })
            if valid_tool_name(name) =>
        {
            write_string_field(&mut choice, 2, name);
        }
        _ => {}
    }
    if !choice.is_empty() {
        write_message_field(&mut out, 12, &choice);
    }
    write_message_field(&mut out, 15, &trajectory_ref(shape));
    write_string_field(&mut out, 16, &shape.cascade_id);
    write_varint_field(&mut out, 20, 1);
    write_string_field(&mut out, 21, &req.model);
    if let Some(jwt) = assignment_jwt {
        write_string_field(&mut out, 26, jwt);
    }
    Ok(out)
}

/// AssignModel resolves a router uid to the real model uid plus a
/// cascade-bound assignment jwt for #26. Unary, `application/proto` — raw
/// protobuf, no Connect envelope.
pub const ASSIGN_MODEL_PATH: &str = "/exa.api_server_pb.ApiServerService/AssignModel";

#[derive(Debug, Default, Clone)]
pub struct ModelAssignment {
    pub jwt: String,
    pub model_uid: String,
}

/// AssignModelRequest: `{1: metadata, 2: router_uid, 3: cascade_id}`.
pub fn encode_assign_model_request_with_platform(
    session_token: &str,
    router_uid: &str,
    cascade_id: &str,
    platform: DevinClientPlatform,
) -> Vec<u8> {
    let mut out = Vec::new();
    write_message_field(
        &mut out,
        1,
        &client_metadata(session_token, false, platform),
    );
    write_string_field(&mut out, 2, router_uid);
    write_string_field(&mut out, 3, cascade_id);
    out
}

/// AssignModelResponse: `{1: assignment{1: assignment_jwt, 2: model_uid}}`.
/// Both fields must be present — an empty assignment is worse than none.
pub fn decode_assign_model_response(body: &[u8]) -> Option<ModelAssignment> {
    let mut out = ModelAssignment::default();
    for field in parse_fields(body).ok()?.iter() {
        if field.number != 1 || field.wire_type != 2 {
            continue;
        }
        for inner in parse_fields(field.bytes).ok()?.iter() {
            match inner.number {
                1 if inner.wire_type == 2 => {
                    out.jwt = String::from_utf8_lossy(inner.bytes).into_owned();
                }
                2 if inner.wire_type == 2 => {
                    out.model_uid = String::from_utf8_lossy(inner.bytes).into_owned();
                }
                _ => {}
            }
        }
    }
    (!out.jwt.is_empty() && !out.model_uid.is_empty()).then_some(out)
}

/// Unary seat-management requests (GetUserStatus, GetCliModelConfigs) carry
/// only `ClientMetadata` at field #1 and travel as `application/proto` —
/// raw protobuf, no Connect envelope. `displays` adds #30
/// (`supported_model_displays` = [3,4,6,7,8]), which only the
/// GetCliModelConfigs capture carries.
pub fn encode_client_metadata_request_with_platform(
    session_token: &str,
    displays: bool,
    platform: DevinClientPlatform,
) -> Vec<u8> {
    let mut out = Vec::new();
    write_message_field(
        &mut out,
        1,
        &client_metadata(session_token, displays, platform),
    );
    out
}

/// One entry from `GetCliModelConfigsResponse`. `ClientModelConfig` repeats
/// at top-level #1; entries flagged #4 disabled are not callable and dropped
/// at decode time. Tag numbers are calibrated from a live 200 response
/// (`dwgx/WindsurfAPI` `devin-connect-catalog.js` decodeCatalog); the pricing
/// rows at #32 were recovered from the CLI's cached `model_configs` payload.
#[derive(Debug, Clone, Default)]
pub struct DevinModelConfig {
    /// #22 — the value `GetChatMessageRequest.model` expects.
    pub selector: String,
    /// #1 — friendly label, e.g. "Claude Opus 4.8 Medium".
    pub label: Option<String>,
    /// #5 — explicit upstream multimodal capability flag.
    pub supports_images: Option<bool>,
    /// #10 — upstream provider enum (1=cognition 2=openai 3=anthropic
    /// 4=google 7=moonshot 9=zhipu; other values surface as the raw number).
    pub provider: Option<u64>,
    /// #18 — context window tokens (observed 200000 on swe-1-6-slow).
    pub context_window: Option<u64>,
    /// #23.#23 — family alias inside ModelInfo, e.g. "claude-opus-4.8". Every
    /// selector of one family carries the same alias; entries without it are
    /// pseudo-selectors (adaptive, *-default, review lanes), not models.
    pub alias: Option<String>,
    /// #23.#20 — short alias the CLI pins on the family's canonical entry
    /// (e.g. `gpt-5-6-sol-medium` carries `gpt-5p6`); marks upstream's own
    /// default pick inside the family.
    pub short_alias: Option<String>,
    /// #23.#25 — `is_model_router`: a router uid must be resolved through
    /// `AssignModel` before `GetChatMessage`; calling it directly is answered
    /// with the canned `unavailable: third-party model provider` trailer.
    pub is_router: bool,
    /// #32 — per-row USD pricing, see [`DevinModelCost`].
    pub cost: Option<DevinModelCost>,
}

/// USD per 1M tokens, decoded from the #32 pricing rows. Each row names a
/// component (`Input` / `Cached input` / `Output`) and carries its price as a
/// float; auxiliary floats on the same row (ACU rate hints) are ignored.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct DevinModelCost {
    pub input: Option<f64>,
    pub cache_read: Option<f64>,
    pub output: Option<f64>,
}

/// The upstream provider enum carried at ClientModelConfig #10.
pub fn devin_upstream_provider_name(id: u64) -> Option<&'static str> {
    Some(match id {
        1 => "cognition",
        2 => "openai",
        3 => "anthropic",
        4 => "google",
        7 => "moonshot",
        9 => "zhipu",
        _ => return None,
    })
}

/// Decode a live `GetCliModelConfigs` response without treating malformed
/// protobuf as an empty, successful inventory.
pub fn decode_cli_model_configs(body: &[u8]) -> anyhow::Result<Vec<DevinModelConfig>> {
    let top = parse_fields(body).context("GetCliModelConfigs response")?;
    let varint = |config: &[ProtoField<'_>], number: u32| -> Option<u64> {
        config
            .iter()
            .find(|f| f.number == number && f.wire_type == 0)
            .map(|f| f.scalar)
    };
    let string = |config: &[ProtoField<'_>], number: u32| -> Option<String> {
        config
            .iter()
            .find(|f| f.number == number && f.wire_type == 2)
            .and_then(|f| std::str::from_utf8(f.bytes).ok())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let mut configs = Vec::new();
    for entry in top.iter().filter(|f| f.number == 1 && f.wire_type == 2) {
        let config = parse_fields(entry.bytes).context("GetCliModelConfigs model entry")?;
        if varint(&config, 4) == Some(1) {
            continue;
        }
        let Some(selector) = string(&config, 22) else {
            continue;
        };
        let (alias, short_alias, is_router) =
            if let Some(info) = config.iter().find(|f| f.number == 23 && f.wire_type == 2) {
                let info = parse_fields(info.bytes).context("GetCliModelConfigs model info")?;
                (
                    string(&info, 23),
                    string(&info, 20),
                    varint(&info, 25) == Some(1),
                )
            } else {
                Default::default()
            };
        configs.push(DevinModelConfig {
            selector,
            label: string(&config, 1),
            supports_images: varint(&config, 5).map(|v| v == 1),
            provider: varint(&config, 10),
            context_window: varint(&config, 18),
            alias,
            short_alias,
            is_router,
            cost: decode_model_cost(&config),
        });
    }
    Ok(configs)
}

/// Decode the repeated #32 pricing rows. A row is a sub-message mixing string
/// fields (component name, unit, tooltip) with floats; the component name is
/// recognized by content, and the price lives at field #2 as fixed32 or
/// fixed64. Values outside a plausible $/1M-token band are ignored so a
/// schema drift degrades to "no pricing" instead of corrupt numbers.
fn decode_model_cost(config: &[ProtoField<'_>]) -> Option<DevinModelCost> {
    let mut cost = DevinModelCost::default();
    for row in config.iter().filter(|f| f.number == 32 && f.wire_type == 2) {
        let Ok(fields) = parse_fields(row.bytes) else {
            continue;
        };
        let kind = fields
            .iter()
            .filter(|f| f.wire_type == 2)
            .filter_map(|f| std::str::from_utf8(f.bytes).ok())
            .find_map(|text| match text.trim().to_ascii_lowercase().as_str() {
                "input" => Some(0),
                "cached input" => Some(1),
                "output" => Some(2),
                _ => None,
            });
        let Some(kind) = kind else {
            continue;
        };
        let price = fields
            .iter()
            .find(|f| f.number == 2)
            .and_then(|f| match f.wire_type {
                1 => Some(f64::from_le_bytes(f.scalar.to_le_bytes())),
                // Widening a fixed32 verbatim keeps its binary noise (0.22f32
                // → 0.2199999988079071); the shortest f32 text form recovers
                // the intended decimal before it reaches model metadata.
                5 => f32::from_bits(f.scalar as u32)
                    .to_string()
                    .parse::<f64>()
                    .ok(),
                _ => None,
            });
        if let Some(price) = price.filter(|p| (0.0..10_000.0).contains(p)) {
            match kind {
                0 => cost.input = Some(price),
                1 => cost.cache_read = Some(price),
                _ => cost.output = Some(price),
            }
        }
    }
    (cost != DevinModelCost::default()).then_some(cost)
}

/// ClientMetadata field set per 3000.10.x captures: GetChatMessage and
/// GetUserStatus send {1,2,3,4,5,7,12,28,31}; GetCliModelConfigs adds #30
/// (`displays`). Fields absent in captures (9 request_id, 10 session_id,
/// 13 user_agent, 15 auth_source, 24 device_fingerprint, 32 team_id) are
/// never emitted.
fn client_metadata(session_token: &str, displays: bool, platform: DevinClientPlatform) -> Vec<u8> {
    let mut out = Vec::new();
    write_string_field(&mut out, 1, CLIENT_NAME);
    write_string_field(&mut out, 2, CLIENT_VERSION);
    write_string_field(&mut out, 3, session_token);
    write_string_field(&mut out, 4, CLIENT_LANGUAGE);
    write_string_field(&mut out, 5, platform.as_wire());
    write_string_field(&mut out, 7, CLIENT_VERSION);
    write_string_field(&mut out, 12, CLIENT_NAME);
    write_string_field(&mut out, 28, CLIENT_NAME);
    if displays {
        write_message_field(&mut out, 30, &[3, 4, 6, 7, 8]);
    }
    write_string_field(&mut out, 31, &device_fingerprint(session_token));
    out
}

/// ClientMetadata #31: 366 bytes rendered as 732 lowercase hex chars. The
/// existing codec derives a deterministic per-credential value instead of
/// reading host device identifiers. The official 3000.11.3 binary contains
/// `windsurf-api-client/src/fingerprint.rs` and `has_fingerprint_set`, but
/// those strings alone do not prove its generation algorithm, validation,
/// or server acceptance. This synthetic value is not an official machine
/// fingerprint implementation; keep its existing mapping unchanged.
fn device_fingerprint(session_token: &str) -> String {
    // Counter-mode expand: block_i = SHA256(token || "devin-clientmeta" || i).
    // 12 blocks × 32 bytes covers 366.
    let mut bytes = Vec::with_capacity(384);
    for index in 0u32..12 {
        let mut hasher = Sha256::new();
        hasher.update(session_token.as_bytes());
        hasher.update(b"devin-clientmeta");
        hasher.update(index.to_be_bytes());
        bytes.extend_from_slice(&hasher.finalize());
    }
    let mut hex = String::with_capacity(732);
    for byte in &bytes[..366] {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex
}

fn completion_config(req: &AiRequest) -> Vec<u8> {
    let mut temperature = req.generation.temperature.unwrap_or(DEFAULT_TEMPERATURE);
    if temperature < MIN_TEMPERATURE {
        temperature = MIN_TEMPERATURE;
    }
    let mut out = Vec::new();
    write_varint_field(&mut out, 1, 1);
    write_varint_field(
        &mut out,
        2,
        u64::from(req.generation.max_tokens.unwrap_or(WIRE_DEFAULT_MAX_TOKENS)),
    );
    write_varint_field(&mut out, 3, WIRE_MAX_NEWLINES);
    write_fixed64_field(&mut out, 5, temperature);
    write_varint_field(
        &mut out,
        7,
        req.parameters.top_k.map(u64::from).unwrap_or(DEFAULT_TOP_K),
    );
    write_fixed64_field(&mut out, 8, req.generation.top_p.unwrap_or(DEFAULT_TOP_P));
    for stop in req.generation.stop.as_deref().unwrap_or_default() {
        write_string_field(&mut out, 9, stop);
    }
    if let Some(seed) = req
        .generation
        .seed
        .and_then(|seed| u64::try_from(seed).ok())
    {
        write_varint_field(&mut out, 10, seed);
    }
    // Devin exposes only a return_logprob boolean, not a candidate count.
    // Some(0) still requests the selected token's log probability.
    if req.parameters.logprobs.is_some() {
        write_varint_field(&mut out, 16, 1);
    }
    // CompletionConfiguration #17 is a string in the first-party Devin
    // extension schema. Priority is the existing native mapping; other
    // unified tiers have no confirmed Devin mapping and remain omitted.
    if req.parameters.service_tier == Some(ServiceTier::Priority) {
        write_string_field(&mut out, 17, "priority");
    }
    out
}

/// CortexTrajectoryReference: `{1: trajectory_id, 2: step_index (only >0),
/// 3: TRAJECTORY_TYPE_CASCADE(4), 4: STEP_TYPE_USER_INPUT(14)}`.
fn trajectory_ref(shape: &SessionShape) -> Vec<u8> {
    let mut out = Vec::new();
    write_string_field(&mut out, 1, &shape.trajectory_id);
    if shape.step_index > 0 {
        write_varint_field(&mut out, 2, shape.step_index);
    }
    write_varint_field(&mut out, 3, 4);
    write_varint_field(&mut out, 4, 14);
    out
}

fn collect_system_prompt(req: &AiRequest) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(instructions) = req.instructions.as_deref() {
        let trimmed = instructions.trim();
        if !trimmed.is_empty() {
            parts.push(trimmed.to_string());
        }
    }
    for item in &req.items {
        if matches!(item.role, Role::System | Role::Developer) {
            let mut text = String::new();
            match &item.content {
                MessageContent::Text(value) => text.push_str(value),
                MessageContent::Blocks(blocks) => {
                    for block in blocks {
                        append_visible_content(block, &mut text, None);
                    }
                }
            }
            let trimmed = text.trim();
            if !trimmed.is_empty() {
                parts.push(trimmed.to_string());
            }
        }
    }
    // The reference joins consecutive system turns with a single newline.
    parts.join("\n")
}

// ── ChatMessage model ─────────────────────────────────────────────────────────

/// One logical ChatMessage before serialization. Keeping a struct (instead of
/// merging encoded bytes) makes the same-source text merge trivially correct.
#[derive(Default, Clone)]
struct ChatMsg {
    source: u64,
    text: String,
    images: Vec<Vec<u8>>,
    /// 同一助手段的调用共用 repeated #6，后接对应 source=4 结果；
    /// 独立签名块仍需单独的 prompt，不能覆盖原生单值签名字段。
    tool_calls: Vec<Vec<u8>>,
    tool_call_id: Option<String>,
    /// #9 — tool result carried an error (`is_error` on the source block).
    tool_error: bool,
    thinking: String,
    thinking_replay: Option<ThinkingReplay>,
    thinking_redacted: bool,
}

impl ChatMsg {
    fn has_thinking(&self) -> bool {
        !self.thinking.is_empty() || self.thinking_replay.is_some() || self.thinking_redacted
    }

    fn is_text_only(&self) -> bool {
        self.tool_calls.is_empty()
            && self.tool_call_id.is_none()
            && self.images.is_empty()
            && !self.has_thinking()
    }

    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        write_string_field(&mut out, 1, &Uuid::new_v4().to_string());
        write_varint_field(&mut out, 2, self.source);
        // #3 text is absent when empty — the reference only emits it with
        // content, and a present-but-empty optional field can read as
        // invalid under strict request validation.
        if !self.text.is_empty() {
            write_string_field(&mut out, 3, &self.text);
        }
        for call in &self.tool_calls {
            write_message_field(&mut out, 6, call);
        }
        if let Some(id) = &self.tool_call_id {
            write_string_field(&mut out, 7, id);
        }
        if self.tool_error {
            write_varint_field(&mut out, 9, 1);
        }
        for image in &self.images {
            write_message_field(&mut out, 10, image);
        }
        if !self.thinking.is_empty() {
            write_string_field(&mut out, 11, &self.thinking);
        }
        if let Some(replay) = &self.thinking_replay {
            if !replay.signature.is_empty() {
                write_string_field(&mut out, 12, &replay.signature);
            }
            // 原生 CLI 不把响应的 output_id 回填到历史 prompt。
            if !replay.signature_type.is_empty() {
                write_string_field(&mut out, 18, &replay.signature_type);
            }
        }
        if self.thinking_redacted {
            write_varint_field(&mut out, 13, 1);
        }
        out
    }
}

fn push_message(out: &mut Vec<ChatMsg>, mut message: ChatMsg) {
    if message.text.is_empty()
        && message.images.is_empty()
        && message.tool_calls.is_empty()
        && !message.has_thinking()
    {
        return;
    }
    // Responses 把正文、reasoning 和每个 call 拆成独立项；这里只恢复上游回合，
    // 不修改 canonical 历史。同一响应的签名增量已由 decoder 聚合，
    // 这里不能再拼接或覆盖历史中的独立思考块。
    if let Some(last) = out.last_mut()
        && last.source == message.source
        && match message.source {
            SOURCE_ASSISTANT => !last.has_thinking() || !message.has_thinking(),
            SOURCE_USER => last.is_text_only() && message.is_text_only(),
            _ => false,
        }
    {
        if message.has_thinking() {
            last.thinking = message.thinking;
            last.thinking_replay = message.thinking_replay;
            last.thinking_redacted = message.thinking_redacted;
        }
        last.tool_calls.append(&mut message.tool_calls);
        last.images.append(&mut message.images);
        if last.text.is_empty() {
            last.text = message.text;
        } else if !message.text.is_empty() {
            last.text.push_str("\n\n");
            last.text.push_str(&message.text);
        }
        return;
    }
    out.push(message);
}

fn collect_chat_messages(req: &AiRequest) -> anyhow::Result<Vec<ChatMsg>> {
    let mut out: Vec<ChatMsg> = Vec::new();
    for item in &req.items {
        match item.role {
            Role::System | Role::Developer => {}
            Role::User => encode_user_item(item, &mut out)?,
            Role::Assistant => encode_assistant_item(item, &mut out)?,
            Role::Tool => encode_tool_result_item(item, &mut out)?,
        }
    }
    let mut out = pair_tool_calls_with_results(out);
    drop_orphan_tool_results(&mut out);
    Ok(out)
}

/// 上游要求助手调用后紧邻对应的 source=4 结果。客户端可将较晚封口的
/// thinking 排到 call 之后，因此先保留同一助手回合的正文与签名，再按
/// call id 排列 call/result；不跨用户或工具结果边界移动内容。
fn pair_tool_calls_with_results(prompts: Vec<ChatMsg>) -> Vec<ChatMsg> {
    let is_call = |m: &ChatMsg| m.source == SOURCE_ASSISTANT && !m.tool_calls.is_empty();
    let is_result = |m: &ChatMsg| m.source == SOURCE_TOOL_RESULT && m.tool_call_id.is_some();
    let mut out: Vec<ChatMsg> = Vec::with_capacity(prompts.len());
    let mut index = 0;
    while index < prompts.len() {
        if prompts[index].source != SOURCE_ASSISTANT {
            out.push(prompts[index].clone());
            index += 1;
            continue;
        }
        // 用户与工具结果是回合边界，不能跨过它们移动另一个回合的思考。
        let calls_start = index;
        while index < prompts.len() && prompts[index].source == SOURCE_ASSISTANT {
            index += 1;
        }
        let results_start = index;
        while index < prompts.len() && is_result(&prompts[index]) {
            index += 1;
        }
        let mut by_id: HashMap<&str, &ChatMsg> = HashMap::new();
        for result in &prompts[results_start..index] {
            if let Some(id) = &result.tool_call_id {
                by_id.entry(id.as_str()).or_insert(result);
            }
        }
        let assistant = &prompts[calls_start..results_start];
        out.extend(assistant.iter().filter(|prompt| !is_call(prompt)).cloned());
        let mut consumed: Vec<&ChatMsg> = Vec::new();
        for call_prompt in assistant.iter().filter(|prompt| is_call(prompt)) {
            out.push((*call_prompt).clone());
            // The tool_call submessages carry the ids but stay encoded; the
            // id list is recovered by decoding each call once.
            for id in call_prompt.tool_call_ids() {
                if let Some(result) = by_id.remove(id.as_str()) {
                    consumed.push(result);
                    out.push((*result).clone());
                }
            }
        }
        // Keep unmatched results ordered until the orphan filter removes them.
        for result in &prompts[results_start..index] {
            if !consumed.iter().any(|c| std::ptr::eq(*c, result)) {
                out.push((*result).clone());
            }
        }
    }
    out
}

/// Results without a matching call cannot be represented upstream.
fn drop_orphan_tool_results(prompts: &mut Vec<ChatMsg>) {
    let call_ids: std::collections::HashSet<String> = prompts
        .iter()
        .flat_map(|prompt| prompt.tool_call_ids())
        .collect();
    prompts.retain(|prompt| {
        prompt.source != SOURCE_TOOL_RESULT
            || prompt
                .tool_call_id
                .as_ref()
                .is_some_and(|id| call_ids.contains(id))
    });
}

impl ChatMsg {
    /// Decode each packed #6 submessage's #1 id — needed for call/result
    /// pairing without keeping a parallel id list on the wire model.
    fn tool_call_ids(&self) -> Vec<String> {
        self.tool_calls
            .iter()
            .filter_map(|call| {
                parse_fields(call).ok()?.iter().find_map(|field| {
                    (field.number == 1 && field.wire_type == 2)
                        .then(|| String::from_utf8_lossy(field.bytes).into_owned())
                })
            })
            .collect()
    }
}

fn encode_user_item(item: &AiItem, out: &mut Vec<ChatMsg>) -> anyhow::Result<()> {
    let mut current = ChatMsg {
        source: SOURCE_USER,
        ..ChatMsg::default()
    };
    let mut has_content = false;
    let blocks: &[ContentBlock] = match &item.content {
        MessageContent::Text(text) => {
            current.text = text.as_ref().clone();
            has_content = !text.is_empty();
            &[]
        }
        MessageContent::Blocks(blocks) => blocks,
    };
    for block in blocks {
        has_content = true;
        match block {
            ContentBlock::Text { text, .. } => {
                if !current.text.is_empty() {
                    current.text.push('\n');
                }
                current.text.push_str(text);
            }
            ContentBlock::Image { source, .. } => {
                current.images.extend(encode_inline_image(source));
            }
            ContentBlock::Document { .. } => {
                append_visible_content(block, &mut current.text, Some(&mut current.images));
            }
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                content_kind,
                is_error,
                ..
            } => {
                // Anthropic-style tool results arrive inside user items; emit
                // each as the native source=4 turn it belongs to.
                if has_content {
                    push_message(out, std::mem::take(&mut current));
                    current.source = SOURCE_USER;
                }
                let mut result = ChatMsg {
                    source: SOURCE_TOOL_RESULT,
                    tool_call_id: Some(tool_use_id.clone().into_string()),
                    tool_error: *is_error == Some(true),
                    ..ChatMsg::default()
                };
                append_tool_result_content(
                    content,
                    *content_kind,
                    &mut result.text,
                    &mut result.images,
                );
                push_message(out, result);
                has_content = false;
            }
            _ => continue,
        }
    }
    if has_content && (!current.text.is_empty() || !current.images.is_empty()) {
        push_message(out, current);
    }
    Ok(())
}

fn encode_assistant_item(item: &AiItem, out: &mut Vec<ChatMsg>) -> anyhow::Result<()> {
    // 思考块先保留各自身份，再由 push_message 与相邻正文/调用合并；
    // 独立思考块之间仍有边界，不能将多个签名拼到同一 #12。
    let mut msg = ChatMsg {
        source: SOURCE_ASSISTANT,
        ..ChatMsg::default()
    };
    let custom = item
        .meta
        .as_ref()
        .and_then(|meta| meta.get(CUSTOM_TOOL_META))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if let MessageContent::Blocks(blocks) = &item.content {
        for block in blocks {
            match block {
                ContentBlock::Text { text, .. } => {
                    if !msg.text.is_empty() {
                        msg.text.push('\n');
                    }
                    msg.text.push_str(text);
                }
                ContentBlock::Document { .. } => {
                    append_visible_content(block, &mut msg.text, Some(&mut msg.images));
                }
                ContentBlock::Thinking { .. }
                | ContentBlock::RedactedThinking { .. }
                | ContentBlock::Reasoning { .. } => {
                    if !msg.text.is_empty() || !msg.tool_calls.is_empty() || !msg.images.is_empty()
                    {
                        push_message(out, std::mem::take(&mut msg));
                        msg.source = SOURCE_ASSISTANT;
                    }
                    if let Some(thinking) = encode_thinking(block) {
                        push_message(out, thinking);
                    }
                }
                ContentBlock::ToolUse {
                    id, name, input, ..
                } => {
                    msg.tool_calls
                        .extend(encode_tool_call(id, name, &input.to_string(), custom));
                }
                _ => continue,
            }
        }
    } else {
        msg.text = item.content.to_text();
    }
    for call in item.tool_calls.as_deref().unwrap_or_default() {
        msg.tool_calls.extend(encode_tool_call(
            &call.id,
            &call.name,
            &call.arguments,
            custom,
        ));
    }
    if !msg.text.is_empty() || !msg.tool_calls.is_empty() || !msg.images.is_empty() {
        push_message(out, msg);
    }
    Ok(())
}

/// Devin 的签名位置可承载不透明载荷，首次回放不按来源提前过滤。
fn encode_thinking(block: &ContentBlock) -> Option<ChatMsg> {
    let (text, signature, redacted) = match block {
        ContentBlock::Thinking {
            thinking,
            signature,
        } => (thinking.clone(), signature.as_deref(), false),
        ContentBlock::Reasoning {
            summary,
            content,
            encrypted_content,
        } => (
            summary
                .iter()
                .chain(content)
                .filter(|text| !text.is_empty())
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join("\n"),
            encrypted_content.as_deref(),
            false,
        ),
        ContentBlock::RedactedThinking { data } => (String::new(), Some(data.as_str()), true),
        _ => unreachable!("thinking block"),
    };
    let Some(signature) = signature.filter(|value| !value.is_empty()) else {
        // 没有签名的历史仍保留可读内容，但不能伪装成上游原生签名思考；
        // 明文降级为普通助手正文，全空则整块丢弃。
        return (!text.is_empty()).then(|| ChatMsg {
            source: SOURCE_ASSISTANT,
            text,
            ..ChatMsg::default()
        });
    };
    // 仅插件前缀表示可解包状态；无法解包时仍让上游尝试原始载荷。
    let mut replay = signature
        .starts_with(ThinkingReplay::PREFIX)
        .then(|| ThinkingReplay::decode(signature).ok())
        .flatten()
        .unwrap_or_else(|| ThinkingReplay {
            signature: signature.to_owned(),
            ..ThinkingReplay::default()
        });
    let thinking = if redacted {
        std::mem::take(&mut replay.redacted_text)
    } else {
        text
    };
    Some(ChatMsg {
        source: SOURCE_ASSISTANT,
        thinking,
        thinking_replay: Some(replay),
        thinking_redacted: redacted,
        ..ChatMsg::default()
    })
}

fn encode_tool_result_item(item: &AiItem, out: &mut Vec<ChatMsg>) -> anyhow::Result<()> {
    let mut tool_error = false;
    let mut images = Vec::new();
    let text = match &item.content {
        MessageContent::Text(value) => value.as_ref().clone(),
        MessageContent::Blocks(blocks) => {
            let mut text = String::new();
            for block in blocks {
                match block {
                    ContentBlock::Text { text: fragment, .. } => {
                        append_result_text(&mut text, fragment)
                    }
                    ContentBlock::ToolResult {
                        content,
                        content_kind,
                        is_error,
                        ..
                    } => {
                        tool_error |= *is_error == Some(true);
                        append_tool_result_content(content, *content_kind, &mut text, &mut images);
                    }
                    ContentBlock::Document { .. } => {
                        append_visible_content(block, &mut text, Some(&mut images));
                    }
                    ContentBlock::Image { source, .. } => {
                        images.extend(encode_inline_image(source));
                    }
                    _ => continue,
                }
            }
            text
        }
    };
    let Some(tool_call_id) = item.tool_call_id.as_deref() else {
        return Ok(());
    };
    if text.is_empty() && images.is_empty() {
        return Ok(());
    }
    push_message(
        out,
        ChatMsg {
            source: SOURCE_TOOL_RESULT,
            text,
            tool_call_id: Some(tool_call_id.to_string()),
            tool_error,
            images,
            ..ChatMsg::default()
        },
    );
    Ok(())
}

/// Documents reuse the role's text/image carriers; other nested blocks have no wire form.
fn append_visible_content(
    block: &ContentBlock,
    text: &mut String,
    mut images: Option<&mut Vec<Vec<u8>>>,
) {
    match block {
        ContentBlock::Text { text: fragment, .. } => append_result_text(text, fragment),
        ContentBlock::Image { source, .. } => {
            if let Some(images) = images {
                images.extend(encode_inline_image(source));
            }
        }
        ContentBlock::Document { source, .. } => match source {
            DocumentSource::PlainText { data } => append_result_text(text, data),
            DocumentSource::Blocks { content } => {
                for block in content {
                    append_visible_content(block, text, images.as_deref_mut());
                }
            }
            _ => {}
        },
        _ => {}
    }
}

fn append_result_text(text: &mut String, fragment: &str) {
    if fragment.is_empty() {
        return;
    }
    if !text.is_empty() {
        text.push('\n');
    }
    text.push_str(fragment);
}

fn append_tool_result_content(
    content: &Value,
    kind: Option<stravia_runtime_contract::protocol::ir::ToolResultContentKind>,
    text: &mut String,
    images: &mut Vec<Vec<u8>>,
) {
    if let Some(fragment) = content.as_str() {
        append_result_text(text, fragment);
        return;
    }
    if kind != Some(stravia_runtime_contract::protocol::ir::ToolResultContentKind::ContentBlocks) {
        use std::fmt::Write as _;
        if !text.is_empty() {
            text.push('\n');
        }
        let _ = write!(text, "{content}");
        return;
    }
    let Some(blocks) = content.as_array() else {
        return;
    };
    append_result_blocks(blocks, text, images);
}

fn append_result_blocks(blocks: &[Value], text: &mut String, images: &mut Vec<Vec<u8>>) {
    for block in blocks {
        match block.get("type").and_then(Value::as_str) {
            Some("document") => {
                let Some(source) = block.get("source") else {
                    continue;
                };
                match source.get("type").and_then(Value::as_str) {
                    Some("plain_text") => {
                        if let Some(data) = source.get("data").and_then(Value::as_str) {
                            append_result_text(text, data);
                        }
                    }
                    Some("blocks") => {
                        if let Some(content) = source.get("content").and_then(Value::as_array) {
                            append_result_blocks(content, text, images);
                        }
                    }
                    _ => {}
                }
            }
            Some("text") => {
                if let Some(fragment) = block.get("text").and_then(Value::as_str) {
                    append_result_text(text, fragment);
                }
            }
            Some("image") => {
                let Some(source) = block.get("source") else {
                    continue;
                };
                if source.get("type").and_then(Value::as_str) != Some("base64") {
                    continue;
                }
                if let (Some(media_type), Some(data)) = (
                    source.get("media_type").and_then(Value::as_str),
                    source.get("data").and_then(Value::as_str),
                ) {
                    images.push(encode_inline_image_data(media_type, data));
                }
            }
            _ => {}
        }
    }
}

fn encode_inline_image(source: &MediaSource) -> Option<Vec<u8>> {
    let MediaSource::Base64 { media_type, data } = source else {
        return None;
    };
    Some(encode_inline_image_data(media_type, data))
}

fn encode_inline_image_data(media_type: &str, data: &str) -> Vec<u8> {
    // ImageData{#1 base64 text, #2 mime} — verified from wire: #1 carries the
    // base64 STRING, not raw image bytes.
    let mut out = Vec::new();
    write_string_field(&mut out, 1, data);
    write_string_field(&mut out, 2, media_type);
    out
}

fn encode_tool_call(id: &str, name: &str, args_json: &str, custom: bool) -> Option<Vec<u8>> {
    if !valid_tool_name(name) {
        return None;
    }
    // ChatToolCall{#1 id, #2 name, #3 arguments JSON} — only #3 is a JSON
    // string; the envelope around it is protobuf.
    let mut call = Vec::new();
    write_string_field(&mut call, 1, id);
    write_string_field(&mut call, 2, name);
    if custom {
        let arguments: Value = serde_json::from_str(args_json).ok()?;
        let input = arguments.get("input").and_then(Value::as_str)?;
        write_string_field(&mut call, 4, input);
        write_varint_field(&mut call, 6, 1);
    } else {
        write_string_field(&mut call, 3, args_json);
    }
    Some(call)
}

/// Wire-encoded ToolDef bodies plus `(name, sanitized description)` pairs for
/// system-prompt relocation.
type EncodedToolDefs = (Vec<Vec<u8>>, Vec<(String, String)>);

/// Returns the wire-encoded ToolDefs plus `(name, sanitized description)`
/// pairs for system-prompt relocation. Tools with names outside the upstream's
/// `[A-Za-z0-9_-]` carrier are omitted; names are never rewritten.
fn encode_tool_defs(
    req: &AiRequest,
    hits: &mut sanitize::SanitizeHits,
) -> anyhow::Result<EncodedToolDefs> {
    let mut defs = Vec::new();
    let mut descriptions = Vec::new();
    for tool in req.tools.as_deref().unwrap_or_default() {
        let name = tool.name.trim();
        if !valid_tool_name(name) {
            continue;
        }
        let mut def = Vec::new();
        write_string_field(&mut def, 1, name);
        // #2 emits the NAME, never prose — the upstream MCP gate scans
        // descriptions for known tool signatures and rejects the request.
        write_string_field(&mut def, 2, name);
        write_string_field(
            &mut def,
            3,
            &normalize_tool_schema(&tool.parameters).to_string(),
        );
        defs.push(def);
        if let Some(description) = tool.description.as_deref() {
            let description = sanitize::sanitize_prompt_text(description.trim(), hits);
            if !description.trim().is_empty() {
                descriptions.push((name.to_string(), description));
            }
        }
    }
    Ok((defs, descriptions))
}

/// The upstream-verified tool-name alphabet.
fn valid_tool_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Append stripped tool descriptions to the system prompt as an XML
/// section. The gate pattern-matches ToolDef fields, not prompt text, so
/// the model recovers the guidance through the instruction channel.
fn with_tool_descriptions(system_prompt: &str, descriptions: &[(String, String)]) -> String {
    if descriptions.is_empty() {
        return system_prompt.to_string();
    }
    let mut section = String::from("# tools descriptions");
    for (name, description) in descriptions {
        section.push_str("\n<tool name=\"");
        escape_xml_into(name, true, &mut section);
        section.push_str("\">\n");
        let formatted = super::tool_description::format_tool_description(description);
        escape_xml_into(&formatted, false, &mut section);
        section.push_str("\n</tool>");
    }
    let trimmed = system_prompt.trim_end();
    if trimmed.is_empty() {
        return section;
    }
    format!("{trimmed}\n\n{section}")
}

/// Minimal XML escaping: attributes also escape quotes; text only needs the
/// markup boundary characters.
fn escape_xml_into(value: &str, attribute: bool, out: &mut String) {
    for ch in value.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' if attribute => out.push_str("&quot;"),
            _ => out.push(ch),
        }
    }
}

/// Schema-annotation keys confirmed to trigger the upstream tool
/// classifier — stripped unless they appear as property names.
fn is_annotation_key(key: &str) -> bool {
    matches!(key, "description" | "title" | "$comment")
        || key.len() >= 2 && key[..2].eq_ignore_ascii_case("x-")
}

/// Keys whose VALUES are business literals, not sub-schemas: a
/// `description`/`$ref`-looking key inside them is data and survives.
fn is_schema_literal(key: &str) -> bool {
    matches!(key, "const" | "default" | "enum" | "example" | "examples")
}

/// Schema keywords — presence marks a map as a schema rather than a bare
/// property map. Existence matters, not exhaustiveness.
fn is_schema_keyword(key: &str) -> bool {
    matches!(
        key,
        "type"
            | "properties"
            | "items"
            | "required"
            | "additionalProperties"
            | "allOf"
            | "anyOf"
            | "oneOf"
            | "not"
            | "enum"
            | "const"
            | "format"
            | "pattern"
            | "minLength"
            | "maxLength"
            | "minimum"
            | "maximum"
            | "exclusiveMinimum"
            | "exclusiveMaximum"
            | "multipleOf"
            | "minItems"
            | "maxItems"
            | "uniqueItems"
            | "contains"
            | "minProperties"
            | "maxProperties"
            | "patternProperties"
            | "propertyNames"
            | "dependentRequired"
            | "dependentSchemas"
            | "prefixItems"
            | "if"
            | "then"
            | "else"
            | "readOnly"
            | "writeOnly"
            | "deprecated"
            | "description"
            | "title"
            | "default"
            | "examples"
    )
}

/// Normalize a JSON Schema for the wire:
/// 1. strip annotation keys the upstream classifier rejects on
///    (`description`/`title`/`$comment`/`x-*`) — `properties` names and
///    literal-valued keys are preserved;
/// 2. inline local `$ref`s and drop `$defs`/`definitions`/`$schema`;
/// 3. flatten top-level combinators into the object envelope;
/// 4. wrap bare property maps (`{"a": {...}}`) in a real object envelope —
///    the upstream rejects them;
/// 5. force `type: "object"` + `properties`, keep `required` ⊆ `properties`.
fn normalize_tool_schema(schema: &Value) -> Value {
    let stripped = strip_schema_annotations(schema, false);
    let mut out = match stripped {
        Value::Object(map) => map,
        _ => serde_json::Map::new(),
    };
    let root = Value::Object(out.clone());
    let normalized = resolve_schema_refs(
        Value::Object(out),
        &root,
        &mut std::collections::HashSet::new(),
        0,
    );
    out = match normalized {
        Value::Object(map) => map,
        _ => serde_json::Map::new(),
    };
    out.remove("$defs");
    out.remove("definitions");
    out.remove("$schema");
    strip_top_level_combinators(&mut out);
    if is_bare_property_map(&out) {
        let mut wrapped = serde_json::Map::new();
        wrapped.insert("type".into(), Value::String("object".into()));
        wrapped.insert("properties".into(), Value::Object(out));
        return Value::Object(wrapped);
    }
    match out.get("type") {
        Some(Value::String(kind)) if kind == "object" => {}
        _ => {
            out.insert("type".into(), Value::String("object".into()));
        }
    }
    if !out.get("properties").is_some_and(Value::is_object) {
        out.insert("properties".into(), Value::Object(serde_json::Map::new()));
    }
    if out.contains_key("required") {
        let keys: Vec<String> = out
            .get("properties")
            .and_then(Value::as_object)
            .map(|props| props.keys().cloned().collect())
            .unwrap_or_default();
        let filtered: Vec<Value> = out
            .get("required")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter(|item| {
                        item.as_str()
                            .is_some_and(|name| keys.iter().any(|k| k == name))
                    })
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        if filtered.is_empty() {
            out.remove("required");
        } else {
            out.insert("required".into(), Value::Array(filtered));
        }
    }
    Value::Object(out)
}

/// A map with no schema keywords, no `$`/`x-` prefixed keys, and only
/// object values is a bare property map the upstream rejects.
fn is_bare_property_map(object: &serde_json::Map<String, Value>) -> bool {
    !object.is_empty()
        && object.iter().all(|(key, child)| {
            !is_schema_keyword(key)
                && !key.starts_with('$')
                && !(key.len() >= 2 && key[..2].eq_ignore_ascii_case("x-"))
                && child.is_object()
        })
}

const MAX_SCHEMA_REF_DEPTH: usize = 32;

/// Inline local `$ref`s (`#/$defs/x` JSON-pointers) — the upstream rejects
/// them. Unresolvable, external, or cyclic refs lose the `$ref` key but keep
/// sibling constraints (≈ `any`), which beats bouncing the whole request
/// off a vague upstream `invalid_argument`.
fn resolve_schema_refs(
    value: Value,
    root: &Value,
    resolving: &mut std::collections::HashSet<String>,
    depth: usize,
) -> Value {
    if depth > MAX_SCHEMA_REF_DEPTH {
        return value;
    }
    match value {
        Value::Array(items) => Value::Array(
            items
                .into_iter()
                .map(|item| resolve_schema_refs(item, root, resolving, depth + 1))
                .collect(),
        ),
        Value::Object(mut map) => {
            if let Some(reference) = map
                .get("$ref")
                .and_then(Value::as_str)
                .filter(|r| r.starts_with('#'))
                .map(str::to_string)
            {
                match resolve_local_ref(root, &reference) {
                    Some(target) if !resolving.contains(&reference) => {
                        resolving.insert(reference.clone());
                        let resolved =
                            resolve_schema_refs(target.clone(), root, resolving, depth + 1);
                        resolving.remove(&reference);
                        map.remove("$ref");
                        // Siblings win over the referenced object's keys.
                        if let Value::Object(resolved) = resolved {
                            for (key, child) in resolved {
                                map.entry(key).or_insert(child);
                            }
                        }
                    }
                    _ => {
                        map.remove("$ref");
                    }
                }
            }
            for (key, child) in map.iter_mut() {
                // Literal-valued keys hold business data, not schemas —
                // never expand `$ref`-looking keys inside them.
                if is_schema_literal(key) {
                    continue;
                }
                *child = resolve_schema_refs(std::mem::take(child), root, resolving, depth + 1);
            }
            Value::Object(map)
        }
        other => other,
    }
}

/// Resolve a `#/a/b` local JSON-pointer against the schema root, honoring
/// `~0`/`~1` escapes.
fn resolve_local_ref<'a>(root: &'a Value, reference: &str) -> Option<&'a Value> {
    if reference == "#" {
        return Some(root);
    }
    let path = reference.strip_prefix("#/")?;
    let mut current = root;
    for segment in path.split('/') {
        let segment = segment.replace("~1", "/").replace("~0", "~");
        current = current.as_object()?.get(&segment)?;
    }
    Some(current)
}

/// Remove a top-level oneOf/anyOf/allOf envelope; when the root had no
/// `properties` of its own, recover them from the first object variant
/// without overwriting existing keys.
fn strip_top_level_combinators(out: &mut serde_json::Map<String, Value>) {
    let had_properties = out.contains_key("properties");
    let mut recovered = false;
    for key in ["oneOf", "anyOf", "allOf"] {
        let Some(variants) = out.remove(key) else {
            continue;
        };
        if had_properties || recovered {
            continue;
        }
        let Some(variants) = variants.as_array() else {
            continue;
        };
        let Some(object_variant) = variants.iter().find_map(|variant| {
            variant
                .as_object()
                .filter(|obj| obj.get("type").and_then(Value::as_str) == Some("object"))
        }) else {
            continue;
        };
        for field in [
            "properties",
            "required",
            "additionalProperties",
            "description",
        ] {
            if !out.contains_key(field)
                && let Some(value) = object_variant.get(field)
            {
                out.insert(field.into(), value.clone());
            }
        }
        recovered = true;
    }
}

/// Strip natural-language annotation keys recursively. Inside a
/// `properties` map the keys are property NAMES (a parameter literally
/// called `description` survives); inside literal-valued keys
/// (`const`/`default`/`enum`/`example(s)`) the subtree is business data and
/// is copied verbatim.
fn strip_schema_annotations(value: &Value, in_properties: bool) -> Value {
    match value {
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| strip_schema_annotations(item, false))
                .collect(),
        ),
        Value::Object(map) => Value::Object(
            map.iter()
                .filter(|(key, _)| in_properties || !is_annotation_key(key))
                .map(|(key, child)| {
                    let child = if !in_properties && is_schema_literal(key) {
                        child.clone()
                    } else {
                        strip_schema_annotations(child, key == "properties")
                    };
                    (key.clone(), child)
                })
                .collect(),
        ),
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::devin_connect::proto::{parse_fields, write_fixed32_field};
    use stravia_runtime_contract::protocol::ir::ToolCall;
    use stravia_runtime_contract::protocol::ir::ToolSpec;

    fn text_item(role: Role, text: &str) -> AiItem {
        AiItem {
            role,
            content: MessageContent::Text(text.to_string().into()),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        }
    }

    fn encode_test_chat_request(
        req: &AiRequest,
        session_token: &str,
        shape: &SessionShape,
        assignment_jwt: Option<&str>,
    ) -> anyhow::Result<Vec<u8>> {
        encode_get_chat_message_request_with_platform(
            req,
            session_token,
            shape,
            assignment_jwt,
            DevinClientPlatform::Linux,
        )
    }

    fn encode_test_assign_request(
        session_token: &str,
        router_uid: &str,
        cascade_id: &str,
    ) -> Vec<u8> {
        encode_assign_model_request_with_platform(
            session_token,
            router_uid,
            cascade_id,
            DevinClientPlatform::Linux,
        )
    }

    /// Encode and leak the wire bytes so parsed fields borrow 'static — tests
    /// only, keeps field access free of lifetime plumbing.
    fn wire(req: &AiRequest) -> &'static [u8] {
        let bytes = encode_test_chat_request(req, "tok", &shape(req), None).unwrap();
        Box::leak(bytes.into_boxed_slice())
    }

    fn shape(_req: &AiRequest) -> SessionShape {
        session_shape(None)
    }

    #[test]
    fn prepared_encoder_rejects_missing_session_credentials() {
        let mut request = AiRequest::new("m", vec![text_item(Role::User, "hello")]);
        super::super::prepare_request(&mut request);
        let shape = shape(&request);
        for token in ["", " \t"] {
            assert!(
                encode_prepared_request(&request, token, &shape, None, DevinClientPlatform::Linux,)
                    .is_err()
            );
        }
    }

    fn top_level(req: &AiRequest) -> Vec<crate::codec::devin_connect::proto::ProtoField<'static>> {
        parse_fields(wire(req)).unwrap()
    }

    #[test]
    fn unknown_parameter_pockets_cannot_add_protobuf_fields() {
        let request = AiRequest::new("m", vec![text_item(Role::User, "hello")]);
        let mut injected = request.clone();
        injected.parameters = serde_json::from_value(serde_json::json!({
            "extra_body":{"permissionMode":"bypass"},
            "options":{"temperature":99},
            "extensions":{"devin":{"arbitrary_field":true}}
        }))
        .unwrap();
        let shape = shape(&request);
        let baseline = encode_test_chat_request(&request, "tok", &shape, None).unwrap();
        let injected = encode_test_chat_request(&injected, "tok", &shape, None).unwrap();
        let baseline = parse_fields(&baseline).unwrap();
        let injected = parse_fields(&injected).unwrap();
        assert_eq!(baseline.len(), injected.len());
        for (baseline, injected) in baseline.iter().zip(&injected) {
            if baseline.number == 3 {
                assert_eq!(injected.number, 3);
                let baseline = parse_fields(baseline.bytes).unwrap();
                let injected = parse_fields(injected.bytes).unwrap();
                // 原生消息 ID 每次编码重新生成，只排除该随机值。
                for fields in [&baseline, &injected] {
                    assert_eq!(fields[0].number, 1);
                    Uuid::parse_str(std::str::from_utf8(fields[0].bytes).unwrap()).unwrap();
                }
                assert_eq!(baseline[1..], injected[1..]);
            } else {
                assert_eq!(baseline, injected);
            }
        }
    }

    fn sub_message<'a>(
        fields: &'a [crate::codec::devin_connect::proto::ProtoField<'a>],
        field: u32,
        index: usize,
    ) -> Vec<crate::codec::devin_connect::proto::ProtoField<'a>> {
        let bytes = fields
            .iter()
            .filter(|f| f.number == field && f.wire_type == 2)
            .nth(index)
            .unwrap_or_else(|| panic!("missing sub-message field #{field} at index {index}"))
            .bytes;
        parse_fields(bytes).unwrap()
    }

    fn responses_prompts(input: Value) -> Vec<Vec<ProtoField<'static>>> {
        let request = stravia_protocol_codec::registry::ProtocolRegistry::global()
            .adapter(&stravia_runtime_contract::protocol::ids::OPEN_RESPONSES_2026_04_24)
            .unwrap()
            .decode_request(serde_json::json!({"model":"swe-2","input":input}))
            .unwrap();
        top_level(&request)
            .into_iter()
            .filter(|field| field.number == 3)
            .map(|field| parse_fields(field.bytes).unwrap())
            .collect()
    }

    #[test]
    fn responses_turn_keeps_text_thinking_and_parallel_calls_together() {
        let replay = ThinkingReplay {
            signature: "signed-thought".into(),
            signature_type: "sealed".into(),
            output_id: "output-1".into(),
            ..Default::default()
        };
        let prompts = responses_prompts(serde_json::json!([
            {"role":"user","content":"Inspect both fixtures."},
            {"type":"reasoning","summary":[],
             "content":[{"type":"reasoning_text","text":"Compare both fixtures."}],
             "encrypted_content":replay.encode()},
            {"type":"message","role":"assistant","content":[
                {"type":"output_text","text":"I will read both fixtures."}
            ]},
            {"type":"function_call","call_id":"call-a","name":"read","arguments":"{\"path\":\"a\"}"},
            {"type":"function_call","call_id":"call-b","name":"read","arguments":"{\"path\":\"b\"}"},
            {"type":"function_call_output","call_id":"call-b","output":"fixture B"},
            {"type":"function_call_output","call_id":"call-a","output":"fixture A"},
            {"role":"assistant","content":"Both fixtures agree."},
            {"role":"user","content":"Now inspect another fixture."},
            {"role":"assistant","content":"This is a separate turn."}
        ]));
        let sources: Vec<_> = prompts
            .iter()
            .map(|prompt| {
                prompt
                    .iter()
                    .find(|field| field.number == 2)
                    .unwrap()
                    .scalar
            })
            .collect();
        assert_eq!(sources, [1, 2, 4, 4, 2, 1, 2]);
        let assistant = &prompts[1];
        for (number, value) in [
            (3, "I will read both fixtures."),
            (11, "Compare both fixtures."),
            (12, "signed-thought"),
            (18, "sealed"),
        ] {
            assert!(
                assistant
                    .iter()
                    .any(|field| { field.number == number && field.bytes == value.as_bytes() })
            );
        }
        assert!(!assistant.iter().any(|field| field.number == 15));
        assert_eq!(
            assistant.iter().filter(|field| field.number == 6).count(),
            2
        );
        for (index, id, arguments, result) in [
            (0, "call-a", r#"{"path":"a"}"#, "fixture A"),
            (1, "call-b", r#"{"path":"b"}"#, "fixture B"),
        ] {
            let call = sub_message(assistant, 6, index);
            assert!(
                call.iter()
                    .any(|field| field.number == 1 && field.bytes == id.as_bytes())
            );
            assert!(
                call.iter()
                    .any(|field| field.number == 3 && field.bytes == arguments.as_bytes())
            );
            assert!(
                prompts[index + 2]
                    .iter()
                    .any(|field| field.number == 7 && field.bytes == id.as_bytes())
            );
            assert!(
                prompts[index + 2]
                    .iter()
                    .any(|field| field.number == 3 && field.bytes == result.as_bytes())
            );
        }
    }

    #[test]
    fn responses_turn_keeps_distinct_signed_blocks_with_their_text_and_calls() {
        let replay = |signature: &str| {
            ThinkingReplay {
                signature: signature.into(),
                signature_type: "sealed".into(),
                output_id: "shared-output".into(),
                ..Default::default()
            }
            .encode()
        };
        let prompts = responses_prompts(serde_json::json!([
            {"role":"user","content":"Inspect the fixtures."},
            {"type":"reasoning","summary":[],
             "content":[{"type":"reasoning_text","text":"First thought."}],
             "encrypted_content":replay("signature-a")},
            {"role":"assistant","content":"Inspecting A."},
            {"type":"function_call","call_id":"call-a","name":"read","arguments":"{\"path\":\"a\"}"},
            {"type":"reasoning","summary":[],
             "content":[{"type":"reasoning_text","text":"Second thought."}],
             "encrypted_content":replay("signature-b")},
            {"role":"assistant","content":"Inspecting B."},
            {"type":"function_call","call_id":"call-b","name":"read","arguments":"{\"path\":\"b\"}"},
            {"type":"function_call_output","call_id":"call-a","output":"fixture A"},
            {"type":"function_call_output","call_id":"call-b","output":"fixture B"}
        ]));
        let signed: Vec<_> = prompts
            .iter()
            .filter(|prompt| prompt.iter().any(|field| field.number == 12))
            .collect();
        assert_eq!(signed.len(), 2);
        for (index, thought, signature, text, call_id) in [
            (
                0,
                "First thought.",
                "signature-a",
                "Inspecting A.",
                "call-a",
            ),
            (
                1,
                "Second thought.",
                "signature-b",
                "Inspecting B.",
                "call-b",
            ),
        ] {
            let prompt = signed[index];
            for (number, value) in [(3, text), (11, thought), (12, signature), (18, "sealed")] {
                assert!(
                    prompt
                        .iter()
                        .any(|field| field.number == number && field.bytes == value.as_bytes())
                );
            }
            assert!(!prompt.iter().any(|field| field.number == 15));
            let call = sub_message(prompt, 6, 0);
            assert!(
                call.iter()
                    .any(|field| field.number == 1 && field.bytes == call_id.as_bytes())
            );
        }
    }

    #[test]
    fn request_carries_metadata_system_and_model() {
        let req = AiRequest::new(
            "swe-1-7",
            vec![
                text_item(Role::System, "be brief"),
                text_item(Role::User, "hi"),
            ],
        );
        let fields = top_level(&req);
        let meta = sub_message(&fields, 1, 0);
        // #3 carries the SINGLE session token (header doubling is separate).
        assert!(meta.iter().any(|f| f.number == 3 && f.bytes == b"tok"));
        assert!(meta.iter().any(|f| f.number == 1 && f.bytes == b"chisel"));
        assert!(meta.iter().any(|f| f.number == 31 && f.bytes.len() == 732));
        // The fingerprint is deterministic per session token — the same
        // credential always presents the same device id.
        assert_eq!(device_fingerprint("tok"), device_fingerprint("tok"));
        assert_ne!(device_fingerprint("tok"), device_fingerprint("tok2"));
        assert!(
            fields
                .iter()
                .any(|f| f.number == 2 && f.bytes == b"be brief")
        );
        assert!(fields.iter().any(|f| f.number == 7 && f.scalar == 5));
        assert!(fields.iter().any(|f| f.number == 20 && f.scalar == 1));
        assert!(
            fields
                .iter()
                .any(|f| f.number == 21 && f.bytes == b"swe-1-7")
        );
        // Turn-1 shape: #16 is a fresh uuid, #15 carries turn 1.
        let session = fields
            .iter()
            .find(|f| f.number == 16)
            .expect("missing session id");
        assert!(Uuid::parse_str(std::str::from_utf8(session.bytes).unwrap()).is_ok());
        let model_cfg = sub_message(&fields, 15, 0);
        // step_index is absent at 0 and counts up on later calls; the
        // trajectory constants are #3=CASCADE(4) and #4=USER_INPUT(14).
        assert!(model_cfg.iter().all(|f| !(f.number == 2 && f.scalar == 0)));
        assert!(model_cfg.iter().any(|f| f.number == 3 && f.scalar == 4));
        assert!(model_cfg.iter().any(|f| f.number == 4 && f.scalar == 14));
    }

    #[test]
    fn consecutive_same_role_text_merges() {
        let req = AiRequest::new(
            "m",
            vec![
                text_item(Role::User, "a"),
                text_item(Role::User, "b"),
                text_item(Role::Assistant, "c"),
                text_item(Role::User, "d"),
            ],
        );
        let fields = top_level(&req);
        let texts: Vec<Vec<u8>> = fields
            .iter()
            .filter(|f| f.number == 3 && f.wire_type == 2)
            .map(|f| {
                parse_fields(f.bytes)
                    .unwrap()
                    .into_iter()
                    .find(|inner| inner.number == 3)
                    .map(|inner| inner.bytes.to_vec())
                    .unwrap_or_default()
            })
            .collect();
        assert_eq!(
            texts,
            vec![b"a\n\nb".to_vec(), b"c".to_vec(), b"d".to_vec()]
        );
    }

    #[test]
    fn assistant_tool_calls_encode_as_submessages() {
        let mut item = text_item(Role::Assistant, "");
        item.tool_calls = Some(vec![ToolCall {
            id: "c1".into(),
            name: "grep".into(),
            arguments: "{\"p\":\"x\"}".into(),
        }]);
        let req = AiRequest::new("m", vec![item]);
        let fields = top_level(&req);
        let msg = sub_message(&fields, 3, 0);
        assert!(
            msg.iter()
                .any(|f| f.number == 2 && f.scalar == SOURCE_ASSISTANT)
        );
        let call = msg.iter().find(|f| f.number == 6).unwrap().bytes;
        let call_fields = parse_fields(call).unwrap();
        assert!(
            call_fields
                .iter()
                .any(|f| f.number == 1 && f.bytes == b"c1")
        );
        assert!(
            call_fields
                .iter()
                .any(|f| f.number == 2 && f.bytes == b"grep")
        );
        assert!(
            call_fields
                .iter()
                .any(|f| f.number == 3 && f.bytes == b"{\"p\":\"x\"}")
        );
    }

    #[test]
    fn tool_result_encodes_source_four_with_call_id() {
        let mut assistant = text_item(Role::Assistant, "");
        assistant.tool_calls = Some(vec![ToolCall {
            id: "c1".into(),
            name: "grep".into(),
            arguments: "{}".into(),
        }]);
        let mut item = text_item(Role::Tool, "42 results");
        item.tool_call_id = Some("c1".into());
        let req = AiRequest::new("m", vec![assistant, item]);
        let fields = top_level(&req);
        let msg = sub_message(&fields, 3, 1);
        assert!(
            msg.iter()
                .any(|f| f.number == 2 && f.scalar == SOURCE_TOOL_RESULT)
        );
        assert!(msg.iter().any(|f| f.number == 7 && f.bytes == b"c1"));
    }

    #[test]
    fn late_thinking_keeps_tool_continuation_adjacent() {
        let replay = ThinkingReplay {
            signature: "signed-thought".into(),
            signature_type: "native".into(),
            output_id: "output-1".into(),
            ..Default::default()
        };
        // Responses 客户端可能在 function_call 之后回传较晚封口的 reasoning。
        let prompts = responses_prompts(serde_json::json!([
            {"role":"user","content":"Read the fixture."},
            {"type":"function_call","call_id":"call-1#output-1",
             "name":"read","arguments":"{\"path\":\"fixture.json\"}"},
            {"type":"reasoning","summary":[],
             "content":[{"type":"reasoning_text","text":"Inspect the fixture."}],
             "encrypted_content":replay.encode()},
            {"type":"function_call_output","call_id":"call-1#output-1",
             "output":"{\"fixture_code\":\"RIVER-593\"}"}
        ]));
        let call_index = prompts
            .iter()
            .position(|prompt| prompt.iter().any(|field| field.number == 6))
            .unwrap();
        let result = &prompts[call_index + 1];
        assert!(
            result
                .iter()
                .any(|field| field.number == 2 && field.scalar == 4)
        );
        assert!(
            result
                .iter()
                .any(|field| field.number == 7 && field.bytes == b"call-1#output-1")
        );
        assert!(result.iter().any(|field| {
            field.number == 3 && field.bytes == br#"{"fixture_code":"RIVER-593"}"#
        }));
        for (number, value) in [
            (11, b"Inspect the fixture.".as_slice()),
            (12, b"signed-thought".as_slice()),
            (18, b"native".as_slice()),
        ] {
            assert!(
                prompts[call_index]
                    .iter()
                    .any(|field| field.number == number && field.bytes == value)
            );
        }
        assert!(!prompts[call_index].iter().any(|field| field.number == 15));
    }

    // 无签名/无密文的明文思考：原生 #11/#12/#13 只承载签名形态，明文按契约
    // 降级为普通助手正文（summary 在前、content 在后，空段跳过）；
    // 明文与载荷全空的块不产生任何 prompt。
    #[test]
    fn unsigned_thinking_degrades_to_assistant_text() {
        let request = AiRequest::new(
            "swe-2",
            vec![
                text_item(Role::User, "go"),
                AiItem {
                    role: Role::Assistant,
                    content: MessageContent::Blocks(vec![
                        ContentBlock::Thinking {
                            thinking: "unsigned draft".into(),
                            signature: None,
                        },
                        ContentBlock::Reasoning {
                            summary: vec!["summary".into(), String::new()],
                            content: vec!["body".into()],
                            encrypted_content: None,
                        },
                    ]),
                    tool_calls: None,
                    tool_call_id: None,
                    meta: None,
                },
                AiItem::thinking("", None),
                text_item(Role::User, "continue"),
            ],
        );
        let prompts: Vec<_> = top_level(&request)
            .iter()
            .filter(|field| field.number == 3)
            .map(|field| parse_fields(field.bytes).unwrap())
            .collect();
        let sources: Vec<u64> = prompts
            .iter()
            .map(|prompt| {
                prompt
                    .iter()
                    .find(|field| field.number == 2)
                    .unwrap()
                    .scalar
            })
            .collect();
        assert_eq!(sources, [SOURCE_USER, SOURCE_ASSISTANT, SOURCE_USER]);
        let assistant = &prompts[1];
        assert!(
            assistant
                .iter()
                .any(|field| field.number == 3 && field.bytes == b"unsigned draft\n\nsummary\nbody")
        );
        for number in [11, 12, 13] {
            assert!(!assistant.iter().any(|field| field.number == number));
        }
    }

    // 原始签名和无法解包的插件载荷都必须首发给上游尝试。
    #[test]
    fn opaque_native_signature_and_corrupt_wrapper_are_replayed() {
        let request = AiRequest::new(
            "swe-2",
            vec![
                text_item(Role::User, "go"),
                AiItem::thinking("foreign draft", Some("anthropic-blob".into())),
                text_item(Role::User, "continue"),
                AiItem::thinking("corrupt draft", Some("devin-thinking-v1:{oops".into())),
            ],
        );
        let prompts: Vec<_> = top_level(&request)
            .iter()
            .filter(|field| field.number == 3)
            .map(|field| parse_fields(field.bytes).unwrap())
            .collect();
        assert_eq!(prompts.len(), 4);
        assert!(
            prompts[1]
                .iter()
                .any(|f| f.number == 11 && f.bytes == b"foreign draft")
        );
        assert!(
            prompts[1]
                .iter()
                .any(|f| f.number == 12 && f.bytes == b"anthropic-blob")
        );
        assert!(
            prompts[3]
                .iter()
                .any(|f| f.number == 11 && f.bytes == b"corrupt draft")
        );
        assert!(
            prompts[3]
                .iter()
                .any(|f| f.number == 12 && f.bytes == b"devin-thinking-v1:{oops")
        );
    }

    // redacted 有原生载体（#12 载荷 + #13 标记）：独立成条的 redacted 历史
    // 原生保留为一条 prompt，不会降级也不会丢成空消息。
    #[test]
    fn redacted_only_item_keeps_native_prompt() {
        let mut request = AiRequest::new(
            "swe-2",
            vec![
                text_item(Role::User, "go"),
                AiItem {
                    role: Role::Assistant,
                    content: MessageContent::Blocks(vec![ContentBlock::RedactedThinking {
                        data: ThinkingReplay {
                            signature: "sealed-blob".into(),
                            ..ThinkingReplay::default()
                        }
                        .encode(),
                    }]),
                    tool_calls: None,
                    tool_call_id: None,
                    meta: None,
                },
            ],
        );
        request.generation.stop = Some(vec!["unsupported stop".into()]);
        let snapshot = serde_json::to_value(&request).unwrap();
        let prompts: Vec<_> = top_level(&request)
            .iter()
            .filter(|field| field.number == 3)
            .map(|field| parse_fields(field.bytes).unwrap())
            .collect();
        assert_eq!(prompts.len(), 2);
        let redacted = &prompts[1];
        assert!(
            redacted
                .iter()
                .any(|field| field.number == 2 && field.scalar == SOURCE_ASSISTANT)
        );
        assert!(
            redacted
                .iter()
                .any(|field| field.number == 12 && field.bytes == b"sealed-blob")
        );
        assert!(
            redacted
                .iter()
                .any(|field| field.number == 13 && field.scalar == 1)
        );
        for number in [3, 11] {
            assert!(!redacted.iter().any(|field| field.number == number));
        }
        assert_eq!(serde_json::to_value(&request).unwrap(), snapshot);
    }

    #[test]
    fn responses_foreign_ciphertext_is_replayed_without_mutating_readable_history() {
        let request = stravia_protocol_codec::registry::ProtocolRegistry::global()
            .adapter(&stravia_runtime_contract::protocol::ids::OPEN_RESPONSES_2026_04_24)
            .unwrap()
            .decode_request(serde_json::json!({
                "model":"swe-2",
                "input":[
                    {"role":"user","content":"Inspect fixtures."},
                    {"type":"reasoning","summary":[{"type":"summary_text","text":"Readable summary."}],
                     "content":[{"type":"reasoning_text","text":"Readable reasoning."}],
                     "encrypted_content":"gAAAA_foreign_responses_ciphertext"},
                    {"type":"reasoning","summary":[],"encrypted_content":"gAAAA_empty_ciphertext"},
                    {"role":"assistant","content":"Visible answer."}
                ]
            })).unwrap();
        let snapshot = serde_json::to_value(&request).unwrap();
        let prompts: Vec<_> = top_level(&request)
            .into_iter()
            .filter(|f| f.number == 3)
            .map(|f| parse_fields(f.bytes).unwrap())
            .collect();
        let text = prompts
            .iter()
            .flat_map(|p| p.iter())
            .filter(|f| [3, 11].contains(&f.number))
            .map(|f| String::from_utf8_lossy(f.bytes))
            .collect::<Vec<_>>()
            .join("");
        for expected in [
            "Readable summary.",
            "Readable reasoning.",
            "Visible answer.",
        ] {
            assert!(text.contains(expected));
        }
        assert!(!text.contains("ciphertext"));
        assert!(
            prompts
                .iter()
                .flatten()
                .any(|f| f.number == 12 && f.bytes == b"gAAAA_foreign_responses_ciphertext")
        );
        assert!(
            prompts
                .iter()
                .flatten()
                .any(|f| f.number == 12 && f.bytes == b"gAAAA_empty_ciphertext")
        );
        assert_eq!(serde_json::to_value(&request).unwrap(), snapshot);

        let mut redacted = text_item(Role::Assistant, "");
        redacted.content = MessageContent::Blocks(vec![ContentBlock::RedactedThinking {
            data: "foreign-redacted-ciphertext".into(),
        }]);
        let request = AiRequest::new("swe-2", vec![text_item(Role::User, "go"), redacted]);
        let fields = top_level(&request);
        let prompts: Vec<_> = fields
            .iter()
            .filter(|f| f.number == 3)
            .map(|f| parse_fields(f.bytes).unwrap())
            .collect();
        assert_eq!(prompts.len(), 2);
        assert!(
            prompts[1]
                .iter()
                .any(|f| f.number == 12 && f.bytes == b"foreign-redacted-ciphertext")
        );
        assert!(prompts[1].iter().any(|f| f.number == 13 && f.scalar == 1));
    }

    #[test]
    fn canonical_controls_encode_only_supported_native_fields() {
        use stravia_runtime_contract::protocol::ir::{NativeToolKind, ToolChoice};
        let mut req = AiRequest::new("m", vec![text_item(Role::User, "hello")]);
        req.generation.temperature = Some(0.25);
        req.generation.top_p = Some(0.8);
        req.generation.seed = Some(42);
        req.generation.stop = Some(vec!["END".into(), "STOP".into()]);
        req.parameters.top_k = Some(7);
        req.parameters.logprobs = Some(0);
        req.parameters.service_tier = Some(ServiceTier::Priority);
        req.parallel_tool_calls = Some(false);
        req.tool_choice = Some(ToolChoice::Named {
            name: "read".into(),
        });
        let fields = top_level(&req);
        let config = sub_message(&fields, 8, 0);
        assert!(
            config
                .iter()
                .any(|f| f.number == 5 && f.scalar == 0.25f64.to_bits())
        );
        assert!(config.iter().any(|f| f.number == 7 && f.scalar == 7));
        assert!(
            config
                .iter()
                .any(|f| f.number == 8 && f.scalar == 0.8f64.to_bits())
        );
        assert_eq!(
            config
                .iter()
                .filter(|f| f.number == 9)
                .map(|f| f.bytes)
                .collect::<Vec<_>>(),
            vec![b"END".as_slice(), b"STOP".as_slice()]
        );
        assert!(config.iter().any(|f| f.number == 10 && f.scalar == 42));
        assert!(config.iter().any(|f| f.number == 16 && f.scalar == 1));
        assert!(
            config
                .iter()
                .any(|f| f.number == 17 && f.bytes == b"priority")
        );
        assert!(fields.iter().any(|f| f.number == 11 && f.scalar == 1));
        assert!(
            sub_message(&fields, 12, 0)
                .iter()
                .any(|f| f.number == 2 && f.bytes == b"read")
        );

        req.parallel_tool_calls = Some(true);
        req.tool_choice = Some(ToolChoice::Auto);
        req.generation.seed = Some(-1);
        req.parameters.service_tier = None;
        let fields = top_level(&req);
        assert!(fields.iter().any(|f| f.number == 11 && f.scalar == 0));
        assert!(
            sub_message(&fields, 12, 0)
                .iter()
                .any(|f| f.number == 1 && f.bytes == b"auto")
        );
        assert!(
            sub_message(&fields, 8, 0)
                .iter()
                .all(|f| ![10, 17].contains(&f.number))
        );
        for choice in [
            ToolChoice::None,
            ToolChoice::Required,
            ToolChoice::Allowed {
                names: vec!["read".into()],
                required: false,
            },
            ToolChoice::Allowed {
                names: vec!["read".into()],
                required: true,
            },
            ToolChoice::Native(NativeToolKind::WebSearch),
        ] {
            req.tool_choice = Some(choice);
            assert!(top_level(&req).iter().all(|f| f.number != 12));
        }
    }

    #[test]
    fn logprobs_count_projects_to_native_boolean() {
        let mut req = AiRequest::new("m", vec![text_item(Role::User, "hello")]);
        let config = completion_config(&req);
        assert!(
            parse_fields(&config)
                .unwrap()
                .iter()
                .all(|field| field.number != 16)
        );
        for count in [0, 1, 20, u32::MAX] {
            req.parameters.logprobs = Some(count);
            let config = completion_config(&req);
            let fields = parse_fields(&config).unwrap();
            assert!(
                fields
                    .iter()
                    .any(|field| field.number == 16 && field.scalar == 1)
            );
        }
    }

    /// 缺省值遵循 CLI 逆向契约；显式 0 与未配置不同，不能被默认值覆盖。
    #[test]
    fn completion_config_follows_cli_default_and_preserves_explicit_limits() {
        let mut req = AiRequest::new("m", vec![text_item(Role::User, "hello")]);
        for (requested, expected) in [
            (None, 128_000),
            (Some(0), 0),
            (Some(1), 1),
            (Some(4_096), 4_096),
            (Some(u32::MAX), u32::MAX),
        ] {
            req.generation.max_tokens = requested;
            let fields = top_level(&req);
            let config = sub_message(&fields, 8, 0);
            assert_eq!(
                config
                    .iter()
                    .filter(|field| field.number == 2)
                    .map(|field| field.scalar)
                    .collect::<Vec<_>>(),
                vec![u64::from(expected)]
            );
        }
    }

    #[test]
    fn unconfirmed_service_tiers_are_omitted() {
        let mut req = AiRequest::new("m", vec![text_item(Role::User, "hello")]);
        for tier in [ServiceTier::Auto, ServiceTier::Default, ServiceTier::Flex] {
            req.parameters.service_tier = Some(tier);
            let config = completion_config(&req);
            let fields = parse_fields(&config).unwrap();
            assert!(fields.iter().all(|field| field.number != 17));
        }
    }

    /// The upstream rejects "all calls, then all results" groupings and
    /// orphan source=4 turns alike: results must interleave directly after
    /// the assistant prompt carrying their call, and unmatched results drop
    /// to user text — both live-verified `invalid_argument` triggers.
    #[test]
    fn tool_calls_pair_with_results_and_orphans_drop() {
        let mut assistant = text_item(Role::Assistant, "");
        assistant.tool_calls = Some(vec![
            ToolCall {
                id: "c1".into(),
                name: "bash".into(),
                arguments: "{}".into(),
            },
            ToolCall {
                id: "c2".into(),
                name: "bash".into(),
                arguments: "{}".into(),
            },
        ]);
        let result = |id: &str| {
            let mut item = text_item(Role::Tool, "out");
            item.tool_call_id = Some(id.into());
            item
        };
        // Results arrive out of call order plus one orphan.
        let req = AiRequest::new(
            "m",
            vec![
                text_item(Role::User, "hi"),
                assistant,
                result("c2"),
                result("c1"),
                result("ghost"),
            ],
        );
        let fields = top_level(&req);
        let prompts: Vec<Vec<ProtoField<'_>>> = fields
            .iter()
            .filter(|f| f.number == 3 && f.wire_type == 2)
            .map(|f| parse_fields(f.bytes).unwrap())
            .collect();
        let source_of = |msg: &[ProtoField<'_>]| {
            msg.iter()
                .find(|f| f.number == 2 && f.wire_type == 0)
                .map(|f| f.scalar)
        };
        let call_id_of = |msg: &[ProtoField<'_>]| {
            msg.iter()
                .find(|f| f.number == 7 && f.wire_type == 2)
                .map(|f| String::from_utf8_lossy(f.bytes).into_owned())
        };
        // user, assistant{2 calls}, result c1, result c2; orphan omitted.
        assert_eq!(prompts.len(), 4);
        assert_eq!(source_of(&prompts[1]), Some(SOURCE_ASSISTANT));
        assert_eq!(prompts[1].iter().filter(|f| f.number == 6).count(), 2);
        assert_eq!(call_id_of(&prompts[2]).as_deref(), Some("c1"));
        assert_eq!(call_id_of(&prompts[3]).as_deref(), Some("c2"));
    }

    #[test]
    fn history_images_remain_available_for_followup_questions() {
        let image_item = || AiItem {
            role: Role::User,
            content: MessageContent::Blocks(vec![ContentBlock::Image {
                source: MediaSource::Base64 {
                    media_type: "image/png".into(),
                    data: "aGk=".into(),
                },
                detail: None,
                cache_control: None,
            }]),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        };
        let fields = top_level(&AiRequest::new(
            "m",
            vec![
                image_item(),
                text_item(Role::Assistant, "done"),
                text_item(Role::User, "next"),
            ],
        ));
        let history = sub_message(&fields, 3, 0);
        let image = sub_message(&history, 10, 0);
        assert!(
            image
                .iter()
                .any(|field| field.number == 1 && field.bytes == b"aGk=")
        );
        assert!(
            image
                .iter()
                .any(|field| field.number == 2 && field.bytes == b"image/png")
        );
    }

    #[test]
    fn nested_tool_result_keeps_text_image_error_and_call_identity() {
        let request = AiRequest::new("swe-2", vec![
            text_item(Role::User, "Read the image."),
            AiItem::function_call(ToolCall {
                id: "read-image".into(),
                name: "read".into(),
                arguments: "{}".into(),
            }),
            AiItem {
                role: Role::User,
                content: MessageContent::Blocks(vec![ContentBlock::ToolResult {
                    tool_use_id: "read-image".into(),
                    content: serde_json::json!([
                        {"type": "text", "text": "Image from read."},
                        {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "aGk="}},
                        {"type": "video", "source": {"type": "url", "url": "https://example.test/clip.mp4"}}
                    ]),
                    content_kind: Some(stravia_runtime_contract::protocol::ir::ToolResultContentKind::ContentBlocks),
                    is_error: Some(true),
                    cache_control: None,
                }]),
                tool_calls: None,
                tool_call_id: None,
                meta: None,
            },
        ]);
        let fields = top_level(&request);
        let prompts: Vec<_> = fields
            .iter()
            .filter(|field| field.number == 3 && field.wire_type == 2)
            .map(|field| parse_fields(field.bytes).unwrap())
            .collect();
        let result = prompts
            .iter()
            .find(|prompt| {
                prompt
                    .iter()
                    .any(|field| field.number == 2 && field.scalar == SOURCE_TOOL_RESULT)
            })
            .expect("nested tool result reaches the native result turn");
        assert!(
            result
                .iter()
                .any(|field| field.number == 3 && field.bytes == b"Image from read.")
        );
        assert!(
            result
                .iter()
                .any(|field| field.number == 7 && field.bytes == b"read-image")
        );
        assert!(
            result
                .iter()
                .any(|field| field.number == 9 && field.scalar == 1)
        );
        let image = sub_message(result, 10, 0);
        assert!(
            image
                .iter()
                .any(|field| field.number == 1 && field.bytes == b"aGk=")
        );
        assert!(
            image
                .iter()
                .any(|field| field.number == 2 && field.bytes == b"image/png")
        );
        assert!(!fields.iter().any(|field| {
            field
                .bytes
                .windows(b"clip.mp4".len())
                .any(|bytes| bytes == b"clip.mp4")
        }));
    }

    #[test]
    fn tool_result_images_preserve_pixels_and_call_association() {
        let req = AiRequest::new(
            "swe-2",
            vec![
                text_item(Role::User, "Read the image."),
                AiItem::function_call(ToolCall {
                    id: "read-image".into(),
                    name: "read".into(),
                    arguments: r#"{"path":"image.png"}"#.into(),
                }),
                AiItem {
                    role: Role::Tool,
                    content: MessageContent::Blocks(vec![
                        ContentBlock::Text {
                            text: "Image from read.".to_owned().into(),
                            cache_control: None,
                        },
                        ContentBlock::Image {
                            source: MediaSource::Base64 {
                                media_type: "image/png".into(),
                                data: "aGk=".into(),
                            },
                            detail: None,
                            cache_control: None,
                        },
                    ]),
                    tool_calls: None,
                    tool_call_id: Some("read-image".into()),
                    meta: None,
                },
            ],
        );
        let fields = top_level(&req);
        let result = sub_message(&fields, 3, 2);
        let image = sub_message(&result, 10, 0);
        assert!(
            image
                .iter()
                .any(|field| field.number == 1 && field.bytes == b"aGk=")
        );
        assert!(
            image
                .iter()
                .any(|field| field.number == 2 && field.bytes == b"image/png")
        );
        assert!(
            result
                .iter()
                .any(|field| field.number == 7 && field.bytes == b"read-image")
        );
        assert!(
            result
                .iter()
                .any(|field| field.number == 3 && field.bytes == b"Image from read.")
        );
    }

    #[test]
    fn tools_inject_fallback_system_when_absent() {
        let mut req = AiRequest::new("m", vec![text_item(Role::User, "hi")]);
        req.tools = Some(vec![ToolSpec {
            name: "grep".into(),
            description: Some("find text".into()),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {"pattern": {"type": "string", "description": "regex"}},
                "required": ["pattern", "missing"],
                "$schema": "http://json-schema.org/draft-07/schema#",
            }),
            strict: None,
            defer_loading: None,
        }]);
        let fields = top_level(&req);
        // Fallback system prompt injected because tools exist without one.
        assert!(fields.iter().any(|f| f.number == 2 && !f.bytes.is_empty()));
        let def_fields = sub_message(&fields, 10, 0);
        // #2 emits the NAME, never the prose description (MCP gate workaround).
        assert!(
            def_fields
                .iter()
                .any(|f| f.number == 2 && f.bytes == b"grep")
        );
        let schema = def_fields.iter().find(|f| f.number == 3).unwrap().bytes;
        let schema: Value = serde_json::from_slice(schema).unwrap();
        // $schema dropped, unknown required key pruned, description stripped.
        assert!(schema.get("$schema").is_none());
        assert_eq!(schema["required"], serde_json::json!(["pattern"]));
        assert!(schema["properties"]["pattern"].get("description").is_none());
    }

    fn system_text(fields: &[crate::codec::devin_connect::proto::ProtoField<'_>]) -> String {
        fields
            .iter()
            .find(|f| f.number == 2 && f.wire_type == 2)
            .map(|f| String::from_utf8_lossy(f.bytes).into_owned())
            .unwrap_or_default()
    }

    fn tool(name: &str, description: Option<&str>, parameters: Value) -> ToolSpec {
        ToolSpec {
            name: name.into(),
            description: description.map(str::to_string),
            parameters,
            strict: None,
            defer_loading: None,
        }
    }

    fn first_tool_schema(fields: &[crate::codec::devin_connect::proto::ProtoField<'_>]) -> Value {
        let def_fields = sub_message(fields, 10, 0);
        serde_json::from_slice(def_fields.iter().find(|f| f.number == 3).unwrap().bytes).unwrap()
    }

    #[test]
    fn tool_descriptions_relocate_into_system_prompt() {
        let mut req = AiRequest::new(
            "m",
            vec![
                text_item(Role::System, "be brief"),
                text_item(Role::User, "hi"),
            ],
        );
        req.tools = Some(vec![tool(
            "grep",
            Some("Find text in files & folders. Returns <matches>."),
            serde_json::json!({"type":"object","properties":{"p":{"type":"string"}}}),
        )]);
        let fields = top_level(&req);
        let system = system_text(&fields);
        assert!(system.starts_with("be brief"));
        assert!(system.contains("# tools descriptions"));
        assert!(system.contains("<tool name=\"grep\">"));
        // 编号不改变描述内容；XML 边界字符不能成为外层工具标签。
        assert!(
            system.contains("1. Find text in files &amp; folders.\n2. Returns &lt;matches&gt;.")
        );
        // The wire ToolDef itself still carries only the name.
        let def_fields = sub_message(&fields, 10, 0);
        assert!(
            def_fields
                .iter()
                .any(|f| f.number == 2 && f.bytes == b"grep")
        );
    }

    #[test]
    fn bare_property_map_wraps_in_object_envelope() {
        let mut req = AiRequest::new("m", vec![text_item(Role::User, "hi")]);
        req.tools = Some(vec![tool(
            "search",
            None,
            serde_json::json!({
                "query": {"type": "string"},
                "limit": {"type": "integer"},
            }),
        )]);
        let fields = top_level(&req);
        let schema = first_tool_schema(&fields);
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["properties"]["query"]["type"], "string");
        assert_eq!(schema["properties"]["limit"]["type"], "integer");
    }

    #[test]
    fn local_refs_inline_and_defs_strip() {
        let mut req = AiRequest::new("m", vec![text_item(Role::User, "hi")]);
        req.tools = Some(vec![tool(
            "edit",
            None,
            serde_json::json!({
                "$defs": {"path_t": {"type": "string", "minLength": 1}},
                "type": "object",
                "properties": {
                    "path": {"$ref": "#/$defs/path_t", "description": "annotation stripped"},
                    "mode": {"enum": ["a", "b"]}
                }
            }),
        )]);
        let fields = top_level(&req);
        let schema = first_tool_schema(&fields);
        assert!(schema.get("$defs").is_none());
        assert_eq!(schema["properties"]["path"]["type"], "string");
        assert_eq!(schema["properties"]["path"]["minLength"], 1);
        assert!(schema["properties"]["path"].get("$ref").is_none());
        assert_eq!(
            schema["properties"]["mode"]["enum"],
            serde_json::json!(["a", "b"])
        );
    }

    #[test]
    fn annotations_strip_but_literals_and_property_names_survive() {
        let mut req = AiRequest::new("m", vec![text_item(Role::User, "hi")]);
        req.tools = Some(vec![tool(
            "set",
            None,
            serde_json::json!({
                "type": "object",
                "title": "Settings",
                "$comment": "internal",
                "x-custom": "vendor",
                "properties": {
                    "description": {"type": "string"},
                    "level": {"type": "integer", "default": 3},
                    "sample": {"type": "object", "examples": [{"description": "data lives"}]}
                }
            }),
        )]);
        let fields = top_level(&req);
        let schema = first_tool_schema(&fields);
        assert!(schema.get("title").is_none());
        assert!(schema.get("$comment").is_none());
        assert!(schema.get("x-custom").is_none());
        // A property NAMED description is a parameter, not an annotation.
        assert_eq!(schema["properties"]["description"]["type"], "string");
        // Literal-valued keys keep their subtrees verbatim.
        assert_eq!(schema["properties"]["level"]["default"], 3);
        assert_eq!(
            schema["properties"]["sample"]["examples"][0]["description"],
            "data lives"
        );
    }

    #[test]
    fn invalid_tool_name_is_omitted() {
        let mut req = AiRequest::new("m", vec![text_item(Role::User, "hi")]);
        req.tools = Some(vec![tool(
            "mcp::search",
            None,
            serde_json::json!({"type": "object"}),
        )]);
        let fields = top_level(&req);
        assert!(fields.iter().all(|field| field.number != 10));
        let prompt = sub_message(&fields, 3, 0);
        assert!(
            prompt
                .iter()
                .any(|field| field.number == 3 && field.bytes == b"hi")
        );
    }

    #[test]
    fn system_prompt_fingerprints_are_neutralized() {
        let req = AiRequest::new(
            "m",
            vec![
                text_item(
                    Role::System,
                    "You are Claude Code, Anthropic's official CLI for Claude.",
                ),
                text_item(Role::User, "hi"),
            ],
        );
        let fields = top_level(&req);
        let system = system_text(&fields);
        assert!(system.contains("You are an AI coding assistant."));
        assert!(!system.contains("Claude Code"));
    }

    #[test]
    fn empty_tool_result_without_id_is_omitted() {
        let req = AiRequest::new(
            "m",
            vec![
                text_item(Role::Tool, ""),
                text_item(Role::User, "preserved"),
            ],
        );
        let fields = top_level(&req);
        let messages: Vec<_> = fields
            .iter()
            .filter(|f| f.number == 3 && f.wire_type == 2)
            .collect();
        assert_eq!(messages.len(), 1);
        let msg = sub_message(&fields, 3, 0);
        assert!(msg.iter().any(|f| f.number == 3 && f.bytes == b"preserved"));
    }

    #[test]
    fn documents_project_text_and_inline_images_for_each_role() {
        let document = |source| ContentBlock::Document {
            source,
            title: None,
            context: None,
            cache_control: None,
        };
        let image = ContentBlock::Image {
            source: MediaSource::Base64 {
                media_type: "image/png".into(),
                data: "aW1hZ2U=".into(),
            },
            detail: None,
            cache_control: None,
        };
        let mut items = Vec::new();
        for role in [Role::System, Role::User, Role::Assistant, Role::Tool] {
            let mut item = text_item(role, "");
            item.content = MessageContent::Blocks(vec![
                document(DocumentSource::PlainText {
                    data: "plain document".into(),
                }),
                document(DocumentSource::Blocks {
                    content: vec![
                        ContentBlock::Text {
                            text: "nested document".to_owned().into(),
                            cache_control: None,
                        },
                        image.clone(),
                        ContentBlock::CompactionTrigger {},
                    ],
                }),
                document(DocumentSource::Base64Pdf {
                    data: "unsupported pdf".into(),
                }),
            ]);
            if role == Role::Assistant {
                item.tool_calls = Some(vec![ToolCall {
                    id: "doc-call".into(),
                    name: "read".into(),
                    arguments: "{}".into(),
                }]);
            }
            if role == Role::Tool {
                item.tool_call_id = Some("doc-call".into());
            }
            items.push(item);
        }
        let req = AiRequest::new("m", items);
        let snapshot = serde_json::to_value(&req).unwrap();
        let fields = top_level(&req);
        assert!(
            fields
                .iter()
                .any(|f| f.number == 2 && f.bytes == b"plain document\nnested document")
        );
        let prompts: Vec<_> = fields
            .iter()
            .filter(|f| f.number == 3)
            .map(|f| parse_fields(f.bytes).unwrap())
            .collect();
        assert_eq!(prompts.len(), 3);
        for prompt in prompts {
            assert!(
                prompt
                    .iter()
                    .any(|f| f.number == 3 && f.bytes == b"plain document\nnested document")
            );
            assert_eq!(prompt.iter().filter(|f| f.number == 10).count(), 1);
        }
        assert_eq!(serde_json::to_value(&req).unwrap(), snapshot);
    }

    #[test]
    fn serialized_tool_documents_preserve_only_convertible_children() {
        let content = serde_json::json!([
            {"type":"document", "source":{"type":"plain_text", "data":"plain"}},
            {"type":"document", "source":{"type":"blocks", "content":[
                {"type":"text", "text":"nested"},
                {"type":"image", "source":{"type":"base64", "media_type":"image/png", "data":"aW1hZ2U="}},
                {"type":"video", "source":{"type":"url", "url":"https://example.test/video"}}
            ]}},
            {"type":"document", "source":{"type":"base64_pdf", "data":"unsupported pdf"}}
        ]);
        let mut text = String::new();
        let mut images = Vec::new();
        append_tool_result_content(
            &content,
            Some(stravia_runtime_contract::protocol::ir::ToolResultContentKind::ContentBlocks),
            &mut text,
            &mut images,
        );
        assert_eq!(text, "plain\nnested");
        assert_eq!(images.len(), 1);
        assert!(
            parse_fields(&images[0])
                .unwrap()
                .iter()
                .any(|f| f.number == 1 && f.bytes == b"aW1hZ2U=")
        );
    }

    #[test]
    fn unsupported_generation_fields_are_pruned_without_mutating_snapshot() {
        let mut req = AiRequest::new("m", vec![text_item(Role::User, "hi")]);
        req.generation.seed = Some(1);
        req.generation.presence_penalty = Some(0.5);
        req.generation.frequency_penalty = Some(-0.5);
        req.generation.stop = Some(vec!["unsupported stop".into()]);
        let snapshot = serde_json::to_value(&req).unwrap();
        let fields = top_level(&req);
        let prompt = sub_message(&fields, 3, 0);
        assert!(
            prompt
                .iter()
                .any(|field| field.number == 3 && field.bytes == b"hi")
        );
        assert_eq!(serde_json::to_value(&req).unwrap(), snapshot);
    }

    #[test]
    fn mixed_content_is_pruned_only_on_encoding_copy() {
        let mut item = text_item(Role::User, "");
        item.content = MessageContent::Blocks(vec![
            ContentBlock::Text {
                text: "preserved prompt".to_owned().into(),
                cache_control: None,
            },
            ContentBlock::CompactionTrigger {},
            ContentBlock::Video {
                source: stravia_runtime_contract::protocol::ir::MediaSource::Url(
                    "https://example.test/video.mp4".into(),
                ),
                media_type: Some("video/mp4".into()),
            },
        ]);
        let req = AiRequest::new("m", vec![item]);
        let snapshot = serde_json::to_value(&req).unwrap();
        let fields = top_level(&req);
        let prompt = sub_message(&fields, 3, 0);
        assert!(
            prompt
                .iter()
                .any(|field| field.number == 3 && field.bytes == b"preserved prompt")
        );
        assert_eq!(serde_json::to_value(&req).unwrap(), snapshot);
    }

    #[test]
    fn unsupported_tool_result_payload_is_omitted_without_fabrication() {
        let mut assistant = text_item(Role::Assistant, "");
        assistant.tool_calls = Some(vec![ToolCall {
            id: "call-result".into(),
            name: "read".into(),
            arguments: "{}".into(),
        }]);
        let mut result = text_item(Role::Tool, "");
        result.tool_call_id = Some("call-result".into());
        result.content = MessageContent::Blocks(vec![
            ContentBlock::Text {
                text: "preserved result".to_owned().into(),
                cache_control: None,
            },
            ContentBlock::ToolResult {
                tool_use_id: "call-result".into(),
                content: serde_json::json!([{"type":"input_audio","data":"aGk="}]),
                content_kind: Some(
                    stravia_runtime_contract::protocol::ir::ToolResultContentKind::ContentBlocks,
                ),
                is_error: None,
                cache_control: None,
            },
        ]);
        let req = AiRequest::new("m", vec![assistant, result]);
        let fields = top_level(&req);
        let result = sub_message(&fields, 3, 1);
        assert!(
            result
                .iter()
                .any(|field| field.number == 7 && field.bytes == b"call-result")
        );
        assert!(
            result
                .iter()
                .any(|field| field.number == 3 && field.bytes == b"preserved result")
        );
    }

    /// Zero penalties (including -0.0) are sampling no-ops upstreams send
    /// unconditionally — they preserve the absent-field sampling configuration.
    #[test]
    fn zero_penalties_encode_as_absent() {
        let fixed = SessionShape {
            trajectory_id: "11111111-1111-1111-1111-111111111111".into(),
            cascade_id: "22222222-2222-2222-2222-222222222222".into(),
            step_index: 0,
        };
        let plain = AiRequest::new("m", vec![text_item(Role::User, "hi")]);
        let baseline = encode_test_chat_request(&plain, "tok", &fixed, None).unwrap();

        let mut zeroed = AiRequest::new("m", vec![text_item(Role::User, "hi")]);
        zeroed.generation.presence_penalty = Some(0.0);
        zeroed.generation.frequency_penalty = Some(-0.0);
        let encoded = encode_test_chat_request(&zeroed, "tok", &fixed, None).unwrap();
        // ChatMessage UUIDs are fresh per encoding; compare the sampling
        // configuration rather than those unrelated message identities.
        let baseline_config = parse_fields(&baseline)
            .unwrap()
            .into_iter()
            .find(|field| field.number == 8)
            .expect("sampling configuration");
        let encoded_config = parse_fields(&encoded)
            .unwrap()
            .into_iter()
            .find(|field| field.number == 8)
            .expect("sampling configuration");
        assert_eq!(encoded_config.bytes, baseline_config.bytes);
    }

    #[test]
    fn explicit_host_platform_is_encoded_in_client_metadata() {
        let request = encode_client_metadata_request_with_platform(
            "tok",
            false,
            DevinClientPlatform::Windows,
        );
        let metadata = parse_fields(&request)
            .unwrap()
            .into_iter()
            .find(|field| field.number == 1)
            .expect("client metadata");
        let platform = parse_fields(metadata.bytes)
            .unwrap()
            .into_iter()
            .find(|field| field.number == 5)
            .expect("client platform");
        assert_eq!(platform.bytes, b"windows");
    }

    #[test]
    fn encoded_session_ids_preserve_complete_scoped_utf8_and_message_lifecycle() {
        let req = AiRequest::new("m", vec![text_item(Role::User, "hi")]);
        let source = " principal/目标/会话 ";
        let shape = session_shape(Some(source));
        assert_eq!(shape.trajectory_id, "550f6c5d-7cef-401a-b1fc-b10e2baf7625");
        assert_eq!(shape.cascade_id, "d3042aec-4029-458c-ba34-e931615046b1");
        let first = encode_test_chat_request(&req, "tok", &shape, Some("synthetic-jwt")).unwrap();
        let second = encode_test_chat_request(&req, "tok", &shape, Some("synthetic-jwt")).unwrap();
        let first_fields = parse_fields(&first).unwrap();
        let second_fields = parse_fields(&second).unwrap();
        let field = |number| {
            first_fields
                .iter()
                .find(|field| field.number == number)
                .unwrap()
        };
        assert_eq!(field(16).bytes, shape.cascade_id.as_bytes());
        assert_eq!(field(26).bytes, b"synthetic-jwt");
        let refreshed = encode_test_chat_request(
            &req,
            "refreshed-token",
            &session_shape(Some(source)),
            Some("synthetic-jwt"),
        )
        .unwrap();
        let refreshed_fields = parse_fields(&refreshed).unwrap();
        assert_eq!(
            refreshed_fields
                .iter()
                .find(|field| field.number == 16)
                .unwrap()
                .bytes,
            field(16).bytes
        );
        assert_eq!(
            refreshed_fields
                .iter()
                .find(|field| field.number == 15)
                .unwrap()
                .bytes,
            field(15).bytes
        );
        let trajectory = parse_fields(field(15).bytes).unwrap();
        assert_eq!(
            trajectory
                .iter()
                .find(|field| field.number == 1)
                .unwrap()
                .bytes,
            shape.trajectory_id.as_bytes()
        );
        let first_message = parse_fields(field(3).bytes).unwrap();
        let second_message = parse_fields(
            second_fields
                .iter()
                .find(|field| field.number == 3)
                .unwrap()
                .bytes,
        )
        .unwrap();
        let message_id = first_message
            .iter()
            .find(|field| field.number == 1)
            .unwrap()
            .bytes;
        let next_message_id = second_message
            .iter()
            .find(|field| field.number == 1)
            .unwrap()
            .bytes;
        assert_ne!(message_id, next_message_id);
        assert_eq!(
            Uuid::parse_str(std::str::from_utf8(message_id).unwrap())
                .unwrap()
                .get_version_num(),
            4
        );
        assert_ne!(
            shape.cascade_id,
            session_shape(Some(source.trim())).cascade_id
        );
        assert_ne!(
            session_shape(Some(&"a".repeat(1024))).cascade_id,
            session_shape(Some(&format!("{}尾", "a".repeat(1024)))).cascade_id
        );
        let whitespace = session_shape(Some("  "));
        assert_eq!(
            whitespace.cascade_id,
            "22aa0fa8-af4b-4016-97ad-5c693e95ff40"
        );
        assert_eq!(whitespace.cascade_id, session_shape(Some("  ")).cascade_id);
    }

    #[test]
    fn session_shape_derives_stable_trajectory_and_steps() {
        let first = session_shape(Some("principal-a/target-a/session"));
        // First GetChatMessage on a trajectory omits step_index on the wire.
        assert_eq!(first.step_index, 0);
        assert!(Uuid::parse_str(&first.trajectory_id).is_ok());
        assert!(Uuid::parse_str(&first.cascade_id).is_ok());
        assert_ne!(first.trajectory_id, first.cascade_id);
        // Ids are deterministic from the start — AssignModel binds its jwt to
        // the cascade id before the chat request exists.
        let second = session_shape(Some("principal-a/target-a/session"));
        assert_eq!(second.cascade_id, first.cascade_id);
        assert_eq!(second.trajectory_id, first.trajectory_id);
        assert_eq!(second.step_index, 0);
        for affinity in [
            Some("principal-b/target-a/session"),
            Some("principal-a/target-b/session"),
            Some("principal-a/target-a/other-session"),
            None,
            Some(""),
        ] {
            assert_ne!(session_shape(affinity).cascade_id, first.cascade_id);
        }
        assert_ne!(
            session_shape(None).cascade_id,
            session_shape(None).cascade_id
        );
    }

    #[test]
    fn trajectory_reference_matches_current_wire_shape() {
        let req = AiRequest::new("m", vec![text_item(Role::User, "hi")]);
        let mut shape = shape(&req);
        shape.step_index = 0;
        let first = trajectory_ref(&shape);
        let fields = parse_fields(&first).unwrap();
        // step_index absent at 0; constants #3=4 (CASCADE), #4=14 (USER_INPUT).
        assert!(fields.iter().all(|f| f.number != 2));
        assert!(fields.iter().any(|f| f.number == 3 && f.scalar == 4));
        assert!(fields.iter().any(|f| f.number == 4 && f.scalar == 14));

        shape.step_index = 3;
        let later = trajectory_ref(&shape);
        let fields = parse_fields(&later).unwrap();
        assert!(fields.iter().any(|f| f.number == 2 && f.scalar == 3));
    }

    #[test]
    fn assignment_jwt_rides_field_26() {
        let req = AiRequest::new("m", vec![text_item(Role::User, "hi")]);
        let without = encode_test_chat_request(&req, "tok", &shape(&req), None).unwrap();
        let with = encode_test_chat_request(&req, "tok", &shape(&req), Some("jwt-1")).unwrap();
        let absent = parse_fields(&without).unwrap();
        assert!(absent.iter().all(|f| f.number != 26));
        let present = parse_fields(&with).unwrap();
        assert!(
            present
                .iter()
                .any(|f| f.number == 26 && f.bytes == b"jwt-1")
        );
    }

    #[test]
    fn assign_model_roundtrip() {
        let body = encode_test_assign_request("tok", "swe-2-max", "cascade-1");
        let fields = parse_fields(&body).unwrap();
        assert!(fields.iter().any(|f| f.number == 1 && f.wire_type == 2));
        assert!(
            fields
                .iter()
                .any(|f| f.number == 2 && f.bytes == b"swe-2-max")
        );
        assert!(
            fields
                .iter()
                .any(|f| f.number == 3 && f.bytes == b"cascade-1")
        );

        // Response {1: {1: jwt, 2: model_uid}}; both fields required.
        let mut inner = Vec::new();
        write_string_field(&mut inner, 1, "jwt-9");
        write_string_field(&mut inner, 2, "swe-2-max-resolved");
        let mut outer = Vec::new();
        write_message_field(&mut outer, 1, &inner);
        let decoded = decode_assign_model_response(&outer).unwrap();
        assert_eq!(decoded.jwt, "jwt-9");
        assert_eq!(decoded.model_uid, "swe-2-max-resolved");

        let mut partial_inner = Vec::new();
        write_string_field(&mut partial_inner, 2, "uid-only");
        let mut partial = Vec::new();
        write_message_field(&mut partial, 1, &partial_inner);
        assert!(decode_assign_model_response(&partial).is_none());
    }

    #[test]
    fn tool_error_rides_the_wire() {
        let assistant = AiItem {
            role: Role::Assistant,
            content: MessageContent::Blocks(vec![ContentBlock::ToolUse {
                id: "c1".into(),
                name: "bash".into(),
                input: serde_json::json!({}),
                cache_control: None,
            }]),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        };
        let mut result = text_item(Role::Tool, "boom");
        result.tool_call_id = Some("c1".into());
        result.content = MessageContent::Blocks(vec![ContentBlock::ToolResult {
            tool_use_id: "c1".into(),
            content: serde_json::json!("boom"),
            content_kind: None,
            is_error: Some(true),
            cache_control: None,
        }]);
        let req = AiRequest::new("m", vec![text_item(Role::User, "hi"), assistant, result]);
        let fields = top_level(&req);
        let prompts: Vec<Vec<ProtoField<'_>>> = fields
            .iter()
            .filter(|f| f.number == 3 && f.wire_type == 2)
            .map(|f| parse_fields(f.bytes).unwrap())
            .collect();
        // The source=4 result is flagged #9.
        let result_msg = prompts
            .iter()
            .find(|msg| msg.iter().any(|f| f.number == 2 && f.scalar == 4))
            .expect("tool result message");
        assert!(result_msg.iter().any(|f| f.number == 9 && f.scalar == 1));
    }

    #[test]
    fn decodes_cli_model_config_entries() {
        let entry = |selector: Option<&str>, disabled: bool| {
            let mut config = Vec::new();
            write_string_field(&mut config, 1, "Label");
            if disabled {
                write_varint_field(&mut config, 4, 1);
            }
            if let Some(selector) = selector {
                write_string_field(&mut config, 22, selector);
            }
            let mut field = Vec::new();
            write_message_field(&mut field, 1, &config);
            field
        };
        let body = [
            entry(Some("swe-1-7"), false),
            entry(Some("claude-sonnet-5-medium"), false),
            entry(Some("retired-model"), true),
            entry(None, false),
        ]
        .concat();

        let selectors = |body: &[u8]| {
            decode_cli_model_configs(body)
                .expect("valid model config protobuf")
                .into_iter()
                .map(|config| config.selector)
                .collect::<Vec<_>>()
        };
        assert_eq!(selectors(&body), ["swe-1-7", "claude-sonnet-5-medium"]);
        assert!(decode_cli_model_configs(b"\xff\xff").is_err());
        assert!(
            decode_cli_model_configs(&[])
                .expect("empty protobuf")
                .is_empty()
        );
    }

    #[test]
    fn decodes_cli_model_config_display_metadata() {
        let mut info = Vec::new();
        write_string_field(&mut info, 20, "cs5m");
        write_string_field(&mut info, 23, "claude-sonnet-5");
        let mut config = Vec::new();
        write_string_field(&mut config, 1, " Claude Sonnet 5 Medium ");
        write_varint_field(&mut config, 5, 1);
        write_varint_field(&mut config, 10, 3);
        write_varint_field(&mut config, 18, 400_000);
        write_string_field(&mut config, 22, "claude-sonnet-5-medium");
        write_message_field(&mut config, 23, &info);
        for (name, price) in [("Input", 5.0_f64), ("Cached input", 0.5), ("Output", 25.0)] {
            let mut row = Vec::new();
            write_string_field(&mut row, 1, name);
            write_fixed64_field(&mut row, 2, price);
            write_message_field(&mut config, 32, &row);
        }
        let mut body = Vec::new();
        write_message_field(&mut body, 1, &config);

        let entries = decode_cli_model_configs(&body).expect("valid model metadata protobuf");
        let entry = entries.first().expect("expected one entry");
        assert_eq!(entries.len(), 1);
        assert_eq!(entry.selector, "claude-sonnet-5-medium");
        assert_eq!(entry.label.as_deref(), Some("Claude Sonnet 5 Medium"));
        assert_eq!(entry.supports_images, Some(true));
        assert_eq!(entry.provider, Some(3));
        assert_eq!(entry.context_window, Some(400_000));
        assert_eq!(entry.alias.as_deref(), Some("claude-sonnet-5"));
        assert_eq!(entry.short_alias.as_deref(), Some("cs5m"));
        let cost = entry.cost.expect("pricing rows");
        assert_eq!(cost.input, Some(5.0));
        assert_eq!(cost.cache_read, Some(0.5));
        assert_eq!(cost.output, Some(25.0));
        assert_eq!(devin_upstream_provider_name(3), Some("anthropic"));
        assert_eq!(devin_upstream_provider_name(6), None);
    }

    #[test]
    fn decodes_fixed32_pricing_without_float_noise() {
        // Upstream emits prices as fixed32; verbatim widening turns 0.22f32
        // into 0.2199999988079071 in Provider Model metadata.
        let mut config = Vec::new();
        write_string_field(&mut config, 22, "swe-1-7");
        for (name, price) in [
            ("Input", 0.22_f32),
            ("Cached input", 0.007),
            ("Output", 0.66),
        ] {
            let mut row = Vec::new();
            write_string_field(&mut row, 1, name);
            write_fixed32_field(&mut row, 2, price);
            write_message_field(&mut config, 32, &row);
        }
        let mut body = Vec::new();
        write_message_field(&mut body, 1, &config);

        let entries = decode_cli_model_configs(&body).expect("valid pricing protobuf");
        let cost = entries[0].cost.expect("pricing rows");
        assert_eq!(cost.input, Some(0.22));
        assert_eq!(cost.cache_read, Some(0.007));
        assert_eq!(cost.output, Some(0.66));
    }
}
