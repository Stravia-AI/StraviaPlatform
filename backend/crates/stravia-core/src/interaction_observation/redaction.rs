use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::types::{IngressStart, RejectedOutcome, RunEvent, RunOutcome};

pub(crate) const REDACTED: &str = "***";

pub(crate) fn user_input_text(
    items: &[stravia_runtime_contract::protocol::ir::AiItem],
) -> Option<String> {
    use stravia_runtime_contract::protocol::ir::{ContentBlock, MessageContent, Role};
    let item = items.iter().rev().find(|item| item.role == Role::User)?;
    let text = match &item.content {
        MessageContent::Text(text) => text.clone(),
        MessageContent::Blocks(blocks) => blocks
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
    };
    (!text.is_empty()).then_some(text)
}

pub(crate) fn input_preview(mut text: String, protected: &ProtectedSecrets) -> String {
    // Full text must cross both filters before taking a Unicode-safe opening window.
    protected.text(&mut text);
    text = redact_text(&text);
    if let Some((end, _)) = text.char_indices().nth(4096) {
        text.truncate(end);
    }
    text
}

// Shared only by a Run and its trace handles. Deliberately has no Debug implementation.
#[derive(Clone, Default)]
pub(crate) struct ProtectedSecrets(std::sync::Arc<std::sync::RwLock<Vec<ProtectedSecret>>>);

struct ProtectedSecret {
    raw: String,
    json: String,
    prefix: Vec<usize>,
    slot: usize,
}

impl ProtectedSecret {
    fn new(secret: &str, slot: usize) -> Self {
        let raw = secret.to_owned();
        let encoded = serde_json::to_string(secret).expect("string serialization");
        let mut prefix = vec![0; raw.len()];
        let bytes = raw.as_bytes();
        let mut matched = 0;
        for index in 1..bytes.len() {
            while matched > 0 && bytes[index] != bytes[matched] {
                matched = prefix[matched - 1];
            }
            if bytes[index] == bytes[matched] {
                matched += 1;
            }
            prefix[index] = matched;
        }
        Self {
            raw,
            json: encoded[1..encoded.len() - 1].to_owned(),
            prefix,
            slot,
        }
    }
}

impl ProtectedSecrets {
    pub(crate) fn register<'a>(&self, secrets: impl IntoIterator<Item = &'a str>) {
        let mut values = self.0.write().expect("protected diagnostic text");
        for secret in secrets {
            if !secret.is_empty() && !values.iter().any(|value| value.raw == secret) {
                let slot = values.len();
                values.push(ProtectedSecret::new(secret, slot));
            }
        }
        values.sort_unstable_by_key(|value| std::cmp::Reverse(value.raw.len()));
    }

    pub(crate) fn text(&self, text: &mut String) {
        if self.0.read().expect("protected diagnostic text").is_empty() {
            return;
        }
        self.text_inner(text, 0);
    }

    fn text_inner(&self, text: &mut String, depth: usize) {
        if depth < 16 && text.contains('\\') {
            if let Ok(mut value) = serde_json::from_str::<Value>(text) {
                self.value_inner(&mut value, depth + 1);
                *text = serde_json::to_string(&value).expect("diagnostic JSON serialization");
            } else if text.starts_with("data:") {
                *text = text
                    .split_inclusive('\n')
                    .map(|line| {
                        if let Some(data) = line.strip_prefix("data:") {
                            let mut data = data.trim_end_matches(['\r', '\n']).to_owned();
                            self.text_inner(&mut data, depth + 1);
                            format!(
                                "data:{data}{}",
                                &line[line.trim_end_matches(['\r', '\n']).len()..]
                            )
                        } else {
                            line.to_owned()
                        }
                    })
                    .collect();
            }
        }
        let values = self.0.read().expect("protected diagnostic text");
        for secret in values.iter() {
            if text.contains(&secret.raw) {
                *text = text.replace(&secret.raw, REDACTED);
            }
            // Wire and tool argument strings may contain another serialized JSON layer.
            if secret.json != secret.raw && text.contains(&secret.json) {
                *text = text.replace(&secret.json, REDACTED);
            }
        }
    }

    pub(crate) fn value(&self, value: &mut Value) {
        if !self.0.read().expect("protected diagnostic text").is_empty() {
            self.value_inner(value, 0);
        }
    }

    fn value_inner(&self, value: &mut Value, depth: usize) {
        match value {
            Value::String(text) => self.text_inner(text, depth),
            Value::Array(values) => values
                .iter_mut()
                .for_each(|value| self.value_inner(value, depth)),
            Value::Object(values) => values
                .values_mut()
                .for_each(|value| self.value_inner(value, depth)),
            _ => {}
        }
    }

    pub(crate) fn event(&self, event: &mut RunEvent) {
        match event {
            RunEvent::ClientVisibleContentDelta { text }
            | RunEvent::ModelThinkingDelta { text, .. } => self.text(text),
            RunEvent::ClientToolHandoff {
                input: Some(value), ..
            }
            | RunEvent::PlatformToolStarted {
                input: Some(value), ..
            }
            | RunEvent::ClientToolResult { content: value, .. } => self.value(value),
            RunEvent::PlatformToolFinished {
                status, content, ..
            } => {
                self.text(status);
                if let Some(value) = content {
                    self.value(value);
                }
            }
            RunEvent::Checkpoint { payload, .. } => self.value(payload),
            RunEvent::Wire {
                direction,
                payload,
                headers,
                url,
                ..
            } if direction != "client_to_platform" => {
                self.value(payload);
                self.value(headers);
                if let Some(url) = url {
                    self.text(url);
                }
            }
            RunEvent::TargetAttemptStarted { upstream_url, .. } => self.text(upstream_url),
            RunEvent::CompactionOperation {
                error_code: Some(reason),
                ..
            }
            | RunEvent::TargetAttemptFinished {
                error_code: Some(reason),
                ..
            }
            | RunEvent::DeliveryFinished {
                reason: Some(reason),
                ..
            }
            | RunEvent::ObservationGap { reason } => self.text(reason),
            RunEvent::ModelTurnFinished { status, .. } => self.text(status),
            _ => {}
        }
    }
}

