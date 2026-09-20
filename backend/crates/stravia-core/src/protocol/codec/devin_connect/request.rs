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
//!   #3  repeated ChatMessage { #1 uuid, #2 source, #3 text,
//!                              #6 ChatToolCall{#1 id,#2 name,#3 args_json},
//!                              #7 tool_call_id, #10 ImageData{#1 b64,#2 mime},
//!                              #11 thinking, #12 signature, #13 redacted,
//!                              #15 output_id, #18 signature_type }
//!   #7  request source enum (5)
//!   #8  CompletionConfig { #1 enabled, #2 max_tokens, #3 max_newlines,
//!                          #5 f64 temperature, #7 top_k, #8 f64 top_p }
//!   #10 repeated ToolDef { #1 name, #2 description, #3 parameters JSON }
//!   #15 CortexTrajectoryReference { #1 trajectory_id, #2 step_index (>0 only),
//!                                   #3 =4 CASCADE, #4 =14 USER_INPUT }
//!   #16 cascade id
//!   #20 = 1
//!   #21 model uid
//!   #26 assignment jwt — present only when AssignModel resolved the uid
//! ```
//!
//! The #15/#16/#26 shape mirrors current devin CLI captures (3000.10.x): the
//! CLI calls `AssignModel` per session and carries the returned jwt at #26.
//! Calling a router uid directly without an assignment is answered with the
//! canned `unavailable: third-party model provider` trailer — a permanent
//! failure disguised as a transient one.
//!
//! ChatMessage.source: 1 = user, 2 = assistant, 4 = tool result.
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

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

use anyhow::bail;
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
use stravia_runtime_contract::protocol::ir::MediaSource;
use stravia_runtime_contract::protocol::ir::MessageContent;
use stravia_runtime_contract::protocol::ir::Role;

/// Connect-RPC method path on the Devin api-server host.
pub(crate) const GET_CHAT_MESSAGE_PATH: &str = "/exa.api_server_pb.ApiServerService/GetChatMessage";

/// ClientMetadata constants observed on live CLI requests. `chisel` is the
/// CLI's internal client name; the version tracks the Devin CLI build —
/// upstream serves a reduced model catalog to older versions, so this must
/// stay near the current release.
const CLIENT_NAME: &str = "chisel";
const CLIENT_VERSION: &str = "3000.10.27";
const CLIENT_LANGUAGE: &str = "en";

/// ClientMetadata #5: the real CLI reports darwin→"mac", windows→"windows",
/// everything else→"linux".
fn client_platform() -> &'static str {
    match std::env::consts::OS {
        "macos" => "mac",
        "windows" => "windows",
        _ => "linux",
    }
}

/// CompletionConfiguration defaults per current CLI captures: max_tokens is
/// the full 128k window and max_newlines caps line count, not tokens.
const WIRE_DEFAULT_MAX_TOKENS: u64 = 128_000;
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
/// Both ids derive deterministically from the request — AssignModel binds
/// its jwt to the cascade id, so it must be computable before the chat
/// request exists.
#[derive(Debug)]
pub(crate) struct SessionShape {
    pub trajectory_id: String,
    pub cascade_id: String,
    pub step_index: u64,
}

/// Derives the session shape. Ids are seeded by the credential plus the
/// system prompt head and the first user message head — stable across turns
/// and restarts (which keeps upstream prompt-prefix caching effective), and
/// distinct between conversations. `#22`/`#18`/`#13` are never emitted:
/// current captures show them absent.
pub(crate) fn session_shape(req: &AiRequest, session_token: &str) -> SessionShape {
    let mut hasher = Sha256::new();
    hasher.update(session_token.as_bytes());
    hasher.update(b"devin-session-root");
    let system = collect_system_prompt(req);
    let bytes = system.as_bytes();
    hasher.update(&bytes[..bytes.len().min(4096)]);
    if let Some(anchor) = req
        .items
        .iter()
        .find(|item| matches!(item.role, Role::User))
        .map(|item| item.content.to_text())
    {
        let bytes = anchor.as_bytes();
        hasher.update(&bytes[..bytes.len().min(1024)]);
    }
    let root: [u8; 32] = hasher.finalize().into();
    let trajectory_id = uuid_from_seed(&root, 0);
    SessionShape {
        cascade_id: uuid_from_seed(&root, 1),
        step_index: next_step_index(&trajectory_id),
        trajectory_id,
    }
}

