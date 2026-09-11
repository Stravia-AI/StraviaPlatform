//! Offline adapter for Kingfisher v1.109.0 (Apache-2.0).
//! See LICENSE.kingfisher, NOTICE.kingfisher and UPSTREAM.kingfisher.json.
//! The rule data and matching semantics are adapted from that pinned release.
//! Validation dependencies are not detection prerequisites in Kingfisher.
use super::{CredentialRule, Detector, Finding, Program, RedactionError, Result, Rule};
use base64::{Engine, prelude::BASE64_STANDARD};
use regex::bytes::{Captures, Match, Regex, RegexBuilder};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[cfg(test)]
#[path = "kingfisher_tests.rs"]
mod tests;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Snapshot {
    version: String,
    commit: String,
    rules: Vec<SourceRule>,
    safe_list: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SourceRule {
    id: String,
    name: String,
    pattern: String,
    #[serde(default)]
    min_entropy: f32,
    #[serde(default = "medium")]
    confidence: String,
    #[serde(default = "visible")]
    visible: bool,
    #[serde(default)]
    pattern_requirements: Requirements,
}

fn medium() -> String {
    "medium".into()
}
fn visible() -> bool {
    true
}

#[derive(Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Requirements {
    #[serde(default)]
    min_digits: usize,
    #[serde(default)]
    min_uppercase: usize,
    #[serde(default)]
    min_lowercase: usize,
    #[serde(default)]
    min_special_chars: usize,
    special_chars: Option<String>,
    #[serde(default)]
    ignore_if_contains: Vec<String>,
    checksum: Option<ChecksumSource>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ChecksumSource {
    actual: ChecksumActual,
    expected: String,
    #[serde(default)]
    skip_if_missing: bool,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ChecksumActual {
    template: String,
    requires_capture: Option<String>,
}

pub(super) struct Conditions {
    min_entropy: f32,
    requirements: Requirements,
    checksum: Option<Checksum>,
}

enum ChecksumKind {
    CrcBase62,
    CrcLittleEndianBase64,
    Sha256Base32,
    GitlabCrcBase36,
    CrcHex,
}

struct Checksum {
    kind: ChecksumKind,
    actual_capture: &'static str,
    lowercase_actual: bool,
}

fn invalid() -> RedactionError {
    RedactionError::Detection
}

pub(super) fn extend(detector: &mut Detector) -> Result<()> {
    let snapshot: Snapshot =
        serde_json::from_str(include_str!("kingfisher.json")).map_err(|_| invalid())?;
    if snapshot.version != "v1.109.0"
        || snapshot.commit != "9ffb8969c4ad5a5c6c24e686cb252c434bc8adce"
        || snapshot.rules.len() != 1013
    {
        return Err(invalid());
    }
    detector.kingfisher_safe_list = snapshot
        .safe_list
        .iter()
        .map(|pattern| Regex::new(pattern).map_err(|_| invalid()))
        .collect::<Result<_>>()?;
    // Match the upstream comment removal and byte/ASCII regex mode. Betterleaks'
    // RE2 translation is deliberately not applied to these Rust-native patterns.
    let comments = Regex::new(r"(?m)(\(\?#[^)]*\))|(\s\#[\sa-zA-Z]*$)").map_err(|_| invalid())?;
    for source in snapshot.rules {
        if !source.id.starts_with("kingfisher.")
            || source.name.is_empty()
            || detector
                .catalog
                .rules
                .iter()
                .any(|rule| rule.id == source.id)
            || !matches!(source.confidence.as_str(), "low" | "medium" | "high")
            || !source.min_entropy.is_finite()
            || source.min_entropy < 0.0
        {
            return Err(invalid());
        }
        let uncommented = comments.replace_all(source.pattern.as_bytes(), &b""[..]);
        let pattern = std::str::from_utf8(&uncommented).map_err(|_| invalid())?;
        let regex = RegexBuilder::new(pattern)
            .unicode(false)
            .size_limit(16 * 1024 * 1024)
            .build()
            .map_err(|_| invalid())?;
        let checksum = source
            .pattern_requirements
            .checksum
            .as_ref()
            .map(Checksum::compile)
            .transpose()?;
        let filter = format!(
            "Kingfisher exclusion conditions: byte entropy <= {} or pattern requirements not satisfied: {}. Built-in Kingfisher safe-list also applies.",
            source.min_entropy,
            serde_json::to_string(&source.pattern_requirements).map_err(|_| invalid())?
        );
        let group = preferred_group(&regex);
        detector.order.push(detector.rules.len());
        detector.rules.push(Rule {
            regex: Some(regex),
            path: None,
            secret_group: group,
            keywords: vec![],
            filter: Program::compile("")?,
            components: vec![],
            skip_report: !source.visible,
            specificity: 100,
            kingfisher: Some(Conditions {
                min_entropy: source.min_entropy,
                requirements: source.pattern_requirements,
                checksum,
            }),
        });
        detector.catalog.rules.push(CredentialRule {
            id: source.id, name: source.name.clone(), target: source.name.clone(),
            description: format!("{} (Kingfisher v1.109.0 offline rule; capture priority: TOKEN, first matched named group, group 1, full match).", source.name),
            regex: Some(source.pattern), path: None, secret_group: group, keywords: vec![],
            filter, components: vec![], skip_report: !source.visible, specificity: 100,
            confidence: source.confidence,
        });
    }
    Ok(())
}

fn preferred_group(regex: &Regex) -> usize {
    regex
        .capture_names()
        .position(|name| name.is_some_and(|name| name.eq_ignore_ascii_case("TOKEN")))
        .or_else(|| regex.capture_names().position(|name| name.is_some()))
        .unwrap_or(usize::from(regex.captures_len() > 1))
}

fn secret_capture<'a>(regex: &Regex, captures: &Captures<'a>) -> Option<Match<'a>> {
    regex
        .capture_names()
        .enumerate()
        .find_map(|(index, name)| {
            name.filter(|name| name.eq_ignore_ascii_case("TOKEN"))
                .and_then(|_| captures.get(index))
        })
        .or_else(|| {
            regex
                .capture_names()
                .enumerate()
                .find_map(|(index, name)| name.and_then(|_| captures.get(index)))
        })
        .or_else(|| captures.get(1))
        .or_else(|| captures.get(0))
}

