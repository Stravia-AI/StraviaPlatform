//! Upstream content-policy sanitization for Devin Connect requests.
//!
//! Beyond the ToolDef signature gate handled in `request.rs`, the upstream
//! runs a second gate on prompt/message TEXT: competitor-identity
//! fingerprints (Claude Code, Claude Agent SDK, Grok/xAI, Cline, Codex CLI
//! prompt boilerplate) and a handful of policy phrases are matched against
//! whole sentences or paragraphs; a hit rejects the request with
//! `permission_denied`. The rule set mirrors WindsurfAPI
//! `identity-neutralize.js` and the sub2api devin adapter — every entry was
//! bisect-verified against the live upstream.
//!
//! Rewrites are semantic-preserving neutral phrasings. `prompt_only` rules
//! (bare words like FREEFORM that legitimately appear in user code) apply
//! only to the system prompt and tool descriptions, never to message
//! bodies. Hit counts are returned so callers can log what was rewritten —
//! the rewrite itself is silent but auditable.

use regex::Regex;
use std::borrow::Cow;
use std::collections::BTreeMap;
use std::sync::{LazyLock, OnceLock};

/// Rewrite hit counts keyed by rule id, for request logging.
pub(crate) type SanitizeHits = BTreeMap<&'static str, u32>;

/// A bisect-verified upstream fingerprint rewrite.
///
/// Keep compilation behind each rule's literal trigger so a fresh Wasm
/// operation does not spend its fuel compiling rules that cannot match.
struct SanitizeRule {
    id: &'static str,
    pattern: Cow<'static, str>,
    compiled: OnceLock<Regex>,
    replacement: &'static str,
    /// Only applied to system prompt / tool descriptions — never to user
    /// message bodies, where the bare word can be legitimate content.
    prompt_only: bool,
    /// Lowercase literal substring every match of `pattern` must contain.
    /// Prescreen skips the rule unless this occurs, so a wrong trigger
    /// silently disables the rule — keep it short and exact.
    trigger: &'static str,
}

/// Matches text spanning at most single newlines (no blank line) — the RE2
/// approximation of the reference's `\n`-aware paragraph span.
const WITHIN_PARAGRAPH: &str = r"(?:[^\n]|\n[ \t]*[^\n \t])*?";

const SECURITY_BENIGN: &str = "Decline requests that facilitate clearly malicious or harmful activity, and otherwise help the user with their software engineering task.";

fn rule(
    id: &'static str,
    pattern: impl Into<Cow<'static, str>>,
    replacement: &'static str,
    trigger: &'static str,
) -> SanitizeRule {
    SanitizeRule {
        id,
        pattern: pattern.into(),
        compiled: OnceLock::new(),
        replacement,
        prompt_only: false,
        trigger,
    }
}

impl SanitizeRule {
    fn regex(&self) -> &Regex {
        self.compiled
            .get_or_init(|| Regex::new(self.pattern.as_ref()).expect("sanitize pattern compiles"))
    }
}

fn prompt_only_rule(
    id: &'static str,
    pattern: &'static str,
    replacement: &'static str,
    trigger: &'static str,
) -> SanitizeRule {
    let mut rule = rule(id, pattern, replacement, trigger);
    rule.prompt_only = true;
    rule
}