/// Per-trajectory step_index counter. The real CLI increments once per
/// GetChatMessage call — tool-loop rounds included — starting absent(0);
/// its title-gen call takes index 1, which we never send, so our sequence
/// jumps straight to 2 to match the observed wire. The map is bounded and
/// wholesale-cleared when full: resetting a bookkeeping counter is harmless.
fn next_step_index(trajectory_id: &str) -> u64 {
    static STEPS: LazyLock<Mutex<HashMap<String, u64>>> =
        LazyLock::new(|| Mutex::new(HashMap::new()));
    let mut counts = STEPS.lock().unwrap_or_else(|e| e.into_inner());
    if counts.len() >= 65_536 {
        counts.clear();
    }
    match counts.get_mut(trajectory_id) {
        None => {
            counts.insert(trajectory_id.to_owned(), 2);
            0
        }
        Some(next) => {
            let value = *next;
            *next += 1;
            value
        }
    }
}

fn uuid_from_seed(root: &[u8; 32], salt: u32) -> String {
    let mut hasher = Sha256::new();
    hasher.update(root);
    hasher.update(salt.to_be_bytes());
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
pub(crate) fn encode_get_chat_message_request(
    req: &AiRequest,
    session_token: &str,
    shape: &SessionShape,
    assignment_jwt: Option<&str>,
) -> anyhow::Result<Vec<u8>> {
    if session_token.trim().is_empty() {
        bail!("Devin Connect request is missing the session token");
    }
    if req.embedding.is_some() {
        bail!("Devin Connect does not expose an embeddings endpoint");
    }
    // A zero penalty (±0.0) is a no-op upstreams often send unconditionally
    // — encode it as absent. A nonzero value would change sampling, which
    // the wire cannot express, so it stays a hard rejection.
    if req.generation.seed.is_some()
        || req
            .generation
            .presence_penalty
            .is_some_and(|penalty| penalty != 0.0)
        || req
            .generation
            .frequency_penalty
            .is_some_and(|penalty| penalty != 0.0)
        || req.generation.stop.is_some()
    {
        bail!(
            "Devin Connect cannot represent seed, nonzero presence/frequency penalties, or stop sequences"
        );
    }
    if req.response_format.is_some() {
        bail!("Devin Connect does not expose a model-neutral response format");
    }
    if req.safety_settings.is_some() {
        bail!("Devin Connect does not expose safety settings");
    }
    // #11 disable_parallel_tool_calls / #12 tool_choice exist in third-party
    // .proto reconstructions but their tags were never wire-confirmed; the
    // reference client deliberately omits them (a wrong tag can silently
    // overwrite adjacent request fields). tool_choice /
    // disable_parallel_tool_calls are therefore not forwarded.

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
            target: "stravia_core::protocol::devin_connect",
            "devin request text sanitized: {hits:?}"
        );
    }

    let mut out = Vec::new();
    write_message_field(&mut out, 1, &client_metadata(session_token, false));
    write_string_field(&mut out, 2, &system_prompt);
    for message in &chat_messages {
        write_message_field(&mut out, 3, &message.encode());
    }
    write_varint_field(&mut out, 7, 5);
    write_message_field(&mut out, 8, &completion_config(req));
    for tool_def in &tool_defs {
        write_message_field(&mut out, 10, tool_def);
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
pub(crate) const ASSIGN_MODEL_PATH: &str = "/exa.api_server_pb.ApiServerService/AssignModel";

#[derive(Debug, Default, Clone)]
pub(crate) struct ModelAssignment {
    pub jwt: String,
    pub model_uid: String,
}

/// AssignModelRequest: `{1: metadata, 2: router_uid, 3: cascade_id}`.
pub(crate) fn encode_assign_model_request(
    session_token: &str,
    router_uid: &str,
    cascade_id: &str,
) -> Vec<u8> {
    let mut out = Vec::new();
    write_message_field(&mut out, 1, &client_metadata(session_token, false));
    write_string_field(&mut out, 2, router_uid);
    write_string_field(&mut out, 3, cascade_id);
    out
}

/// AssignModelResponse: `{1: assignment{1: assignment_jwt, 2: model_uid}}`.
/// Both fields must be present — an empty assignment is worse than none.
pub(crate) fn decode_assign_model_response(body: &[u8]) -> Option<ModelAssignment> {
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
pub(crate) fn encode_client_metadata_request(session_token: &str, displays: bool) -> Vec<u8> {
    let mut out = Vec::new();
    write_message_field(&mut out, 1, &client_metadata(session_token, displays));
    out
}

/// One entry from `GetCliModelConfigsResponse`. `ClientModelConfig` repeats
/// at top-level #1; entries flagged #4 disabled are not callable and dropped
/// at decode time. Tag numbers are calibrated from a live 200 response
/// (`dwgx/WindsurfAPI` `devin-connect-catalog.js` decodeCatalog); the pricing
/// rows at #32 were recovered from the CLI's cached `model_configs` payload.
#[derive(Debug, Clone, Default)]
pub(crate) struct DevinModelConfig {
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
pub(crate) struct DevinModelCost {
    pub input: Option<f64>,
    pub cache_read: Option<f64>,
    pub output: Option<f64>,
}

/// The upstream provider enum carried at ClientModelConfig #10.
pub(crate) fn devin_upstream_provider_name(id: u64) -> Option<&'static str> {
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

pub(crate) fn decode_cli_model_configs(body: &[u8]) -> Vec<DevinModelConfig> {
    let Ok(top) = parse_fields(body) else {
        return Vec::new();
    };
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
        let Ok(config) = parse_fields(entry.bytes) else {
            continue;
        };
        if varint(&config, 4) == Some(1) {
            continue;
        }
        let Some(selector) = string(&config, 22) else {
            continue;
        };
        let (alias, short_alias, is_router) = config
            .iter()
            .find(|f| f.number == 23 && f.wire_type == 2)
            .and_then(|f| parse_fields(f.bytes).ok())
            .map(|info| {
                (
                    string(&info, 23),
                    string(&info, 20),
                    varint(&info, 25) == Some(1),
                )
            })
            .unwrap_or_default();
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
    configs
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
fn client_metadata(session_token: &str, displays: bool) -> Vec<u8> {
    let mut out = Vec::new();
    write_string_field(&mut out, 1, CLIENT_NAME);
    write_string_field(&mut out, 2, CLIENT_VERSION);
    write_string_field(&mut out, 3, session_token);
    write_string_field(&mut out, 4, CLIENT_LANGUAGE);
    write_string_field(&mut out, 5, client_platform());
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
/// real CLI derives a stable machine fingerprint from MAC addresses
/// (`windsurf-api-client/fingerprint.rs`) and the server tracks it per
/// account (`has_fingerprint_set`); the wire itself only checks shape. We
/// derive a deterministic per-credential id instead of a fresh random one —
/// the same token always presents the same device, matching the CLI's
/// stable-device semantics and the reference's opt-in stable-device mode.
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
        u64::from(
            req.generation
                .max_tokens
                .unwrap_or(WIRE_DEFAULT_MAX_TOKENS as u32),
        ),
    );
    write_varint_field(&mut out, 3, WIRE_MAX_NEWLINES);
    write_fixed64_field(&mut out, 5, temperature);
    write_varint_field(&mut out, 7, DEFAULT_TOP_K);
    write_fixed64_field(&mut out, 8, req.generation.top_p.unwrap_or(DEFAULT_TOP_P));
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
            let text = item.content.to_text();
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
    /// Repeated #6 — one assistant prompt carries every tool call of the
    /// turn, so the upstream sees the call set followed by the matching
    /// source=4 results. Splitting calls across consecutive assistant
    /// prompts breaks the alternation strict validation expects.
    tool_calls: Vec<Vec<u8>>,
    tool_call_id: Option<String>,
    /// #9 — tool result carried an error (`is_error` on the source block).
    tool_error: bool,
    thinking: String,
    thinking_replay: Option<ThinkingReplay>,
    thinking_redacted: bool,
}

impl ChatMsg {
    fn is_text_only(&self) -> bool {
        self.tool_calls.is_empty()
            && self.tool_call_id.is_none()
            && self.images.is_empty()
            && self.thinking.is_empty()
            && self.thinking_replay.is_none()
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
            if !replay.output_id.is_empty() {
                write_string_field(&mut out, 15, &replay.output_id);
            }
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

fn push_message(out: &mut Vec<ChatMsg>, message: ChatMsg) {
    // The upstream rejects long runs of consecutive same-source turns; fold
    // text-only user/assistant messages into the previous one.
    if message.is_text_only()
        && matches!(message.source, SOURCE_USER | SOURCE_ASSISTANT)
        && let Some(last) = out.last_mut()
        && last.source == message.source
        && last.is_text_only()
    {
        if !last.text.is_empty() {
            last.text.push_str("\n\n");
        }
        last.text.push_str(&message.text);
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
    demote_orphan_tool_results(&mut out);
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
        // Orphan results keep their relative order rather than being dropped.
        for result in &prompts[results_start..index] {
            if !consumed.iter().any(|c| std::ptr::eq(*c, result)) {
                out.push((*result).clone());
            }
        }
    }
    out
}

/// A source=4 result with no matching assistant tool call is rejected with
/// invalid_argument upstream — demote it to a user-text turn so the result
/// content survives (e.g. after client-side history compaction dropped the
/// originating call).
fn demote_orphan_tool_results(prompts: &mut [ChatMsg]) {
    let call_ids: std::collections::HashSet<String> = prompts
        .iter()
        .flat_map(|prompt| prompt.tool_call_ids())
        .collect();
    for prompt in prompts.iter_mut() {
        if prompt.source != SOURCE_TOOL_RESULT {
            continue;
        }
        let matched = prompt
            .tool_call_id
            .as_ref()
            .is_some_and(|id| call_ids.contains(id));
        if matched {
            continue;
        }
        let text = std::mem::take(&mut prompt.text);
        prompt.source = SOURCE_USER;
        prompt.text = format!("[tool result, original call lost]\n{text}");
        prompt.tool_call_id = None;
        prompt.tool_error = false;
    }
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
            current.text = text.clone();
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
                current.images.push(encode_inline_image(source)?);
            }
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
                ..
            } => {
                // Anthropic-style tool results arrive inside user items; emit
                // each as the native source=4 turn it belongs to.
                if has_content {
                    push_message(out, std::mem::take(&mut current));
                    current.source = SOURCE_USER;
                }
                push_message(
                    out,
                    ChatMsg {
                        source: SOURCE_TOOL_RESULT,
                        text: tool_result_text(content),
                        tool_call_id: Some(tool_use_id.clone()),
                        tool_error: *is_error == Some(true),
                        ..ChatMsg::default()
                    },
                );
                has_content = false;
            }
            other => bail!(
                "Devin Connect cannot represent user content block `{}`",
                content_block_name(other)
            ),
        }
    }
    if has_content && (!current.text.is_empty() || !current.images.is_empty()) {
        push_message(out, current);
    }
    Ok(())
}

fn encode_assistant_item(item: &AiItem, out: &mut Vec<ChatMsg>) -> anyhow::Result<()> {
    // 带签名的思考块独立编码，避免把多个签名拼到同一 #12；
    // 同一普通助手项的全部工具调用仍共用一个 prompt，保持结果配对。
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
                ContentBlock::Thinking { .. }
                | ContentBlock::RedactedThinking { .. }
                | ContentBlock::Reasoning { .. } => {
                    if !msg.text.is_empty() || !msg.tool_calls.is_empty() {
                        push_message(out, std::mem::take(&mut msg));
                        msg.source = SOURCE_ASSISTANT;
                    }
                    push_message(out, encode_thinking(block)?);
                }
                ContentBlock::ToolUse {
                    id, name, input, ..
                } => {
                    msg.tool_calls.push(encode_tool_call(
                        id,
                        name,
                        &serde_json::to_string(input).unwrap_or_else(|_| input.to_string()),
                        custom,
                    )?);
                }
                other => bail!(
                    "Devin Connect cannot represent assistant content block `{}`",
                    content_block_name(other)
                ),
            }
        }
    } else {
        msg.text = item.content.to_text();
    }
    for call in item.tool_calls.as_deref().unwrap_or_default() {
        msg.tool_calls.push(encode_tool_call(
            &call.id,
            &call.name,
            &call.arguments,
            custom,
        )?);
    }
    if !msg.text.is_empty() || !msg.tool_calls.is_empty() {
        push_message(out, msg);
    }
    Ok(())
}