pub(super) fn find<'a>(
    detector: &Detector,
    raw: &'a str,
    index: usize,
    conditions: &Conditions,
) -> Result<Vec<Finding<'a>>> {
    let regex = detector.rules[index].regex.as_ref().ok_or_else(invalid)?;
    let mut findings = Vec::new();
    for captures in regex.captures_iter(raw.as_bytes()) {
        let full = captures.get(0).ok_or_else(invalid)?;
        let secret = secret_capture(regex, &captures).ok_or_else(invalid)?;
        if secret.is_empty()
            || entropy(secret.as_bytes()) <= conditions.min_entropy
            || detector
                .kingfisher_safe_list
                .iter()
                .any(|pattern| pattern.is_match(secret.as_bytes()))
            || !conditions.accepts(regex, &captures, full, secret)?
        {
            continue;
        }
        // Byte regexes can match a fragment of a multibyte character. Such a span
        // cannot be reversibly substituted in canonical UTF-8 model text.
        let Some(value) = raw.get(secret.start()..secret.end()) else {
            continue;
        };
        let prefix = &raw.as_bytes()[..full.start()];
        let start_line = prefix.iter().filter(|b| **b == b'\n').count();
        let end_line = start_line + full.as_bytes().iter().filter(|b| **b == b'\n').count();
        findings.push(Finding {
            rule: index,
            secret: value,
            start: secret.start(),
            end: secret.end(),
            start_line,
            end_line,
            column: full.start()
                - prefix
                    .iter()
                    .rposition(|b| *b == b'\n')
                    .map_or(0, |i| i + 1),
            end_column: full.end()
                - raw.as_bytes()[..full.end()]
                    .iter()
                    .rposition(|b| *b == b'\n')
                    .map_or(0, |i| i + 1),
        });
    }
    Ok(findings)
}

impl Conditions {
    fn accepts(
        &self,
        regex: &Regex,
        captures: &Captures<'_>,
        full: Match<'_>,
        secret: Match<'_>,
    ) -> Result<bool> {
        // Upstream requirements inspect the full match when named/multiple
        // captures exist, while entropy always inspects the selected secret.
        let bytes = if regex.capture_names().any(|name| name.is_some()) || captures.len() > 2 {
            full.as_bytes()
        } else {
            secret.as_bytes()
        };
        let value = String::from_utf8_lossy(bytes);
        let req = &self.requirements;
        let special = req
            .special_chars
            .as_deref()
            .unwrap_or("!@#$%^&*()_+-=[]{}|;:'\",.<>?/\\`~");
        if value.chars().filter(char::is_ascii_digit).count() < req.min_digits
            || value.chars().filter(char::is_ascii_uppercase).count() < req.min_uppercase
            || value.chars().filter(char::is_ascii_lowercase).count() < req.min_lowercase
            || value.chars().filter(|ch| special.contains(*ch)).count() < req.min_special_chars
        {
            return Ok(false);
        }
        let lower = value.to_lowercase();
        if req
            .ignore_if_contains
            .iter()
            .map(|term| term.trim())
            .any(|term| !term.is_empty() && lower.contains(&term.to_lowercase()))
        {
            return Ok(false);
        }
        if let (Some(checksum), Some(source)) = (&self.checksum, &req.checksum) {
            if source
                .actual
                .requires_capture
                .as_ref()
                .is_some_and(|name| captures.name(name).is_none())
            {
                return Ok(source.skip_if_missing);
            }
            return checksum.matches(regex, captures);
        }
        Ok(true)
    }
}

