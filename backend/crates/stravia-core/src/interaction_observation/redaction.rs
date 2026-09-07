use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::types::{IngressStart, RejectedOutcome, RunEvent, RunOutcome};

pub(crate) const REDACTED: &str = "***";
const VISIBLE_AMBIGUOUS_SUFFIX_BYTES: usize = 128;
const VISIBLE_URL_AUTHORITY_BYTES: usize = 4096;

#[derive(Clone, Copy)]
enum CredentialContinuation {
    Quoted { quote: u8, escaped: bool },
    Token,
    UrlAuthority,
}

pub(crate) struct VisibleTextRedactor {
    pending: String,
    continuation: Option<CredentialContinuation>,
}

impl VisibleTextRedactor {
    pub(crate) fn new() -> Self {
        Self {
            pending: String::new(),
            continuation: None,
        }
    }

    pub(crate) fn push(&mut self, text: String) -> Option<String> {
        let mut output = String::new();
        let remainder = self.consume_continuation(&text, &mut output);
        self.pending.push_str(remainder);
        self.emit_safe(&mut output, false);
        (!output.is_empty()).then_some(output)
    }

    pub(crate) fn finish(&mut self) -> Option<String> {
        let mut output = String::new();
        self.continuation = None;
        self.emit_safe(&mut output, true);
        (!output.is_empty()).then_some(output)
    }

    fn consume_continuation<'a>(&mut self, text: &'a str, output: &mut String) -> &'a str {
        let Some(mode) = self.continuation else {
            return text;
        };
        let bytes = text.as_bytes();
        let mut cursor = 0usize;
        match mode {
            CredentialContinuation::Quoted { quote, mut escaped } => loop {
                if cursor == bytes.len() {
                    self.continuation = Some(CredentialContinuation::Quoted { quote, escaped });
                    return "";
                }
                let byte = bytes[cursor];
                cursor += 1;
                if escaped {
                    escaped = false;
                } else if byte == b'\\' {
                    escaped = true;
                } else if byte == quote {
                    self.continuation = None;
                    output.push(char::from(quote));
                    return &text[cursor..];
                }
            },
            CredentialContinuation::Token => {
                while cursor < bytes.len()
                    && !bytes[cursor].is_ascii_whitespace()
                    && !matches!(bytes[cursor], b'&' | b',' | b';' | b'}' | b']')
                {
                    cursor += 1;
                }
            }
            CredentialContinuation::UrlAuthority => {
                while cursor < bytes.len()
                    && !bytes[cursor].is_ascii_whitespace()
                    && !matches!(bytes[cursor], b'/' | b'?' | b'#')
                {
                    cursor += 1;
                }
            }
        }
        if cursor == bytes.len() {
            return "";
        }
        self.continuation = None;
        &text[cursor..]
    }

    fn emit_safe(&mut self, output: &mut String, finish: bool) {
        if self.pending.is_empty() {
            return;
        }
        if let Some(trailing) = trailing_credential(&self.pending) {
            match trailing {
                TrailingCredential::Ambiguous(start) if !finish => {
                    if start > 0 {
                        output.push_str(&redact_text(&self.pending[..start]));
                        self.pending.drain(..start);
                    }
                    return;
                }
                TrailingCredential::Active(mode) if !finish => {
                    output.push_str(&redact_text(&self.pending));
                    self.pending.clear();
                    self.continuation = Some(mode);
                    return;
                }
                _ => {}
            }
        }
        if !finish {
            if let Some(url_start) = trailing_url_authority(&self.pending) {
                if self.pending.len() - url_start <= VISIBLE_URL_AUTHORITY_BYTES {
                    if url_start > 0 {
                        output.push_str(&redact_text(&self.pending[..url_start]));
                        self.pending.drain(..url_start);
                    }
                    return;
                }
                output.push_str(&redact_text(&self.pending[..url_start]));
                let scheme_end = self.pending[url_start..]
                    .find("://")
                    .map_or(url_start, |offset| url_start + offset + 3);
                output.push_str(&self.pending[url_start..scheme_end]);
                output.push_str("***@");
                self.pending.clear();
                self.continuation = Some(CredentialContinuation::UrlAuthority);
                return;
            }
        }
        if finish || text_may_need_redaction(&self.pending) {
            let (redacted, report) = redact_text_with_report(&self.pending);
            if finish || !report.kinds.is_empty() {
                output.push_str(&redacted);
                self.pending.clear();
                return;
            }
        }
        if let Some(start) = ambiguous_credential_suffix(&self.pending) {
            if start > 0 {
                output.push_str(&self.pending[..start]);
                self.pending.drain(..start);
            }
        } else {
            output.push_str(&self.pending);
            self.pending.clear();
        }
    }
}