const VISIBLE_AMBIGUOUS_SUFFIX_BYTES: usize = 128;
const VISIBLE_URL_AUTHORITY_BYTES: usize = 4096;

#[derive(Clone, Copy)]
enum CredentialContinuation {
    Quoted { quote: u8, escaped: bool },
    Token,
    UploadGrant,
    UrlAuthority,
}

// Each byte advances each pattern once (amortized KMP). The deque retains only
// an unfinished prefix; masking intervals merge overlaps without rescanning it.
#[derive(Default)]
struct ProtectedTextStream {
    matched: Vec<usize>,
    pending: std::collections::VecDeque<u8>,
    masks: std::collections::VecDeque<(usize, usize)>,
    offset: usize,
    masking: bool,
}

impl ProtectedTextStream {
    fn push(&mut self, text: &str, protected: &ProtectedSecrets) -> String {
        let values = protected.0.read().expect("protected diagnostic text");
        if values.is_empty() {
            return text.to_owned();
        }
        self.matched.resize(values.len(), 0);
        let mut output = Vec::with_capacity(text.len());
        for character in text.chars() {
            for &byte in character.encode_utf8(&mut [0; 4]).as_bytes() {
                self.pending.push_back(byte);
                let end = self.offset + self.pending.len();
                let mut longest_match = 0;
                for secret in values.iter() {
                    let matched = &mut self.matched[secret.slot];
                    let pattern = secret.raw.as_bytes();
                    while *matched > 0 && pattern[*matched] != byte {
                        *matched = secret.prefix[*matched - 1];
                    }
                    if pattern[*matched] == byte {
                        *matched += 1;
                    }
                    if *matched == pattern.len() {
                        longest_match = longest_match.max(pattern.len());
                        *matched = secret.prefix[*matched - 1];
                    }
                }
                if longest_match > 0 {
                    let mut start = end - longest_match;
                    while self
                        .masks
                        .back()
                        .is_some_and(|&(_, previous_end)| previous_end >= start)
                    {
                        start = start.min(self.masks.pop_back().expect("overlapping mask").0);
                    }
                    self.masks.push_back((start, end));
                }
            }
            let retained = self.matched.iter().copied().max().unwrap_or(0);
            self.emit(self.pending.len() - retained, &mut output);
        }
        String::from_utf8(output).expect("whole diagnostic text characters")
    }