impl Checksum {
    fn compile(source: &ChecksumSource) -> Result<Self> {
        let (actual_capture, lowercase_actual) = match source.actual.template.as_str() {
            "{{ checksum }}" => ("checksum", false),
            "{{ crc32 }}" => ("crc32", false),
            "{{ CHECKSUM | downcase }}" => ("CHECKSUM", true),
            _ => return Err(invalid()),
        };
        // A closed set of typed transforms replaces Liquid: no executable
        // templates, network functions or unknown syntax can reach the runtime.
        let kind = match source.expected.as_str() {
            "{{ body | crc32 | base62: 6 }}" => ChecksumKind::CrcBase62,
            "{{ body | crc32_le_b64: 6 }}" => ChecksumKind::CrcLittleEndianBase64,
            "{{ body | sha256_b32: 8 }}" => ChecksumKind::Sha256Base32,
            "{{ \"glpat-\" | append: base64_payload | append: \".01.\" | append: base36_payload_length | crc32 | base36: 7 }}" => {
                ChecksumKind::GitlabCrcBase36
            }
            "{{ BODY | crc32_hex }}" => ChecksumKind::CrcHex,
            _ => return Err(invalid()),
        };
        Ok(Self {
            kind,
            actual_capture,
            lowercase_actual,
        })
    }

    fn matches(&self, regex: &Regex, captures: &Captures<'_>) -> Result<bool> {
        let capture = |name: &str| -> Result<&str> {
            let value = regex
                .capture_names()
                .enumerate()
                .find_map(|(index, item)| {
                    item.filter(|item| *item == name || item.to_ascii_uppercase() == name)
                        .and_then(|_| captures.get(index))
                })
                .ok_or_else(invalid)?;
            std::str::from_utf8(value.as_bytes()).map_err(|_| invalid())
        };
        let actual = capture(self.actual_capture)?;
        let actual = if self.lowercase_actual {
            actual.to_lowercase()
        } else {
            actual.to_owned()
        };
        let expected = match self.kind {
            ChecksumKind::GitlabCrcBase36 => {
                let body = format!(
                    "glpat-{}.01.{}",
                    capture("base64_payload")?,
                    capture("base36_payload_length")?
                );
                radix(
                    crc32(body.as_bytes()),
                    b"0123456789abcdefghijklmnopqrstuvwxyz",
                    7,
                )
            }
            ChecksumKind::CrcHex => format!("{:08x}", crc32(capture("BODY")?.as_bytes())),
            ChecksumKind::CrcBase62 => radix(
                crc32(capture("body")?.as_bytes()),
                b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz",
                6,
            ),
            ChecksumKind::CrcLittleEndianBase64 => BASE64_STANDARD
                .encode(crc32(capture("body")?.as_bytes()).to_le_bytes())[..6]
                .to_owned(),
            ChecksumKind::Sha256Base32 => {
                let hash = Sha256::digest(capture("body")?.as_bytes());
                let bits =
                    u64::from_be_bytes([0, 0, 0, hash[0], hash[1], hash[2], hash[3], hash[4]]);
                (0..8)
                    .rev()
                    .map(|index| {
                        b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567"[((bits >> (index * 5)) & 31) as usize]
                            as char
                    })
                    .collect()
            }
        };
        Ok(actual == expected)
    }
}

fn entropy(bytes: &[u8]) -> f32 {
    let mut counts = [0usize; 256];
    for byte in bytes {
        counts[*byte as usize] += 1;
    }
    counts
        .into_iter()
        .filter(|count| *count > 0)
        .fold(0.0, |sum, count| {
            let probability = count as f32 / bytes.len() as f32;
            sum - probability * probability.log2()
        })
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = u32::MAX;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb88320 & 0u32.wrapping_sub(crc & 1));
        }
    }
    !crc
}

fn radix(mut value: u32, alphabet: &[u8], width: usize) -> String {
    let mut digits = Vec::new();
    loop {
        digits.push(alphabet[value as usize % alphabet.len()] as char);
        value /= alphabet.len() as u32;
        if value == 0 {
            break;
        }
    }
    while digits.len() < width {
        digits.push('0');
    }
    digits.into_iter().rev().collect()
}