#[derive(Clone, Copy)]
enum TrailingCredential {
    Ambiguous(usize),
    Active(CredentialContinuation),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RedactionKind {
    CredentialHeader,
    UrlUserinfo,
    CredentialQuery,
    CredentialField,
    CredentialText,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct RedactionReport {
    kinds: BTreeSet<RedactionKind>,
}

impl RedactionReport {
    pub(crate) fn into_kinds(self) -> impl Iterator<Item = RedactionKind> {
        self.kinds.into_iter()
    }

    pub(crate) fn merge(&mut self, other: Self) {
        self.kinds.extend(other.kinds);
    }

    fn record(&mut self, kind: RedactionKind) {
        self.kinds.insert(kind);
    }
}

pub(crate) fn redact_headers(headers: &mut Value) -> RedactionReport {
    let mut report = RedactionReport::default();
    redact_header_node(headers, &mut report);
    report
}

pub(crate) fn redact_value(value: &mut Value) -> RedactionReport {
    let mut report = RedactionReport::default();
    redact_value_node(value, &mut report);
    report
}

pub(crate) fn redact_url(value: &str) -> (String, RedactionReport) {
    let mut report = RedactionReport::default();
    let (mut url, relative) = match reqwest::Url::parse(value) {
        Ok(url) => (url, false),
        Err(_) if value.starts_with('/') => {
            let Ok(url) = reqwest::Url::parse(&format!("http://redaction.invalid{value}")) else {
                return (value.to_owned(), report);
            };
            (url, true)
        }
        Err(_) => return (value.to_owned(), report),
    };

    if !url.username().is_empty() || url.password().is_some() {
        let _ = url.set_username(REDACTED);
        if url.password().is_some() {
            let _ = url.set_password(Some(REDACTED));
        }
        report.record(RedactionKind::UrlUserinfo);
    }

    let pairs: Vec<(String, String)> = url
        .query_pairs()
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect();
    if pairs.iter().any(|(key, _)| is_credential_key(key)) {
        let mut query = url.query_pairs_mut();
        query.clear();
        for (key, value) in pairs {
            if is_credential_key(&key) {
                query.append_pair(&key, REDACTED);
                report.record(RedactionKind::CredentialQuery);
            } else {
                query.append_pair(&key, &value);
            }
        }
    }

    if relative {
        let mut output = url.path().to_owned();
        if let Some(query) = url.query() {
            output.push('?');
            output.push_str(query);
        }
        if let Some(fragment) = url.fragment() {
            output.push('#');
            output.push_str(fragment);
        }
        (output, report)
    } else {
        (url.into(), report)
    }
}

pub(crate) fn redact_ingress(start: &mut IngressStart) -> RedactionReport {
    let (path, report) = redact_url(&start.path);
    start.path = path;
    report
}

pub(crate) fn redact_rejected_outcome(outcome: &mut RejectedOutcome) -> RedactionReport {
    let mut report = RedactionReport::default();
    redact_string(&mut outcome.stage, &mut report);
    redact_string(&mut outcome.code, &mut report);
    report
}

pub(crate) fn redact_run_outcome(outcome: &mut RunOutcome) -> RedactionReport {
    let mut report = RedactionReport::default();
    redact_string(&mut outcome.status, &mut report);
    if let Some(reason) = &mut outcome.terminal_reason {
        redact_string(reason, &mut report);
    }
    report
}

pub(crate) fn redact_run_event(event: &mut RunEvent) -> RedactionReport {
    let mut report = RedactionReport::default();
    match event {
        RunEvent::TargetAttemptStarted { upstream_url, .. } => {
            let (redacted, url_report) = redact_url(upstream_url);
            *upstream_url = redacted;
            report.merge(url_report);
        }
        RunEvent::TargetAttemptFinished { error_code, .. } => {
            if let Some(error) = error_code {
                redact_string(error, &mut report);
            }
        }
        RunEvent::DeliveryFinished { reason, .. } => {
            if let Some(reason) = reason {
                redact_string(reason, &mut report);
            }
        }
        RunEvent::ClientVisibleContentDelta { text } => redact_string(text, &mut report),
        RunEvent::ObservationGap { reason } => redact_string(reason, &mut report),
        // Checkpoint and Wire payloads are redacted by TraceHandle::record before its queue.
        _ => {}
    }
    report
}

pub(crate) fn redact_text(message: &str) -> String {
    redact_text_with_report(message).0
}

pub(crate) fn redact_error(message: &str) -> (String, RedactionReport) {
    redact_text_with_report(message)
}

fn redact_string(value: &mut String, report: &mut RedactionReport) {
    if !text_may_need_redaction(value) {
        return;
    }
    let (redacted, text_report) = redact_text_with_report(value);
    *value = redacted;
    report.merge(text_report);
}

fn text_may_need_redaction(value: &str) -> bool {
    value.bytes().any(|byte| matches!(byte, b':' | b'='))
        || value.split_whitespace().any(|token| {
            token.eq_ignore_ascii_case("bearer") || token.eq_ignore_ascii_case("basic")
        })
}

fn trailing_credential(value: &str) -> Option<TrailingCredential> {
    let bytes = value.as_bytes();
    for separator in (0..bytes.len()).rev() {
        if !matches!(bytes[separator], b':' | b'=') {
            continue;
        }
        let key_start = value[..separator]
            .char_indices()
            .rev()
            .find(|(_, character)| {
                character.is_whitespace() || matches!(character, '{' | '[' | ',' | ';' | '&' | '?')
            })
            .map_or(0, |(index, character)| index + character.len_utf8());
        let key = value[key_start..separator].trim_matches(text_wrapper);
        if !is_credential_key(key) {
            continue;
        }
        let mut start = separator + 1;
        while start < bytes.len() && bytes[start].is_ascii_whitespace() {
            start += 1;
        }
        if start == bytes.len() {
            return Some(TrailingCredential::Ambiguous(key_start));
        }
        if is_credential_header(key) {
            let field = &value[start..];
            for scheme in ["bearer", "basic"] {
                if scheme
                    .as_bytes()
                    .get(..field.len())
                    .is_some_and(|prefix| prefix.eq_ignore_ascii_case(field.as_bytes()))
                {
                    return Some(TrailingCredential::Ambiguous(key_start));
                }
                if field.len() > scheme.len()
                    && field
                        .as_bytes()
                        .get(..scheme.len())
                        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(scheme.as_bytes()))
                    && field.as_bytes()[scheme.len()].is_ascii_whitespace()
                {
                    let secret = field[scheme.len()..].trim_start();
                    if secret.is_empty() {
                        return Some(TrailingCredential::Ambiguous(key_start));
                    }
                    if !secret.bytes().any(|byte| byte.is_ascii_whitespace()) {
                        return Some(TrailingCredential::Active(CredentialContinuation::Token));
                    }
                }
            }
        }
        if matches!(bytes[start], b'"' | b'\'') {
            if quoted_value_end(bytes, start, bytes[start]).is_none() {
                return Some(TrailingCredential::Active(CredentialContinuation::Quoted {
                    quote: bytes[start],
                    escaped: false,
                }));
            }
            continue;
        }
        if !bytes[start..].iter().any(|byte| {
            byte.is_ascii_whitespace() || matches!(*byte, b'&' | b',' | b';' | b'}' | b']')
        }) {
            return Some(TrailingCredential::Active(CredentialContinuation::Token));
        }
    }
    None
}

fn trailing_url_authority(value: &str) -> Option<usize> {
    let marker = value.rfind("://")?;
    let start = value[..marker]
        .char_indices()
        .rev()
        .find(|(_, character)| character.is_whitespace() || matches!(character, '(' | '[' | '{'))
        .map_or(0, |(index, character)| index + character.len_utf8());
    let authority = &value[marker + 3..];
    (!authority.contains('@')
        && !authority
            .bytes()
            .any(|byte| byte.is_ascii_whitespace() || matches!(byte, b'/' | b'?' | b'#')))
    .then_some(start)
}

fn ambiguous_credential_suffix(value: &str) -> Option<usize> {
    if value.is_empty() || value.chars().next_back().is_some_and(char::is_whitespace) {
        return None;
    }
    let start = value
        .char_indices()
        .rev()
        .find(|(_, character)| {
            character.is_whitespace() || matches!(character, '{' | '[' | ',' | ';' | '&' | '?')
        })
        .map_or(0, |(index, character)| index + character.len_utf8());
    let raw = value[start..].trim_matches(text_wrapper);
    if raw.is_empty() || raw.len() > VISIBLE_AMBIGUOUS_SUFFIX_BYTES {
        return None;
    }
    let normalized = normalize(raw);
    if (!normalized.is_empty()
        && CREDENTIAL_KEY_NAMES
            .iter()
            .any(|key| key.starts_with(&normalized)))
        || raw.ends_with("_k")
        || raw.ends_with("-k")
        || raw.ends_with(".k")
        || raw.ends_with(":/")
    {
        Some(start)
    } else {
        None
    }
}

fn redact_text_with_report(message: &str) -> (String, RedactionReport) {
    let (redacted, mut report) = redact_quoted_credential_fields(message);
    let redacted = redact_credential_text(&redacted, &mut report);
    let (redacted, form_report) = redact_form_encoded(&redacted);
    report.merge(form_report);
    (redacted, report)
}

fn redact_quoted_credential_fields(message: &str) -> (String, RedactionReport) {
    let bytes = message.as_bytes();
    let mut report = RedactionReport::default();
    let mut output = String::with_capacity(message.len());
    let mut copied_through = 0usize;
    let mut cursor = 0usize;

    while cursor < bytes.len() {
        if bytes[cursor] != b'"' {
            cursor += 1;
            continue;
        }
        let Some(key_end) = quoted_value_end(bytes, cursor, b'"') else {
            break;
        };
        let Ok(key) = serde_json::from_str::<String>(&message[cursor..key_end]) else {
            cursor = key_end;
            continue;
        };
        if !is_credential_key(&key) {
            cursor = key_end;
            continue;
        }

        let mut separator = key_end;
        while separator < bytes.len() && bytes[separator].is_ascii_whitespace() {
            separator += 1;
        }
        if bytes.get(separator) != Some(&b':') {
            cursor = key_end;
            continue;
        }
        let mut value_start = separator + 1;
        while value_start < bytes.len() && bytes[value_start].is_ascii_whitespace() {
            value_start += 1;
        }
        if value_start == bytes.len() {
            break;
        }

        let (replacement_start, replacement_end, replacement) = match bytes[value_start] {
            quote @ (b'"' | b'\'') => match quoted_value_end(bytes, value_start, quote) {
                Some(end) => (
                    value_start,
                    end,
                    if quote == b'"' { "\"***\"" } else { "'***'" },
                ),
                None => (
                    value_start,
                    bytes.len(),
                    if quote == b'"' { "\"***" } else { "'***" },
                ),
            },
            b'{' | b'[' => (
                value_start,
                structured_value_end(bytes, value_start),
                REDACTED,
            ),
            _ => {
                let end = bytes[value_start..]
                    .iter()
                    .position(|byte| {
                        byte.is_ascii_whitespace() || matches!(*byte, b',' | b'}' | b']')
                    })
                    .map_or(bytes.len(), |offset| value_start + offset);
                (value_start, end, REDACTED)
            }
        };

        output.push_str(&message[copied_through..replacement_start]);
        output.push_str(replacement);
        copied_through = replacement_end;
        cursor = replacement_end;
        report.record(RedactionKind::CredentialField);
    }

    if report.kinds.is_empty() {
        return (message.to_owned(), report);
    }
    output.push_str(&message[copied_through..]);
    (output, report)
}

fn quoted_value_end(bytes: &[u8], start: usize, quote: u8) -> Option<usize> {
    let mut cursor = start + 1;
    while cursor < bytes.len() {
        match bytes[cursor] {
            b'\\' => cursor = (cursor + 2).min(bytes.len()),
            byte if byte == quote => return Some(cursor + 1),
            _ => cursor += 1,
        }
    }
    None
}

fn structured_value_end(bytes: &[u8], start: usize) -> usize {
    let mut delimiters = Vec::with_capacity(4);
    delimiters.push(if bytes[start] == b'{' { b'}' } else { b']' });
    let mut cursor = start + 1;
    while cursor < bytes.len() {
        match bytes[cursor] {
            quote @ (b'"' | b'\'') => {
                let Some(end) = quoted_value_end(bytes, cursor, quote) else {
                    return bytes.len();
                };
                cursor = end;
            }
            b'{' => {
                delimiters.push(b'}');
                cursor += 1;
            }
            b'[' => {
                delimiters.push(b']');
                cursor += 1;
            }
            byte if delimiters.last() == Some(&byte) => {
                delimiters.pop();
                cursor += 1;
                if delimiters.is_empty() {
                    return cursor;
                }
            }
            _ => cursor += 1,
        }
    }
    bytes.len()
}

fn redact_form_encoded(value: &str) -> (String, RedactionReport) {
    let mut report = RedactionReport::default();
    if !value.contains('=') || value.contains(char::is_whitespace) {
        return (value.to_owned(), report);
    }
    let Ok(mut url) = reqwest::Url::parse(&format!("http://redaction.invalid/?{value}")) else {
        return (value.to_owned(), report);
    };
    let pairs: Vec<(String, String)> = url
        .query_pairs()
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect();
    if !pairs.iter().any(|(key, _)| is_credential_key(key)) {
        return (value.to_owned(), report);
    }
    {
        let mut query = url.query_pairs_mut();
        query.clear();
        for (key, field_value) in pairs {
            if is_credential_key(&key) {
                query.append_pair(&key, REDACTED);
                report.record(RedactionKind::CredentialField);
            } else {
                query.append_pair(&key, &field_value);
            }
        }
    }
    (url.query().unwrap_or_default().to_owned(), report)
}

fn redact_header_node(value: &mut Value, report: &mut RedactionReport) {
    let mut pending = vec![value];
    while let Some(value) = pending.pop() {
        match value {
            Value::Object(object) => {
                for (key, value) in object {
                    if is_credential_header(key) {
                        *value = Value::String(REDACTED.to_owned());
                        report.record(RedactionKind::CredentialHeader);
                    } else {
                        pending.push(value);
                    }
                }
            }
            Value::Array(values) => pending.extend(values.iter_mut()),
            Value::String(text) => *text = redact_credential_text(text, report),
            _ => {}
        }
    }
}

fn redact_value_node(value: &mut Value, report: &mut RedactionReport) {
    let mut pending = vec![value];
    while let Some(value) = pending.pop() {
        match value {
            Value::Object(object) => {
                for (key, value) in object {
                    if is_credential_key(key) {
                        *value = Value::String(REDACTED.to_owned());
                        report.record(RedactionKind::CredentialField);
                    } else {
                        pending.push(value);
                    }
                }
            }
            Value::Array(values) => pending.extend(values.iter_mut()),
            Value::String(text) => {
                if let Some((redacted, embedded_report)) =
                    redact_embedded_json(text).or_else(|| redact_sse_json(text))
                {
                    *text = redacted;
                    report.merge(embedded_report);
                    continue;
                }
                let (redacted_url, url_report) = redact_url(text);
                if !url_report.kinds.is_empty() {
                    *text = redacted_url;
                    report.merge(url_report);
                } else {
                    let (redacted, text_report) = redact_text_with_report(text);
                    *text = redacted;
                    report.merge(text_report);
                }
            }
            _ => {}
        }
    }
}

fn redact_embedded_json(text: &str) -> Option<(String, RedactionReport)> {
    let first = text.bytes().find(|byte| !byte.is_ascii_whitespace())?;
    if !matches!(first, b'{' | b'[' | b'"') {
        return None;
    }
    let mut parsed: Value = serde_json::from_str(text).ok()?;
    let report = redact_value(&mut parsed);
    if report.kinds.is_empty() {
        return None;
    }
    let redacted = serde_json::to_string(&parsed).ok()?;
    Some((redacted, report))
}

fn redact_sse_json(text: &str) -> Option<(String, RedactionReport)> {
    if !text.starts_with("data:") && !text.contains("\ndata:") {
        return None;
    }
    let mut output = String::new();
    let mut report = RedactionReport::default();
    let mut offset = 0;
    let mut copied_through = 0;
    for line in text.split_inclusive('\n') {
        if let Some(data) = line.strip_prefix("data:") {
            let json = data.trim();
            if let Some((redacted, json_report)) = redact_embedded_json(json) {
                let start = offset + line.len() - data.trim_start().len();
                output.push_str(&text[copied_through..start]);
                output.push_str(&redacted);
                copied_through = start + json.len();
                report.merge(json_report);
            }
        }
        offset += line.len();
    }
    if report.kinds.is_empty() {
        return None;
    }
    output.push_str(&text[copied_through..]);
    Some((output, report))
}

fn is_credential_header(key: &str) -> bool {
    matches!(
        normalize(key).as_str(),
        "authorization"
            | "proxyauthorization"
            | "cookie"
            | "setcookie"
            | "apikey"
            | "xapikey"
            | "xgoogapikey"
            | "anthropicapikey"
    ) || is_credential_key(key)
}

const CREDENTIAL_KEY_NAMES: &[&str] = &[
    "key",
    "apikey",
    "accesskey",
    "accesskeyid",
    "awsaccesskeyid",
    "secretkey",
    "secretaccesskey",
    "privatekey",
    "token",
    "accesstoken",
    "refreshtoken",
    "idtoken",
    "password",
    "passwd",
    "pwd",
    "secret",
    "clientsecret",
    "credential",
    "credentials",
    "signature",
    "sig",
    "xamzsignature",
    "xgoogsignature",
    "authorization",
    "proxyauthorization",
    "cookie",
    "setcookie",
];

fn is_credential_key(key: &str) -> bool {
    let normalized = normalize(key);
    CREDENTIAL_KEY_NAMES.contains(&normalized.as_str())
        || normalized.ends_with("apikey")
        || normalized.ends_with("token")
        || normalized.ends_with("secret")
        || normalized.ends_with("password")
        || normalized.ends_with("credential")
        || normalized.ends_with("signature")
        || has_credential_key_suffix(key)
}

fn has_credential_key_suffix(key: &str) -> bool {
    let lower = key.to_ascii_lowercase();
    lower == "privatekey"
        || lower.ends_with("_key")
        || lower.ends_with("-key")
        || lower.ends_with(".key")
        || lower.ends_with(" key")
}

fn normalize(value: &str) -> String {
    value
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn redact_credential_text(input: &str, report: &mut RedactionReport) -> String {
    let mut output = String::with_capacity(input.len());
    let mut cursor = 0usize;
    let mut redact_next_token = false;

    while cursor < input.len() {
        let whitespace = input[cursor..]
            .find(|character: char| !character.is_whitespace())
            .unwrap_or(input.len() - cursor);
        output.push_str(&input[cursor..cursor + whitespace]);
        cursor += whitespace;
        if cursor == input.len() {
            break;
        }
        let token_len = input[cursor..]
            .find(char::is_whitespace)
            .unwrap_or(input.len() - cursor);
        let token = &input[cursor..cursor + token_len];

        if redact_next_token {
            let lower = token.to_ascii_lowercase();
            if matches!(lower.as_str(), "bearer" | "basic") {
                output.push_str(token);
            } else if token.trim_matches(text_wrapper) == REDACTED {
                output.push_str(token);
                redact_next_token = false;
            } else if token.trim_matches(text_wrapper).starts_with(REDACTED) {
                output.push_str(REDACTED);
                report.record(RedactionKind::CredentialText);
            } else {
                output.push_str(REDACTED);
                report.record(RedactionKind::CredentialText);
                redact_next_token = false;
            }
        } else {
            let lower = token.to_ascii_lowercase();
            if matches!(lower.as_str(), "bearer" | "basic") || credential_header_scheme(token) {
                output.push_str(token);
                redact_next_token = true;
            } else if token.ends_with(':') && is_credential_key(token[..token.len() - 1].trim()) {
                output.push_str(token);
                redact_next_token = true;
            } else {
                output.push_str(&redact_text_token(token, report));
            }
        }
        cursor += token_len;
    }
    output
}

fn credential_header_scheme(token: &str) -> bool {
    let Some(separator) = token.find(['=', ':']) else {
        return false;
    };
    is_credential_key(token[..separator].trim_matches(text_wrapper))
        && matches!(
            token[separator + 1..].to_ascii_lowercase().as_str(),
            "bearer" | "basic"
        )
}

fn redact_text_token(token: &str, report: &mut RedactionReport) -> String {
    let trimmed_start = token.trim_start_matches(text_wrapper);
    let leading = token.len() - trimmed_start.len();
    let trimmed = trimmed_start.trim_end_matches(text_wrapper);
    if let Some(marker) = trimmed.find("://") {
        let url_start = trimmed[..marker]
            .rfind(|character: char| {
                !(character.is_ascii_alphanumeric() || matches!(character, '+' | '-' | '.'))
            })
            .map_or(0, |boundary| boundary + 1);
        let candidate = &trimmed[url_start..];
        let (redacted, url_report) = redact_url(candidate);
        if !url_report.kinds.is_empty() {
            let mut output = String::with_capacity(token.len());
            output.push_str(&token[..leading + url_start]);
            output.push_str(&redacted);
            output.push_str(&token[leading + trimmed.len()..]);
            report.merge(url_report);
            return output;
        }
    }

    let mut output = String::with_capacity(token.len());
    let mut copied_through = 0usize;
    let mut component_start = 0usize;
    for (index, character) in token.char_indices() {
        if index < copied_through {
            continue;
        }
        if matches!(character, '&' | ',' | ';' | '{' | '[') {
            component_start = index + character.len_utf8();
            continue;
        }
        if !matches!(character, '=' | ':')
            || !is_credential_key(token[component_start..index].trim_matches(text_wrapper))
        {
            continue;
        }
        let value_start = index + character.len_utf8();
        let value_end = token[value_start..]
            .find(['&', ',', ';', '}', ']'])
            .map_or(token.len(), |offset| value_start + offset);
        output.push_str(&token[copied_through..value_start]);
        if token[value_start..value_end].trim_matches(text_wrapper) == REDACTED {
            output.push_str(&token[value_start..value_end]);
        } else {
            output.push_str(REDACTED);
            report.record(RedactionKind::CredentialText);
        }
        copied_through = value_end;
        component_start = value_end;
    }
    output.push_str(&token[copied_through..]);
    output
}

fn text_wrapper(character: char) -> bool {
    matches!(
        character,
        '(' | ')' | '[' | ']' | '{' | '}' | ',' | ';' | '"' | '\''
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials_are_removed_from_every_supported_location() {
        let sentinel = "never-persist-this";
        let mut headers = serde_json::json!({
            "Authorization": format!("Bearer {sentinel}"),
            "set-cookie": format!("session={sentinel}"),
            "x-request-id": "business-value"
        });
        let mut body = serde_json::json!({
            "nested": [{"client_secret": sentinel}],
            "callback": format!("https://user:{sentinel}@example.test/cb?signature={sentinel}&safe=yes"),
            "form": format!("name=visible&access_token={sentinel}"),
            "serialized": format!(r#"{{"outer":{{"api_key":"{sentinel}"}},"content":"preserve"}}"#),
            "prompt": "keep this business content"
        });
        redact_headers(&mut headers);
        redact_value(&mut body);
        let error = redact_text(&format!(
            "upstream https://user:{sentinel}@example.test/path?api_key={sentinel} Authorization: Bearer {sentinel}"
        ));

        let artifacts = format!("{headers}{body}{error}");
        assert!(!artifacts.contains(sentinel));
        assert!(artifacts.contains("business-value"));
        assert!(artifacts.contains("keep this business content"));
        assert!(artifacts.contains("preserve"));
        assert!(artifacts.contains("visible"));
    }

    #[test]
    fn nonsecret_raw_application_payloads_are_unchanged() {
        let raw_json = " {\n  \"z\": 1,\n  \"a\": \"business value\"\n}\n";
        let raw_form = "name=Alice%20Smith&redirect=%2faccount%3Ftab%3Done";

        for original in [raw_json, raw_form] {
            let mut payload = Value::String(original.to_owned());
            let report = redact_value(&mut payload);

            assert_eq!(payload, Value::String(original.to_owned()));
            assert!(report.kinds.is_empty());
        }
    }

    #[test]
    fn nested_structured_credentials_are_redacted_without_erasing_business_data() {
        let mut payload = Value::String(
            r#"{
  "customer": {"name": "Ada", "sessions": [{"access_token": "never-persist"}]},
  "request": {"prompt": "retain this", "options": {"temperature": 0.2}}
}"#
            .to_owned(),
        );

        let report = redact_value(&mut payload);
        let Value::String(redacted) = payload else {
            panic!("string payload changed type");
        };
        let parsed: Value = serde_json::from_str(&redacted).expect("redacted JSON remains valid");

        assert_eq!(parsed["customer"]["name"], "Ada");
        assert_eq!(parsed["customer"]["sessions"][0]["access_token"], REDACTED);
        assert_eq!(parsed["request"]["prompt"], "retain this");
        assert!(!redacted.contains("never-persist"));
        assert_eq!(
            report.kinds.into_iter().collect::<Vec<_>>(),
            vec![RedactionKind::CredentialField]
        );
    }

    #[test]
    fn malformed_quoted_credential_fields_do_not_leak() {
        let cases = [
            (
                r#"{"safe":"keep","api_key": "never-persist"#,
                r#"{"safe":"keep","api_key": "***"#,
            ),
            (
                r#"{"password":"never-persist", "safe":"keep"#,
                r#"{"password":"***", "safe":"keep"#,
            ),
            (
                r#"{"credentials":{"secret":"never-persist"#,
                r#"{"credentials":***"#,
            ),
        ];

        for (original, expected) in cases {
            let mut payload = Value::String(original.to_owned());
            let report = redact_value(&mut payload);

            assert_eq!(payload, Value::String(expected.to_owned()));
            assert!(!expected.contains("never-persist"));
            assert_eq!(
                report.kinds.into_iter().collect::<Vec<_>>(),
                vec![RedactionKind::CredentialField]
            );
        }
    }

    #[test]
    fn client_visible_content_uses_the_shared_credential_policy() {
        let sentinel = "VISIBLE_SECRET_5f1e";
        let safe = "retain-visible-business-output";
        let mut event = RunEvent::ClientVisibleContentDelta {
            text: format!(
                "{safe} Authorization: Bearer {sentinel} callback=https://user:{sentinel}@example.test/path?signature={sentinel} metadata={{\"api_key\":\"{sentinel}\"}} form=name=Ada&access_token={sentinel}"
            ),
        };

        let report = redact_run_event(&mut event);
        let RunEvent::ClientVisibleContentDelta { text } = event else {
            unreachable!();
        };

        assert!(!text.contains(sentinel));
        assert!(text.contains(safe));
        assert!(text.contains("name=Ada"));
        assert!(text.contains(REDACTED));
        assert!(!report.kinds.is_empty());
    }

    #[test]
    fn buffered_visible_fragments_remove_credentials_across_queue_flushes() {
        let fragments = [
            "retain Authorization: Bear",
            "er ",
            "HEADER_SECRET callback=https://user:",
            "URL_SECRET@example.test/cb?signature=",
            "QUERY_SECRET metadata={\"api_",
            "key\":\"",
            "JSON_SECRET\"} form=name=Ada&access_",
            "token=",
            "FORM_SECRET finish",
        ];
        let mut redactor = VisibleTextRedactor::new();
        let mut text = String::new();
        for fragment in fragments {
            if let Some(emitted) = redactor.push(fragment.to_owned()) {
                text.push_str(&emitted);
            }
        }
        if let Some(emitted) = redactor.finish() {
            text.push_str(&emitted);
        }

        for sentinel in [
            "HEADER_SECRET",
            "URL_SECRET",
            "QUERY_SECRET",
            "JSON_SECRET",
            "FORM_SECRET",
        ] {
            assert!(!text.contains(sentinel));
        }
        assert!(text.contains("retain"));
        assert!(text.contains("name=Ada"));
        assert!(text.contains("finish"));
    }

    #[test]
    fn short_safe_prose_emits_before_finish_without_truncation() {
        let mut redactor = VisibleTextRedactor::new();

        let emitted = redactor
            .push("hello world ".to_owned())
            .expect("completed safe prose emits immediately");

        assert_eq!(emitted, "hello world ");
        assert!(redactor.finish().is_none());
    }

    #[test]
    fn long_split_credential_value_stays_redacted_past_buffer_thresholds() {
        let secret = "S".repeat(128 * 1024);
        let mut redactor = VisibleTextRedactor::new();
        let mut emitted = String::new();

        emitted.push_str(
            &redactor
                .push("safe access_".to_owned())
                .expect("safe prefix emits before split key"),
        );
        assert!(redactor.push("token=".to_owned()).is_none());
        emitted.push_str(
            &redactor
                .push(secret.clone())
                .expect("recognized credential emits a marker"),
        );
        if let Some(text) = redactor.push("&name=Ada finish".to_owned()) {
            emitted.push_str(&text);
        }
        if let Some(text) = redactor.finish() {
            emitted.push_str(&text);
        }

        assert!(!emitted.contains(&secret));
        assert!(emitted.contains(REDACTED));
        assert!(emitted.contains("name=Ada"));
        assert!(emitted.contains("finish"));
    }

    #[test]
    fn credential_key_split_across_transport_chunks_is_redacted_after_reconstruction() {
        let raw_body = [
            r#"{"customer":"Ada","client_"#,
            r#"secret":"never-persist","prompt":"keep"}"#,
        ]
        .concat();
        let mut payload = Value::String(raw_body);

        let report = redact_value(&mut payload);
        let Value::String(redacted) = payload else {
            panic!("string payload changed type");
        };

        assert!(!redacted.contains("never-persist"));
        assert!(redacted.contains(r#""customer":"Ada""#));
        assert!(redacted.contains(r#""client_secret":"***""#));
        assert!(redacted.contains(r#""prompt":"keep""#));
        assert_eq!(
            report.kinds.into_iter().collect::<Vec<_>>(),
            vec![RedactionKind::CredentialField]
        );
    }
}
