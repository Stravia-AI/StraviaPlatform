//! StraviaRead 的唯一 path 契约：统一读取表达式的解析与工具 schema。
//!
//! 模型只提供一个 `path` 字符串和若干结构化字段，由 [`parse_read_input`] 解析为
//! [`ReadTarget`]。这里没有任何 percent 解码或编码路径：自由文本放在 `question`，
//! 结构化选项写成 path 末尾的 `:` 选择器。
//!
//! 支持的 path 形式：
//! - `stravia://search`：搜索；搜索文本放在 `question`，`allowed_domains` 限定最终来源。
//! - `stravia://turns/<28 lowercase>`：续接上一轮；问题放在 `question`，附加媒体放在 `attachments`，
//!   域名限制放在 `allowed_domains`。解析器不知道 turn 的类型，两者由 core 在查明类型后校验。
//! - `stravia://tool-executions/<28 lowercase>`：观察已有执行；等待秒数放在 `wait`。
//! - `stravia://artifacts/<55 lowercase>[选择器]`：Artifact 资源。
//! - `http(s)://…[选择器]`：公网资源。
//!
//! 选择器只对资源路径有效，从 path 末尾按 `:` 自右向左剥离：
//! `raw`、`download`、`cursor=<base64url>`，以及行范围 `N`、`N-M`、`N+K`、`-K`
//! （逗号分隔多段）。剥离有“底线”：Artifact 的底线是 55 位 id 结束处；HTTP(S) 的
//! 底线是 authority 结束处，即 `scheme://` 之后第一个 `/`、`?` 或 `#`。没有该字符时
//! 不剥离，因此 `https://host:8080` 里的 `:8080` 是端口。token 严格符合语法才剥离，
//! 否则停止，剩余部分按字面作为地址。源 URL 本身以选择器形态结尾时，把那个 `:`
//! 写成 `%3A`；`%3A` 原样发往源站。
//!
//! 旧语法（`search://`、`query://`、`#stravia?`、Artifact 的 `?options`、`?wait=`）
//! 不保留别名，只返回带正确写法的错误。
//!
//! [`input_schema`] 是 MCP 使用的宽松形式（只有 `path` 必填）；[`model_input_schema`]
//! 是模型工具声明使用的严格形式（出现的字段全部必填，可选字段类型可为 `null`）。
//! 字段按 [`ReadSchemaScope`] 的暴露范围裁剪。

use serde::{Deserialize, Serialize};

use crate::normalize_domains;

/// 搜索文本的最大字节数，与 Web Search 执行侧既有限制一致。
pub const MAX_SEARCH_TEXT_BYTES: usize = 64 * 1024;
/// `allowed_domains` 在规范化前允许的原始条目上限。
pub const MAX_ALLOWED_DOMAINS: usize = 20;
/// 行范围允许的最大分段数。
pub const MAX_LINE_SELECTIONS: usize = 16;

const READ_SCHEME_PREFIX: &str = "stravia://";
const SEARCH_PATH: &str = "stravia://search";
const TURN_REFERENCE_PREFIX: &str = "stravia://turns/";
const TOOL_EXECUTION_PREFIX: &str = "stravia://tool-executions/";
const ARTIFACT_REFERENCE_PREFIX: &str = "stravia://artifacts/";
/// 旧 HTTP(S) 工具选项所在的保留 fragment 前缀。
const LEGACY_OPTIONS_FRAGMENT_PREFIX: &str = "stravia?";
const TURN_ID_LEN: usize = 28;
const ARTIFACT_DIGEST_LEN: usize = 55;

/// StraviaRead 工具的全部输入。
///
/// JSON 中的 `null` 与字段缺省含义相同；旧顶层 `url`、`previous_path` 等字段由
/// `deny_unknown_fields` 明确拒绝，不做兼容别名。
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ReadInput {
    pub path: String,
    #[serde(default)]
    pub question: Option<String>,
    #[serde(default)]
    pub attachments: Option<Vec<String>>,
    #[serde(default)]
    pub allowed_domains: Option<Vec<String>>,
    #[serde(default)]
    pub wait: Option<u64>,
}

/// 解析后的读取目标。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReadTarget {
    Search(SearchTarget),
    Continuation(ContinuationTarget),
    Resource(ResourcePath),
    ToolExecution(ToolExecutionPath),
}

/// 一次搜索读取。
///
/// `allowed_domains` 为 `None` 表示字段缺省：新根搜索即无限制，续接时由调用方按
/// 父策略解析继承；`Some` 为显式非空列表，整体替换父策略。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SearchTarget {
    pub query: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_domains: Option<Vec<String>>,
}

/// 一次轮次续接；具体是搜索轮还是媒体轮由 core 用 `TurnChainStore::node_kind` 判定。
///
/// `turn_path` 是精确的 `stravia://turns/<id>`；`attachments` 已按逐项规则校验，可为空。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ContinuationTarget {
    pub turn_path: String,
    pub question: String,
    #[serde(default)]
    pub attachments: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_domains: Option<Vec<String>>,
}

/// 一次资源读取：HTTP(S) URL 或 Artifact Reference。
///
/// `url` 保留源 URL 的 path、query 与普通 fragment；只剥离末尾的 `:` 选择器。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResourcePath {
    pub url: String,
    #[serde(default)]
    pub options: ReadOptions,
}

/// 资源读取选项；全部缺省表示默认读取。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReadOptions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub question: Option<String>,
    #[serde(default)]
    pub raw: bool,
    #[serde(default)]
    pub download: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lines: Option<Vec<LineSelection>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}

/// Read-only observation of an existing execution; `wait` never changes its deadline.
///
/// `wait_seconds` 缺省为 0；正数只等待当前查询，不创建执行或延长原期限。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolExecutionPath {
    pub id: String,
    pub wait_seconds: u64,
}

/// 一次行选择，行号 1-based，按源文本 LF 计行。
///
/// 解析产生的列表经过排序与合并的规范化：有界区间一律为
/// [`LineSelection::Inclusive`]，开放区间（到 EOF）为 [`LineSelection::From`]，
/// [`LineSelection::Last`] 依赖总行数，只能在末尾保留（多个 `Last` 合并为最大
/// 计数），与绝对区间的最终合并由快照层在已知总行数后完成。
/// [`LineSelection::Count`] 仅供直接构造方使用。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum LineSelection {
    /// 从该行到 EOF。
    From(u64),
    /// 闭区间 `[start, end]`。
    Inclusive { start: u64, end: u64 },
    /// 从 `start` 起共 `count` 行。
    Count { start: u64, count: u64 },
    /// 最后 `count` 行。
    Last(u64),
}