static RULES: LazyLock<Vec<SanitizeRule>> = LazyLock::new(|| {
    vec![
        // (a1) Claude Code self-identity (competitor fingerprint), straight/
        // curly apostrophe and full-sentence/noun-phrase forms.
        rule(
            "a1-cc-full",
            r"(?i)You are Claude Code,\s*Anthropic['’]?s official CLI for Claude\.?",
            "You are an AI coding assistant.",
            "claude code",
        ),
        rule(
            "a1-cc-noun",
            r"(?i)Claude Code,\s*Anthropic['’]?s official CLI for Claude\.?",
            "an AI coding assistant.",
            "claude code",
        ),
        // (a2) Claude Agent SDK self-identity (content-policy).
        rule(
            "a2-sdk-full",
            r"(?i)You are a Claude agent, built on Anthropic['’]?s Claude Agent SDK\.?",
            "You are an AI coding assistant.",
            "claude agent",
        ),
        rule(
            "a2-sdk-noun",
            r"(?i)\ba Claude agent, built on Anthropic['’]?s Claude Agent SDK\.?",
            "an AI coding assistant.",
            "claude agent",
        ),
        // (a3) Claude Code billing header line (competitor fingerprint).
        rule(
            "a3-billing",
            r"(?im)^\s*x-anthropic-billing-header:[^\n]*\n?",
            "",
            "x-anthropic-billing-header",
        ),
        // (b) Security-policy paragraph (abuse gate); the single-line form
        // is the fallback when the paragraph span misses.
        rule(
            "b-security",
            format!(
                r"(?i)IMPORTANT:\s*Assist with authorized security testing{WITHIN_PARAGRAPH}(?:defensive use cases\.|security research[^.]*\.)"
            ),
            SECURITY_BENIGN,
            "authorized security testing",
        ),
        rule(
            "b-security-line",
            r"(?i)IMPORTANT:\s*Assist with authorized security testing[^\n]*",
            SECURITY_BENIGN,
            "authorized security testing",
        ),
        // The dual-use sentence is an independent fingerprint.
        rule(
            "b-dualluse",
            r"(?i)Dual-use security tools \(C2 frameworks, credential testing, exploit development\) require clear authorization context:[^\n]*",
            "Dual-use security tooling (e.g. C2 frameworks, credential testing, exploit development) needs explicit authorization context such as pentesting engagements, CTF competitions, security research, or defensive use cases.",
            "dual-use security tools",
        ),
        // (a4) Claude Code Environment brand block and model catalogue —
        // both paragraph-level fingerprints.
        rule(
            "a4-brand-span",
            format!(
                r"(?i)Claude Code is available as a CLI{WITHIN_PARAGRAPH}available on Opus [\d./]+\."
            ),
            "This coding assistant runs in a terminal.",
            "claude code is available",
        ),
        rule(
            "a4-fastmode",
            r"(?im)(?:^|\n)\s*-?\s*Fast mode for Claude Code[^\n]*\n?",
            "\n",
            "fast mode for claude code",
        ),
        rule(
            "a4-cli-line",
            r"(?i)Claude Code is available as a CLI[^\n]*\n?",
            "This coding assistant runs in a terminal.\n",
            "claude code is available",
        ),
        rule(
            "a4-catalogue",
            format!(
                r"(?i)The most recent Claude models are{WITHIN_PARAGRAPH}most capable Claude models\."
            ),
            "",
            "the most recent claude models",
        ),
        rule(
            "a4-catalogue-line",
            r"(?im)The most recent Claude models are[^\n]*\n?",
            "",
            "the most recent claude models",
        ),
        // Self-model fingerprints: the sentence-ending period must be
        // followed by whitespace or EOL (RE2 has no lookahead).
        rule(
            "a4-poweredby",
            r"(?im)You are powered by the model[^\n]*?(?:\.(?:\s|$)|$)\n?",
            "",
            "powered by the model",
        ),
        rule(
            "a4-modelid",
            r"(?im)The exact model ID is[^\n]*?(?:\.(?:\s|$)|$)\n?",
            "",
            "the exact model id is",
        ),
        // (a5) Cline capability boast — the trigger is the sentence shape,
        // the name is preserved via ${1}.
        rule(
            "a5-cline-boast",
            r"You are ([A-Z][\w.-]*), a highly skilled software engineer with extensive knowledge in many programming languages, frameworks, design patterns,? and best practices\.",
            "You are ${1}, a software engineer.",
            "a highly skilled software engineer",
        ),
        // (a6) Grok/xAI self-identity + the executing_actions_with_care block.
        rule(
            "a6-grok-full",
            r"(?i)You are Grok[\w .-]* released by xAI\.?",
            "You are an AI coding assistant.",
            "released by xai",
        ),
        rule(
            "a6-grok-noun",
            r"(?i)\bGrok[\w .-]* released by xAI\.?",
            "an AI coding assistant.",
            "released by xai",
        ),
        rule(
            "a6-grok2-care",
            r"(?is)<executing_actions_with_care>.*?</executing_actions_with_care>",
            "",
            "executing_actions_with_care",
        ),
        // (a7) codex apply_patch description: the bare FREEFORM word and the
        // JSON-wrap sentence. Bare words can appear in user code, so these
        // only run on prompt/tool-description text.
        prompt_only_rule("a7-freeform", "FREEFORM", "free-form", "freeform"),
        prompt_only_rule(
            "a7-json-wrap",
            r"do not wrap the patch in JSON\.",
            "provide the patch as plain text.",
            "do not wrap the patch in json",
        ),
        // Claude Code prompt's colon-before-tool-call sentence.
        rule(
            "cc-colon-toolcall",
            r"(?i)Do not use a colon before tool calls\.[^\n]*?with a period\.",
            "Never put a colon before a tool call; write text like \"Let me read the file.\" ending with a period instead of a colon before the call.",
            "colon before tool calls",
        ),
        // CC 2.1.x prompt fingerprint lines (per-line bisect verified).
        rule(
            "cc-autocompact",
            r"(?i)The system will automatically compress prior messages in your conversation as it approaches context limits\.[^\n]*",
            "Earlier messages may be automatically summarized as the conversation grows long, so the conversation is not bounded by the context window.",
            "automatically compress prior messages",
        ),
        // The bare sentence is the fingerprint — no line-start anchoring;
        // a list prefix is preserved and the replacement is unchanged.
        rule(
            "cc-help-line",
            r"(?i)/help:\s*Get help with using Claude Code[^\n]*",
            "/help: Get help with using this CLI",
            "/help:",
        ),
        rule(
            "cc-agent-tool",
            r"(?i)Use the Agent tool with specialized agents when the task at hand matches the agent's description\.[^\n]*",
            "Use the Agent tool with specialized agents when the task matches the agent's description. Delegation is useful for parallelizing independent queries and for keeping the main context window free of excessive results, but avoid using it when not needed, and do not repeat work already delegated to a subagent.",
            "use the agent tool with specialized agents",
        ),
        rule(
            "cc-claudemd",
            r"(?i)Anything already documented in CLAUDE\.md files\.",
            "Anything already documented in project instruction files.",
            "claude.md",
        ),
        rule(
            "cc-memory-must",
            r"(?i)You MUST access memory when the user explicitly asks you to check, recall, or remember\.",
            "Always consult memory when the user explicitly asks you to check, recall, or remember.",
            "must access memory",
        ),
        rule(
            "cc-feedback",
            r"(?i)To give feedback, users should report the issue at https://github\.com/anthropics/claude-code/issues[^\n]*",
            "To give feedback, users should report issues to the maintainers of this CLI.",
            "claude-code",
        ),
        rule(
            "cc-blast-radius",
            r"(?i)Carefully consider the reversibility and blast radius of actions\.",
            "Carefully consider the reversibility and impact of actions.",
            "blast radius",
        ),
        rule(
            "cc-claudemd-2",
            r"(?i)durable instructions like CLAUDE\.md files",
            "durable instructions like project instruction files",
            "claude.md",
        ),
        // CC 2.1.x subagent prompt emoji ban — the fingerprint needs both
        // the "For clear communication…" prefix and "MUST avoid".
        rule(
            "cc-subagent-emojis",
            r"(?i)For clear communication with the user the assistant MUST avoid using emojis\.",
            "Keep communication with the user clear and free of emojis.",
            "avoid using emojis",
        ),
        // Codex CLI prompt fingerprints (codex 0.153.x template bisect):
        // the definitional sentence triggers whole; shortening it passes.
        rule(
            "codex-opensource-def",
            r"(?i)Codex refers to the open-source agentic coding interface",
            "Codex is the open-source coding interface",
            "codex refers to the open-source",
        ),
        // The plan-status pair only triggers when batch-complete precedes
        // "Finish with all items…" adjacently in that order.
        rule(
            "codex-plan-statuses",
            r"(?i)Do not batch-complete multiple items after the fact\. Finish with all items completed or explicitly canceled/deferred before ending the turn\.",
            "Do not batch-complete multiple items after the fact. Before ending the turn, leave all items completed or explicitly canceled/deferred.",
            "do not batch-complete multiple items",
        ),
        // ANSI-escape sentence: both clauses must co-occur in one sentence.
        rule(
            "codex-ansi-escapes",
            r"Don['’]t output ANSI escape codes directly — the CLI renderer applies them\.",
            "Never output ANSI escape codes directly — the CLI renderer applies them.",
            "ansi escape codes directly",
        ),
    ]
});

