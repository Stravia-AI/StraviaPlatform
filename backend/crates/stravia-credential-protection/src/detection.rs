//! Local Betterleaks snapshot: 95237cf8eb4d8e9f67409595b245e674832992cf.
//!
//! Each model-text field is one virtual source with an empty trusted path. File
//! rules and path filters are compiled and evaluated, never inferred from text.
//! File-only findings therefore cannot identify a secret in a model-text field.
//! Inline `gitleaks:allow` comments are not honored: model text is untrusted.
//! No decoding passes or network validation are performed. Component context
//! never crosses model-text field boundaries. Report-only confidence mutations
//! are evaluated but do not gate local protection.
#[path = "detection/expression.rs"]
mod expression;

use super::RedactionError;
use expression::{Context, Program};
use regex::bytes::Regex;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

#[derive(Clone, Serialize)]
pub struct CredentialRuleCatalog {
    pub rules: Vec<CredentialRule>,
    pub prefilter: String,
    pub filter: String,
}

#[derive(Clone, Serialize)]
pub struct CredentialRule {
    pub id: String,
    pub name: String,
    pub target: String,
    pub description: String,
    pub regex: Option<String>,
    pub path: Option<String>,
    pub secret_group: usize,
    pub keywords: Vec<String>,
    pub filter: String,
    pub components: Vec<CredentialRuleComponent>,
    pub skip_report: bool,
    pub specificity: i64,
    pub confidence: String,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CredentialRuleComponent {
    pub id: String,
    #[serde(default)]
    pub within: String,
    #[serde(default)]
    pub optional: bool,
}

#[derive(Serialize)]
pub struct CredentialMatch {
    pub rule_id: String,
    pub start: usize,
    pub end: usize,
    pub start_line: usize,
    pub start_column: usize,
    pub end_line: usize,
    pub end_column: usize,
}

pub(super) struct DetectedCredential {
    pub secret: String,
    pub findings: Vec<DetectedFinding>,
}

pub(super) struct DetectedFinding {
    pub rule_id: String,
    pub source_index: usize,
    pub start: usize,
    pub end: usize,
}

fn detector() -> Result<&'static Detector> {
    static DETECTOR: OnceLock<Result<Detector>> = OnceLock::new();
    DETECTOR
        .get_or_init(Detector::new)
        .as_ref()
        .map_err(|error| *error)
}

pub async fn rule_catalog() -> Result<CredentialRuleCatalog> {
    tokio::task::spawn_blocking(|| Ok(detector()?.catalog.clone()))
        .await
        .map_err(|_| RedactionError::Detection)?
}

pub(super) async fn detect(texts: Vec<String>) -> Result<Vec<DetectedCredential>> {
    tokio::task::spawn_blocking(move || {
        detector()?.detect(&texts.iter().map(String::as_str).collect::<Vec<_>>())
    })
    .await
    .map_err(|_| RedactionError::Detection)?
}

pub async fn test_text(text: String) -> Result<Vec<CredentialMatch>> {
    tokio::task::spawn_blocking(move || {
        let mut findings: Vec<_> = detector()?
            .detect(&[&text])?
            .into_iter()
            .flat_map(|credential| credential.findings)
            .collect();
        findings.sort_by(|a, b| (a.start, a.end, &a.rule_id).cmp(&(b.start, b.end, &b.rule_id)));
        let mut matches = Vec::with_capacity(findings.len());
        let mut previous_start = 0;
        let mut start_position = (0, 1, 1);
        for finding in findings {
            // 按位置前进，避免大量重复命中时反复扫描整段输入前缀。
            start_position = advance_position(&text[previous_start..finding.start], start_position);
            previous_start = finding.start;
            let (start, start_line, start_column) = start_position;
            let (end, end_line, end_column) =
                advance_position(&text[finding.start..finding.end], start_position);
            matches.push(CredentialMatch {
                rule_id: finding.rule_id,
                start,
                end,
                start_line,
                start_column,
                end_line,
                end_column,
            });
        }
        Ok(matches)
    })
    .await
    .map_err(|_| RedactionError::Detection)?
}