/// path 与字段解析错误。
///
/// Display 消息不回显完整 URL、question 或其他字段值，并给出可执行的正确写法。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ReadPathError {
    /// path 为空或仅空白。
    #[error("read path must not be blank")]
    BlankPath,
    /// 旧 `search://` 或 `query://` 表达式。
    #[error(
        "legacy search paths are not accepted; set path to `stravia://search` and put the search text in question"
    )]
    LegacySearchScheme,
    /// 不是受支持的 path 形式。
    #[error(
        "read path must be `stravia://search`, a `stravia://turns/<turn-id>`, a `stravia://tool-executions/<id>`, a `stravia://artifacts/<artifact-id>`, or an absolute http(s) URL"
    )]
    InvalidScheme,
    /// HTTP(S) URL 结构非法。
    #[error(
        "invalid http(s) URL; use an absolute URL and add a trailing slash after a bare host:port"
    )]
    InvalidUrl,
    /// 精确匹配的 path 带上了后缀或选择器。
    #[error("`{target}` accepts no suffix; {hint}")]
    UnexpectedSuffix {
        target: &'static str,
        hint: &'static str,
    },
    /// 该 path 形式缺少必填字段。
    #[error("`{field}` is required for {target}; {hint}")]
    MissingField {
        field: &'static str,
        target: &'static str,
        hint: &'static str,
    },
    /// 字段在该 path 形式下不允许出现。
    #[error("`{field}` is not accepted for {target}; {hint}")]
    FieldNotAllowed {
        field: &'static str,
        target: &'static str,
        hint: &'static str,
    },
    /// `question` 为空或仅空白。
    #[error("question must not be blank")]
    BlankQuestion,
    /// `question` 超过 [`MAX_SEARCH_TEXT_BYTES`]。
    #[error("question must not exceed 65536 UTF-8 bytes")]
    QuestionTooLarge,
    /// `allowed_domains` 原始条目超过 [`MAX_ALLOWED_DOMAINS`] 项。
    #[error("allowed_domains accepts at most 20 entries before normalization")]
    TooManyAllowedDomains { count: usize },
    /// `allowed_domains` 是显式空数组。
    #[error(
        "allowed_domains must not be an empty list; omit it or stravia null to inherit or leave sources unrestricted, or list domains to replace them"
    )]
    EmptyAllowedDomains,
    /// `allowed_domains` 含无效条目或规范化后无可保留条目。
    #[error("allowed_domains entries must be valid domain filters, for example `docs.rs`")]
    InvalidAllowedDomains,
    /// `attachments` 条目不是精确 Artifact Reference 或公网 http(s) URL。
    #[error(
        "each attachment must be an exact `stravia://artifacts/<artifact-id>` or a public http(s) URL, without `:` selectors or tool option fragments"
    )]
    InvalidAttachment,
    /// `attachments` 含重复条目。
    #[error("attachments must not contain duplicate entries")]
    DuplicateAttachment,
    /// Artifact 引用不是裸身份或身份字符非法。
    #[error("invalid Artifact Reference: {reason}")]
    InvalidArtifactReference { reason: &'static str },
    /// Turn 引用不是精确的稳定 URI。
    #[error(
        "invalid Turn Reference; use exactly `stravia://turns/<turn-id>` with a 28-character lowercase id"
    )]
    InvalidTurnReference,
    /// Tool Execution 引用不是精确的稳定 URI。
    #[error(
        "invalid Tool Execution Reference; use exactly `stravia://tool-executions/<id>` with a 28-character lowercase id"
    )]
    InvalidToolExecutionReference,
    /// 同一种选择器出现多次。
    #[error("`{selector}` may appear at most once in a path")]
    DuplicateSelector { selector: &'static str },
    /// 旧 `?options` 语法；已改为 `:` 选择器。
    #[error(
        "legacy path options are not accepted; options are `:` selectors such as :raw, :download or :1-50; put questions in the question field"
    )]
    LegacyOptionsSyntax,
    /// 旧 `?wait=` 语法；已改为 `wait` 字段。
    #[error("legacy `?wait=` is not accepted; put the wait seconds in the wait field")]
    LegacyWaitParameter,
    /// 行选择语法或数值非法。
    #[error("invalid lines selection: {reason}")]
    InvalidLineSelection { reason: &'static str },
    /// 选择器互斥或上下文限制被违反。
    #[error("conflicting read options: {reason}")]
    ConflictingOptions { reason: &'static str },
}

/// 工具 schema 的暴露范围；由 core 的 `ReadExposureScope` 映射而来。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadSchemaScope {
    pub networking: bool,
    pub media: bool,
    pub internal: bool,
}

impl ReadSchemaScope {
    /// 公网工具：networking 与 media 均可见，`internal` 为 false。
    pub const FULL: Self = Self {
        networking: true,
        media: true,
        internal: false,
    };
}

/// 宽松输入 schema：只有 `path` 必填，供 MCP 使用。
pub fn input_schema(scope: ReadSchemaScope) -> serde_json::Value {
    let properties = schema_properties(scope, false);
    serde_json::json!({
        "type": "object",
        "properties": properties,
        "required": ["path"],
        "additionalProperties": false
    })
}

/// 严格输入 schema：出现的字段全部必填，可选字段类型含 `"null"`。
///
/// 供模型侧工具声明使用；跨协议编码器都能处理这种写法。
pub fn model_input_schema(scope: ReadSchemaScope) -> serde_json::Value {
    let properties = schema_properties(scope, true);
    let required: Vec<&str> = properties.keys().map(String::as_str).collect();
    serde_json::json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false
    })
}

fn schema_properties(
    scope: ReadSchemaScope,
    strict: bool,
) -> serde_json::Map<String, serde_json::Value> {
    let mut properties = serde_json::Map::new();
    properties.insert("path".into(), path_property());
    if scope.networking || scope.media {
        properties.insert("question".into(), question_property(strict));
    }
    if scope.media {
        properties.insert("attachments".into(), attachments_property(strict));
    }
    if scope.networking {
        properties.insert("allowed_domains".into(), allowed_domains_property(strict));
    }
    if !scope.internal {
        properties.insert("wait".into(), wait_property(strict));
    }
    properties
}

fn path_property() -> serde_json::Value {
    serde_json::json!({
        "type": "string",
        "minLength": 1,
        "description": "Resource path; append `:` selectors such as :raw, :download or :1-50."
    })
}

fn question_property(strict: bool) -> serde_json::Value {
    let kind = if strict {
        serde_json::json!(["string", "null"])
    } else {
        serde_json::json!("string")
    };
    serde_json::json!({
        "type": kind,
        "description": "Plain-text question or search text; no encoding."
    })
}

fn attachments_property(strict: bool) -> serde_json::Value {
    let kind = if strict {
        serde_json::json!(["array", "null"])
    } else {
        serde_json::json!("array")
    };
    serde_json::json!({
        "type": kind,
        "items": { "type": "string" },
        "description": "Additional Artifact paths or public URLs for a media follow-up."
    })
}

fn allowed_domains_property(strict: bool) -> serde_json::Value {
    let kind = if strict {
        serde_json::json!(["array", "null"])
    } else {
        serde_json::json!("array")
    };
    serde_json::json!({
        "type": kind,
        "items": { "type": "string" },
        "description": "Domains that final sources must belong to."
    })
}

fn wait_property(strict: bool) -> serde_json::Value {
    let kind = if strict {
        serde_json::json!(["integer", "null"])
    } else {
        serde_json::json!("integer")
    };
    serde_json::json!({
        "type": kind,
        "minimum": 0,
        "description": "Seconds to wait for a tool execution."
    })
}

/// 工具交付分页元数据；不改变研究或媒体完整报告的校验契约。
pub fn pagination_schema() -> serde_json::Value {
    serde_json::json!({
        "type":"object",
        "properties":{
            "read_path":{"type":"string"},
            "next_path":{"type":"string"},
            "representation":{"type":"string","enum":["text","markdown","raw"]},
            "returned_ranges":{"type":"array","items":{
                "type":"object",
                "properties":{
                    "start_line":{"type":"integer","minimum":1},
                    "end_line":{"type":"integer","minimum":1},
                    "start_byte":{"type":"integer","minimum":0},
                    "end_byte":{"type":"integer","minimum":0}
                },
                "required":["start_line","end_line","start_byte","end_byte"],
                "additionalProperties":false
            }},
            "has_more":{"type":"boolean"},
            "source_truncated":{"type":"boolean"},
            "limitations":{"type":"array","items":{"type":"string"}},
            "source_url":{"type":"string"},
            "question_applied":{"type":"boolean"}
        },
        "required":["read_path","representation","returned_ranges","has_more","source_truncated","limitations"],
        "additionalProperties":false
    })
}

