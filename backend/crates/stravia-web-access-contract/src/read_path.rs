//! StraviaRead 唯一 path 契约：统一读取表达式的解析、schema 与格式化。
//!
//! 公开 MCP、外层模型与内部搜索 Agent 都只接收单个必填字符串 `path`，由
//! [`parse_read_path`] 统一解析为 [`ReadTarget`]。工具 schema 统一使用
//! [`input_schema`]，Hook 重新编码搜索 path 统一使用 [`format_search_path`]，
//! 不再各自拼接字符串。
//!
//! 语法概览：
//! - 搜索：`search://<percent-encoded-text>?allowed_domains=a&allowed_domains=b&previous_turn_id=<id>`。
//!   首个未编码 `?` 分隔文本与参数；文本严格 percent-decode（`+` 不转换为空格），
//!   参数按 form 解码（`+` 视为空格）；所有 `%` 转义与 UTF-8 均先验证。
//! - 资源：HTTP(S) URL 的工具选项使用 `#stravia?<options>`；Artifact Reference
//!   使用 `sa:<digest>?<options>`。HTTP(S) 的普通 fragment 保留源 URL 语义，
//!   Artifact fragment 一律拒绝；不支持旧 Artifact wrapper。
//! - 选项为 `question`、`raw`、`lines`、`download`、`previous_turn_id`、`cursor`，
//!   互斥规则见 [`parse_read_path`]。
//!
//! 错误（[`ReadPathError`]）区分无效 scheme、选项、编码与范围，且不在消息中
//! 回显完整带凭据 URL 或参数值。

use serde::{Deserialize, Serialize};

use crate::normalize_domains;

/// 搜索文本解码后的最大字节数，与 Web Search 执行侧既有限制一致。
pub const MAX_SEARCH_TEXT_BYTES: usize = 64 * 1024;
/// 搜索 `allowed_domains` 参数在规范化前允许的原始条目上限。
pub const MAX_ALLOWED_DOMAINS: usize = 20;
/// `lines` 选项允许的最大分段数。
pub const MAX_LINE_SELECTIONS: usize = 16;

/// Artifact Reference 前缀与 digest 长度；镜像 runtime-contract 的语法，
/// 本 crate 不引入对该 crate 的依赖。
const ARTIFACT_REFERENCE_PREFIX: &str = "sa:";
const ARTIFACT_DIGEST_LEN: usize = 55;

/// StraviaRead 工具的唯一输入：单个必填 `path`。
///
/// 旧顶层 `url`、`i` 等字段由 `deny_unknown_fields` 明确拒绝，不做兼容别名。
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ReadInput {
    pub path: String,
}

/// 解析后的读取目标：一次搜索或一次资源读取。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReadTarget {
    Search(SearchPath),
    Resource(ResourcePath),
}

/// 一次搜索读取。
///
/// `allowed_domains` 为 `None` 表示参数缺省：新根搜索即无限制，续接由调用方
/// 按父策略解析继承；`Some` 为显式非空列表，整体替换父策略。不存在用空列表
/// 清空限制的语法。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SearchPath {
    pub query: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_domains: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_turn_id: Option<String>,
}

/// 一次资源读取：HTTP(S) URL 或 Artifact Reference，附工具选项。
///
/// `url` 保留 HTTP(S) 源站 path/query 与普通 fragment；工具选项已剥离。
/// Artifact 的 `url` 是不含查询选项的裸 `sa:<digest>`。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResourcePath {
    pub url: String,
    #[serde(default)]
    pub options: ReadOptions,
}

/// 资源读取选项。全部缺省表示默认读取。
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
    pub previous_turn_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}

/// 一次行选择，行号 1-based，按源文本 LF 计行。
///
/// `parse_read_path` 产出的列表经过排序与合并的规范化：有界区间一律为
/// [`LineSelection::Inclusive`]，开放区间（到 EOF）为 [`LineSelection::From`]，
/// [`LineSelection::Last`] 因依赖总行数只能在末尾保留（多个 `Last` 合并为最大
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

