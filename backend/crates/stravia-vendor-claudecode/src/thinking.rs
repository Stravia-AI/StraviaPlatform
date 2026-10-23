//! 把 codec 编码出的思考控制改写为目标 Claude 型号接受的形态。
//!
//! 通用 Anthropic 编码器只做一对一转换：`Effort` 写成 `adaptive` 加
//! `output_config.effort`，`Budget` 写成 `enabled` 加 `budget_tokens`，`Disabled`
//! 写成 `disabled`。Claude 各代模型对这些值的接受范围不同，直接发送会得到 400。
//! 规则依据官方文档 *Thinking* 与 *Effort* 的逐型号矩阵（Claude 4.5 至 5.5、
//! Fable/Mythos 5.x），并沿用 oh-my-pi 的策略：思考关不掉的型号省略 `thinking`
//! 并把 effort 固定为 `low`，强制工具调用与思考冲突时优先保住工具调用。

use serde_json::{Map, Value, json};

use crate::capabilities::ModelCapabilities;

type Revision = [u8; 3];

/// Anthropic effort 档位，按强度升序。
const EFFORTS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];
const EFFORTS_WITHOUT_XHIGH: [&str; 4] = ["low", "medium", "high", "max"];
/// 手动预算模式的最小 `budget_tokens`。
const MIN_BUDGET: u64 = 1024;
/// 显式输出上限高于思考预算时为正文与工具调用保留的空间
/// （与 oh-my-pi `OUTPUT_FALLBACK_BUFFER` 一致）。
const OUTPUT_BUFFER: u64 = 4000;
const CLEAR_THINKING_EDIT: &str = "clear_thinking_20251015";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Family {
    Opus,
    Sonnet,
    Haiku,
    Fable,
    Mythos,
}

const FAMILIES: [(&str, Family); 5] = [
    ("opus", Family::Opus),
    ("sonnet", Family::Sonnet),
    ("haiku", Family::Haiku),
    ("fable", Family::Fable),
    ("mythos", Family::Mythos),
];

/// 目标型号的线上能力。`/v1/models` 发现到的 [`ModelCapabilities`] 优先；接口没有
/// 覆盖的规则（能否关闭思考、`between_tools`、强制工具调用、`display` 默认值）以及
/// 尚未同步发现数据的型号，回退到按型号 ID 解析出的静态规则。
pub(crate) struct ModelProfile<'a> {
    known: Option<(Family, Revision)>,
    caps: &'a ModelCapabilities,
}

impl<'a> ModelProfile<'a> {
    /// 型号 ID 与发现数据都无法识别时返回 `None`，调用方应原样透传。
    pub(crate) fn new(model: &str, caps: &'a ModelCapabilities) -> Option<Self> {
        let known = parse_known(model);
        (known.is_some() || !caps.is_empty()).then_some(Self { known, caps })
    }

    pub(crate) fn of_request(
        object: &Map<String, Value>,
        caps: &'a ModelCapabilities,
    ) -> Option<Self> {
        Self::new(object.get("model").and_then(Value::as_str)?, caps)
    }

    /// Anthropic 从 Claude 4.6 起不再接受末尾 assistant 作为响应前缀。
    /// 未识别型号沿用原生行为，不从思考能力推断 prefill 支持。
    pub(crate) fn accepts_prefill(&self) -> bool {
        match self.known {
            Some((Family::Opus | Family::Sonnet | Family::Haiku, revision)) => revision < [4, 6, 0],
            Some((Family::Fable | Family::Mythos, _)) => false,
            None => true,
        }
    }

    /// 只接受 `adaptive`，拒绝 `enabled` 与非默认采样参数的型号：Opus ≥ 4.7、
    /// Sonnet ≥ 5、Fable/Mythos。对应 oh-my-pi `anthropicAdaptiveGenAtLeast("4.7")`。
    pub(crate) fn adaptive_only(&self) -> bool {
        if let (Some(adaptive), Some(enabled)) = (self.caps.adaptive, self.caps.enabled) {
            return adaptive && !enabled;
        }
        match self.known {
            Some((Family::Opus, revision)) => revision >= [4, 7, 0],
            Some((Family::Sonnet, revision)) => revision >= [5, 0, 0],
            Some((Family::Fable | Family::Mythos, _)) => true,
            Some((Family::Haiku, _)) | None => false,
        }
    }

    fn supports_adaptive(&self) -> bool {
        self.caps.adaptive.unwrap_or(match self.known {
            Some((Family::Opus | Family::Sonnet, revision)) => revision >= [4, 6, 0],
            Some((Family::Fable | Family::Mythos, _)) => true,
            Some((Family::Haiku, _)) | None => false,
        })
    }

    /// 思考常开：`thinking: disabled` 在任何 effort 下都是 400。
    fn always_on(&self) -> bool {
        match self.known {
            Some((Family::Opus, revision)) => revision >= [5, 5, 0],
            Some((Family::Fable | Family::Mythos, _)) => true,
            Some((Family::Sonnet | Family::Haiku, _)) | None => false,
        }
    }