/// 解析 [`ReadInput`] 为读取目标。
///
/// 每种 path 形式只接受表中列出的字段；其余字段报错，不做静默忽略。
pub fn parse_read_input(input: &ReadInput) -> Result<ReadTarget, ReadPathError> {
    let path = input.path.as_str();
    if path.trim().is_empty() {
        return Err(ReadPathError::BlankPath);
    }
    if path.starts_with("search://") || path.starts_with("query://") {
        return Err(ReadPathError::LegacySearchScheme);
    }
    if path == SEARCH_PATH {
        return parse_search(input);
    }
    if path.starts_with(SEARCH_PATH) {
        return Err(ReadPathError::UnexpectedSuffix {
            target: "stravia://search",
            hint: "put the search text in question",
        });
    }
    if path.starts_with(TURN_REFERENCE_PREFIX) {
        return parse_continuation(input, path);
    }
    if path.starts_with(TOOL_EXECUTION_PREFIX) {
        return parse_tool_execution(input, path);
    }
    if path.starts_with(ARTIFACT_REFERENCE_PREFIX) {
        return parse_artifact(input, path);
    }
    if path.starts_with(READ_SCHEME_PREFIX) {
        return Err(ReadPathError::InvalidScheme);
    }
    parse_http_resource(input, path)
}

fn parse_search(input: &ReadInput) -> Result<ReadTarget, ReadPathError> {
    let query = required_question(
        input,
        "`stravia://search`",
        "set question to the plain-text search text",
    )?;
    if query.len() > MAX_SEARCH_TEXT_BYTES {
        return Err(ReadPathError::QuestionTooLarge);
    }
    reject_field(
        non_empty(input.attachments.as_ref()),
        "attachments",
        SEARCH_TARGET,
        "attachments only apply to media turn continuations",
    )?;
    reject_field(
        input.wait.as_ref(),
        "wait",
        SEARCH_TARGET,
        "a search returns its report directly",
    )?;
    let allowed_domains = parse_allowed_domains(input.allowed_domains.as_ref())?;
    Ok(ReadTarget::Search(SearchTarget {
        query,
        allowed_domains,
    }))
}

fn parse_continuation(input: &ReadInput, path: &str) -> Result<ReadTarget, ReadPathError> {
    let id = &path[TURN_REFERENCE_PREFIX.len()..];
    if !is_stable_id(id) {
        return Err(ReadPathError::InvalidTurnReference);
    }
    let question = required_question(
        input,
        "a `stravia://turns/<turn-id>` continuation",
        "set question to the follow-up text",
    )?;
    let attachments = parse_attachments(input.attachments.as_ref())?;
    reject_field(
        input.wait.as_ref(),
        "wait",
        CONTINUATION_TARGET,
        "a turn continuation completes inline",
    )?;
    let allowed_domains = parse_allowed_domains(input.allowed_domains.as_ref())?;
    Ok(ReadTarget::Continuation(ContinuationTarget {
        turn_path: path.to_owned(),
        question,
        attachments,
        allowed_domains,
    }))
}

fn parse_tool_execution(input: &ReadInput, path: &str) -> Result<ReadTarget, ReadPathError> {
    let rest = &path[TOOL_EXECUTION_PREFIX.len()..];
    if let Some((_, parameters)) = rest.split_once('?') {
        if parameters.starts_with("wait") {
            return Err(ReadPathError::LegacyWaitParameter);
        }
    }
    let (id, suffix) = rest
        .split_once('?')
        .map_or((rest, None), |(id, parameters)| (id, Some(parameters)));
    if suffix.is_some() || !is_stable_id(id) {
        return Err(ReadPathError::InvalidToolExecutionReference);
    }
    reject_field(
        input.question.as_ref(),
        "question",
        TOOL_EXECUTION_TARGET,
        "read it with wait for status and the original result",
    )?;
    reject_field(
        non_empty(input.attachments.as_ref()),
        "attachments",
        TOOL_EXECUTION_TARGET,
        "attachments only apply to media turn continuations",
    )?;
    reject_field(
        input.allowed_domains.as_ref(),
        "allowed_domains",
        TOOL_EXECUTION_TARGET,
        "allowed_domains only applies to search and turn continuations",
    )?;
    Ok(ReadTarget::ToolExecution(ToolExecutionPath {
        id: id.to_owned(),
        wait_seconds: input.wait.unwrap_or(0),
    }))
}

fn parse_artifact(input: &ReadInput, path: &str) -> Result<ReadTarget, ReadPathError> {
    if path.contains('?') || path.contains('#') {
        return Err(ReadPathError::LegacyOptionsSyntax);
    }
    let floor = ARTIFACT_REFERENCE_PREFIX.len() + ARTIFACT_DIGEST_LEN;
    let (url, selectors) = split_selectors(path, Some(floor))?;
    if !is_artifact_reference(&url) {
        return Err(ReadPathError::InvalidArtifactReference {
            reason: "identity must be exactly 55 lowercase ASCII letters after `stravia://artifacts/`; append options as `:` selectors such as :raw, :download or :1-50",
        });
    }
    reject_resource_fields(input)?;
    let options = finish_resource_options(input, selectors, true)?;
    Ok(ReadTarget::Resource(ResourcePath { url, options }))
}

fn parse_http_resource(input: &ReadInput, path: &str) -> Result<ReadTarget, ReadPathError> {
    let (url, selectors) = split_selectors(path, http_authority_floor(path))?;
    if url
        .split_once('#')
        .is_some_and(|(_, fragment)| fragment.starts_with(LEGACY_OPTIONS_FRAGMENT_PREFIX))
    {
        return Err(ReadPathError::LegacyOptionsSyntax);
    }
    validate_http_url(&url)?;
    reject_resource_fields(input)?;
    let options = finish_resource_options(input, selectors, false)?;
    Ok(ReadTarget::Resource(ResourcePath { url, options }))
}

/// 资源 path 不接受的字段；`attachments: []` 视为缺省，不会触发。
fn reject_resource_fields(input: &ReadInput) -> Result<(), ReadPathError> {
    reject_field(
        non_empty(input.attachments.as_ref()),
        "attachments",
        RESOURCE_TARGET,
        "attachments only apply to media turn continuations",
    )?;
    reject_field(
        input.allowed_domains.as_ref(),
        "allowed_domains",
        RESOURCE_TARGET,
        "allowed_domains only applies to search and turn continuations",
    )?;
    reject_field(
        input.wait.as_ref(),
        "wait",
        RESOURCE_TARGET,
        "use a `stravia://tool-executions/<id>` path with wait for status",
    )
}

fn finish_resource_options(
    input: &ReadInput,
    selectors: SelectorSet,
    is_artifact: bool,
) -> Result<ReadOptions, ReadPathError> {
    if selectors.cursor.is_some() && !is_artifact {
        return Err(ReadPathError::ConflictingOptions {
            reason: "cursor is only valid on an Artifact Reference",
        });
    }
    let question = match input.question.as_deref() {
        Some(text) if text.trim().is_empty() => return Err(ReadPathError::BlankQuestion),
        Some(text) => Some(text.to_owned()),
        None => None,
    };
    if selectors.raw && question.is_some() {
        return Err(ReadPathError::ConflictingOptions {
            reason: "raw cannot be combined with question",
        });
    }
    if selectors.download
        && (question.is_some()
            || selectors.raw
            || selectors.lines.is_some()
            || selectors.cursor.is_some())
    {
        return Err(ReadPathError::ConflictingOptions {
            reason: "download cannot be combined with other options",
        });
    }
    if selectors.cursor.is_some()
        && (question.is_some() || selectors.raw || selectors.lines.is_some())
    {
        return Err(ReadPathError::ConflictingOptions {
            reason: "cursor cannot be combined with other options",
        });
    }
    Ok(ReadOptions {
        question,
        raw: selectors.raw,
        download: selectors.download,
        lines: selectors.lines,
        cursor: selectors.cursor,
    })
}

const SEARCH_TARGET: &str = "`stravia://search`";
const CONTINUATION_TARGET: &str = "a `stravia://turns/<turn-id>` continuation";
const TOOL_EXECUTION_TARGET: &str = "a `stravia://tool-executions/<id>` path";
const RESOURCE_TARGET: &str = "a resource path";