/// path 解析错误；区分无效 scheme、选项、编码与范围。
///
/// Display 消息不回显完整 URL、搜索文本或选项值，避免泄露带凭据地址。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ReadPathError {
    /// path 为空或仅空白。
    #[error("read path must not be blank")]
    BlankPath,
    /// 旧 `query://` 表达式；需改写为 `search://`。
    #[error("legacy query:// read paths are no longer accepted; use search:// for search")]
    LegacyQueryScheme,
    /// 不是可识别的 http(s) URL、Artifact Reference 或 search 表达式。
    #[error(
        "read path must be an absolute http(s) URL, an Artifact Reference, or a search:// path"
    )]
    InvalidScheme,
    /// percent 转义或 UTF-8 解码失败；`component` 标明出错位置。
    #[error("invalid percent-encoding in {component}: {reason}")]
    InvalidEncoding {
        component: &'static str,
        reason: &'static str,
    },
    /// 搜索文本解码后为空或仅空白。
    #[error("search text must not be blank")]
    BlankSearchText,
    /// 搜索文本解码后超过 65536 字节。
    #[error("search text exceeds the 65536-byte limit after percent-decoding")]
    SearchTextTooLarge,
    /// 参数区结构非法（空参数区、空分段、嵌套 fragment 等）。
    #[error("malformed read path parameters: {reason}")]
    MalformedParameter { reason: &'static str },
    /// 未知参数或选项；仅回显可安全打印的参数名。
    #[error("unknown parameter: {parameter}")]
    UnknownParameter { parameter: String },
    /// 单值参数或选项重复出现。
    #[error("duplicate parameter: {parameter}")]
    DuplicateParameter { parameter: &'static str },
    /// 参数或选项值为空。
    #[error("parameter must not be empty: {parameter}")]
    EmptyParameter { parameter: &'static str },
    /// `raw`/`download` 只接受值 `1`。
    #[error("option only accepts the value 1: {option}")]
    InvalidFlagValue { option: &'static str },
    /// `allowed_domains` 原始条目超过 20 项。
    #[error("allowed_domains accepts at most 20 entries before normalization")]
    TooManyAllowedDomains { count: usize },
    /// `allowed_domains` 含无效域名条目或规范化后无可保留条目。
    #[error("allowed_domains entries must be valid domain filters")]
    InvalidAllowedDomains,
    /// Artifact 引用不是裸身份或身份字符非法。
    #[error("invalid Artifact Reference: {reason}")]
    InvalidArtifactReference { reason: &'static str },
    /// 行选择语法或数值非法。
    #[error("invalid lines selection: {reason}")]
    InvalidLineSelection { reason: &'static str },
    /// 选项互斥或上下文限制被违反。
    #[error("conflicting read options: {reason}")]
    ConflictingOptions { reason: &'static str },
}

/// StraviaRead 的唯一输入 schema；模型与 MCP 均复用此定义。
pub fn input_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "path": { "type": "string", "minLength": 1 }
        },
        "required": ["path"],
        "additionalProperties": false
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

/// 解析唯一 `path` 为搜索或资源读取目标。
///
/// - 空白 path 报错；合法 URL 内容不做 trim。
/// - `query://` 前缀明确拒绝，无兼容别名。
/// - 搜索文本严格 percent-decode（`+` 保留），参数值按 form 解码（`+` 为空格）；
///   拒绝空白文本、未知参数、重复单值参数、空域名与超过
///   [`MAX_ALLOWED_DOMAINS`] 个原始条目；域名经 [`crate::normalize_domains`]
///   规范化去重。
/// - HTTP(S) 资源从 `#stravia?` 读取工具选项；Artifact 从 `?` 读取工具选项；
///   Artifact 必须是 `sa:` 加 55 位小写 digest，并拒绝所有 fragment。
/// - 选项约束：`question` 与 `previous_turn_id` 非空；`raw`/`download` 只接受
///   `1`；`raw` 与 `question` 互斥；`download` 与其他所有选项互斥；
///   `previous_turn_id` 仅媒体问题（资源侧需伴随 `question`）或搜索允许；
///   `cursor` 仅对 Artifact 有效且与其他所有选项互斥。
pub fn parse_read_path(path: &str) -> Result<ReadTarget, ReadPathError> {
    if path.trim().is_empty() {
        return Err(ReadPathError::BlankPath);
    }
    if path.starts_with("query://") {
        return Err(ReadPathError::LegacyQueryScheme);
    }
    if let Some(rest) = path.strip_prefix("search://") {
        return parse_search(rest);
    }
    parse_resource(path)
}

/// 将搜索目标重新编码为 `search://` path；与 [`parse_read_path`] 严格互逆。
///
/// 文本与参数值对所有非 unreserved 字节做 percent 编码，因此文本中的 `?`、`#`、
/// `+` 均不会破坏解析。`allowed_domains` 为 `Some(vec![])` 时会产出空值参数——
/// 解析侧明确拒绝——以避免把显式空列表静默变成无限制。
pub fn format_search_path(search: &SearchPath) -> String {
    let mut path = String::with_capacity(13 + search.query.len());
    path.push_str("search://");
    encode_component(&mut path, &search.query);
    let mut separator = '?';
    if let Some(domains) = &search.allowed_domains {
        if domains.is_empty() {
            path.push_str("?allowed_domains=");
            separator = '&';
        } else {
            for domain in domains {
                path.push(separator);
                separator = '&';
                path.push_str("allowed_domains=");
                encode_component(&mut path, domain);
            }
        }
    }
    if let Some(previous_turn_id) = &search.previous_turn_id {
        path.push(separator);
        path.push_str("previous_turn_id=");
        encode_component(&mut path, previous_turn_id);
    }
    path
}

fn parse_search(rest: &str) -> Result<ReadTarget, ReadPathError> {
    let (text_raw, query_raw) = match rest.split_once('?') {
        Some((text, query)) => (text, Some(query)),
        None => (rest, None),
    };
    let query = decode_percent(text_raw, "search text", false)?;
    if query.trim().is_empty() {
        return Err(ReadPathError::BlankSearchText);
    }
    if query.len() > MAX_SEARCH_TEXT_BYTES {
        return Err(ReadPathError::SearchTextTooLarge);
    }
    let mut domains_raw: Vec<String> = Vec::new();
    let mut previous_turn_id: Option<String> = None;
    if let Some(parameters) = query_raw {
        if parameters.is_empty() {
            return Err(ReadPathError::MalformedParameter {
                reason: "search parameters must contain at least one entry after '?'",
            });
        }
        for segment in parameters.split('&') {
            if segment.is_empty() {
                return Err(ReadPathError::MalformedParameter {
                    reason: "search parameters must not contain empty entries",
                });
            }
            let (key_raw, value_raw) = match segment.split_once('=') {
                Some((key, value)) => (key, value),
                None => (segment, ""),
            };
            let key = decode_percent(key_raw, "search parameters", true)?;
            let value = decode_percent(value_raw, "search parameters", true)?;
            match key.as_str() {
                "allowed_domains" => {
                    if value.trim().is_empty() {
                        return Err(ReadPathError::EmptyParameter {
                            parameter: "allowed_domains",
                        });
                    }
                    if domains_raw.len() == MAX_ALLOWED_DOMAINS {
                        return Err(ReadPathError::TooManyAllowedDomains {
                            count: MAX_ALLOWED_DOMAINS + 1,
                        });
                    }
                    domains_raw.push(value);
                }
                "previous_turn_id" => {
                    if previous_turn_id.is_some() {
                        return Err(ReadPathError::DuplicateParameter {
                            parameter: "previous_turn_id",
                        });
                    }
                    if value.trim().is_empty() {
                        return Err(ReadPathError::EmptyParameter {
                            parameter: "previous_turn_id",
                        });
                    }
                    previous_turn_id = Some(value);
                }
                _ => {
                    return Err(ReadPathError::UnknownParameter {
                        parameter: printable_parameter(&key),
                    })
                }
            }
        }
    }
    let allowed_domains = if domains_raw.is_empty() {
        None
    } else {
        if domains_raw.len() > MAX_ALLOWED_DOMAINS {
            return Err(ReadPathError::TooManyAllowedDomains {
                count: domains_raw.len(),
            });
        }
        let normalized =
            normalize_domains(domains_raw).map_err(|_| ReadPathError::InvalidAllowedDomains)?;
        if normalized.is_empty() {
            return Err(ReadPathError::InvalidAllowedDomains);
        }
        Some(normalized)
    };
    Ok(ReadTarget::Search(SearchPath {
        query,
        allowed_domains,
        previous_turn_id,
    }))
}

fn parse_resource(path: &str) -> Result<ReadTarget, ReadPathError> {
    let is_artifact = path.starts_with(ARTIFACT_REFERENCE_PREFIX);
    let (url, options_raw) = if is_artifact {
        if path.contains('#') {
            return Err(ReadPathError::InvalidArtifactReference {
                reason: "fragments are not supported",
            });
        }
        path.split_once('?')
            .map_or((path, None), |(identity, options)| {
                (identity, Some(options))
            })
    } else {
        if path
            .split_once('#')
            .is_some_and(|(_, fragment)| fragment.contains('#'))
        {
            return Err(ReadPathError::MalformedParameter {
                reason: "nested fragments are not supported in read options",
            });
        }
        // HTTP(S) tool options stay in the reserved fragment so source queries retain
        // their original signed-URL semantics.
        match path.split_once('#') {
            Some((base, fragment)) => match fragment.strip_prefix("stravia?") {
                Some(options) => (base, Some(options)),
                None => (path, None),
            },
            None => (path, None),
        }
    };
    if let Some(options) = options_raw {
        if options.is_empty() {
            return Err(ReadPathError::MalformedParameter {
                reason: "read options must contain at least one entry",
            });
        }
        if options.contains('#') {
            return Err(ReadPathError::MalformedParameter {
                reason: "nested fragments are not supported in read options",
            });
        }
    }
    if is_artifact {
        validate_artifact_reference(url)?;
    } else {
        validate_http_url(url)?;
    }
    let options = match options_raw {
        Some(raw) => parse_read_options(raw)?,
        None => ReadOptions::default(),
    };
    if options.cursor.is_some() && !is_artifact {
        return Err(ReadPathError::ConflictingOptions {
            reason: "cursor is only valid on an Artifact Reference",
        });
    }
    if options.previous_turn_id.is_some() && options.question.is_none() {
        return Err(ReadPathError::ConflictingOptions {
            reason: "previous_turn_id requires a media question",
        });
    }
    Ok(ReadTarget::Resource(ResourcePath {
        url: url.to_owned(),
        options,
    }))
}

/// 校验 Artifact 裸身份：完整 SHA-256 的 55 位小写 base26 编码。
fn validate_artifact_reference(url: &str) -> Result<(), ReadPathError> {
    let identity = &url[ARTIFACT_REFERENCE_PREFIX.len()..];
    if identity.len() == ARTIFACT_DIGEST_LEN
        && identity.bytes().all(|byte| byte.is_ascii_lowercase())
    {
        return Ok(());
    }
    Err(ReadPathError::InvalidArtifactReference {
        reason: "identity must contain exactly 55 lowercase ASCII letters",
    })
}

fn validate_http_url(url: &str) -> Result<(), ReadPathError> {
    let parsed = url::Url::parse(url).map_err(|_| ReadPathError::InvalidScheme)?;
    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        return Err(ReadPathError::InvalidScheme);
    }
    if parsed.host_str().is_none_or(|host| host.is_empty()) || parsed.host_str() == Some("stravia")
    {
        return Err(ReadPathError::InvalidScheme);
    }
    Ok(())
}

fn parse_read_options(raw: &str) -> Result<ReadOptions, ReadPathError> {
    let mut question: Option<String> = None;
    let mut raw_flag = false;
    let mut download = false;
    let mut lines: Option<Vec<LineSelection>> = None;
    let mut previous_turn_id: Option<String> = None;
    let mut cursor: Option<String> = None;
    for segment in raw.split('&') {
        if segment.is_empty() {
            return Err(ReadPathError::MalformedParameter {
                reason: "read options must not contain empty entries",
            });
        }
        let (key_raw, value_raw) = match segment.split_once('=') {
            Some((key, value)) => (key, value),
            None => (segment, ""),
        };
        let key = decode_percent(key_raw, "read options", true)?;
        let value = decode_percent(value_raw, "read options", true)?;
        match key.as_str() {
            "question" => {
                if question.is_some() {
                    return Err(ReadPathError::DuplicateParameter {
                        parameter: "question",
                    });
                }
                if value.trim().is_empty() {
                    return Err(ReadPathError::EmptyParameter {
                        parameter: "question",
                    });
                }
                question = Some(value);
            }
            "raw" => {
                if raw_flag {
                    return Err(ReadPathError::DuplicateParameter { parameter: "raw" });
                }
                if value != "1" {
                    return Err(ReadPathError::InvalidFlagValue { option: "raw" });
                }
                raw_flag = true;
            }
            "download" => {
                if download {
                    return Err(ReadPathError::DuplicateParameter {
                        parameter: "download",
                    });
                }
                if value != "1" {
                    return Err(ReadPathError::InvalidFlagValue { option: "download" });
                }
                download = true;
            }
            "lines" => {
                if lines.is_some() {
                    return Err(ReadPathError::DuplicateParameter { parameter: "lines" });
                }
                if value.is_empty() {
                    return Err(ReadPathError::EmptyParameter { parameter: "lines" });
                }
                lines = Some(parse_line_selections(&value)?);
            }
            "previous_turn_id" => {
                if previous_turn_id.is_some() {
                    return Err(ReadPathError::DuplicateParameter {
                        parameter: "previous_turn_id",
                    });
                }
                if value.trim().is_empty() {
                    return Err(ReadPathError::EmptyParameter {
                        parameter: "previous_turn_id",
                    });
                }
                previous_turn_id = Some(value);
            }
            "cursor" => {
                if cursor.is_some() {
                    return Err(ReadPathError::DuplicateParameter {
                        parameter: "cursor",
                    });
                }
                if value.is_empty() {
                    return Err(ReadPathError::EmptyParameter {
                        parameter: "cursor",
                    });
                }
                cursor = Some(value);
            }
            _ => {
                return Err(ReadPathError::UnknownParameter {
                    parameter: printable_parameter(&key),
                })
            }
        }
    }
    if raw_flag && question.is_some() {
        return Err(ReadPathError::ConflictingOptions {
            reason: "raw cannot be combined with question",
        });
    }
    if download
        && (question.is_some()
            || raw_flag
            || lines.is_some()
            || previous_turn_id.is_some()
            || cursor.is_some())
    {
        return Err(ReadPathError::ConflictingOptions {
            reason: "download cannot be combined with other options",
        });
    }
    if cursor.is_some()
        && (question.is_some() || raw_flag || lines.is_some() || previous_turn_id.is_some())
    {
        return Err(ReadPathError::ConflictingOptions {
            reason: "cursor cannot be combined with other options",
        });
    }
    if previous_turn_id.is_some() && question.is_none() {
        return Err(ReadPathError::ConflictingOptions {
            reason: "previous_turn_id requires a media question",
        });
    }
    Ok(ReadOptions {
        question,
        raw: raw_flag,
        download,
        lines,
        previous_turn_id,
        cursor,
    })
}

/// 解析 `lines` 值：`N`、`N-M`、`N+K`（`+` 在 URL 中须编码为 `%2B`）、`-K`，
/// 逗号分隔多段。全部 1-based 正数、checked 运算、最多 [`MAX_LINE_SELECTIONS`]
/// 段；按源顺序排序并合并相邻/重叠区间，多个 `Last` 合并为最大计数。
fn parse_line_selections(value: &str) -> Result<Vec<LineSelection>, ReadPathError> {
    let segments = value.split(',');
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
    let value: u64 = text.parse().map_err(|_| invalid())?;
    if value == 0 {
        return Err(invalid());
    }
    Ok(value)
}

/// 严格 percent 解码；`plus_as_space` 为真时按 form 语义把 `+` 解码为空格。
/// 所有 `%` 转义必须是两个十六进制位，解码结果必须是有效 UTF-8。
fn decode_percent(
    input: &str,
    component: &'static str,
    plus_as_space: bool,
) -> Result<String, ReadPathError> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte == b'%' {
            let high = bytes.get(index + 1).and_then(|byte| hex_value(*byte));
            let low = bytes.get(index + 2).and_then(|byte| hex_value(*byte));
            match (high, low) {
                (Some(high), Some(low)) => {
                    out.push((high << 4) | low);
                    index += 3;
                }
                _ => {
                    return Err(ReadPathError::InvalidEncoding {
                        component,
                        reason: "percent escapes must be two hexadecimal digits",
                    })
                }
            }
        } else {
            out.push(if byte == b'+' && plus_as_space {
                b' '
            } else {
                byte
            });
            index += 1;
        }
    }
    String::from_utf8(out).map_err(|_| ReadPathError::InvalidEncoding {
        component,
        reason: "decoded bytes are not valid UTF-8",
    })
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// 仅回显可安全打印的参数名，避免把带凭据的 URL 片段带进错误消息。
fn printable_parameter(key: &str) -> String {
    if key.len() <= 64
        && key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
    {
        key.to_owned()
    } else {
        "<unrecognized>".to_owned()
    }
}

/// 对所有非 unreserved 字节做 percent 编码（含 `?`、`#`、`&`、`=`、`+`、`%`
/// 与非 ASCII UTF-8），保证编码结果可被 [`parse_read_path`] 无歧义还原。
fn encode_component(out: &mut String, value: &str) {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    for &byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char)
            }
            _ => {
                out.push('%');
                out.push(HEX[(byte >> 4) as usize] as char);
                out.push(HEX[(byte & 0x0f) as usize] as char);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_encoding_round_trips_without_form_decoding_the_text() {
        let search = SearchPath {
            query: "中文 C++ & ? # %3F".into(),
            allowed_domains: Some(vec!["docs.rs".into(), "example.com".into()]),
            previous_turn_id: Some("turn+1".into()),
        };
        assert_eq!(
            parse_read_path(&format_search_path(&search)).unwrap(),
            ReadTarget::Search(search)
        );
        let ReadTarget::Search(search) =
            parse_read_path("search://C++%2520?previous_turn_id=turn+1").unwrap()
        else {
            panic!("search")
        };
        assert_eq!(search.query, "C++%20");
        assert_eq!(search.previous_turn_id.as_deref(), Some("turn 1"));
    }

    #[test]
    fn invalid_legacy_and_ambiguous_inputs_fail_closed() {
        let digest = "a".repeat(ARTIFACT_DIGEST_LEN);
        let ReadTarget::Resource(resource) =
            parse_read_path(&format!("sa:{digest}?question=Private%20question")).unwrap()
        else {
            panic!("Artifact resource")
        };
        assert_eq!(resource.url, format!("sa:{digest}"));
        assert_eq!(
            resource.options.question.as_deref(),
            Some("Private question")
        );

        for path in [
            "",
            " ",
            "query://old",
            "file:///etc/passwd",
            "/local/file",
            "search://%FF",
            "search://%",
            "search://%2G",
            "search://%20",
            "search://q?blocked_domains=a.com",
            "search://q?allowed_domains=",
            "search://q?allowed_domains=+",
            "search://q?allowed_domains=docs.rs&allowed_domains=+",
            "search://q?previous_turn_id=a&previous_turn_id=b",
            "search://q?previous_turn_id=+",
            "search://q?allowed_domains=%FF",
            "search://q?unknown=x",
            "https://stravia/artifact/a?question=legacy",
            "https://stravia/artifact/a#ordinary",
            "https://stravia/artifact/a/b",
            "https://stravia/artifact/a%20b",
            "https://example.com#ordinary#stravia?raw=1",
            "https://example.com#stravia?raw=1#nested",
            "https://example.com#stravia?raw=true",
            "https://example.com#stravia?raw=1&raw=1",
            "https://example.com#stravia?raw=1&question=q",
            "https://example.com#stravia?question=+",
            "https://example.com#stravia?question=%FF",
            "https://example.com#stravia?download=1&lines=1",
            "https://example.com#stravia?previous_turn_id=t",
            "https://example.com#stravia?cursor=abc",
            "https://stravia/artifact/a#stravia?cursor=abc&raw=1",
        ] {
            assert!(
                parse_read_path(path).is_err(),
                "accepted invalid path: {path}"
            );
        }
        for path in [
            format!("sa:{}", "a".repeat(54)),
            format!("sa:{}", "A".repeat(55)),
            format!("sa:{digest}#stravia?question=q"),
            format!("sa:{digest}?question=q#fragment"),
        ] {
            assert!(
                parse_read_path(&path).is_err(),
                "accepted invalid Artifact path: {path}"
            );
        }
        for value in [
            serde_json::json!({"url":"https://example.com"}),
            serde_json::json!({"path":"search://q","i":"read"}),
            serde_json::json!({"path":null}),
        ] {
            assert!(serde_json::from_value::<ReadInput>(value).is_err());
        }
    }

    #[test]
    fn search_limits_apply_before_domain_deduplication_and_after_text_decoding() {
        let ReadTarget::Search(search) =
            parse_read_path("search://q?allowed_domains=Docs.RS&allowed_domains=docs.rs.").unwrap()
        else {
            panic!("search")
        };
        assert_eq!(search.allowed_domains.unwrap(), ["docs.rs"]);
        let domains = std::iter::repeat_n("allowed_domains=docs.rs", 21)
            .collect::<Vec<_>>()
            .join("&");
        assert!(parse_read_path(&format!("search://q?{domains}")).is_err());
        assert!(
            parse_read_path(&format!("search://{}", "a".repeat(MAX_SEARCH_TEXT_BYTES))).is_ok()
        );
        assert!(parse_read_path(&format!(
            "search://{}",
            "a".repeat(MAX_SEARCH_TEXT_BYTES + 1)
        ))
        .is_err());
    }

    #[test]
    fn resource_options_do_not_rewrite_signed_source_queries() {
        let source = "https://example.com/a?sig=a%2Bb&question=origin&x=%2520";
        let ReadTarget::Resource(resource) =
            parse_read_path(&format!("{source}#stravia?question=Private%20question")).unwrap()
        else {
            panic!("resource")
        };
        assert_eq!(resource.url, source);
        assert_eq!(
            resource.options.question.as_deref(),
            Some("Private question")
        );
        let ReadTarget::Resource(resource) =
            parse_read_path("https://example.com#section").unwrap()
        else {
            panic!("resource")
        };
        assert_eq!(resource.url, "https://example.com#section");
    }

    #[test]
    fn line_selections_check_arithmetic_and_merge_ranges() {
        let ReadTarget::Resource(resource) =
            parse_read_path("https://example.com#stravia?lines=5%2B3,1-2,2-5,-2").unwrap()
        else {
            panic!("resource")
        };
        assert_eq!(
            resource.options.lines.unwrap(),
            [
                LineSelection::Inclusive { start: 1, end: 7 },
                LineSelection::Last(2)
            ]
        );
        for selection in [
            "0",
            "-0",
            "-%2B1",
            "1+2",
            "3-2",
            "1%2B0",
            "18446744073709551615%2B2",
            "18446744073709551616",
            "1,,2",
        ] {
            assert!(
                parse_read_path(&format!("https://example.com#stravia?lines={selection}")).is_err()
            );
        }
        let ranges = std::iter::repeat_n("1-2", 17).collect::<Vec<_>>().join(",");
        assert!(parse_read_path(&format!("https://example.com#stravia?lines={ranges}")).is_err());
    }

    #[test]
    fn parse_errors_do_not_echo_credentials_or_arbitrary_parameter_names() {
        for path in [
            "ftp://user:secret@example.com",
            "https://user:secret@example.com#stravia?raw=0",
            "https://example.com#stravia?https://user:secret@example.com=x",
        ] {
            let error = parse_read_path(path).unwrap_err().to_string();
            assert!(!error.contains("secret"));
            assert!(!error.contains("user"));
        }
    }
}