fn encode_thinking(block: &ContentBlock) -> anyhow::Result<ChatMsg> {
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
        // 没有签名的历史仍保留可读内容，但不能伪装成上游原生签名思考。
        return Ok(ChatMsg {
            source: SOURCE_ASSISTANT,
            text,
            ..ChatMsg::default()
        });
    };
    let mut replay = ThinkingReplay::decode(signature)?;
    let thinking = if redacted {
        std::mem::take(&mut replay.redacted_text)
    } else {
        text
    };
    Ok(ChatMsg {
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
        MessageContent::Text(value) => value.clone(),
        MessageContent::Blocks(blocks) => {
            let mut text = String::new();
            let mut separator = "";
            for block in blocks {
                let fragment = match block {
                    ContentBlock::Text { text, .. } => Cow::Borrowed(text.as_str()),
                    ContentBlock::ToolResult {
                        content, is_error, ..
                    } => {
                        tool_error |= *is_error == Some(true);
                        Cow::Owned(tool_result_text(content))
                    }
                    ContentBlock::Image { source, .. } => {
                        images.push(encode_inline_image(source)?);
                        continue;
                    }
                    other => bail!(
                        "Devin Connect cannot represent tool result content block `{}`",
                        content_block_name(other)
                    ),
                };
                text.push_str(separator);
                text.push_str(&fragment);
                separator = "\n";
            }
            text
        }
    };
    let text = if text.is_empty() {
        "[tool result]".to_string()
    } else {
        text
    };
    match item.tool_call_id.as_deref() {
        Some(tool_call_id) => push_message(
            out,
            ChatMsg {
                source: SOURCE_TOOL_RESULT,
                text,
                tool_call_id: Some(tool_call_id.to_string()),
                tool_error,
                images,
                ..ChatMsg::default()
            },
        ),
        // No id to echo: fold into a user turn like the reference's
        // emulation path instead of dropping the result.
        None => push_message(
            out,
            ChatMsg {
                source: SOURCE_USER,
                text: format!("[tool result]: {text}"),
                images,
                ..ChatMsg::default()
            },
        ),
    }
    Ok(())
}