fn required_question(
    input: &ReadInput,
    target: &'static str,
    hint: &'static str,
) -> Result<String, ReadPathError> {
    match input.question.as_deref() {
        None => Err(ReadPathError::MissingField {
            field: "question",
            target,
            hint,
        }),
        Some(text) if text.trim().is_empty() => Err(ReadPathError::BlankQuestion),
        Some(text) => Ok(text.to_owned()),
    }
}

/// 字段存在且有效时报错；`attachments: []` 先归一为缺省。
fn reject_field<T>(
    field_value: Option<&T>,
    field: &'static str,
    target: &'static str,
    hint: &'static str,
) -> Result<(), ReadPathError> {
    match field_value {
        Some(_) => Err(ReadPathError::FieldNotAllowed {
            field,
            target,
            hint,
        }),
        None => Ok(()),
    }
}

fn non_empty(values: Option<&Vec<String>>) -> Option<&Vec<String>> {
    values.filter(|values| !values.is_empty())
}

fn parse_allowed_domains(
    domains: Option<&Vec<String>>,
) -> Result<Option<Vec<String>>, ReadPathError> {
    let Some(domains) = domains else {
        return Ok(None);
    };
    if domains.is_empty() {
        return Err(ReadPathError::EmptyAllowedDomains);
    }
    if domains.len() > MAX_ALLOWED_DOMAINS {
        return Err(ReadPathError::TooManyAllowedDomains {
            count: domains.len(),
        });
    }
    let normalized =
        normalize_domains(domains.clone()).map_err(|_| ReadPathError::InvalidAllowedDomains)?;
    if normalized.is_empty() {
        return Err(ReadPathError::InvalidAllowedDomains);
    }
    Ok(Some(normalized))
}

fn parse_attachments(attachments: Option<&Vec<String>>) -> Result<Vec<String>, ReadPathError> {
    let Some(attachments) = attachments else {
        return Ok(Vec::new());
    };
    let mut seen = std::collections::HashSet::with_capacity(attachments.len());
    for attachment in attachments {
        if !is_attachment(attachment) {
            return Err(ReadPathError::InvalidAttachment);
        }
        if !seen.insert(attachment.as_str()) {
            return Err(ReadPathError::DuplicateAttachment);
        }
    }
    Ok(attachments.clone())
}

fn is_attachment(attachment: &str) -> bool {
    if attachment.starts_with(ARTIFACT_REFERENCE_PREFIX) {
        return is_artifact_reference(attachment);
    }
    let Ok(parsed) = url::Url::parse(attachment) else {
        return false;
    };
    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        return false;
    }
    if parsed.host_str().is_none_or(|host| host.is_empty()) {
        return false;
    }
    if parsed.host_str() == Some("stravia") && parsed.path().starts_with("/artifact/") {
        return false;
    }
    !parsed
        .fragment()
        .is_some_and(|fragment| fragment.starts_with(LEGACY_OPTIONS_FRAGMENT_PREFIX))
}

fn is_stable_id(id: &str) -> bool {
    id.len() == TURN_ID_LEN && id.bytes().all(|byte| byte.is_ascii_lowercase())
}

/// 校验裸 Artifact 身份：完整 SHA-256 的 55 位小写 base26 编码。
fn is_artifact_reference(url: &str) -> bool {
    url.strip_prefix(ARTIFACT_REFERENCE_PREFIX)
        .is_some_and(|identity| {
            identity.len() == ARTIFACT_DIGEST_LEN
                && identity.bytes().all(|byte| byte.is_ascii_lowercase())
        })
}

fn validate_http_url(url: &str) -> Result<(), ReadPathError> {
    let parsed = url::Url::parse(url).map_err(|_| ReadPathError::InvalidUrl)?;
    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        return Err(ReadPathError::InvalidScheme);
    }
    if parsed.host_str().is_none_or(|host| host.is_empty())
        || (parsed.host_str() == Some("stravia") && parsed.path().starts_with("/artifact/"))
    {
        return Err(ReadPathError::InvalidScheme);
    }
    Ok(())
}

/// HTTP(S) 选择器剥离底线：`scheme://` 之后第一个 `/`、`?` 或 `#` 的字节位置。
///
/// 没有该字符时返回 `None`，整段按 URL 处理，端口里的 `:` 不会被剥离。
fn http_authority_floor(path: &str) -> Option<usize> {
    let (scheme, rest) = path.split_once("://")?;
    if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
        return None;
    }
    let base = scheme.len() + "://".len();
    rest.find(['/', '?', '#']).map(|index| base + index)
}

#[derive(Default)]
struct SelectorSet {
    raw: bool,
    download: bool,
    cursor: Option<String>,
    lines: Option<Vec<LineSelection>>,
}

enum Selector {
    Raw,
    Download,
    Cursor(String),
    Lines(Vec<LineSelection>),
}

/// 从 path 末尾自右向左剥离 `:` 选择器。
///
/// `floor` 是允许出现选择器冒号的最小字节位置；`None` 表示不剥离。token 严格符合
/// 语法才剥离，否则停止，剩余部分按字面返回。
fn split_selectors(
    path: &str,
    floor: Option<usize>,
) -> Result<(String, SelectorSet), ReadPathError> {
    let Some(floor) = floor else {
        return Ok((path.to_owned(), SelectorSet::default()));
    };
    let mut end = path.len();
    let mut selectors = SelectorSet::default();
    while let Some(colon) = path[..end].rfind(':') {
        if colon < floor {
            break;
        }
        let Some(selector) = classify_selector(&path[colon + 1..end]) else {
            break;
        };
        match selector {
            Selector::Raw => {
                if selectors.raw {
                    return Err(ReadPathError::DuplicateSelector { selector: "raw" });
                }
                selectors.raw = true;
            }
            Selector::Download => {
                if selectors.download {
                    return Err(ReadPathError::DuplicateSelector {
                        selector: "download",
                    });
                }
                selectors.download = true;
            }
            Selector::Cursor(cursor) => {
                if selectors.cursor.is_some() {
                    return Err(ReadPathError::DuplicateSelector { selector: "cursor" });
                }
                selectors.cursor = Some(cursor);
            }
            Selector::Lines(lines) => {
                if selectors.lines.is_some() {
                    return Err(ReadPathError::InvalidLineSelection {
                        reason: "combine line ranges with commas, e.g. :1-50,90-100",
                    });
                }
                selectors.lines = Some(lines);
            }
        }
        end = colon;
    }
    Ok((path[..end].to_owned(), selectors))
}

fn classify_selector(token: &str) -> Option<Selector> {
    match token {
        "raw" => return Some(Selector::Raw),
        "download" => return Some(Selector::Download),
        _ => {}
    }
    if let Some(cursor) = token.strip_prefix("cursor=") {
        if !cursor.is_empty()
            && cursor
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
        {
            return Some(Selector::Cursor(cursor.to_owned()));
        }
        return None;
    }
    if token.is_empty() {
        return None;
    }
    parse_line_selections(token).ok().map(Selector::Lines)
}