fn advance_position(
    text: &str,
    (mut offset, mut line, mut column): (usize, usize, usize),
) -> (usize, usize, usize) {
    for character in text.chars() {
        offset += character.len_utf16();
        if character == '\n' {
            line += 1;
            column = 1;
        } else {
            column += 1;
        }
    }
    (offset, line, column)
}

type Result<T, E = RedactionError> = std::result::Result<T, E>;

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct Snapshot {
    title: String,
    min_version: String,
    betterleaks_min_version: String,
    prefilter: String,
    filter: String,
    rules: Vec<SourceRule>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct SourceRule {
    id: String,
    description: String,
    #[serde(default)]
    regex: Option<String>,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    secret_group: usize,
    #[serde(default)]
    keywords: Vec<String>,
    #[serde(default)]
    filter: String,
    #[serde(default)]
    validate: String,
    #[serde(default)]
    components: Vec<CredentialRuleComponent>,
    #[serde(default)]
    skip_report: bool,
    #[serde(default = "default_specificity")]
    specificity: i64,
    confidence: String,
}
fn default_specificity() -> i64 {
    100
}
struct Component {
    rule: usize,
    lines: Option<usize>,
    columns: Option<usize>,
    optional: bool,
}
struct Rule {
    regex: Option<Regex>,
    path: Option<Regex>,
    secret_group: usize,
    keywords: Vec<String>,
    filter: Program,
    components: Vec<Component>,
    skip_report: bool,
    specificity: i64,
}

pub(super) struct Detector {
    catalog: CredentialRuleCatalog,
    rules: Vec<Rule>,
    order: Vec<usize>,
    prefilter: Program,
    filter: Program,
    tokenizer: tiktoken_rs::CoreBPE,
    words: HashSet<&'static str>,
    word_lengths: Vec<usize>,
}
#[derive(Clone)]
struct Finding<'a> {
    rule: usize,
    secret: &'a str,
    start: usize,
    end: usize,
    start_line: usize,
    end_line: usize,
    column: usize,
    end_column: usize,
}
struct Composite<'a> {
    primary: Finding<'a>,
    components: Vec<Finding<'a>>,
}