    /// Sonnet 5.5 用 `between_tools` 代替 `disabled` 表示最低思考。
    fn between_tools(&self) -> bool {
        matches!(self.known, Some((Family::Sonnet, revision)) if revision >= [5, 5, 0])
    }

    /// Opus 5 仅在 effort ≤ high 时接受 `disabled`。
    fn disabled_requires_effort_cap(&self) -> bool {
        matches!(
            self.known,
            Some((Family::Opus, revision)) if ([5, 0, 0]..[5, 5, 0]).contains(&revision)
        )
    }

    /// 强制 `tool_choice`（`any`/`tool`）在这些型号上无论思考设置都是 400。
    fn forced_tool_use(&self) -> bool {
        !match self.known {
            Some((Family::Opus | Family::Sonnet, revision)) => revision >= [5, 5, 0],
            Some((Family::Fable | Family::Mythos, revision)) => revision >= [5, 1, 0],
            Some((Family::Haiku, _)) | None => false,
        }
    }

    /// 新型号默认不返回思考文本；沿用 oh-my-pi，客户端未指定时请求摘要。
    fn default_display_summarized(&self) -> bool {
        self.adaptive_only()
    }

    /// 型号接受的 effort 原生值；未知词表不推断顺序。
    fn efforts(&self) -> impl Iterator<Item = &str> {
        let ladder: &[&str] = if self.caps.efforts.is_some() {
            &[]
        } else {
            match self.known {
                Some((Family::Haiku, _)) | None => &[],
                Some((Family::Sonnet, revision)) if revision < [4, 6, 0] => &[],
                Some((Family::Opus, revision)) if revision < [4, 5, 0] => &[],
                Some((Family::Opus, revision)) if revision < [4, 6, 0] => &EFFORTS[..3],
                Some((Family::Opus, revision)) if revision < [4, 7, 0] => &EFFORTS_WITHOUT_XHIGH,
                Some((Family::Sonnet, revision)) if revision < [5, 0, 0] => &EFFORTS_WITHOUT_XHIGH,
                Some(_) => &EFFORTS,
            }
        };
        ladder
            .iter()
            .copied()
            .chain(self.caps.efforts.iter().flatten().map(String::as_str))
    }

    /// 取不高于请求档位的最高受支持档位；请求低于全部档位时取最低档。
    fn clamp_effort(&self, requested: &str) -> Option<&str> {
        let rank = effort_rank(requested);
        let mut lower: Option<(usize, &str)> = None;
        let mut lowest: Option<(usize, &str)> = None;
        // 未来原生值只允许精确命中，不参与邻档比较或最低档回退。
        for candidate in self.efforts() {
            if candidate == requested {
                return Some(candidate);
            }
            let (Some(rank), Some(value)) = (rank, effort_rank(candidate)) else {
                continue;
            };
            if lowest.is_none_or(|(current, _)| value < current) {
                lowest = Some((value, candidate));
            }
            if value <= rank && lower.is_none_or(|(current, _)| value > current) {
                lower = Some((value, candidate));
            }
        }
        lower.or(lowest).map(|(_, candidate)| candidate)
    }
}

/// 从型号 ID 解析家族与版本。Opus/Sonnet/Haiku 缺少版本号
/// （`claude-3-opus-20240229`、日期后缀）视为只支持手动预算的旧型号；
/// Fable/Mythos 缺少版本号（如 `claude-mythos-preview`）能力不明，不猜测。
fn parse_known(model: &str) -> Option<(Family, Revision)> {
    let model = model.to_ascii_lowercase();
    let (name, family) = FAMILIES
        .into_iter()
        .find(|(name, _)| model.contains(name))?;
    let index = model.find(name)?;
    let tail = &model[index + name.len()..];
    let tail = tail.strip_prefix(['-', '.']).unwrap_or(tail);
    match (family, revision_prefix(tail)) {
        (Family::Fable | Family::Mythos, None) => None,
        (family, revision) => Some((family, revision.unwrap_or([0; 3]))),
    }
}

/// 发现阶段仅发布上游明确声明的 effort；预算和开关仍由请求协议编码。
pub(crate) fn reasoning_efforts(caps: &ModelCapabilities) -> Option<Vec<Value>> {
    caps.efforts
        .as_ref()
        .map(|efforts| efforts.iter().map(|effort| json!(effort)).collect())
}

fn effort_rank(effort: &str) -> Option<usize> {
    EFFORTS.iter().position(|candidate| *candidate == effort)
}