/// 解析行范围：`N`、`N-M`、`N+K`、`-K`，逗号分隔多段。
///
/// 全部 1-based 正数、checked 运算、最多 [`MAX_LINE_SELECTIONS`] 段；按源顺序排序并
/// 合并相邻/重叠区间，多个 `Last` 合并为最大计数。
fn parse_line_selections(text: &str) -> Result<Vec<LineSelection>, ReadPathError> {
    let segments = text.split(',');
    let count = segments.clone().take(MAX_LINE_SELECTIONS + 1).count();
    if count > MAX_LINE_SELECTIONS {
        return Err(ReadPathError::InvalidLineSelection {
            reason: "at most 16 segments are allowed",
        });
    }
    let mut absolutes: Vec<(u64, Option<u64>)> = Vec::with_capacity(count);
    let mut last: Option<u64> = None;
    for segment in segments {
        if segment.is_empty() {
            return Err(ReadPathError::InvalidLineSelection {
                reason: "segments must not be empty",
            });
        }
        if let Some(count) = segment.strip_prefix('-') {
            let count = parse_line_number(count)?;
            last = Some(match last {
                Some(previous) => previous.max(count),
                None => count,
            });
            continue;
        }
        if let Some((start_raw, count_raw)) = segment.split_once('+') {
            let start = parse_line_number(start_raw)?;
            let count = parse_line_number(count_raw)?;
            let end = start
                .checked_add(count - 1)
                .ok_or(ReadPathError::InvalidLineSelection {
                    reason: "line range arithmetic overflows",
                })?;
            absolutes.push((start, Some(end)));
            continue;
        }
        if let Some((start_raw, end_raw)) = segment.split_once('-') {
            let start = parse_line_number(start_raw)?;
            let end = parse_line_number(end_raw)?;
            if end < start {
                return Err(ReadPathError::InvalidLineSelection {
                    reason: "range start must not exceed end",
                });
            }
            absolutes.push((start, Some(end)));
            continue;
        }
        let start = parse_line_number(segment)?;
        absolutes.push((start, None));
    }
    absolutes.sort_unstable();
    let mut merged: Vec<(u64, Option<u64>)> = Vec::with_capacity(absolutes.len());
    for (start, end) in absolutes {
        if let Some((_, last_end)) = merged.last_mut() {
            let touches = match *last_end {
                None => true,
                Some(previous_end) => start <= previous_end.saturating_add(1),
            };
            if touches {
                *last_end = match (*last_end, end) {
                    (Some(previous_end), Some(end)) => Some(previous_end.max(end)),
                    _ => None,
                };
                continue;
            }
        }
        merged.push((start, end));
    }
    let mut selections: Vec<LineSelection> = merged
        .into_iter()
        .map(|(start, end)| match end {
            None => LineSelection::From(start),
            Some(end) => LineSelection::Inclusive { start, end },
        })
        .collect();
    if let Some(count) = last {
        selections.push(LineSelection::Last(count));
    }
    Ok(selections)
}