impl Detector {
    pub(super) fn new() -> Result<Self> {
        let snapshot: Snapshot = toml::from_str(include_str!("detection/betterleaks.toml"))
            .map_err(|_| RedactionError::Detection)?;
        if snapshot.title.is_empty()
            || snapshot.min_version.is_empty()
            || snapshot.betterleaks_min_version.is_empty()
        {
            return Err(RedactionError::Detection);
        }
        let mut ids = HashMap::with_capacity(snapshot.rules.len());
        for (index, rule) in snapshot.rules.iter().enumerate() {
            if rule.id.is_empty() || ids.insert(rule.id.as_str(), index).is_some() {
                return Err(RedactionError::Detection);
            }
        }
        let mut rules = Vec::with_capacity(snapshot.rules.len());
        for source in &snapshot.rules {
            if source.description.is_empty()
                || !matches!(source.confidence.as_str(), "low" | "medium" | "high")
            {
                return Err(RedactionError::Detection);
            }
            let regex = source.regex.as_deref().map(expression::regex).transpose()?;
            let path = source.path.as_deref().map(expression::regex).transpose()?;
            if regex.is_none() && path.is_none()
                || source.secret_group >= regex.as_ref().map_or(1, Regex::captures_len)
            {
                return Err(RedactionError::Detection);
            }
            let mut components = Vec::with_capacity(source.components.len());
            let mut component_ids = HashSet::new();
            for component in &source.components {
                let rule = *ids
                    .get(component.id.as_str())
                    .ok_or(RedactionError::Detection)?;
                if !component_ids.insert(rule) {
                    return Err(RedactionError::Detection);
                }
                let (lines, columns) = match component.within.as_str() {
                    "" => (None, None),
                    "5L" => (Some(4), None),
                    "8L" => (Some(7), None),
                    "10L" => (Some(9), None),
                    "20L" => (Some(19), None),
                    "30L" => (Some(29), None),
                    "7L,200C" => (Some(6), Some(200)),
                    _ => return Err(RedactionError::Detection),
                };
                components.push(Component {
                    rule,
                    lines,
                    columns,
                    optional: component.optional,
                });
            }
            // `validate` is deliberately retained in the verbatim asset but never
            // compiled: its network requests must not participate in local scanning.
            let _ = &source.validate;
            rules.push(Rule {
                regex,
                path,
                secret_group: source.secret_group,
                keywords: source.keywords.iter().map(|s| s.to_lowercase()).collect(),
                filter: Program::compile(&source.filter)?,
                components,
                skip_report: source.skip_report,
                specificity: source.specificity,
            });
        }
        let mut order: Vec<usize> = (0..rules.len()).collect();
        order.sort_by_key(|index| std::cmp::Reverse(rules[*index].specificity));
        let words: HashSet<_> = include_str!("detection/words.txt")
            .lines()
            .filter(|word| !word.is_empty())
            .collect();
        let word_lengths = words
            .iter()
            .map(|word| word.len())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        Ok(Self {
            rules,
            order,
            prefilter: Program::compile(&snapshot.prefilter)?,
            filter: Program::compile(&snapshot.filter)?,
            tokenizer: tiktoken_rs::cl100k_base().map_err(|_| RedactionError::Detection)?,
            words,
            word_lengths,
            catalog: CredentialRuleCatalog {
                prefilter: snapshot.prefilter,
                filter: snapshot.filter,
                rules: snapshot
                    .rules
                    .into_iter()
                    .map(|source| {
                        // 名称与检测对象直接来自规则身份，不维护第二份手工目录。
                        let name = source
                            .id
                            .split('-')
                            .map(|word| {
                                let mut chars = word.chars();
                                match chars.next() {
                                    Some(first) => {
                                        first.to_uppercase().collect::<String>() + chars.as_str()
                                    }
                                    None => String::new(),
                                }
                            })
                            .collect::<Vec<_>>()
                            .join(" ");
                        CredentialRule {
                            id: source.id,
                            target: name.clone(),
                            name,
                            description: source.description,
                            regex: source.regex,
                            path: source.path,
                            secret_group: source.secret_group,
                            keywords: source.keywords,
                            filter: source.filter,
                            components: source.components,
                            skip_report: source.skip_report,
                            specificity: source.specificity,
                            confidence: source.confidence,
                        }
                    })
                    .collect(),
            },
        })
    }