/// 解析型号尾部的 `major[.minor[.patch]]`，分隔符为 `.` 或后接数字的 `-`。
/// 超过 255 的分量（日期后缀 `20250805`）与紧跟字母的分量（`32b`）不属于版本。
fn revision_prefix(tail: &str) -> Option<Revision> {
    let bytes = tail.as_bytes();
    let mut revision = [0; 3];
    let mut index = 0;
    for (count, slot) in revision.iter_mut().enumerate() {
        let start = index;
        while bytes.get(index).is_some_and(u8::is_ascii_digit) {
            index += 1;
        }
        let component = if bytes.get(index).is_some_and(u8::is_ascii_alphabetic) {
            None
        } else {
            tail[start..index].parse::<u8>().ok()
        };
        let Some(component) = component else {
            return (count > 0).then_some(revision);
        };
        *slot = component;
        let continues = matches!(bytes.get(index), Some(b'.' | b'-'))
            && bytes.get(index + 1).is_some_and(u8::is_ascii_digit);
        if !continues {
            break;
        }
        index += 1;
    }
    Some(revision)
}

/// 请求值归一为 Anthropic 词表：`minimal` 没有对应档位，取最低的 `low`；
/// `none`、`default`、`adaptive` 等不是 effort，返回 `None`。
fn parse_effort(value: &str) -> Option<&'static str> {
    let value = if value == "minimal" { "low" } else { value };
    EFFORTS.into_iter().find(|effort| *effort == value)
}

/// 预算到 effort 的近似换算，覆盖 Stravia 预算档位表（1024/2048/8192/16384）。
fn effort_from_budget(budget: u64) -> &'static str {
    if budget <= 4096 {
        "low"
    } else if budget <= 12288 {
        "medium"
    } else {
        "high"
    }
}

/// effort 到预算的换算，与 Stravia 生成预算表的默认值一致。
fn budget_from_effort(effort: Option<&str>) -> u64 {
    match effort {
        Some("low") => 2048,
        Some("medium") => 8192,
        Some("high" | "xhigh" | "max") => 16384,
        _ => MIN_BUDGET,
    }
}

fn is_forced_tool_choice(object: &Map<String, Value>) -> bool {
    object
        .get("tool_choice")
        .and_then(|choice| choice.get("type"))
        .and_then(Value::as_str)
        .is_some_and(|kind| matches!(kind, "any" | "tool"))
}

fn set_effort(object: &mut Map<String, Value>, effort: Option<&str>) {
    match effort {
        Some(effort) => {
            let config = object.entry("output_config").or_insert_with(|| json!({}));
            if let Some(config) = config.as_object_mut() {
                config.insert("effort".into(), Value::String(effort.into()));
            }
        }
        None => {
            if let Some(config) = object
                .get_mut("output_config")
                .and_then(Value::as_object_mut)
            {
                config.remove("effort");
                if config.is_empty() {
                    object.remove("output_config");
                }
            }
        }
    }
}

/// `clear_thinking` 编辑要求思考开启；关闭思考时一并移除，避免 400。
fn drop_clear_thinking(object: &mut Map<String, Value>) {
    let Some(management) = object
        .get_mut("context_management")
        .and_then(Value::as_object_mut)
    else {
        return;
    };
    if let Some(edits) = management.get_mut("edits").and_then(Value::as_array_mut) {
        edits.retain(|edit| edit.get("type").and_then(Value::as_str) != Some(CLEAR_THINKING_EDIT));
        if edits.is_empty() {
            object.remove("context_management");
        }
    }
}

/// 客户端显式给出输出上限时，为思考预算保留正文空间；未给出上限时保持缺省，
/// 不根据思考预算或型号规格推导 `max_tokens`（必填回退属于编码器）。
fn raise_max_tokens(object: &mut Map<String, Value>, budget: u64) {
    let Some(current) = object.get("max_tokens").and_then(Value::as_u64) else {
        return;
    };
    let floor = budget + OUTPUT_BUFFER;
    if current < floor {
        object.insert("max_tokens".into(), json!(floor));
    }
}

/// 自动补出的 `max_tokens` 是发现到的型号上限或编码器的必填回退，不能被思考
/// 预算抬高；反过来把预算压进上限，并尽量为正文保留 [`OUTPUT_BUFFER`]。
/// 返回 `None` 表示上限放不下合法预算（预算须不小于 [`MIN_BUDGET`] 且小于上限）。
fn fit_budget_within_default(object: &Map<String, Value>, budget: u64) -> Option<u64> {
    let Some(limit) = object.get("max_tokens").and_then(Value::as_u64) else {
        return Some(budget);
    };
    if limit <= MIN_BUDGET {
        return None;
    }
    Some(
        budget
            .min(limit.saturating_sub(OUTPUT_BUFFER))
            .max(MIN_BUDGET)
            .min(limit - 1),
    )
}

/// The CLI accepts an omitted or summarized thinking transcript, not canonical
/// summary verbosity names.
pub(crate) fn summary_display(
    summary: &stravia_runtime_contract::protocol::ir::ReasoningSummary,
) -> &'static str {
    use stravia_runtime_contract::protocol::ir::ReasoningSummary;
    match summary {
        ReasoningSummary::None => "omitted",
        ReasoningSummary::Auto | ReasoningSummary::Concise | ReasoningSummary::Detailed => {
            "summarized"
        }
    }
}