fn parse_line_number(text: &str) -> Result<u64, ReadPathError> {
    let invalid = || ReadPathError::InvalidLineSelection {
        reason: "line numbers and counts must be 1-based positive integers",
    };
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(invalid());
    }
    let number: u64 = text.parse().map_err(|_| invalid())?;
    if number == 0 {
        return Err(invalid());
    }
    Ok(number)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn read_input(path: &str) -> ReadInput {
        ReadInput {
            path: path.to_owned(),
            question: None,
            attachments: None,
            allowed_domains: None,
            wait: None,
        }
    }

    fn with_question(path: &str, question: &str) -> ReadInput {
        ReadInput {
            question: Some(question.to_owned()),
            ..read_input(path)
        }
    }

    fn parse(path: &str) -> Result<ReadTarget, ReadPathError> {
        parse_read_input(&read_input(path))
    }

    fn artifact_path() -> String {
        format!(
            "{ARTIFACT_REFERENCE_PREFIX}{}",
            "a".repeat(ARTIFACT_DIGEST_LEN)
        )
    }

    fn turn_path() -> String {
        format!("{TURN_REFERENCE_PREFIX}{}", "t".repeat(TURN_ID_LEN))
    }

    fn tool_execution_path() -> String {
        format!("{TOOL_EXECUTION_PREFIX}{}", "e".repeat(TURN_ID_LEN))
    }

    fn resource(url: &str, options: ReadOptions) -> ReadTarget {
        ReadTarget::Resource(ResourcePath {
            url: url.to_owned(),
            options,
        })
    }

    #[test]
    fn search_requires_question_and_normalizes_domains() {
        assert_eq!(
            parse_read_input(&with_question(SEARCH_PATH, "rust async")).unwrap(),
            ReadTarget::Search(SearchTarget {
                query: "rust async".into(),
                allowed_domains: None,
            })
        );
        assert!(matches!(
            parse(SEARCH_PATH).unwrap_err(),
            ReadPathError::MissingField {
                field: "question",
                ..
            }
        ));
        assert_eq!(
            parse_read_input(&with_question(SEARCH_PATH, " \t ")).unwrap_err(),
            ReadPathError::BlankQuestion
        );
        assert!(parse_read_input(&with_question(
            SEARCH_PATH,
            &"a".repeat(MAX_SEARCH_TEXT_BYTES)
        ))
        .is_ok());
        assert_eq!(
            parse_read_input(&with_question(
                SEARCH_PATH,
                &"a".repeat(MAX_SEARCH_TEXT_BYTES + 1)
            ))
            .unwrap_err(),
            ReadPathError::QuestionTooLarge
        );

        let mut domains = with_question(SEARCH_PATH, "q");
        domains.allowed_domains = Some(vec![
            "Docs.RS".into(),
            "docs.rs.".into(),
            "example.com".into(),
        ]);
        assert_eq!(
            parse_read_input(&domains).unwrap(),
            ReadTarget::Search(SearchTarget {
                query: "q".into(),
                allowed_domains: Some(vec!["docs.rs".into(), "example.com".into()]),
            })
        );
        domains.allowed_domains = Some(Vec::new());
        assert_eq!(
            parse_read_input(&domains).unwrap_err(),
            ReadPathError::EmptyAllowedDomains
        );
        domains.allowed_domains = Some(vec!["docs.rs/path".into()]);
        assert_eq!(
            parse_read_input(&domains).unwrap_err(),
            ReadPathError::InvalidAllowedDomains
        );
        domains.allowed_domains = Some(vec![" ".into()]);
        assert_eq!(
            parse_read_input(&domains).unwrap_err(),
            ReadPathError::InvalidAllowedDomains
        );
        domains.allowed_domains = Some(
            (0..=MAX_ALLOWED_DOMAINS)
                .map(|index| format!("docs{index}.rs"))
                .collect(),
        );
        assert!(matches!(
            parse_read_input(&domains).unwrap_err(),
            ReadPathError::TooManyAllowedDomains { .. }
        ));
    }

    #[test]
    fn search_path_is_exact_and_empty_attachments_equal_absent() {
        for suffix in [":raw", "?q=x", "/extra", "#frag"] {
            assert!(
                matches!(
                    parse_read_input(&with_question(&format!("{SEARCH_PATH}{suffix}"), "q"))
                        .unwrap_err(),
                    ReadPathError::UnexpectedSuffix { .. }
                ),
                "{suffix}"
            );
        }
        let mut input = with_question(SEARCH_PATH, "q");
        input.attachments = Some(Vec::new());
        assert!(parse_read_input(&input).is_ok());
    }

    #[test]
    fn continuation_requires_question_and_validates_attachments() {
        let path = turn_path();
        let mut follow_up = with_question(&path, "what changed?");
        follow_up.attachments = Some(vec![artifact_path(), "https://example.com/a.png".into()]);
        follow_up.allowed_domains = Some(vec!["Docs.RS".into()]);
        assert_eq!(
            parse_read_input(&follow_up).unwrap(),
            ReadTarget::Continuation(ContinuationTarget {
                turn_path: path.clone(),
                question: "what changed?".into(),
                attachments: vec![artifact_path(), "https://example.com/a.png".into()],
                allowed_domains: Some(vec!["docs.rs".into()]),
            })
        );

        let mut empty = with_question(&path, "q");
        empty.attachments = Some(Vec::new());
        assert_eq!(
            parse_read_input(&empty).unwrap(),
            ReadTarget::Continuation(ContinuationTarget {
                turn_path: path.clone(),
                question: "q".into(),
                attachments: Vec::new(),
                allowed_domains: None,
            })
        );
        empty.allowed_domains = Some(Vec::new());
        assert_eq!(
            parse_read_input(&empty).unwrap_err(),
            ReadPathError::EmptyAllowedDomains
        );

        assert!(matches!(
            parse(&path).unwrap_err(),
            ReadPathError::MissingField {
                field: "question",
                ..
            }
        ));
        assert_eq!(
            parse_read_input(&with_question(&path, "  ")).unwrap_err(),
            ReadPathError::BlankQuestion
        );

        let digest = "a".repeat(ARTIFACT_DIGEST_LEN);
        for attachment in [
            "sa:abc".to_owned(),
            "relative/path".to_owned(),
            "ftp://example.com/a.png".to_owned(),
            format!(
                "{ARTIFACT_REFERENCE_PREFIX}{}",
                "A".repeat(ARTIFACT_DIGEST_LEN)
            ),
            format!("{ARTIFACT_REFERENCE_PREFIX}{digest}:download"),
            format!("{ARTIFACT_REFERENCE_PREFIX}{digest}?question=q"),
            format!("https://stravia/artifact/{digest}"),
            "https://example.com/a#stravia?raw=1".to_owned(),
            turn_path(),
        ] {
            let mut input = with_question(&path, "q");
            input.attachments = Some(vec![attachment.clone()]);
            assert_eq!(
                parse_read_input(&input).unwrap_err(),
                ReadPathError::InvalidAttachment,
                "{attachment}"
            );
        }

        // 附件按字面值处理：URL 末尾的选择器形态不被剥离，也不算无效。
        let mut literal = with_question(&path, "q");
        literal.attachments = Some(vec!["https://example.com/a:raw".into()]);
        assert!(parse_read_input(&literal).is_ok());

        let mut duplicate = with_question(&path, "q");
        duplicate.attachments = Some(vec![artifact_path(), artifact_path()]);
        assert_eq!(
            parse_read_input(&duplicate).unwrap_err(),
            ReadPathError::DuplicateAttachment
        );
    }

    #[test]
    fn continuation_path_is_exact_and_rejects_wait() {
        let id = "a".repeat(TURN_ID_LEN);
        for path in [
            format!("{TURN_REFERENCE_PREFIX}{}", "a".repeat(TURN_ID_LEN - 1)),
            format!("{TURN_REFERENCE_PREFIX}{}", "A".repeat(TURN_ID_LEN)),
            format!("{TURN_REFERENCE_PREFIX}{id}:raw"),
            format!("{TURN_REFERENCE_PREFIX}{id}?x=1"),
            format!("{TURN_REFERENCE_PREFIX}{id}#frag"),
        ] {
            assert_eq!(
                parse_read_input(&with_question(&path, "q")).unwrap_err(),
                ReadPathError::InvalidTurnReference,
                "{path}"
            );
        }
        let mut wait = with_question(&turn_path(), "q");
        wait.wait = Some(1);
        assert!(matches!(
            parse_read_input(&wait).unwrap_err(),
            ReadPathError::FieldNotAllowed { field: "wait", .. }
        ));
    }

    #[test]
    fn tool_execution_takes_wait_from_the_field() {
        let path = tool_execution_path();
        let id = "e".repeat(TURN_ID_LEN);
        assert_eq!(
            parse(&path).unwrap(),
            ReadTarget::ToolExecution(ToolExecutionPath {
                id: id.clone(),
                wait_seconds: 0,
            })
        );
        for wait in [0, 61, u64::MAX] {
            let mut input = read_input(&path);
            input.wait = Some(wait);
            assert_eq!(
                parse_read_input(&input).unwrap(),
                ReadTarget::ToolExecution(ToolExecutionPath {
                    id: id.clone(),
                    wait_seconds: wait,
                })
            );
        }
        let mut empty_attachments = read_input(&path);
        empty_attachments.attachments = Some(Vec::new());
        assert!(parse_read_input(&empty_attachments).is_ok());

        for suffix in ["?wait=61", "?wait=", "?wait"] {
            assert_eq!(
                parse(&format!("{path}{suffix}")).unwrap_err(),
                ReadPathError::LegacyWaitParameter,
                "{suffix}"
            );
        }
        for suffix in ["#frag", ":download", "?other=1", "/extra"] {
            assert_eq!(
                parse(&format!("{path}{suffix}")).unwrap_err(),
                ReadPathError::InvalidToolExecutionReference,
                "{suffix}"
            );
        }
        for id in [
            "a",
            "AAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaa1",
        ] {
            assert_eq!(
                parse(&format!("{TOOL_EXECUTION_PREFIX}{id}")).unwrap_err(),
                ReadPathError::InvalidToolExecutionReference,
                "{id}"
            );
        }
    }

    #[test]
    fn field_matrix_rejects_fields_outside_each_path_kind() {
        type Setter = fn(&mut ReadInput);
        let attachments: Setter = |input| input.attachments = Some(vec![artifact_path()]);
        let domains: Setter = |input| input.allowed_domains = Some(vec!["docs.rs".into()]);
        let wait: Setter = |input| input.wait = Some(1);
        let question: Setter = |input| input.question = Some("q".into());
        let http = "https://example.com/a".to_owned();
        let cases: Vec<(String, bool, Setter, &str)> = vec![
            (SEARCH_PATH.to_owned(), true, attachments, "attachments"),
            (SEARCH_PATH.to_owned(), true, wait, "wait"),
            (turn_path(), true, wait, "wait"),
            (tool_execution_path(), false, question, "question"),
            (tool_execution_path(), false, attachments, "attachments"),
            (tool_execution_path(), false, domains, "allowed_domains"),
            (artifact_path(), false, attachments, "attachments"),
            (artifact_path(), false, domains, "allowed_domains"),
            (artifact_path(), false, wait, "wait"),
            (http.clone(), false, attachments, "attachments"),
            (http.clone(), false, domains, "allowed_domains"),
            (http, false, wait, "wait"),
        ];
        for (path, needs_question, set, field) in cases {
            let mut input = read_input(&path);
            if needs_question {
                input.question = Some("q".into());
            }
            set(&mut input);
            assert!(
                matches!(
                    parse_read_input(&input).unwrap_err(),
                    ReadPathError::FieldNotAllowed { field: actual, .. } if actual == field
                ),
                "{path} {field}"
            );
        }
    }

    #[test]
    fn artifact_selectors_strip_in_either_order() {
        let base = artifact_path();
        let lines = Some(vec![LineSelection::Inclusive { start: 1, end: 20 }]);
        let cases = [
            (
                format!("{base}:raw"),
                ReadOptions {
                    raw: true,
                    ..ReadOptions::default()
                },
            ),
            (
                format!("{base}:download"),
                ReadOptions {
                    download: true,
                    ..ReadOptions::default()
                },
            ),
            (
                format!("{base}:cursor=Abc_123-x"),
                ReadOptions {
                    cursor: Some("Abc_123-x".into()),
                    ..ReadOptions::default()
                },
            ),
            (
                format!("{base}:1-20"),
                ReadOptions {
                    lines: lines.clone(),
                    ..ReadOptions::default()
                },
            ),
            (
                format!("{base}:raw:1-20"),
                ReadOptions {
                    raw: true,
                    lines: lines.clone(),
                    ..ReadOptions::default()
                },
            ),
            (
                format!("{base}:1-20:raw"),
                ReadOptions {
                    raw: true,
                    lines,
                    ..ReadOptions::default()
                },
            ),
            (
                format!("{base}:1-50,90-100"),
                ReadOptions {
                    lines: Some(vec![
                        LineSelection::Inclusive { start: 1, end: 50 },
                        LineSelection::Inclusive {
                            start: 90,
                            end: 100,
                        },
                    ]),
                    ..ReadOptions::default()
                },
            ),
        ];
        for (path, options) in cases {
            assert_eq!(parse(&path).unwrap(), resource(&base, options), "{path}");
        }
        assert_eq!(
            parse(&base).unwrap(),
            resource(&base, ReadOptions::default())
        );

        let mut question = read_input(&format!("{base}:1-5"));
        question.question = Some("what is this?".into());
        assert_eq!(
            parse_read_input(&question).unwrap(),
            resource(
                &base,
                ReadOptions {
                    question: Some("what is this?".into()),
                    lines: Some(vec![LineSelection::Inclusive { start: 1, end: 5 }]),
                    ..ReadOptions::default()
                }
            )
        );
    }

    #[test]
    fn artifact_leftovers_and_legacy_forms_are_rejected() {
        let base = artifact_path();
        for path in [
            format!("{base}x:raw"),
            format!("{base}:cursor=bad!"),
            format!("{base}:cursor="),
            format!("{base}:cursor=a=b"),
            format!("{base}:question=x"),
            format!("{base}:0"),
            format!("{ARTIFACT_REFERENCE_PREFIX}abc:raw"),
            format!("{base}/extra"),
        ] {
            assert!(
                matches!(
                    parse(&path).unwrap_err(),
                    ReadPathError::InvalidArtifactReference { .. }
                ),
                "{path}"
            );
        }
        let message = parse(&format!("{base}x")).unwrap_err().to_string();
        assert!(message.contains(":download"), "{message}");

        for suffix in ["?question=%20", "?raw=1", "#section", ":1-2?x=y"] {
            assert_eq!(
                parse(&format!("{base}{suffix}")).unwrap_err(),
                ReadPathError::LegacyOptionsSyntax,
                "{suffix}"
            );
        }
        let message = parse(&format!("{base}?question=x"))
            .unwrap_err()
            .to_string();
        assert!(message.contains(":raw"), "{message}");
        assert!(message.contains(":download"), "{message}");
        assert!(message.contains("question field"), "{message}");
    }

    #[test]
    fn http_selectors_respect_the_authority_floor_and_keep_source_text() {
        let source = "https://example.com/a?sig=a%2Bb&x=%3A";
        assert_eq!(
            parse(&format!("{source}:raw")).unwrap(),
            resource(
                source,
                ReadOptions {
                    raw: true,
                    ..ReadOptions::default()
                }
            )
        );
        // 端口属于 authority，没有 path 时整段按 URL 处理。
        assert_eq!(
            parse("https://host:8080").unwrap(),
            resource("https://host:8080", ReadOptions::default())
        );
        assert_eq!(
            parse("https://host:8080/:raw").unwrap(),
            resource(
                "https://host:8080/",
                ReadOptions {
                    raw: true,
                    ..ReadOptions::default()
                }
            )
        );
        assert_eq!(
            parse("https://host:8080:raw").unwrap_err(),
            ReadPathError::InvalidUrl
        );
        // 普通 fragment 保留源语义；末尾选择器仍可剥离。
        assert_eq!(
            parse("https://example.com/a#section").unwrap(),
            resource("https://example.com/a#section", ReadOptions::default())
        );
        assert_eq!(
            parse("https://example.com/a#section:1-5").unwrap(),
            resource(
                "https://example.com/a#section",
                ReadOptions {
                    lines: Some(vec![LineSelection::Inclusive { start: 1, end: 5 }]),
                    ..ReadOptions::default()
                }
            )
        );
        // 不符合语法的 token 停止剥离，剩余部分按字面作为地址。
        assert_eq!(
            parse("https://example.com/a:b:raw").unwrap(),
            resource(
                "https://example.com/a:b",
                ReadOptions {
                    raw: true,
                    ..ReadOptions::default()
                }
            )
        );
        assert_eq!(
            parse("https://example.com/:0").unwrap(),
            resource("https://example.com/:0", ReadOptions::default())
        );
        // `%3A` 是字面值，原样保留。
        assert_eq!(
            parse("https://example.com/a%3A:download").unwrap(),
            resource(
                "https://example.com/a%3A",
                ReadOptions {
                    download: true,
                    ..ReadOptions::default()
                }
            )
        );
        assert_eq!(
            parse("https://example.com/a%3Araw").unwrap(),
            resource("https://example.com/a%3Araw", ReadOptions::default())
        );
        // 大小写不敏感的 scheme 同样适用底线规则。
        assert_eq!(
            parse("HTTPS://host:8080").unwrap(),
            resource("HTTPS://host:8080", ReadOptions::default())
        );
    }

    #[test]
    fn http_rejects_legacy_fragments_and_foreign_forms() {
        for path in [
            "https://example.com#stravia?raw=1",
            "https://example.com/a?x=1#stravia?question=q",
            "https://example.com/a#stravia?lines=1-2:download",
        ] {
            assert_eq!(
                parse(path).unwrap_err(),
                ReadPathError::LegacyOptionsSyntax,
                "{path}"
            );
        }
        let digest = "a".repeat(ARTIFACT_DIGEST_LEN);
        for path in [
            "https://stravia/artifact/abc".to_owned(),
            format!("sa:{digest}"),
            "file:///etc/passwd".to_owned(),
            "ftp://example.com/a".to_owned(),
            "stravia://unknown".to_owned(),
            "stravia://turns".to_owned(),
        ] {
            assert_eq!(
                parse(&path).unwrap_err(),
                ReadPathError::InvalidScheme,
                "{path}"
            );
        }
        assert_eq!(parse("/local/file").unwrap_err(), ReadPathError::InvalidUrl);
        for path in ["", "  ", "\t"] {
            assert_eq!(parse(path).unwrap_err(), ReadPathError::BlankPath);
        }
    }

    #[test]
    fn selector_and_question_combinations_are_enforced() {
        let base = artifact_path();
        let http = "https://example.com/a";
        let conflict = |path: String, question: Option<&str>| {
            let mut input = read_input(&path);
            input.question = question.map(str::to_owned);
            match parse_read_input(&input) {
                Err(ReadPathError::ConflictingOptions { reason }) => reason,
                other => panic!("expected conflict for {path}: {other:?}"),
            }
        };
        assert!(conflict(format!("{base}:raw"), Some("q")).contains("question"));
        for path in [
            format!("{base}:download:raw"),
            format!("{base}:download:1-2"),
            format!("{base}:download:cursor=a"),
        ] {
            assert!(conflict(path, None).starts_with("download"));
        }
        assert!(conflict(format!("{base}:download"), Some("q")).starts_with("download"));
        for path in [
            format!("{base}:cursor=a:raw"),
            format!("{base}:cursor=a:1-2"),
        ] {
            assert!(conflict(path, None).starts_with("cursor"));
        }
        assert!(conflict(format!("{base}:cursor=a"), Some("q")).starts_with("cursor"));
        assert!(conflict(format!("{http}:cursor=a"), None).contains("Artifact"));

        // 行范围与 question 可以共存；raw 与行范围可以共存。
        let mut lines = read_input(&format!("{http}:1-5"));
        lines.question = Some("q".into());
        assert!(parse_read_input(&lines).is_ok());
        assert!(parse(&format!("{http}:raw:1-5")).is_ok());

        for path in [&base, &http.to_owned()] {
            assert_eq!(
                parse_read_input(&with_question(path, " ")).unwrap_err(),
                ReadPathError::BlankQuestion
            );
        }
    }

    #[test]
    fn repeated_selectors_are_rejected() {
        let base = artifact_path();
        assert_eq!(
            parse(&format!("{base}:raw:raw")).unwrap_err(),
            ReadPathError::DuplicateSelector { selector: "raw" }
        );
        assert_eq!(
            parse(&format!("{base}:download:download")).unwrap_err(),
            ReadPathError::DuplicateSelector {
                selector: "download"
            }
        );
        assert_eq!(
            parse(&format!("{base}:cursor=a:cursor=b")).unwrap_err(),
            ReadPathError::DuplicateSelector { selector: "cursor" }
        );
        let error = parse(&format!("{base}:1-2:3-4")).unwrap_err();
        assert!(matches!(error, ReadPathError::InvalidLineSelection { .. }));
        assert!(error
            .to_string()
            .contains("combine line ranges with commas"));
        assert!(matches!(
            parse("https://example.com/a:1-2:3-4").unwrap_err(),
            ReadPathError::InvalidLineSelection { .. }
        ));
    }

    #[test]
    fn line_selections_merge_and_check_bounds() {
        assert_eq!(
            parse_line_selections("5+3,1-2,2-5,-2").unwrap(),
            [
                LineSelection::Inclusive { start: 1, end: 7 },
                LineSelection::Last(2)
            ]
        );
        assert_eq!(
            parse_line_selections("10,3-4,-5,-9").unwrap(),
            [
                LineSelection::Inclusive { start: 3, end: 4 },
                LineSelection::From(10),
                LineSelection::Last(9)
            ]
        );
        for selection in [
            "",
            "0",
            "-0",
            "3-2",
            "1+0",
            "1-2-3",
            "18446744073709551615+2",
            "18446744073709551616",
            "1,,2",
            "+1",
            "1 2",
        ] {
            assert!(
                matches!(
                    parse_line_selections(selection),
                    Err(ReadPathError::InvalidLineSelection { .. })
                ),
                "{selection:?}"
            );
        }
        let at_limit = vec!["1-2"; MAX_LINE_SELECTIONS].join(",");
        assert!(parse_line_selections(&at_limit).is_ok());
        let over_limit = vec!["1-2"; MAX_LINE_SELECTIONS + 1].join(",");
        assert!(matches!(
            parse_line_selections(&over_limit),
            Err(ReadPathError::InvalidLineSelection { .. })
        ));
    }

    #[test]
    fn legacy_syntax_errors_name_the_correct_form() {
        for path in ["search://rust", "query://rust"] {
            let error = parse(path).unwrap_err();
            assert_eq!(error, ReadPathError::LegacySearchScheme);
            let message = error.to_string();
            assert!(message.contains("stravia://search"), "{message}");
            assert!(message.contains("question"), "{message}");
        }
        let message = parse(&format!("{}?wait=1", tool_execution_path()))
            .unwrap_err()
            .to_string();
        assert!(message.contains("wait field"), "{message}");
        let message = parse("https://example.com#stravia?raw=1")
            .unwrap_err()
            .to_string();
        assert!(
            message.contains(":raw") && message.contains("question field"),
            "{message}"
        );
    }

    #[test]
    fn errors_do_not_echo_urls_or_field_values() {
        let secret = "secret";
        let conflicting = with_question("https://example.com/a:raw", secret);
        let mut bad_attachment = with_question(&turn_path(), secret);
        bad_attachment.attachments = Some(vec![format!("ftp://user:{secret}@example.com")]);
        let mut bad_domain = with_question(SEARCH_PATH, secret);
        bad_domain.allowed_domains = Some(vec![format!("user:{secret}@example.com")]);
        let inputs = [
            read_input(&format!("ftp://user:{secret}@example.com")),
            read_input(&format!("https://user:{secret}@example.com:raw")),
            read_input(&format!("https://user:{secret}@example.com#stravia?raw=1")),
            conflicting,
            bad_attachment,
            bad_domain,
        ];
        for input in inputs {
            let message = parse_read_input(&input).unwrap_err().to_string();
            assert!(
                !message.contains(secret) && !message.contains("user"),
                "{message}"
            );
        }
    }

    #[test]
    fn null_and_absent_fields_are_equivalent() {
        let absent: ReadInput =
            serde_json::from_value(json!({ "path": SEARCH_PATH, "question": "q" })).unwrap();
        let nulls: ReadInput = serde_json::from_value(json!({
            "path": SEARCH_PATH,
            "question": "q",
            "attachments": null,
            "allowed_domains": null,
            "wait": null,
        }))
        .unwrap();
        assert_eq!(absent, nulls);
        assert_eq!(
            parse_read_input(&absent).unwrap(),
            parse_read_input(&nulls).unwrap()
        );
        for value in [
            json!({ "url": "https://example.com" }),
            json!({ "path": SEARCH_PATH, "previous_path": null }),
            json!({ "path": SEARCH_PATH, "i": "read" }),
            json!({ "path": null }),
            json!({ "path": SEARCH_PATH, "wait": -1 }),
        ] {
            assert!(serde_json::from_value::<ReadInput>(value).is_err());
        }
    }

    #[test]
    fn schemas_expose_fields_by_scope() {
        let scope = |networking, media, internal| ReadSchemaScope {
            networking,
            media,
            internal,
        };
        assert_eq!(ReadSchemaScope::FULL, scope(true, true, false));
        let cases = [
            (
                scope(true, true, false),
                vec!["allowed_domains", "attachments", "path", "question", "wait"],
            ),
            (
                scope(true, true, true),
                vec!["allowed_domains", "attachments", "path", "question"],
            ),
            (
                scope(true, false, false),
                vec!["allowed_domains", "path", "question", "wait"],
            ),
            (
                scope(false, true, false),
                vec!["attachments", "path", "question", "wait"],
            ),
            (scope(false, false, false), vec!["path", "wait"]),
            (scope(false, false, true), vec!["path"]),
        ];
        for (scope, expected) in cases {
            for schema in [input_schema(scope), model_input_schema(scope)] {
                let mut keys: Vec<&str> = schema["properties"]
                    .as_object()
                    .unwrap()
                    .keys()
                    .map(String::as_str)
                    .collect();
                keys.sort_unstable();
                assert_eq!(keys, expected, "{scope:?}");
                assert_eq!(schema["type"], json!("object"));
                assert_eq!(schema["additionalProperties"], json!(false));
            }
            assert_eq!(input_schema(scope)["required"], json!(["path"]));
            let strict = model_input_schema(scope);
            let mut required: Vec<&str> = strict["required"]
                .as_array()
                .unwrap()
                .iter()
                .map(|name| name.as_str().unwrap())
                .collect();
            required.sort_unstable();
            assert_eq!(required, expected, "{scope:?}");
        }
    }

    #[test]
    fn strict_schema_makes_every_field_required_and_nullable() {
        let strict = model_input_schema(ReadSchemaScope::FULL);
        for (name, property) in strict["properties"].as_object().unwrap() {
            if name == "path" {
                assert_eq!(property["type"], json!("string"));
                continue;
            }
            let kinds = property["type"].as_array().unwrap();
            assert!(
                kinds.iter().any(|kind| kind.as_str() == Some("null")),
                "{name}"
            );
            assert!(
                property["description"]
                    .as_str()
                    .is_some_and(|text| !text.is_empty()),
                "{name}"
            );
        }
        assert_eq!(
            strict["properties"]["attachments"]["type"],
            json!(["array", "null"])
        );
        assert_eq!(
            strict["properties"]["attachments"]["items"],
            json!({"type": "string"})
        );
        assert_eq!(strict["properties"]["wait"]["minimum"], json!(0));

        let loose = input_schema(ReadSchemaScope::FULL);
        assert_eq!(loose["properties"]["question"]["type"], json!("string"));
        assert_eq!(loose["properties"]["wait"]["type"], json!("integer"));

        // 严格形式要求出现的字段全部给出；可选字段写成 null 仍是合法参数。
        let arguments = json!({
            "path": SEARCH_PATH,
            "question": "q",
            "attachments": null,
            "allowed_domains": null,
            "wait": null,
        });
        let keys = arguments
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        for name in strict["required"].as_array().unwrap() {
            assert!(keys.iter().any(|key| Some(key.as_str()) == name.as_str()));
        }
        let input: ReadInput = serde_json::from_value(arguments).unwrap();
        assert!(parse_read_input(&input).is_ok());
    }
}