    fn emit(&mut self, count: usize, output: &mut Vec<u8>) {
        for _ in 0..count {
            while self
                .masks
                .front()
                .is_some_and(|&(_, end)| end <= self.offset)
            {
                self.masks.pop_front();
            }
            let masked = self
                .masks
                .front()
                .is_some_and(|&(start, _)| start <= self.offset);
            let byte = self.pending.pop_front().expect("retained diagnostic byte");
            if masked {
                if !self.masking {
                    output.extend_from_slice(REDACTED.as_bytes());
                }
            } else {
                output.push(byte);
            }
            self.masking = masked;
            self.offset += 1;
        }
    }

    fn finish(&mut self) -> String {
        let mut output = Vec::with_capacity(self.pending.len());
        self.emit(self.pending.len(), &mut output);
        self.matched.fill(0);
        self.masks.clear();
        self.masking = false;
        self.offset = 0;
        String::from_utf8(output).expect("whole diagnostic text characters")
    }
}

pub(crate) struct VisibleTextRedactor {
    pending: String,
    continuation: Option<CredentialContinuation>,
    protected: ProtectedSecrets,
    protected_stream: ProtectedTextStream,
}

impl VisibleTextRedactor {
    pub(crate) fn new() -> Self {
        Self {
            pending: String::new(),
            continuation: None,
            protected: ProtectedSecrets::default(),
            protected_stream: ProtectedTextStream::default(),
        }
    }

    pub(crate) fn with_protected(protected: ProtectedSecrets) -> Self {
        Self {
            protected,
            ..Self::new()
        }
    }

    pub(crate) fn push(&mut self, text: String) -> Option<String> {
        let text = self.protected_stream.push(&text, &self.protected);
        let mut output = String::new();
        let remainder = self.consume_continuation(&text, &mut output);
        self.pending.push_str(remainder);
        self.emit_safe(&mut output, false);
        (!output.is_empty()).then_some(output)
    }