/// Preserve explicit summary-only intent when reasoning is implicitly always on.
pub(crate) fn preserve_implicit_summary_intent(
    body: &mut Value,
    caps: &ModelCapabilities,
    display: Option<&str>,
) {
    let Some(display) = display else { return };
    let Some(object) = body.as_object_mut() else {
        return;
    };
    if !object.contains_key("thinking")
        && ModelProfile::of_request(object, caps).is_some_and(|profile| profile.always_on())
        && caps.thinking != Some(false)
    {
        object.insert(
            "thinking".into(),
            json!({"type": "adaptive", "display": display}),
        );
    }
}

/// 改写 `thinking`、`output_config.effort`、显式给出的 `max_tokens` 与强制
/// `tool_choice`，使请求落在目标型号的接受范围内。`auto_output_limit` 表示
/// `max_tokens` 由发现数据或编码器必填回退补出，此时保留该上限并收缩思考预算，
/// 不把它当作可抬高的显式选择。无法识别的型号原样透传。
/// 自动上限容不下合法思考预算时返回错误，不静默关闭客户端请求的思考。
pub(crate) fn normalize(
    object: &mut Map<String, Value>,
    caps: &ModelCapabilities,
    auto_output_limit: bool,
) -> Result<(), String> {
    sanitize_display(object);
    let Some(profile) = ModelProfile::of_request(object, caps) else {
        return Ok(());
    };
    if profile.known.is_none()
        && caps.thinking != Some(false)
        && caps.adaptive.is_none()
        && caps.enabled.is_none()
    {
        // effort 声明不提供思考模式或预算证据；未知型号仅约束授权，其余保持透传。
        if let Some(efforts) = &caps.efforts {
            let unauthorized = object
                .get("output_config")
                .and_then(|config| config.get("effort"))
                .and_then(Value::as_str)
                .is_some_and(|value| !efforts.iter().any(|effort| effort == value));
            if unauthorized {
                set_effort(object, None);
            }
        }
        return Ok(());
    }
    if caps.thinking == Some(false) {
        // 型号声明不支持思考：任何思考控制都会 400，整体移除。
        object.remove("thinking");
        drop_clear_thinking(object);
        set_effort(object, None);
        return Ok(());
    }
    let thinking = object.get("thinking").and_then(Value::as_object);
    let kind = thinking
        .and_then(|thinking| thinking.get("type"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let budget = thinking
        .and_then(|thinking| thinking.get("budget_tokens"))
        .and_then(Value::as_u64);
    let display = thinking
        .and_then(|thinking| thinking.get("display"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let requested = object
        .get("output_config")
        .and_then(|config| config.get("effort"))
        .and_then(Value::as_str)
        .map(str::to_owned);

    let mut forced = is_forced_tool_choice(object);
    if forced && !profile.forced_tool_use() {
        if let Some(choice) = object.get_mut("tool_choice").and_then(Value::as_object_mut) {
            choice.insert("type".into(), Value::String("auto".into()));
            choice.remove("name");
        }
        forced = false;
    }

    let effort = requested.as_deref().and_then(|value| {
        caps.efforts
            .as_ref()
            .filter(|efforts| efforts.iter().any(|candidate| candidate == value))
            .map(|_| value)
            .or_else(|| parse_effort(value))
    });
    let off = kind.as_deref() == Some("disabled") || requested.as_deref() == Some("none");
    let on = !off && matches!(kind.as_deref(), Some("adaptive" | "enabled"));

    if off {
        let effort = if profile.always_on() {
            // 关不掉思考的型号：省略 `thinking`（缺省即自适应），用最低 effort 近似关闭。
            object.remove("thinking");
            Some("low")
        } else if profile.between_tools() {
            object.insert("thinking".into(), json!({ "type": "between_tools" }));
            effort.and_then(parse_effort).map(cap_at_high)
        } else {
            object.insert("thinking".into(), json!({ "type": "disabled" }));
            if profile.disabled_requires_effort_cap() {
                effort.and_then(parse_effort).map(cap_at_high)
            } else {
                effort
            }
        };
        drop_clear_thinking(object);
        set_effort(object, effort.and_then(|value| profile.clamp_effort(value)));
        return Ok(());
    }

    if !on {
        if profile.always_on() {
            let thinking = json!({
                "type": "adaptive",
                "display": display.as_deref().unwrap_or("summarized")
            });
            // Keep summary opt-in independent of the model's native intensity.
            object.insert("thinking".into(), thinking);
        }
        set_effort(object, effort.and_then(|value| profile.clamp_effort(value)));
        return Ok(());
    }

    let manual = if !profile.supports_adaptive() {
        true
    } else if profile.adaptive_only() {
        false
    } else {
        // Opus/Sonnet 4.6 两种模式都接受：保留客户端明确给出的预算，其余走自适应。
        kind.as_deref() == Some("enabled") && budget.is_some() && !forced
    };
    if manual && forced {
        // 手动预算与强制工具调用互斥，且该型号没有自适应可换：以工具调用为准。
        object.remove("thinking");
        drop_clear_thinking(object);
        set_effort(object, effort.and_then(|value| profile.clamp_effort(value)));
        return Ok(());
    }

    if manual {
        let mut budget = budget
            .unwrap_or_else(|| budget_from_effort(effort))
            .max(MIN_BUDGET);
        if auto_output_limit {
            budget = fit_budget_within_default(object, budget)
                .ok_or("Anthropic enabled thinking requires max_tokens greater than 1024")?;
        }
        let mut thinking = json!({ "type": "enabled", "budget_tokens": budget });
        if let Some(display) = display {
            thinking["display"] = Value::String(display);
        }
        object.insert("thinking".into(), thinking);
        // 自动补出的上限不得被预算抬高；显式上限沿用既有抬高规则。
        if !auto_output_limit {
            raise_max_tokens(object, budget);
        }
        set_effort(object, effort.and_then(|value| profile.clamp_effort(value)));
    } else {
        let mut thinking = json!({ "type": "adaptive" });
        let display = display.or_else(|| {
            profile
                .default_display_summarized()
                .then(|| "summarized".to_owned())
        });
        if let Some(display) = display {
            thinking["display"] = Value::String(display);
        }
        object.insert("thinking".into(), thinking);
        let effort = effort.or_else(|| budget.map(effort_from_budget));
        set_effort(object, effort.and_then(|value| profile.clamp_effort(value)));
    }
    Ok(())
}

/// `thinking.display` 接受官方 `summarized` / `omitted` / `updates`。Open Responses 入口把
/// `reasoning.summary`（`auto`/`concise`/`detailed`）原样带成 display，这些值都表示
/// 要摘要；其余取值丢弃后由型号默认值决定。`updates` 的 beta 由 request 模块添加。
fn sanitize_display(object: &mut Map<String, Value>) {
    let Some(thinking) = object.get_mut("thinking").and_then(Value::as_object_mut) else {
        return;
    };
    let display = match thinking.get("display").and_then(Value::as_str) {
        None => return,
        Some("summarized" | "auto" | "concise" | "detailed") => Some("summarized"),
        Some("omitted" | "none" | "disabled" | "hidden") => Some("omitted"),
        Some("updates") => Some("updates"),
        Some(_) => None,
    };
    match display {
        Some(display) => thinking.insert("display".into(), Value::String(display.into())),
        None => thinking.remove("display"),
    };
}

fn cap_at_high(effort: &'static str) -> &'static str {
    if effort_rank(effort) > effort_rank("high") {
        "high"
    } else {
        effort
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_summary_maps_to_official_display_and_preserves_implicit_intent() {
        use stravia_runtime_contract::protocol::ir::ReasoningSummary;
        for (summary, expected) in [
            (ReasoningSummary::None, "omitted"),
            (ReasoningSummary::Auto, "summarized"),
            (ReasoningSummary::Concise, "summarized"),
            (ReasoningSummary::Detailed, "summarized"),
        ] {
            let display = summary_display(&summary);
            assert_eq!(display, expected);
            let mut body = json!({"model":"claude-opus-5-5","max_tokens":1024});
            preserve_implicit_summary_intent(
                &mut body,
                &ModelCapabilities::default(),
                Some(display),
            );
            normalize(
                body.as_object_mut().unwrap(),
                &ModelCapabilities::default(),
                false,
            )
            .unwrap();
            assert_eq!(
                body["thinking"],
                json!({"type":"adaptive","display":expected})
            );
        }
        let mut body = json!({"model":"claude-opus-5-5","thinking":{"type":"disabled"}});
        preserve_implicit_summary_intent(&mut body, &ModelCapabilities::default(), Some("omitted"));
        assert_eq!(body["thinking"], json!({"type":"disabled"}));
    }

    fn run(model: &str, extra: Value) -> Value {
        run_with(model, extra, &ModelCapabilities::default())
    }

    /// 未配置输出上限的请求体：`max_tokens` 缺省。
    fn run_with(model: &str, extra: Value, caps: &ModelCapabilities) -> Value {
        let mut body = json!({
            "model": model,
            "messages": [{"role": "user", "content": "hi"}],
        });
        body.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        normalize(body.as_object_mut().unwrap(), caps, false).unwrap();
        body
    }

    fn run_with_limit(
        model: &str,
        max_tokens: u64,
        extra: Value,
        caps: &ModelCapabilities,
    ) -> Value {
        let mut extra = extra;
        extra["max_tokens"] = json!(max_tokens);
        run_with(model, extra, caps)
    }

    fn adaptive(effort: &str) -> Value {
        json!({"thinking": {"type": "adaptive"}, "output_config": {"effort": effort}})
    }

    #[test]
    fn revision_parsing_ignores_date_suffixes() {
        let parse = parse_known;
        assert_eq!(parse("claude-opus-5-5"), Some((Family::Opus, [5, 5, 0])));
        assert_eq!(parse("claude-opus-4.8"), Some((Family::Opus, [4, 8, 0])));
        assert_eq!(
            parse("claude-opus-4-1-20250805"),
            Some((Family::Opus, [4, 1, 0]))
        );
        assert_eq!(
            parse("claude-3-opus-20240229"),
            Some((Family::Opus, [0, 0, 0]))
        );
        assert!(parse("claude-mythos-preview").is_none());
        assert!(parse("gpt-5").is_none());
    }

    #[test]
    fn adaptive_only_models_turn_budgets_into_effort_and_summarized_display() {
        let body = run(
            "claude-opus-4-7",
            json!({"thinking": {"type": "enabled", "budget_tokens": 8192}}),
        );
        assert_eq!(
            body["thinking"],
            json!({"type": "adaptive", "display": "summarized"})
        );
        assert_eq!(body["output_config"]["effort"], "medium");

        let body = run(
            "claude-sonnet-5",
            json!({"thinking": {"type": "adaptive", "display": "omitted"}, "output_config": {"effort": "xhigh"}}),
        );
        assert_eq!(
            body["thinking"],
            json!({"type": "adaptive", "display": "omitted"})
        );
        assert_eq!(body["output_config"]["effort"], "xhigh");

        // 4.6 不默认请求摘要，且客户端明确的预算原样保留。
        let body = run("claude-opus-4-6", adaptive("low"));
        assert_eq!(body["thinking"], json!({"type": "adaptive"}));
        let body = run(
            "claude-opus-4-6",
            json!({"thinking": {"type": "enabled", "budget_tokens": 3000}}),
        );
        assert_eq!(body["thinking"]["budget_tokens"], 3000);
    }

    #[test]
    fn manual_only_models_turn_effort_into_budget() {
        let body = run("claude-sonnet-4-5", adaptive("high"));
        assert_eq!(
            body["thinking"],
            json!({"type": "enabled", "budget_tokens": 16384})
        );
        assert!(
            body.get("max_tokens").is_none(),
            "thinking budget must not become an output limit"
        );
        assert!(
            body.get("output_config").is_none(),
            "no effort on Sonnet 4.5"
        );

        // Opus 4.5 的 effort 与预算并存，超出其档位的取最高受支持档。
        let body = run("claude-opus-4-5-20251101", adaptive("max"));
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["output_config"]["effort"], "high");

        // `Enabled` 没有预算时补最小预算。
        let body = run("claude-haiku-4-5", json!({"thinking": {"type": "enabled"}}));
        assert_eq!(body["thinking"]["budget_tokens"], 1024);
    }

    #[test]
    fn normalize_does_not_invent_an_output_limit() {
        // 客户端给出预算但没有输出上限：预算原样发送，不补 max_tokens。
        // 必填回退属于编码器，normalize 不重新发明默认值。
        let body = run_with(
            "claude-sonnet-4-5",
            json!({"thinking":{"type":"enabled","budget_tokens":16000}}),
            &caps(false, true, &[]),
        );
        assert_eq!(body["thinking"]["budget_tokens"], 16000);
        assert!(body.get("max_tokens").is_none());
    }

    #[test]
    fn explicit_output_limit_keeps_room_above_the_thinking_budget() {
        // 显式上限低于预算加正文缓冲时抬高到下限。
        let body = run_with_limit(
            "claude-sonnet-4-5",
            2048,
            json!({"thinking":{"type":"enabled","budget_tokens":16000}}),
            &caps(false, true, &[]),
        );
        assert_eq!(body["thinking"]["budget_tokens"], 16000);
        assert_eq!(body["max_tokens"], 16000 + OUTPUT_BUFFER);

        // 已足够的显式上限不改动。
        let body = run_with_limit(
            "claude-haiku-4-5",
            64000,
            json!({"thinking": {"type": "enabled"}}),
            &ModelCapabilities::default(),
        );
        assert_eq!(body["thinking"]["budget_tokens"], 1024);
        assert_eq!(body["max_tokens"], 64000);

        // 恰好等于下限时保持原值。
        let body = run_with_limit(
            "claude-sonnet-4-5",
            16000 + OUTPUT_BUFFER,
            json!({"thinking":{"type":"enabled","budget_tokens":16000}}),
            &caps(false, true, &[]),
        );
        assert_eq!(body["max_tokens"], 16000 + OUTPUT_BUFFER);
    }

    #[test]
    fn automatic_output_limit_is_kept_and_the_budget_fits_below_it() {
        // 发现到的型号上限：预算超出时压到上限内，并保留正文缓冲。
        let mut body = json!({
            "model": "claude-sonnet-4-5",
            "max_tokens": 32000,
            "thinking": {"type": "enabled", "budget_tokens": 63000},
        });
        normalize(body.as_object_mut().unwrap(), &caps(false, true, &[]), true).unwrap();
        assert_eq!(body["max_tokens"], 32000);
        assert_eq!(body["thinking"]["budget_tokens"], 32000 - OUTPUT_BUFFER);

        // 预算已能放下时原样保留。
        let mut body = json!({
            "model": "claude-sonnet-4-5",
            "max_tokens": 32000,
            "thinking": {"type": "enabled", "budget_tokens": 16000},
        });
        normalize(body.as_object_mut().unwrap(), &caps(false, true, &[]), true).unwrap();
        assert_eq!(body["max_tokens"], 32000);
        assert_eq!(body["thinking"]["budget_tokens"], 16000);

        // 上限放不下缓冲加最小预算时保留最小合法预算，而不是抬高上限。
        let mut body = json!({
            "model": "claude-sonnet-4-5",
            "max_tokens": 3000,
            "thinking": {"type": "enabled", "budget_tokens": 16000},
        });
        normalize(body.as_object_mut().unwrap(), &caps(false, true, &[]), true).unwrap();
        assert_eq!(body["max_tokens"], 3000);
        assert_eq!(body["thinking"]["budget_tokens"], MIN_BUDGET);

        // 上限无法容纳最小预算时明确失败，不能静默关闭思考或抬高上限。
        let mut body = json!({
            "model": "claude-sonnet-4-5",
            "max_tokens": MIN_BUDGET,
            "thinking": {"type": "adaptive"},
        });
        assert!(normalize(body.as_object_mut().unwrap(), &caps(false, true, &[]), true).is_err());
    }

    #[test]
    fn switching_thinking_off_follows_each_model_generation() {
        let off = json!({"thinking": {"type": "disabled"}});
        for model in ["claude-opus-5-5", "claude-fable-5", "claude-mythos-5-1"] {
            let body = run(model, off.clone());
            assert!(body.get("thinking").is_none(), "{model} cannot be disabled");
            assert_eq!(body["output_config"]["effort"], "low");
        }
        let body = run(
            "claude-sonnet-5-5",
            json!({"thinking": {"type": "disabled"}, "output_config": {"effort": "max"}}),
        );
        assert_eq!(body["thinking"], json!({"type": "between_tools"}));
        assert_eq!(body["output_config"]["effort"], "high");
        let body = run(
            "claude-opus-5",
            json!({"thinking": {"type": "disabled"}, "output_config": {"effort": "xhigh"}}),
        );
        assert_eq!(body["thinking"], json!({"type": "disabled"}));
        assert_eq!(body["output_config"]["effort"], "high");
        for model in ["claude-opus-4-8", "claude-sonnet-5", "claude-haiku-4-5"] {
            assert_eq!(
                run(model, off.clone())["thinking"],
                json!({"type": "disabled"})
            );
        }
    }

    #[test]
    fn effort_none_means_off_and_is_never_sent() {
        let body = run("claude-opus-4-7", adaptive("none"));
        assert_eq!(body["thinking"], json!({"type": "disabled"}));
        assert!(body.get("output_config").is_none());
    }

    #[test]
    fn effort_is_clamped_to_the_model_ladder() {
        assert_eq!(
            run("claude-opus-4-6", adaptive("xhigh"))["output_config"]["effort"],
            "high"
        );
        assert_eq!(
            run("claude-opus-4-6", adaptive("max"))["output_config"]["effort"],
            "max"
        );
        assert_eq!(
            run("claude-opus-5", adaptive("minimal"))["output_config"]["effort"],
            "low"
        );
        assert!(
            run("claude-opus-5", adaptive("adaptive"))
                .get("output_config")
                .is_none()
        );
        // 未设置思考、只有 effort 时同样按档位处理。
        let body = run(
            "claude-haiku-4-5",
            json!({"output_config": {"effort": "low"}}),
        );
        assert!(body.get("output_config").is_none());
    }

    #[test]
    fn forced_tool_choice_never_meets_incompatible_thinking() {
        let forced = |extra: Value| {
            let mut extra = extra;
            extra["tool_choice"] = json!({"type": "tool", "name": "read"});
            extra
        };
        // 不支持强制工具调用的型号降级为 auto，思考保持。
        let body = run("claude-opus-5-5", forced(adaptive("high")));
        assert_eq!(body["tool_choice"], json!({"type": "auto"}));
        assert_eq!(body["thinking"]["type"], "adaptive");
        // 手动预算与强制工具互斥：能换自适应就换，否则去掉思考。
        let manual = json!({"thinking": {"type": "enabled", "budget_tokens": 2048}});
        let body = run("claude-opus-4-6", forced(manual.clone()));
        assert_eq!(body["thinking"]["type"], "adaptive");
        assert_eq!(body["tool_choice"]["type"], "tool");
        let body = run(
            "claude-sonnet-4-5",
            forced(json!({
                "thinking": {"type": "enabled", "budget_tokens": 2048},
                "context_management": {"edits": [{"type": CLEAR_THINKING_EDIT, "keep": "all"}]}
            })),
        );
        assert!(body.get("thinking").is_none());
        assert!(body.get("context_management").is_none());
        // 自适应思考允许强制工具调用。
        let body = run("claude-opus-5", forced(adaptive("high")));
        assert_eq!(body["tool_choice"]["type"], "tool");
        assert_eq!(body["thinking"]["type"], "adaptive");
    }

    #[test]
    fn display_only_carries_values_anthropic_accepts() {
        // 回归：open-responses 的 `reasoning.summary: "auto"` 被当作 display 发出，上游 400。
        let with = |model: &str, display: &str| {
            run(
                model,
                json!({"thinking": {"type": "adaptive", "display": display}, "output_config": {"effort": "low"}}),
            )["thinking"]
                .clone()
        };
        for summary in ["auto", "concise", "detailed", "summarized"] {
            assert_eq!(
                with("claude-opus-5-5", summary),
                json!({"type": "adaptive", "display": "summarized"})
            );
        }
        assert_eq!(
            with("claude-opus-5-5", "omitted"),
            json!({"type": "adaptive", "display": "omitted"})
        );
        // 无法表示的取值被丢弃，Opus 4.6 没有默认摘要，因此不带 display。
        assert_eq!(
            with("claude-opus-4-6", "updates"),
            json!({"type": "adaptive", "display":"updates"})
        );
        // 型号未识别时同样清洗，display 属于协议层约束。
        assert_eq!(
            with("some-proxy-model", "auto"),
            json!({"type": "adaptive", "display": "summarized"})
        );
    }

    fn caps(adaptive: bool, enabled: bool, efforts: &[&'static str]) -> ModelCapabilities {
        ModelCapabilities {
            thinking: Some(adaptive || enabled),
            adaptive: Some(adaptive),
            enabled: Some(enabled),
            efforts: Some(efforts.iter().map(|effort| (*effort).to_owned()).collect()),
            ..ModelCapabilities::default()
        }
    }

    #[test]
    fn discovered_capabilities_cover_models_the_static_table_does_not_know() {
        // 型号 ID 无法识别，仅凭 /v1/models 数据即可正确改写。
        let body = run_with(
            "claude-nova-1",
            json!({"thinking": {"type": "enabled", "budget_tokens": 8192}, "temperature": 0}),
            &caps(true, false, &["low", "high"]),
        );
        assert_eq!(body["thinking"]["type"], "adaptive");
        // 预算 8192 对应 medium，钳到型号最高不超过它的档位 low。
        assert_eq!(body["output_config"]["effort"], "low");
        let body = run_with(
            "claude-nova-1",
            adaptive("max"),
            &caps(true, false, &["low", "high"]),
        );
        assert_eq!(body["output_config"]["effort"], "high");
    }

    #[test]
    fn discovered_capabilities_override_the_static_table() {
        // 静态表认为 Opus 4.6 两种模式都支持；发现数据说只剩 adaptive，则预算改成 effort。
        let body = run_with(
            "claude-opus-4-6",
            json!({"thinking": {"type": "enabled", "budget_tokens": 8192}}),
            &caps(true, false, &["low", "medium", "high"]),
        );
        assert_eq!(body["thinking"]["type"], "adaptive");
        assert_eq!(body["output_config"]["effort"], "medium");

        // 发现数据里没有 xhigh，即使静态表认为该型号有。
        let body = run_with(
            "claude-opus-4-7",
            adaptive("xhigh"),
            &caps(true, false, &["low", "medium", "high", "max"]),
        );
        assert_eq!(body["output_config"]["effort"], "high");
    }

    #[test]
    fn models_without_thinking_lose_every_thinking_control() {
        let no_thinking = ModelCapabilities {
            thinking: Some(false),
            adaptive: Some(false),
            enabled: Some(false),
            efforts: Some(Vec::new()),
            ..ModelCapabilities::default()
        };
        let body = run_with(
            "claude-haiku-3",
            json!({
                "thinking": {"type": "adaptive"},
                "output_config": {"effort": "high"},
                "context_management": {"edits": [{"type": "clear_thinking_20251015"}]},
            }),
            &no_thinking,
        );
        for field in ["thinking", "output_config", "context_management"] {
            assert!(body.get(field).is_none(), "{field} must be removed");
        }
    }

    #[test]
    fn unrecognised_models_are_left_alone() {
        let extra = json!({"thinking": {"type": "enabled"}, "output_config": {"effort": "none"}});
        let body = run("some-proxy-model", extra.clone());
        assert_eq!(body["thinking"], extra["thinking"]);
        assert_eq!(body["output_config"], extra["output_config"]);
    }
}