fn tool_result_text(content: &Value) -> String {
    let text = match content {
        Value::String(text) => text.clone(),
        other => serde_json::to_string(other).unwrap_or_else(|_| other.to_string()),
    };
    // An empty tool result still needs a body — the reference emits a
    // placeholder rather than a zero-length #3.
    if text.is_empty() {
        "[tool result]".to_string()
    } else {
        text
    }
}

fn encode_inline_image(source: &MediaSource) -> anyhow::Result<Vec<u8>> {
    let MediaSource::Base64 { media_type, data } = source else {
        bail!("Devin Connect accepts only inline image bytes");
    };
    // ImageData{#1 base64 text, #2 mime} — verified from wire: #1 carries the
    // base64 STRING, not raw image bytes.
    let mut out = Vec::new();
    write_string_field(&mut out, 1, data);
    write_string_field(&mut out, 2, media_type);
    Ok(out)
}

fn encode_tool_call(
    id: &str,
    name: &str,
    args_json: &str,
    custom: bool,
) -> anyhow::Result<Vec<u8>> {
    // ChatToolCall{#1 id, #2 name, #3 arguments JSON} — only #3 is a JSON
    // string; the envelope around it is protobuf.
    let mut call = Vec::new();
    write_string_field(&mut call, 1, id);
    write_string_field(&mut call, 2, name);
    if custom {
        let arguments: Value = serde_json::from_str(args_json)?;
        let input = arguments
            .get("input")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("Devin custom tool arguments require a string input"))?;
        write_string_field(&mut call, 4, input);
        write_varint_field(&mut call, 6, 1);
    } else {
        write_string_field(&mut call, 3, args_json);
    }
    Ok(call)
}