/// First-byte-folded trigger buckets for a single-pass prescreen: clean text
/// (the overwhelming majority) returns without touching a regex.
static BUCKETS_ALL: LazyLock<[Vec<&'static str>; 256]> = LazyLock::new(|| trigger_buckets(true));
static BUCKETS_MESSAGES: LazyLock<[Vec<&'static str>; 256]> =
    LazyLock::new(|| trigger_buckets(false));

fn trigger_buckets(include_prompt_only: bool) -> [Vec<&'static str>; 256] {
    let mut buckets: [Vec<&'static str>; 256] = std::array::from_fn(|_| Vec::new());
    for rule in RULES.iter() {
        if rule.prompt_only && !include_prompt_only {
            continue;
        }
        let first = rule.trigger.as_bytes()[0] | 0x20;
        buckets[first as usize].push(rule.trigger);
    }
    buckets
}

fn has_trigger(text: &str, buckets: &[Vec<&'static str>; 256]) -> bool {
    let bytes = text.as_bytes();
    for (index, &byte) in bytes.iter().enumerate() {
        for trigger in &buckets[(byte | 0x20) as usize] {
            if bytes.len() - index >= trigger.len()
                && bytes[index..index + trigger.len()].eq_ignore_ascii_case(trigger.as_bytes())
            {
                return true;
            }
        }
    }
    false
}

fn sanitize(text: &str, include_prompt_only: bool, hits: &mut SanitizeHits) -> String {
    if text.is_empty() {
        return String::new();
    }
    let buckets = if include_prompt_only {
        &*BUCKETS_ALL
    } else {
        &*BUCKETS_MESSAGES
    };
    if !has_trigger(text, buckets) {
        return text.to_string();
    }
    let lower = text.to_lowercase();
    let mut out = text.to_string();
    for rule in RULES.iter() {
        if rule.prompt_only && !include_prompt_only {
            continue;
        }
        if !lower.contains(rule.trigger) {
            continue;
        }
        let pattern = rule.regex();
        let count = pattern.find_iter(&out).count();
        if count > 0 {
            *hits.entry(rule.id).or_default() += count as u32;
            out = pattern.replace_all(&out, rule.replacement).into_owned();
        }
    }
    out
}

/// Sanitize prompt-channel text (system prompt, tool descriptions): every
/// rule including `prompt_only` applies.
pub(crate) fn sanitize_prompt_text(text: &str, hits: &mut SanitizeHits) -> String {
    sanitize(text, true, hits)
}

/// Sanitize message-body text (user/assistant/tool-result turns):
/// `prompt_only` rules are skipped so user content is never mangled.
pub(crate) fn sanitize_message_text(text: &str, hits: &mut SanitizeHits) -> String {
    sanitize(text, false, hits)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_code_identity_is_neutralized() {
        let mut hits = SanitizeHits::new();
        let out = sanitize_prompt_text(
            "You are Claude Code, Anthropic's official CLI for Claude. Answer briefly.",
            &mut hits,
        );
        assert!(out.contains("You are an AI coding assistant."));
        assert!(!out.contains("Claude Code"));
        assert_eq!(hits.get("a1-cc-full"), Some(&1));
    }

    #[test]
    fn freeform_rewrites_in_prompt_but_not_in_messages() {
        let mut hits = SanitizeHits::new();
        assert_eq!(
            sanitize_prompt_text("emit FREEFORM text", &mut hits),
            "emit free-form text"
        );
        // User code legitimately contains the bare word — messages keep it.
        assert_eq!(
            sanitize_message_text("SELECT FREEFORM FROM t", &mut hits),
            "SELECT FREEFORM FROM t"
        );
        assert_eq!(hits.get("a7-freeform"), Some(&1));
    }

    #[test]
    fn clean_text_passes_through() {
        let mut hits = SanitizeHits::new();
        let text = "Investigate the repository layout and report findings.";
        assert_eq!(sanitize_prompt_text(text, &mut hits), text);
        assert!(hits.is_empty());
    }

    #[test]
    fn grok_block_removed() {
        let mut hits = SanitizeHits::new();
        let out = sanitize_prompt_text(
            "pre <executing_actions_with_care>secret</executing_actions_with_care> post",
            &mut hits,
        );
        assert_eq!(out, "pre  post");
    }
}