    pub(super) fn detect(&self, texts: &[&str]) -> Result<Vec<DetectedCredential>> {
        let mut unique = HashMap::new();
        let mut reported_positions = HashSet::new();
        let mut secrets: Vec<DetectedCredential> = Vec::new();
        for (source_index, raw) in texts.iter().enumerate() {
            let empty = self.context(raw, "", "", 0, 0);
            if self.prefilter.evaluate(&empty)? {
                continue;
            }
            let lower = raw.to_lowercase();
            let mut candidates: Vec<Option<Vec<Finding<'_>>>> =
                (0..self.rules.len()).map(|_| None).collect();
            let mut composites: Vec<Composite<'_>> = Vec::new();
            for &index in &self.order {
                let rule = &self.rules[index];
                if rule.skip_report
                    || !rule.keywords.is_empty()
                        && !rule.keywords.iter().any(|key| lower.contains(key))
                {
                    continue;
                }
                if candidates[index].is_none() {
                    candidates[index] = Some(self.find(raw, index)?);
                }
                for component in &rule.components {
                    // Upstream component scans bypass keyword routing and do not
                    // recursively require their own components or skipReport.
                    if candidates[component.rule].is_none() {
                        candidates[component.rule] = Some(self.find(raw, component.rule)?);
                    }
                }
                for primary in candidates[index].as_deref().unwrap_or_default() {
                    if self.suppressed(primary, &composites) {
                        continue;
                    }
                    let mut attached = Vec::new();
                    let mut satisfied = true;
                    for component in &rule.components {
                        let mut found = false;
                        for candidate in candidates[component.rule].as_deref().unwrap_or_default() {
                            if component.near(primary, candidate) {
                                found = true;
                                attached.push(candidate.clone());
                            }
                        }
                        if !found && !component.optional {
                            satisfied = false;
                            break;
                        }
                    }
                    if satisfied {
                        composites.push(Composite {
                            primary: primary.clone(),
                            components: attached,
                        });
                    }
                }
            }
            for composite in &composites {
                if self.suppressed(&composite.primary, &composites) {
                    continue;
                }
                for finding in std::iter::once(&composite.primary).chain(&composite.components) {
                    // Optional generic usernames are contextual identity evidence,
                    // not credential secrets; do not turn this into PII redaction.
                    if finding.rule != composite.primary.rule
                        && self.rules[composite.primary.rule]
                            .components
                            .iter()
                            .any(|c| c.rule == finding.rule && c.optional)
                    {
                        continue;
                    }
                    if finding.secret.is_empty() {
                        continue;
                    }
                    let credential_index = *unique.entry(finding.secret).or_insert_with(|| {
                        secrets.push(DetectedCredential {
                            secret: finding.secret.to_owned(),
                            findings: Vec::new(),
                        });
                        secrets.len() - 1
                    });
                    let findings = &mut secrets[credential_index].findings;
                    let rule_id = &self.catalog.rules[finding.rule].id;
                    if reported_positions.insert((
                        source_index,
                        finding.start,
                        finding.end,
                        finding.rule,
                    )) {
                        findings.push(DetectedFinding {
                            rule_id: rule_id.clone(),
                            source_index,
                            start: finding.start,
                            end: finding.end,
                        });
                    }
                }
            }
        }
        Ok(secrets)
    }