/// Wire-encoded ToolDef bodies plus `(name, sanitized description)` pairs for
/// system-prompt relocation.
type EncodedToolDefs = (Vec<Vec<u8>>, Vec<(String, String)>);

/// Returns the wire-encoded ToolDefs plus `(name, sanitized description)`
/// pairs for system-prompt relocation. Tool names are validated locally:
/// upstream only accepts `[A-Za-z0-9_-]` and answers anything else with a
/// vague `invalid_argument`. Names are never rewritten — renaming would
/// break the tool_call name echo in replayed history.
fn encode_tool_defs(
    req: &AiRequest,
    hits: &mut sanitize::SanitizeHits,
) -> anyhow::Result<EncodedToolDefs> {
    let mut defs = Vec::new();
    let mut descriptions = Vec::new();
    for tool in req.tools.as_deref().unwrap_or_default() {
        let name = tool.name.trim();
        if !valid_tool_name(name) {
            bail!(
                "Devin Connect rejects tool name {name:?}: characters outside [A-Za-z0-9_-] fail upstream"
            );
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
        escape_xml_into(description, false, &mut section);
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

fn content_block_name(block: &ContentBlock) -> &'static str {
    match block {
        ContentBlock::Text { .. } => "text",
        ContentBlock::Image { .. } => "image",
        ContentBlock::Audio { .. } => "audio",
        ContentBlock::File { .. } => "file",
        ContentBlock::Video { .. } => "video",
        ContentBlock::Thinking { .. } => "thinking",
        ContentBlock::Reasoning { .. } => "reasoning",
        ContentBlock::Compaction { .. } => "compaction",
        ContentBlock::CompactionTrigger {} => "compaction_trigger",
        ContentBlock::RedactedThinking { .. } => "redacted_thinking",
        ContentBlock::ToolUse { .. } => "tool_use",
        ContentBlock::ToolResult { .. } => "tool_result",
        ContentBlock::ServerToolUse { .. } => "server_tool_use",
        ContentBlock::ServerToolResult { .. } => "server_tool_result",
        ContentBlock::Document { .. } => "document",
        ContentBlock::SearchResult { .. } => "search_result",
        ContentBlock::Citation { .. } => "citation",
        ContentBlock::ExecutableCode { .. } => "executable_code",
        ContentBlock::CodeExecutionResult { .. } => "code_execution_result",
        ContentBlock::ContainerUpload { .. } => "container_upload",
        ContentBlock::Refusal { .. } => "refusal",
        ContentBlock::Unknown { .. } => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::codec::devin_connect::proto::{parse_fields, write_fixed32_field};
    use stravia_runtime_contract::protocol::ir::ToolCall;
    use stravia_runtime_contract::protocol::ir::ToolSpec;

    fn text_item(role: Role, text: &str) -> AiItem {
        AiItem {
            role,
            content: MessageContent::Text(text.to_string()),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        }
    }

    /// Encode and leak the wire bytes so parsed fields borrow 'static — tests
    /// only, keeps field access free of lifetime plumbing.
    fn wire(req: &AiRequest) -> &'static [u8] {
        let bytes = encode_get_chat_message_request(req, "tok", &shape(req), None).unwrap();
        Box::leak(bytes.into_boxed_slice())
    }

    fn shape(req: &AiRequest) -> SessionShape {
        session_shape(req, "tok")
    }

    fn top_level(
        req: &AiRequest,
    ) -> Vec<crate::protocol::codec::devin_connect::proto::ProtoField<'static>> {
        parse_fields(wire(req)).unwrap()
    }

    fn sub_message<'a>(
        fields: &'a [crate::protocol::codec::devin_connect::proto::ProtoField<'a>],
        field: u32,
        index: usize,
    ) -> Vec<crate::protocol::codec::devin_connect::proto::ProtoField<'a>> {
        let bytes = fields
            .iter()
            .filter(|f| f.number == field && f.wire_type == 2)
            .nth(index)
            .unwrap_or_else(|| panic!("missing sub-message field #{field} at index {index}"))
            .bytes;
        parse_fields(bytes).unwrap()
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
        let req = AiRequest::new(
            "swe-2",
            vec![
                text_item(Role::User, "Read the fixture."),
                AiItem::function_call(ToolCall {
                    id: "call-1#output-1".into(),
                    name: "read".into(),
                    arguments: r#"{"path":"fixture.json"}"#.into(),
                }),
                AiItem::reasoning(
                    Vec::new(),
                    vec!["Inspect the fixture.".into()],
                    Some(replay.encode()),
                ),
                AiItem {
                    tool_call_id: Some("call-1#output-1".into()),
                    ..text_item(Role::Tool, r#"{"fixture_code":"RIVER-593"}"#)
                },
            ],
        );
        let fields = top_level(&req);
        let prompts: Vec<_> = fields
            .iter()
            .filter(|field| field.number == 3 && field.wire_type == 2)
            .map(|field| parse_fields(field.bytes).unwrap())
            .collect();
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
        let thinking = prompts
            .iter()
            .position(|prompt| prompt.iter().any(|field| field.number == 12))
            .unwrap();
        assert!(thinking < call_index);
        for (number, value) in [
            (11, b"Inspect the fixture.".as_slice()),
            (12, b"signed-thought".as_slice()),
            (15, b"output-1".as_slice()),
            (18, b"native".as_slice()),
        ] {
            assert!(
                prompts[thinking]
                    .iter()
                    .any(|field| field.number == number && field.bytes == value)
            );
        }
    }

    /// The upstream rejects "all calls, then all results" groupings and
    /// orphan source=4 turns alike: results must interleave directly after
    /// the assistant prompt carrying their call, and unmatched results drop
    /// to user text — both live-verified `invalid_argument` triggers.
    #[test]
    fn tool_calls_pair_with_results_and_orphans_demote() {
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
            item.tool_call_id = Some(id.to_string());
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
        // user, assistant{2 calls}, result c1, result c2, demoted ghost.
        assert_eq!(prompts.len(), 5);
        assert_eq!(source_of(&prompts[1]), Some(SOURCE_ASSISTANT));
        assert_eq!(prompts[1].iter().filter(|f| f.number == 6).count(), 2);
        assert_eq!(call_id_of(&prompts[2]).as_deref(), Some("c1"));
        assert_eq!(call_id_of(&prompts[3]).as_deref(), Some("c2"));
        assert_eq!(source_of(&prompts[4]), Some(SOURCE_USER));
        assert!(
            prompts[4]
                .iter()
                .any(|f| f.number == 3 && f.bytes.starts_with(b"[tool result, original call lost]"))
        );
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
                            text: "Image from read.".into(),
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
            cache_control: None,
            meta: None,
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

    fn system_text(
        fields: &[crate::protocol::codec::devin_connect::proto::ProtoField<'_>],
    ) -> String {
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
            cache_control: None,
            meta: None,
        }
    }

    fn first_tool_schema(
        fields: &[crate::protocol::codec::devin_connect::proto::ProtoField<'_>],
    ) -> Value {
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
        // XML-escaped so the description can't break the section markup.
        assert!(system.contains("Find text in files &amp; folders. Returns &lt;matches&gt;."));
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
    fn invalid_tool_name_fails_fast() {
        let mut req = AiRequest::new("m", vec![text_item(Role::User, "hi")]);
        req.tools = Some(vec![tool(
            "mcp::search",
            None,
            serde_json::json!({"type": "object"}),
        )]);
        assert!(encode_get_chat_message_request(&req, "tok", &shape(&req), None).is_err());
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
    fn empty_tool_result_folds_to_user_without_id() {
        let item = text_item(Role::Tool, "");
        let req = AiRequest::new("m", vec![item]);
        let fields = top_level(&req);
        let msg = sub_message(&fields, 3, 0);
        assert!(msg.iter().any(|f| f.number == 2 && f.scalar == SOURCE_USER));
        assert!(
            msg.iter()
                .any(|f| f.number == 3 && f.bytes == b"[tool result]: [tool result]")
        );
    }

    #[test]
    fn unrepresentable_generation_fields_fail() {
        let mut req = AiRequest::new("m", vec![text_item(Role::User, "hi")]);
        req.generation.seed = Some(1);
        assert!(encode_get_chat_message_request(&req, "t", &shape(&req), None).is_err());

        // A nonzero penalty changes sampling and must not be dropped
        // silently — the wire cannot express it.
        let mut req = AiRequest::new("m", vec![text_item(Role::User, "hi")]);
        req.generation.presence_penalty = Some(0.5);
        assert!(encode_get_chat_message_request(&req, "t", &shape(&req), None).is_err());

        let mut req = AiRequest::new("m", vec![text_item(Role::User, "hi")]);
        req.generation.frequency_penalty = Some(-0.5);
        assert!(encode_get_chat_message_request(&req, "t", &shape(&req), None).is_err());
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
        let baseline = encode_get_chat_message_request(&plain, "tok", &fixed, None).unwrap();

        let mut zeroed = AiRequest::new("m", vec![text_item(Role::User, "hi")]);
        zeroed.generation.presence_penalty = Some(0.0);
        zeroed.generation.frequency_penalty = Some(-0.0);
        let encoded = encode_get_chat_message_request(&zeroed, "tok", &fixed, None).unwrap();
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
    fn session_shape_derives_stable_trajectory_and_steps() {
        let turn1 = AiRequest::new("m", vec![text_item(Role::User, "open")]);
        let first = session_shape(&turn1, "tok");
        // First GetChatMessage on a trajectory omits step_index on the wire.
        assert_eq!(first.step_index, 0);
        assert!(Uuid::parse_str(&first.trajectory_id).is_ok());
        assert!(Uuid::parse_str(&first.cascade_id).is_ok());
        assert_ne!(first.trajectory_id, first.cascade_id);
        // Ids are deterministic from the start — AssignModel binds its jwt to
        // the cascade id before the chat request exists.
        let second = session_shape(&turn1, "tok");
        assert_eq!(second.cascade_id, first.cascade_id);
        // step_index increments per call: 0, 2, 3… (the CLI's title-gen
        // takes index 1).
        assert_eq!(second.step_index, 2);

        // A continuing conversation keeps the same ids — the seed reads the
        // system prompt head and first user message, not the whole history.
        let turn2 = AiRequest::new(
            "m",
            vec![
                text_item(Role::User, "open"),
                text_item(Role::Assistant, "reply"),
                text_item(Role::User, "again"),
            ],
        );
        let pinned = session_shape(&turn2, "tok");
        assert_eq!(pinned.cascade_id, first.cascade_id);
        assert_eq!(pinned.trajectory_id, first.trajectory_id);

        // A different opener or credential is a different session.
        let other = AiRequest::new(
            "m",
            vec![
                text_item(Role::User, "different opener"),
                text_item(Role::Assistant, "reply"),
                text_item(Role::User, "again"),
            ],
        );
        assert_ne!(session_shape(&other, "tok").cascade_id, pinned.cascade_id);
        assert_ne!(session_shape(&turn2, "tok2").cascade_id, pinned.cascade_id);
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
        let without = encode_get_chat_message_request(&req, "tok", &shape(&req), None).unwrap();
        let with =
            encode_get_chat_message_request(&req, "tok", &shape(&req), Some("jwt-1")).unwrap();
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
        let body = encode_assign_model_request("tok", "swe-2-max", "cascade-1");
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
                .into_iter()
                .map(|config| config.selector)
                .collect::<Vec<_>>()
        };
        assert_eq!(selectors(&body), ["swe-1-7", "claude-sonnet-5-medium"]);
        assert!(selectors(b"\xff\xff").is_empty());
        assert!(selectors(&[]).is_empty());
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

        let entries = decode_cli_model_configs(&body);
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

        let entries = decode_cli_model_configs(&body);
        let cost = entries[0].cost.expect("pricing rows");
        assert_eq!(cost.input, Some(0.22));
        assert_eq!(cost.cache_read, Some(0.007));
        assert_eq!(cost.output, Some(0.66));
    }
}