    pub(crate) fn finish(&mut self) -> Option<String> {
        let mut output = String::new();
        let text = self.protected_stream.finish();
        let remainder = self.consume_continuation(&text, &mut output);
        self.pending.push_str(remainder);
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
            CredentialContinuation::UploadGrant => {
                while cursor < bytes.len()
                    && (bytes[cursor].is_ascii_alphanumeric()
                        || matches!(bytes[cursor], b'_' | b'-' | b'.'))
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
        if !finish {
            const PREFIX: &str = "stravia_upload_";
            if let Some(start) = self.pending.rfind(PREFIX) {
                if self.pending[start + PREFIX.len()..]
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
                {
                    output.push_str(&redact_text(&self.pending[..start]));
                    output.push_str("<stravia-upload-key>");
                    self.pending.clear();
                    self.continuation = Some(CredentialContinuation::UploadGrant);
                    return;
                }
            }
            for length in (1..PREFIX.len()).rev() {
                if self.pending.ends_with(&PREFIX[..length]) {
                    let start = self.pending.len() - length;
                    output.push_str(&redact_text(&self.pending[..start]));
                    self.pending.drain(..start);
                    return;
                }
            }
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
    MediaExternalized,
    MediaUnrecoverable,
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
    crate::agent::upload_grant::scrub_upload_grant_value(headers);
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

    if let Some(start) = url.path().find("/v1/artifacts/downloads/") {
        let prefix_end = start + "/v1/artifacts/downloads/".len();
        if url.path().len() > prefix_end {
            let path = format!("{}{}", &url.path()[..prefix_end], REDACTED);
            url.set_path(&path);
            report.record(RedactionKind::CredentialText);
        }
    }
    let scrubbed = crate::agent::upload_grant::scrub_upload_grants(url.as_str());
    if scrubbed != url.as_str() {
        if let Ok(scrubbed_url) = reqwest::Url::parse(&scrubbed) {
            url = scrubbed_url;
            report.record(RedactionKind::CredentialText);
        }
    }
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
        RunEvent::CompactionOperation { error_code, .. }
        | RunEvent::TargetAttemptFinished { error_code, .. } => {
            if let Some(error) = error_code {
                redact_string(error, &mut report);
            }
        }
        RunEvent::DeliveryFinished { reason, .. } => {
            if let Some(reason) = reason {
                redact_string(reason, &mut report);
            }
        }
        RunEvent::ClientVisibleContentDelta { text }
        | RunEvent::ModelThinkingDelta { text, .. } => redact_string(text, &mut report),
        RunEvent::ClientToolHandoff {
            input: Some(value), ..
        }
        | RunEvent::PlatformToolStarted {
            input: Some(value), ..
        }
        | RunEvent::PlatformToolFinished {
            content: Some(value),
            ..
        }
        | RunEvent::ClientToolResult { content: value, .. } => {
            report.merge(redact_value(value));
        }
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
    value.contains("stravia_upload_")
        || value.bytes().any(|byte| matches!(byte, b':' | b'='))
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
    let scrubbed = crate::agent::upload_grant::scrub_upload_grants(message);
    let (redacted, mut report) = redact_quoted_credential_fields(&scrubbed);
    if scrubbed != message {
        report.record(RedactionKind::CredentialText);
    }
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

/// Parse transport envelopes only at the adapter boundary. Business strings and tool
/// arguments are credential-scrubbed separately and never interpreted as media.
pub(crate) fn externalize_capture(value: &mut Value, wire: bool) -> RedactionReport {
    let mut report = RedactionReport::default();
    if wire {
        if let Value::String(text) = value {
            if let Ok(mut envelope) = serde_json::from_str::<Value>(text) {
                externalize_envelope(&mut envelope, &mut report);
                if !report.kinds.is_empty() {
                    *text = envelope.to_string();
                }
            } else if text.starts_with("data:")
                || text.starts_with("event:")
                || text.starts_with(':')
            {
                let mut output = String::with_capacity(text.len());
                for line in text.split_inclusive('\n') {
                    if let Some(data) = line.strip_prefix("data:") {
                        if let Ok(mut envelope) = serde_json::from_str::<Value>(data.trim()) {
                            let mut line_report = RedactionReport::default();
                            externalize_envelope(&mut envelope, &mut line_report);
                            if !line_report.kinds.is_empty() {
                                output.push_str("data: ");
                                output.push_str(&envelope.to_string());
                                if line.ends_with('\n') {
                                    output.push('\n');
                                }
                                report.merge(line_report);
                                continue;
                            }
                        }
                    }
                    output.push_str(line);
                }
                if !report.kinds.is_empty() {
                    *text = output;
                }
            } else if text.trim_start().starts_with(['{', '['])
                && ([
                    "\"image_url\"",
                    "\"input_audio\"",
                    "\"inlineData\"",
                    "\"inline_data\"",
                    "\"file_data\"",
                    "\"base64\"",
                    "\"base64_pdf\"",
                ]
                .iter()
                .any(|key| text.contains(key))
                    || (text.contains("\"image\"")
                        && text.contains("\"source\"")
                        && text.contains("\"bytes\"")))
            {
                *text = serde_json::json!({"media_externalized":true, "original_wire_bytes":false,
                    "content_capture":"unrecoverable", "reason":"malformed_structured_media"})
                .to_string();
                report.record(RedactionKind::MediaUnrecoverable);
            }
            return report;
        }
    }
    externalize_envelope(value, &mut report);
    report
}

fn externalize_envelope(value: &mut Value, report: &mut RedactionReport) {
    if let Value::Array(envelopes) = value {
        // Adapter response batches and client projection batches contain envelopes,
        // never recursively decoded business strings or tool argument values.
        for envelope in envelopes {
            externalize_envelope(envelope, report);
        }
        return;
    }
    let Value::Object(object) = value else { return };
    let kind = object
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    // Tool arguments/results, provider function payloads, text and reasoning are opaque
    // business values. Only explicit content-block arrays may contain media blocks.
    if matches!(
        kind,
        "text"
            | "input_text"
            | "output_text"
            | "tool_use"
            | "function_call"
            | "function_call_output"
            | "thinking"
            | "reasoning"
            | "tool_result"
    ) {
        return;
    }
    if matches!(
        kind,
        "response.audio.delta"
            | "response.output_audio.delta"
            | "response.image_generation_call.partial_image"
    ) {
        externalize_media(value, report);
        return;
    }
    let anthropic_media = matches!(kind, "content_block_start" | "content_block_stop");
    if object.get("kind").and_then(Value::as_str) == Some("item_done") {
        if let Some(item) = object.get_mut("data").and_then(|data| data.get_mut("item")) {
            externalize_envelope(item, report);
        }
    }
    if anthropic_media {
        if let Some(block) = object.get_mut("content_block") {
            externalize_media(block, report);
        }
    }
    for key in ["content", "parts"] {
        if let Some(Value::Array(blocks)) = object.get_mut(key) {
            for block in blocks {
                externalize_media(block, report);
            }
        }
    }
    if object
        .get("audio")
        .and_then(|audio| audio.get("data"))
        .is_some()
    {
        externalize_media(value, report);
    }
    let Value::Object(object) = value else { return };
    for key in [
        "messages",
        "items",
        "contents",
        "input",
        "output",
        "choices",
        "candidates",
    ] {
        if let Some(Value::Array(items)) = object.get_mut(key) {
            for item in items {
                // Responses input/output arrays mix messages with explicit media blocks.
                externalize_media(item, report);
                externalize_envelope(item, report);
            }
        }
    }
    for key in [
        "message",
        "content",
        "delta",
        "response",
        "request",
        "canonical_request",
        "canonical_response",
    ] {
        if let Some(nested) = object.get_mut(key) {
            externalize_envelope(nested, report);
        }
    }
}

// Called exclusively for a known protocol media block, never arbitrary business JSON.
fn externalize_media(value: &mut Value, report: &mut RedactionReport) {
    let Value::Object(object) = value else { return };
    if object.get("type").and_then(Value::as_str) == Some("tool_result") {
        if object.get("content_kind").and_then(Value::as_str) != Some("json") {
            if let Some(Value::Array(blocks)) = object.get_mut("content") {
                for block in blocks {
                    externalize_media(block, report);
                }
            }
        }
        return;
    }
    // Bedrock Converse's currently supported image block has no type discriminator.
    // This function is called only for a protocol content block, not tool input JSON.
    if let Some(image) = object.get_mut("image").and_then(Value::as_object_mut) {
        if image
            .get("source")
            .and_then(|source| source.get("bytes"))
            .is_some()
        {
            image.insert("source".into(), serde_json::json!({
                "media_externalized": true, "original_wire_bytes": false,
                "content_capture": "unrecoverable", "reason": "artifact_not_available_at_capture"
            }));
            report.record(RedactionKind::MediaUnrecoverable);
        }
    }
    let kind = object
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let media = matches!(
        kind,
        "image"
            | "audio"
            | "video"
            | "file"
            | "document"
            | "image_url"
            | "input_image"
            | "input_audio"
            | "input_file"
            | "output_audio"
            | "base64"
            | "base64_pdf"
            | "response.audio.delta"
            | "response.output_audio.delta"
            | "response.image_generation_call.partial_image"
    ) || object.contains_key("inlineData")
        || object.contains_key("inline_data")
        || object.contains_key("fileData")
        || object.contains_key("file_data")
        || object
            .get("audio")
            .and_then(|audio| audio.get("data"))
            .is_some();
    if !media {
        return;
    }
    // Plain-text documents and nested document blocks are not binary media.
    if object
        .get("source")
        .and_then(|source| source.get("type"))
        .and_then(Value::as_str)
        .is_some_and(|kind| matches!(kind, "plain_text" | "blocks"))
    {
        return;
    }
    for key in [
        "source",
        "image_url",
        "input_audio",
        "inlineData",
        "inline_data",
        "fileData",
        "file_data",
        "data",
        "url",
        "file_url",
        "audio",
        "delta",
        "partial_image_b64",
    ] {
        let Some(source) = object.get_mut(key) else {
            continue;
        };
        let reference = source
            .as_str()
            .or_else(|| source.get("url").and_then(Value::as_str))
            .filter(|url| url.starts_with("https://stravia/artifact/"))
            .map(str::to_owned);
        let mut metadata = serde_json::Map::new();
        if let Some(fields) = source.as_object() {
            for name in [
                "media_type",
                "mimeType",
                "mime_type",
                "filename",
                "size",
                "format",
                "detail",
                "id",
                "transcript",
                "expires_at",
            ] {
                if let Some(field) = fields.get(name) {
                    metadata.insert(name.to_owned(), field.clone());
                }
            }
        }
        metadata.insert("media_externalized".into(), Value::Bool(true));
        metadata.insert("original_wire_bytes".into(), Value::Bool(false));
        if let Some(reference) = reference {
            metadata.insert("artifact_reference".into(), Value::String(reference));
            metadata.insert(
                "content_capture".into(),
                Value::String("reference_only".into()),
            );
            report.record(RedactionKind::MediaExternalized);
        } else {
            metadata.insert(
                "content_capture".into(),
                Value::String("unrecoverable".into()),
            );
            metadata.insert(
                "reason".into(),
                Value::String("artifact_not_available_at_capture".into()),
            );
            report.record(RedactionKind::MediaUnrecoverable);
        }
        *source = Value::Object(metadata);
    }
}

fn redact_value_node(value: &mut Value, report: &mut RedactionReport) {
    let mut pending = vec![value];
    while let Some(value) = pending.pop() {
        if value
            .as_object()
            .is_some_and(|object| object.keys().any(|key| key.contains("stravia_upload_")))
        {
            crate::agent::upload_grant::scrub_upload_grant_value(value);
            report.record(RedactionKind::CredentialText);
        }
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
    "uploadkey",
    "uploadgrant",
    "straviauploadkey",
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
    #[test]
    fn input_preview_selects_latest_user_text_without_history_or_tool_payloads() {
        let items: Vec<stravia_runtime_contract::protocol::ir::AiItem> = serde_json::from_value(serde_json::json!([
            {"role":"system","content":"system-secret"},
            {"role":"user","content":"old-user"},
            {"role":"assistant","content":"assistant-secret"},
            {"role":"user","content":[{"type":"text","text":"first"},{"type":"text","text":"second"}]},
            {"role":"tool","content":"tool-secret","tool_call_id":"call"}
        ])).unwrap();
        assert_eq!(super::user_input_text(&items), Some("first\nsecond".into()));
        let items: Vec<stravia_runtime_contract::protocol::ir::AiItem> =
            serde_json::from_value(serde_json::json!([
                {"role":"user","content":"old-user"},
                {"role":"user","content":[]}
            ]))
            .unwrap();
        assert_eq!(super::user_input_text(&items), None);
    }

    #[test]
    fn input_preview_redacts_complete_secrets_before_unicode_truncation() {
        let protected = super::ProtectedSecrets::default();
        let secret = format!("{}private-ending", "密".repeat(4100));
        protected.register([secret.as_str()]);
        let text = format!(
            "start {secret}\napi_key=credential-sentinel\n{}",
            "文".repeat(4200)
        );
        let preview = super::input_preview(text, &protected);
        assert!(preview.starts_with("start ***\napi_key=***\n"));
        assert!(!preview.contains("密"));
        assert!(!preview.contains("credential-sentinel"));
        assert_eq!(preview.chars().count(), 4096);
        assert!(preview.ends_with('文'));
    }

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
    fn mapped_visible_secret_is_held_across_real_fragments_without_losing_prose() {
        let protected = ProtectedSecrets::default();
        protected.register(["private-mapped-value"]);
        let mut redactor = VisibleTextRedactor::with_protected(protected);
        let mut observed = String::new();
        for fragment in [
            "ordinary-before pri",
            "vate-mapped",
            "-value ordinary-after",
        ] {
            if let Some(text) = redactor.push(fragment.to_owned()) {
                observed.push_str(&text);
            }
            assert!(!observed.contains("private"));
        }
        if let Some(text) = redactor.finish() {
            observed.push_str(&text);
        }
        assert_eq!(observed, "ordinary-before *** ordinary-after");
    }

    #[test]
    fn mapped_long_periodic_prefix_hits_and_diverges_without_losing_text() {
        let prefix = "ab".repeat(8192);
        let secret = format!("{prefix}Z");
        for terminal in ['Z', 'Y'] {
            let protected = ProtectedSecrets::default();
            protected.register([secret.as_str()]);
            let mut redactor = VisibleTextRedactor::with_protected(protected);
            let input = format!("ordinary-before {prefix}{prefix}{terminal} ordinary-after");
            let mut observed = String::new();
            for character in input.chars() {
                if let Some(text) = redactor.push(character.to_string()) {
                    observed.push_str(&text);
                }
            }
            if let Some(text) = redactor.finish() {
                observed.push_str(&text);
            }
            let expected = if terminal == 'Z' {
                format!("ordinary-before {prefix}*** ordinary-after")
            } else {
                input
            };
            assert_eq!(observed, expected);
        }
    }

    #[test]
    fn mapped_nested_json_escapes_are_scrubbed_without_cross_run_state() {
        let protected = ProtectedSecrets::default();
        protected.register(["private\"mapped\nvalue"]);
        let mut payload = serde_json::json!({
            "arguments": "{\"value\":\"before private\\u0022mapped\\nvalue after\"}"
        });
        protected.value(&mut payload);
        let arguments: Value =
            serde_json::from_str(payload["arguments"].as_str().unwrap()).unwrap();
        assert_eq!(arguments["value"], "before *** after");
        let mut unrelated = "private\"mapped\nvalue".to_owned();
        ProtectedSecrets::default().text(&mut unrelated);
        assert_eq!(unrelated, "private\"mapped\nvalue");
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