    fn context<'a>(
        &'a self,
        raw: &'a str,
        secret: &'a str,
        matched: &'a str,
        start: usize,
        end: usize,
    ) -> Context<'a> {
        let line_start = raw.as_bytes()[..start]
            .iter()
            .rposition(|b| matches!(b, b'\r' | b'\n'))
            .map_or(0, |i| i + 1);
        // Location uses the newline-trimmed match; fragment indexes retain
        // the original regex range, as required by Betterleaks filters.
        let location_end = start + matched.len();
        let location_line_end = raw.as_bytes()[location_end..]
            .iter()
            .position(|b| matches!(b, b'\r' | b'\n'))
            .map_or(raw.len(), |i| location_end + i);
        let line_end = if location_end == end {
            location_line_end
        } else {
            raw.as_bytes()[end..]
                .iter()
                .position(|b| matches!(b, b'\r' | b'\n'))
                .map_or(raw.len(), |i| end + i)
        };
        Context {
            raw,
            secret,
            matched,
            start,
            end,
            line_start,
            line_end,
            line: &raw[line_start..location_line_end],
            tokenizer: &self.tokenizer,
            words: &self.words,
            word_lengths: &self.word_lengths,
        }
    }

    fn find<'a>(&self, raw: &'a str, index: usize) -> Result<Vec<Finding<'a>>> {
        let rule = &self.rules[index];
        if rule.path.as_ref().is_some_and(|path| !path.is_match(b"")) {
            return Ok(Vec::new());
        }
        let Some(pattern) = &rule.regex else {
            return Ok(Vec::new());
        };
        let mut findings = Vec::new();
        for full in pattern.find_iter(raw.as_bytes()) {
            let start = full.start();
            let end = full.end();
            let matched = raw
                .get(start..end)
                .ok_or(RedactionError::Detection)?
                .trim_matches('\n');
            // Betterleaks re-matches after trimming newlines, then selects the
            // configured group, otherwise its first nonempty capture.
            let captures = pattern.captures(matched.as_bytes());
            let secret = if let Some(captures) = &captures {
                let group = if rule.secret_group > 0 {
                    captures.get(rule.secret_group)
                } else {
                    (1..captures.len()).find_map(|i| captures.get(i).filter(|m| !m.is_empty()))
                };
                if let Some(group) = group {
                    std::str::from_utf8(group.as_bytes()).map_err(|_| RedactionError::Detection)?
                } else {
                    matched
                }
            } else {
                matched
            };
            if secret.is_empty() {
                continue;
            }
            let context = self.context(raw, secret, matched, start, end);
            if self.filter.evaluate(&context)? || rule.filter.evaluate(&context)? {
                continue;
            }
            let start_line = raw.as_bytes()[..start]
                .iter()
                .filter(|b| **b == b'\n')
                .count();
            let location_end = start + matched.len();
            let end_line = start_line
                + raw.as_bytes()[start..location_end]
                    .iter()
                    .filter(|b| **b == b'\n')
                    .count();
            let column = start
                - raw.as_bytes()[..start]
                    .iter()
                    .rposition(|b| *b == b'\n')
                    .map_or(0, |i| i + 1);
            let end_column = location_end
                - raw.as_bytes()[..location_end]
                    .iter()
                    .rposition(|b| *b == b'\n')
                    .map_or(0, |i| i + 1);
            // 捕获片段仍借用原输入；保留秘密自身的位置，而非外围正则上下文。
            let secret_start = secret.as_ptr() as usize - raw.as_ptr() as usize;
            findings.push(Finding {
                rule: index,
                secret,
                start: secret_start,
                end: secret_start + secret.len(),
                start_line,
                end_line,
                column,
                end_column,
            });
        }
        Ok(findings)
    }

    fn suppressed(&self, finding: &Finding<'_>, composites: &[Composite<'_>]) -> bool {
        composites.iter().any(|other| {
            other.primary.rule != finding.rule
                && std::iter::once(&other.primary)
                    .chain(&other.components)
                    .any(|candidate| {
                        candidate.rule != finding.rule
                            && candidate.start_line == finding.start_line
                            && candidate.secret.contains(finding.secret)
                            && self.rules[candidate.rule].specificity
                                > self.rules[finding.rule].specificity
                    })
        })
    }
}
impl Component {
    fn near(&self, primary: &Finding<'_>, candidate: &Finding<'_>) -> bool {
        if let Some(lines) = self.lines {
            if candidate.start_line < primary.start_line.saturating_sub(lines)
                || candidate.start_line > primary.end_line.saturating_add(lines)
            {
                return false;
            }
        }
        if primary.start_line == primary.end_line {
            if let Some(columns) = self.columns {
                return candidate.column >= primary.column.saturating_sub(columns)
                    && candidate.column < primary.end_column.saturating_add(columns);
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn tester_locates_repeated_captures_and_multiline_unicode_input() {
        let secret = "Q8n4Vk7sT2p9X5a3Lc6D0h1R";
        let private_key = concat!(
            "-----BEGIN PRIVATE KEY-----\n",
            "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8gISIjJCUmJygpKissLS4vMDEyMzQ1Njc4OTo7PD0+P0BBQkNERUZH\n",
            "-----END PRIVATE KEY-----"
        );
        let text = format!("中文😀 api_key={secret}\n次行 api_key={secret}\n{private_key}\n结束");
        let matches = test_text(text.clone()).await.unwrap();
        let utf16: Vec<_> = text.encode_utf16().collect();
        let generic: Vec<_> = matches
            .iter()
            .filter(|found| String::from_utf16(&utf16[found.start..found.end]).unwrap() == secret)
            .collect();
        assert_eq!(generic.len(), 2);
        assert_eq!((generic[0].start_line, generic[0].start_column), (1, 13));
        assert_eq!((generic[1].start_line, generic[1].start_column), (2, 12));
        for found in &generic {
            assert_eq!(found.end_line, found.start_line);
            assert_eq!(
                found.end_column - found.start_column,
                secret.chars().count()
            );
        }
        let multiline = matches
            .iter()
            .find(|found| {
                String::from_utf16(&utf16[found.start..found.end]).unwrap() == private_key
            })
            .unwrap();
        assert_eq!((multiline.start_line, multiline.start_column), (3, 1));
        assert_eq!((multiline.end_line, multiline.end_column), (5, 26));
    }
}
