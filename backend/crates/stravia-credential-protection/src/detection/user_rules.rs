//! 用户自定义规则的本地检测。规则先编译，请求路径只做匹配。
//!
//! 与内置规则相同：不解码、不联网，且与已有脱敏标记重叠的命中不再替换。
use std::collections::HashMap;

use regex::bytes::Regex;

use super::expression;
use super::{DetectedCredential, DetectedFinding, reference_ranges};
use crate::custom::{CustomRuleError, CustomRuleSpec, invalid};

enum Matcher {
    Literal(String),
    Pattern {
        regex: Regex,
        secret_group: usize,
        /// 已转小写；命中任一关键词才扫描，语义与内置规则一致。
        keywords: Vec<String>,
        min_entropy: Option<f64>,
    },
}

struct CompiledRule {
    id: String,
    matcher: Matcher,
}

pub struct CompiledCustomRules(Vec<CompiledRule>);

impl CompiledCustomRules {
    pub(crate) fn compile<'a>(
        rules: impl IntoIterator<Item = (&'a str, &'a CustomRuleSpec)>,
    ) -> Result<Self, CustomRuleError> {
        rules
            .into_iter()
            .map(|(id, spec)| {
                let matcher = match spec {
                    CustomRuleSpec::Simple { text } => Matcher::Literal(text.clone()),
                    CustomRuleSpec::Pattern {
                        regex,
                        secret_group,
                        keywords,
                        min_entropy,
                    } => {
                        let regex =
                            expression::regex(regex).map_err(|_| invalid("regex", "invalid"))?;
                        if *secret_group >= regex.captures_len() {
                            return Err(invalid("secret_group", "out_of_range"));
                        }
                        Matcher::Pattern {
                            regex,
                            secret_group: *secret_group,
                            keywords: keywords.iter().map(|word| word.to_lowercase()).collect(),
                            min_entropy: *min_entropy,
                        }
                    }
                };
                Ok(CompiledRule {
                    id: id.to_owned(),
                    matcher,
                })
            })
            .collect::<Result<_, _>>()
            .map(Self)
    }

    pub(super) fn detect(&self, texts: &[&str]) -> Vec<DetectedCredential> {
        let mut credentials: Vec<DetectedCredential> = Vec::new();
        if self.0.is_empty() {
            return credentials;
        }
        let mut index: HashMap<&str, usize> = HashMap::new();
        for (source_index, raw) in texts.iter().enumerate() {
            let references = reference_ranges(raw);
            let lower = raw.to_lowercase();
            for rule in &self.0 {
                for (start, end) in rule.matcher.find(raw, &lower) {
                    if references
                        .iter()
                        .any(|reference| start < reference.end && reference.start < end)
                    {
                        continue;
                    }
                    let secret = &raw[start..end];
                    let position = *index.entry(secret).or_insert_with(|| {
                        credentials.push(DetectedCredential {
                            secret: secret.to_owned(),
                            findings: Vec::new(),
                        });
                        credentials.len() - 1
                    });
                    credentials[position].findings.push(DetectedFinding {
                        rule_id: rule.id.clone(),
                        source_index,
                        start,
                        end,
                    });
                }
            }
        }
        credentials
    }
}

impl Matcher {
    /// 返回秘密在 `raw` 中的字节区间；区间总落在字符边界上且非空。
    fn find(&self, raw: &str, lower: &str) -> Vec<(usize, usize)> {
        match self {
            Self::Literal(text) => raw
                .match_indices(text.as_str())
                .map(|(start, matched)| (start, start + matched.len()))
                .collect(),
            Self::Pattern {
                regex,
                secret_group,
                keywords,
                min_entropy,
            } => {
                if !keywords.is_empty() && !keywords.iter().any(|word| lower.contains(word)) {
                    return Vec::new();
                }
                regex
                    .captures_iter(raw.as_bytes())
                    .filter_map(|captures| {
                        let whole = captures.get(0)?;
                        let group = if *secret_group > 0 {
                            captures.get(*secret_group)
                        } else {
                            (1..captures.len())
                                .find_map(|i| captures.get(i).filter(|m| !m.is_empty()))
                        }
                        .unwrap_or(whole);
                        // `(?-u)` 等写法可能产生字符中间的区间，无法作为文本替换。
                        let secret = raw.get(group.start()..group.end())?;
                        if secret.is_empty()
                            || min_entropy.is_some_and(|minimum| {
                                expression::entropy(secret.as_bytes()) < minimum
                            })
                        {
                            return None;
                        }
                        Some((group.start(), group.end()))
                    })
                    .collect()
            }
        }
    }
}
