use std::{borrow::Cow, collections::BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use stravia_credential_protection::marker;

pub(crate) const REDACTED: &str = "***";

/// Maximum number of Unicode scalar values retained for an input preview.
pub(crate) const INPUT_PREVIEW_CHARS: usize = 4096;

/// Bounded preview of the latest user text in the received canonical window.
///
/// Runs when the preview is captured so the waiting state does not pin the
/// request tree. Multiple text blocks are joined with `\n` while copying at
/// most [`INPUT_PREVIEW_CHARS`] scalar values; the full joined body is never
/// materialized.
pub(crate) fn user_input_preview(
    items: &[stravia_runtime_contract::protocol::ir::AiItem],
) -> Option<String> {
    use stravia_runtime_contract::protocol::ir::{ContentBlock, MessageContent, Role};
    let item = items.iter().rev().find(|item| item.role == Role::User)?;
    match &item.content {
        MessageContent::Text(text) => {
            (!text.is_empty()).then(|| preview_prefix(text, INPUT_PREVIEW_CHARS).0.to_owned())
        }
        MessageContent::Blocks(blocks) => {
            let texts = blocks.iter().filter_map(|block| match block {
                ContentBlock::Text { text, .. } => Some(text.as_str()),
                _ => None,
            });
            let mut remaining = INPUT_PREVIEW_CHARS;
            let mut text_count = 0;
            let mut bytes = 0;
            let mut last_end = 0;
            for text in texts.clone() {
                if text_count > 0 {
                    bytes += 1;
                    remaining -= 1;
                }
                let (prefix, chars) = preview_prefix(text, remaining);
                last_end = prefix.len();
                bytes += last_end;
                remaining -= chars;
                text_count += 1;
                if remaining == 0 {
                    break;
                }
            }
            if bytes == 0 {
                return None;
            }
            let mut preview = String::with_capacity(bytes);
            for (index, text) in texts.take(text_count).enumerate() {
                if index > 0 {
                    preview.push('\n');
                }
                if index + 1 == text_count {
                    preview.push_str(&text[..last_end]);
                } else {
                    preview.push_str(text);
                }
            }
            Some(preview)
        }
    }
}

fn preview_prefix(text: &str, limit: usize) -> (&str, usize) {
    let mut end = 0;
    let mut chars = 0;
    for (offset, character) in text.char_indices().take(limit) {
        end = offset + character.len_utf8();
        chars += 1;
    }
    (&text[..end], chars)
}

// Shared only by a Run and its trace handles. Deliberately has no Debug implementation.
#[derive(Clone, Default)]
pub(crate) struct ProtectedSecrets(std::sync::Arc<parking_lot::RwLock<Vec<ProtectedSecret>>>);

struct ProtectedSecret {
    raw: String,
    json: String,
}

impl ProtectedSecret {
    fn new(secret: &str) -> Self {
        let raw = secret.to_owned();
        let encoded = serde_json::to_string(secret).expect("string serialization");
        Self {
            raw,
            json: encoded[1..encoded.len() - 1].to_owned(),
        }
    }
}

impl ProtectedSecrets {
    pub(crate) fn register<'a>(&self, secrets: impl IntoIterator<Item = &'a str>) {
        let mut values = self.0.write();
        for secret in secrets {
            if !secret.is_empty() && !values.iter().any(|value| value.raw == secret) {
                values.push(ProtectedSecret::new(secret));
            }
        }
        values.sort_unstable_by_key(|value| std::cmp::Reverse(value.raw.len()));
    }

    pub(crate) fn text(&self, text: &mut String) {
        if self.0.read().is_empty() {
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
        let values = self.0.read();
        redact_protected_literals(text, &values);
    }

    pub(crate) fn value(&self, value: &mut Value) {
        if !self.0.read().is_empty() {
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
}

fn redact_protected_literals(text: &mut String, values: &[ProtectedSecret]) {
    let Some(first_marker) = marker::find_reference(text) else {
        for secret in values {
            redact_protected_literal(text, &secret.raw);
            // Wire and tool argument strings may contain another serialized JSON layer.
            if secret.json != secret.raw {
                redact_protected_literal(text, &secret.json);
            }
        }
        return;
    };

    let mut output = String::with_capacity(text.len());
    let mut cursor = 0usize;
    let mut next_marker = Some(first_marker);
    while let Some((offset, reference)) = next_marker {
        let marker_start = cursor + offset;
        let mut unprotected = text[cursor..marker_start].to_owned();
        for secret in values {
            redact_protected_literal(&mut unprotected, &secret.raw);
            if secret.json != secret.raw {
                redact_protected_literal(&mut unprotected, &secret.json);
            }
        }
        output.push_str(&unprotected);
        output.push_str(reference);
        cursor = marker_start + reference.len();
        next_marker = marker::find_reference(&text[cursor..]);
    }

    let mut unprotected = text[cursor..].to_owned();
    for secret in values {
        redact_protected_literal(&mut unprotected, &secret.raw);
        if secret.json != secret.raw {
            redact_protected_literal(&mut unprotected, &secret.json);
        }
    }
    output.push_str(&unprotected);
    *text = output;
}

fn redact_protected_literal(text: &mut String, secret: &str) {
    if text.contains(secret) {
        *text = text.replace(secret, REDACTED);
    }
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
    pub(crate) fn merge(&mut self, other: Self) {
        self.kinds.extend(other.kinds);
    }

    fn record(&mut self, kind: RedactionKind) {
        self.kinds.insert(kind);
    }
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
    if scrubbed != url.as_str()
        && let Ok(scrubbed_url) = reqwest::Url::parse(&scrubbed)
    {
        url = scrubbed_url;
        report.record(RedactionKind::CredentialText);
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

pub(crate) fn redact_text(message: &str) -> String {
    redact_text_with_report(message).0.into_owned()
}

fn redact_structured_text(text: &mut String, report: &mut RedactionReport) {
    if let Some((redacted, embedded_report)) =
        redact_embedded_json(text).or_else(|| redact_sse_json(text))
    {
        *text = redacted;
        report.merge(embedded_report);
        return;
    }
    let (redacted_url, url_report) = redact_url(text);
    if !url_report.kinds.is_empty() {
        *text = redacted_url;
        report.merge(url_report);
    } else {
        let (redacted, text_report) = redact_text_with_report(text);
        if let Cow::Owned(redacted) = redacted {
            *text = redacted;
        }
        report.merge(text_report);
    }
}

fn redact_text_with_report(message: &str) -> (Cow<'_, str>, RedactionReport) {
    let mut redacted = crate::agent::upload_grant::scrub_upload_grants(message);
    let mut report = RedactionReport::default();
    if redacted != message {
        report.record(RedactionKind::CredentialText);
    }
    let (quoted, quoted_report) = redact_quoted_credential_fields(&redacted);
    report.merge(quoted_report);
    if let Cow::Owned(quoted) = quoted {
        redacted = Cow::Owned(quoted);
    }
    if let Cow::Owned(credentials) = redact_credential_text(&redacted, &mut report) {
        redacted = Cow::Owned(credentials);
    }
    let (form, form_report) = redact_form_encoded(&redacted);
    report.merge(form_report);
    if let Cow::Owned(form) = form {
        redacted = Cow::Owned(form);
    }
    (redacted, report)
}

fn redact_quoted_credential_fields(message: &str) -> (Cow<'_, str>, RedactionReport) {
    let bytes = message.as_bytes();
    let mut report = RedactionReport::default();
    let mut output = String::new();
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

        if output.capacity() == 0 {
            output.reserve_exact(message.len());
        }
        output.push_str(&message[copied_through..replacement_start]);
        output.push_str(replacement);
        copied_through = replacement_end;
        cursor = replacement_end;
        report.record(RedactionKind::CredentialField);
    }

    if report.kinds.is_empty() {
        return (Cow::Borrowed(message), report);
    }
    output.push_str(&message[copied_through..]);
    (Cow::Owned(output), report)
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

fn redact_form_encoded(value: &str) -> (Cow<'_, str>, RedactionReport) {
    let report = RedactionReport::default();
    if !value.contains('=') || value.contains(char::is_whitespace) {
        return (Cow::Borrowed(value), report);
    }
    if marker::find_reference(value).is_some() {
        return redact_form_encoded_with_markers(value);
    }
    redact_form_encoded_without_markers(value)
}

fn redact_form_encoded_without_markers(value: &str) -> (Cow<'_, str>, RedactionReport) {
    let mut report = RedactionReport::default();
    let Ok(mut url) = reqwest::Url::parse(&format!("http://redaction.invalid/?{value}")) else {
        return (Cow::Borrowed(value), report);
    };
    let pairs: Vec<(String, String)> = url
        .query_pairs()
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect();
    if !pairs.iter().any(|(key, _)| is_credential_key(key)) {
        return (Cow::Borrowed(value), report);
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
    (
        Cow::Owned(url.query().unwrap_or_default().to_owned()),
        report,
    )
}

fn redact_form_encoded_with_markers(value: &str) -> (Cow<'_, str>, RedactionReport) {
    let mut output = String::new();
    let mut report = RedactionReport::default();
    let mut position = 0;
    let mut copied_through = 0;
    for part in value.split_inclusive('&') {
        let component = part.strip_suffix('&').unwrap_or(part);
        let (redacted, component_report) = redact_form_component_with_markers(component);
        if let Cow::Owned(redacted) = redacted {
            if output.capacity() == 0 {
                output.reserve_exact(value.len());
            }
            output.push_str(&value[copied_through..position]);
            output.push_str(&redacted);
            copied_through = position + component.len();
        }
        position += part.len();
        report.merge(component_report);
    }
    if copied_through == 0 {
        (Cow::Borrowed(value), report)
    } else {
        output.push_str(&value[copied_through..]);
        (Cow::Owned(output), report)
    }
}

fn redact_form_component_with_markers(component: &str) -> (Cow<'_, str>, RedactionReport) {
    if marker::find_reference(component).is_none() {
        return redact_form_encoded_without_markers(component);
    }
    let mut report = RedactionReport::default();
    let Some((raw_key, raw_value)) = component.split_once('=') else {
        return (Cow::Borrowed(component), report);
    };
    let Ok(url) = reqwest::Url::parse(&format!("http://redaction.invalid/?{raw_key}=")) else {
        return (Cow::Borrowed(component), report);
    };
    let Some((key, _)) = url.query_pairs().next() else {
        return (Cow::Borrowed(component), report);
    };
    if !is_credential_key(&key) {
        return (Cow::Borrowed(component), report);
    }

    let redacted = redact_credential_value_with_markers(
        raw_value,
        &mut report,
        RedactionKind::CredentialField,
    );
    if redacted == raw_value {
        (Cow::Borrowed(component), report)
    } else {
        (Cow::Owned(format!("{raw_key}={redacted}")), report)
    }
}

fn redact_credential_value_with_markers(
    value: &str,
    report: &mut RedactionReport,
    kind: RedactionKind,
) -> String {
    let Some((first_offset, first_reference)) = marker::find_reference(value) else {
        return REDACTED.to_owned();
    };
    let mut output = String::with_capacity(value.len());
    append_redacted_credential_fragment(&mut output, &value[..first_offset], report, kind);
    output.push_str(first_reference);
    let mut cursor = first_offset + first_reference.len();

    while cursor < value.len() {
        let next_marker = marker::find_reference(&value[cursor..]);
        let segment_end = next_marker.map_or(value.len(), |(offset, _)| cursor + offset);
        let segment = &value[cursor..segment_end];
        let adjacent = credential_adjacent_fragment_len(segment);
        append_redacted_credential_fragment(&mut output, &segment[..adjacent], report, kind);
        output.push_str(&segment[adjacent..]);
        cursor = segment_end;
        let Some((_, reference)) = next_marker else {
            break;
        };
        output.push_str(reference);
        cursor += reference.len();
    }
    output
}

fn credential_adjacent_fragment_len(value: &str) -> usize {
    let mut cursor = 0usize;
    while cursor < value.len() {
        if marker::reference_prefix(&value[cursor..]).is_some() {
            break;
        }
        let character = value[cursor..]
            .chars()
            .next()
            .expect("credential value character");
        if character.is_whitespace()
            || matches!(
                character,
                '/' | '?' | '#' | '&' | ',' | ';' | '}' | ']' | ')' | '"' | '\''
            )
        {
            break;
        }
        cursor += character.len_utf8();
    }
    cursor
}

fn append_redacted_credential_fragment(
    output: &mut String,
    fragment: &str,
    report: &mut RedactionReport,
    kind: RedactionKind,
) {
    if fragment.is_empty() || fragment == REDACTED {
        output.push_str(fragment);
    } else {
        output.push_str(REDACTED);
        report.record(kind);
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
            Value::String(text) => redact_structured_text(text, report),
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

fn redact_credential_text<'a>(input: &'a str, report: &mut RedactionReport) -> Cow<'a, str> {
    let mut output = String::new();
    let mut copied_through = 0;
    let mut cursor = 0usize;
    let mut redact_next_token = false;

    while cursor < input.len() {
        let whitespace = input[cursor..]
            .find(|character: char| !character.is_whitespace())
            .unwrap_or(input.len() - cursor);
        cursor += whitespace;
        if cursor == input.len() {
            break;
        }
        let token_len = credential_text_token_len(&input[cursor..]);
        let token = &input[cursor..cursor + token_len];

        let replacement = if redact_next_token {
            if marker::find_reference(token).is_some() {
                redact_next_token = false;
                Some(Cow::Owned(redact_header_credential_with_marker(
                    token, report,
                )))
            } else if token.eq_ignore_ascii_case("bearer") || token.eq_ignore_ascii_case("basic") {
                None
            } else if token.trim_matches(text_wrapper) == REDACTED {
                redact_next_token = false;
                None
            } else {
                report.record(RedactionKind::CredentialText);
                redact_next_token = token.trim_matches(text_wrapper).starts_with(REDACTED);
                Some(Cow::Borrowed(REDACTED))
            }
        } else if token.eq_ignore_ascii_case("bearer")
            || token.eq_ignore_ascii_case("basic")
            || credential_header_scheme(token)
            || (token.ends_with(':') && is_credential_key(token[..token.len() - 1].trim()))
        {
            redact_next_token = true;
            None
        } else {
            match redact_text_token(token, report) {
                Cow::Borrowed(_) => None,
                Cow::Owned(redacted) => Some(Cow::Owned(redacted)),
            }
        };
        if let Some(replacement) = replacement
            && replacement != token
        {
            if output.capacity() == 0 {
                output.reserve_exact(input.len());
            }
            output.push_str(&input[copied_through..cursor]);
            output.push_str(&replacement);
            copied_through = cursor + token_len;
        }
        cursor += token_len;
    }
    if copied_through == 0 {
        Cow::Borrowed(input)
    } else {
        output.push_str(&input[copied_through..]);
        Cow::Owned(output)
    }
}

fn redact_header_credential_with_marker(token: &str, report: &mut RedactionReport) -> String {
    let Some((offset, _)) = marker::find_reference(token) else {
        return REDACTED.to_owned();
    };
    let mut output = String::with_capacity(token.len());
    let prefix = &token[..offset];
    if prefix.trim_matches(text_wrapper).is_empty() {
        output.push_str(prefix);
    } else {
        output.push_str(REDACTED);
        report.record(RedactionKind::CredentialText);
    }
    output.push_str(&redact_credential_value_with_markers(
        &token[offset..],
        report,
        RedactionKind::CredentialText,
    ));
    output
}

fn credential_text_token_len(input: &str) -> usize {
    let mut cursor = 0usize;
    while cursor < input.len() {
        if let Some(reference) = marker::reference_prefix(&input[cursor..]) {
            cursor += reference.len();
            continue;
        }
        let character = input[cursor..]
            .chars()
            .next()
            .expect("credential token character");
        if character.is_whitespace() {
            break;
        }
        cursor += character.len_utf8();
    }
    cursor
}

fn credential_header_scheme(token: &str) -> bool {
    let Some(separator) = token.find(['=', ':']) else {
        return false;
    };
    is_credential_key(token[..separator].trim_matches(text_wrapper))
        && (token[separator + 1..].eq_ignore_ascii_case("bearer")
            || token[separator + 1..].eq_ignore_ascii_case("basic"))
}

fn redact_text_token<'a>(token: &'a str, report: &mut RedactionReport) -> Cow<'a, str> {
    let Some(first_marker) = marker::find_reference(token) else {
        return redact_text_token_without_markers(token, report);
    };

    let mut output = String::with_capacity(token.len());
    let mut cursor = 0usize;
    let mut marker_is_credential_value = false;
    let mut next_marker = Some(first_marker);
    while let Some((offset, reference)) = next_marker {
        let marker_start = cursor + offset;
        let before = &token[cursor..marker_start];
        if let Some(component_start) = credential_marker_component_start(before) {
            output.push_str(&redact_text_token_without_markers(
                &before[..component_start],
                report,
            ));
            output.push_str(&before[component_start..]);
            marker_is_credential_value = true;
        } else {
            output.push_str(&redact_text_token_without_markers(before, report));
        }
        output.push_str(reference);
        cursor = marker_start + reference.len();
        if marker_is_credential_value {
            let adjacent = credential_adjacent_fragment_len(&token[cursor..]);
            append_redacted_credential_fragment(
                &mut output,
                &token[cursor..cursor + adjacent],
                report,
                RedactionKind::CredentialText,
            );
            cursor += adjacent;
            marker_is_credential_value = marker::reference_prefix(&token[cursor..]).is_some();
        }
        next_marker = marker::find_reference(&token[cursor..]);
    }
    output.push_str(&redact_text_token_without_markers(&token[cursor..], report));
    if output == token {
        Cow::Borrowed(token)
    } else {
        Cow::Owned(output)
    }
}

fn credential_marker_component_start(value: &str) -> Option<usize> {
    let trimmed = value.trim_end_matches(text_wrapper);
    let separator = trimmed.len().checked_sub(1)?;
    if !matches!(trimmed.as_bytes()[separator], b'=' | b':') {
        return None;
    }
    let component_start = trimmed[..separator]
        .char_indices()
        .rev()
        .find(|(_, character)| matches!(character, '&' | ',' | ';' | '?' | '{' | '['))
        .map_or(0, |(index, character)| index + character.len_utf8());
    is_credential_key(trimmed[component_start..separator].trim_matches(text_wrapper))
        .then_some(component_start)
}

fn redact_text_token_without_markers<'a>(
    token: &'a str,
    report: &mut RedactionReport,
) -> Cow<'a, str> {
    let trimmed_start = token.trim_start_matches(text_wrapper);
    let leading = token.len() - trimmed_start.len();
    let trimmed = trimmed_start.trim_end_matches(text_wrapper);
    if let Some(marker) = trimmed.find("://") {
        let url_start = trimmed[..marker]
            .char_indices()
            .rev()
            .find(|(_, character)| {
                !(character.is_ascii_alphanumeric() || matches!(character, '+' | '-' | '.'))
            })
            .map_or(0, |(index, character)| index + character.len_utf8());
        let candidate = &trimmed[url_start..];
        let (redacted, url_report) = redact_url(candidate);
        if !url_report.kinds.is_empty() {
            let mut output = String::with_capacity(token.len());
            output.push_str(&token[..leading + url_start]);
            output.push_str(&redacted);
            output.push_str(&token[leading + trimmed.len()..]);
            report.merge(url_report);
            return Cow::Owned(output);
        }
    }

    let mut output = String::new();
    let mut copied_through = 0usize;
    let mut scanned_through = 0usize;
    let mut component_start = 0usize;
    for (index, character) in token.char_indices() {
        if index < scanned_through {
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
        scanned_through = value_end;
        component_start = value_end;
        if token[value_start..value_end].trim_matches(text_wrapper) == REDACTED {
            continue;
        }
        if output.capacity() == 0 {
            output.reserve_exact(token.len());
        }
        output.push_str(&token[copied_through..value_start]);
        output.push_str(REDACTED);
        report.record(RedactionKind::CredentialText);
        copied_through = value_end;
    }
    if copied_through == 0 {
        Cow::Borrowed(token)
    } else {
        output.push_str(&token[copied_through..]);
        Cow::Owned(output)
    }
}

fn text_wrapper(character: char) -> bool {
    matches!(
        character,
        '(' | ')' | '[' | ']' | '{' | '}' | ',' | ';' | '"' | '\''
    )
}

#[cfg(test)]
mod tests {
    fn user_text_items(text: &str) -> Vec<stravia_runtime_contract::protocol::ir::AiItem> {
        use stravia_runtime_contract::protocol::ir::{AiItem, MessageContent, Role};
        vec![AiItem {
            role: Role::User,
            content: MessageContent::Text(std::sync::Arc::new(text.to_owned())),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        }]
    }

    #[test]
    fn input_preview_selects_latest_user_text_without_history_or_tool_payloads() {
        let items: Vec<stravia_runtime_contract::protocol::ir::AiItem> =
            serde_json::from_value(serde_json::json!([
                {"role":"system","content":"system-secret"},
                {"role":"user","content":"old-user"},
                {"role":"assistant","content":"assistant-secret"},
                {"role":"user","content":[
                    {"type":"text","text":"first"},
                    {"type":"thinking","thinking":"ignored"},
                    {"type":"text","text":"second"}
                ]},
                {"role":"tool","content":"tool-secret","tool_call_id":"call"}
            ]))
            .unwrap();
        assert_eq!(
            super::user_input_preview(&items),
            Some("first\nsecond".into())
        );
        let items: Vec<stravia_runtime_contract::protocol::ir::AiItem> =
            serde_json::from_value(serde_json::json!([
                {"role":"user","content":"old-user"},
                {"role":"user","content":[]}
            ]))
            .unwrap();
        assert_eq!(super::user_input_preview(&items), None);
        let items: Vec<stravia_runtime_contract::protocol::ir::AiItem> =
            serde_json::from_value(serde_json::json!([
                {"role":"user","content":[{"type":"text","text":""}]}
            ]))
            .unwrap();
        assert_eq!(super::user_input_preview(&items), None);
        let items: Vec<stravia_runtime_contract::protocol::ir::AiItem> =
            serde_json::from_value(serde_json::json!([
                {"role":"user","content":[
                    {"type":"text","text":""},
                    {"type":"text","text":""}
                ]}
            ]))
            .unwrap();
        assert_eq!(super::user_input_preview(&items), Some("\n".into()));
    }

    #[test]
    fn input_preview_stops_at_the_limit_without_an_extra_separator() {
        let full = "a".repeat(4096);
        let items: Vec<stravia_runtime_contract::protocol::ir::AiItem> =
            serde_json::from_value(serde_json::json!([
                {"role":"user","content":[
                    {"type":"text","text":full},
                    {"type":"text","text":"b"}
                ]}
            ]))
            .unwrap();
        assert_eq!(super::user_input_preview(&items).unwrap(), full);

        let almost = "a".repeat(4095);
        let items: Vec<stravia_runtime_contract::protocol::ir::AiItem> =
            serde_json::from_value(serde_json::json!([
                {"role":"user","content":[
                    {"type":"text","text":almost},
                    {"type":"text","text":"b"}
                ]}
            ]))
            .unwrap();
        assert_eq!(
            super::user_input_preview(&items).unwrap(),
            format!("{almost}\n")
        );
    }

    #[test]
    fn input_preview_preserves_literals_before_unicode_truncation() {
        let secret = format!("{}private-ending", "密".repeat(4100));
        let text = format!(
            "start {secret}\napi_key=credential-sentinel\n{}",
            "文".repeat(4200)
        );
        let items = user_text_items(&text);
        let preview = super::user_input_preview(&items).unwrap();
        assert!(preview.starts_with("start 密"));
        assert!(preview.contains("密"));
        assert!(!preview.contains("credential-sentinel"));
        assert_eq!(preview.chars().count(), 4096);
        assert!(preview.ends_with('密'));
    }

    use super::*;

    #[test]
    fn input_preview_preserves_unicode_urls_and_credentials() {
        let text = concat!(
            "接口https://example.test/cb?api_key=never-persist&safe=yes ",
            "🔗https://example.test/public ",
            "éhttps://user:never-persist@example.test/private ",
            "地址：https://example.test/path?token=never-persist"
        );

        let preview = user_input_preview(&user_text_items(text)).unwrap();

        assert_eq!(preview, text);
    }

    #[test]
    fn input_preview_preserves_header_schemes_in_semantic_text() {
        let text = "safe prose bEaReR bearer-sentinel BaSiC basic-sentinel";
        let preview = user_input_preview(&user_text_items(text)).unwrap();

        assert_eq!(preview, text);
    }

    #[test]
    fn input_preview_limits_unprotected_unicode_text_to_whole_characters() {
        let opening = "𠮷".repeat(4096);
        let text = format!("{opening}last");

        let preview = user_input_preview(&user_text_items(&text)).unwrap();

        assert_eq!(preview, opening);
    }

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
        redact_value(&mut headers);
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
    fn safe_diagnostic_text_uses_the_shared_credential_policy() {
        let sentinel = "VISIBLE_SECRET_5f1e";
        let safe = "retain-visible-business-output";
        let input = format!(
            "{safe} Authorization: Bearer {sentinel} callback=https://user:{sentinel}@example.test/path?signature={sentinel} metadata={{\"api_key\":\"{sentinel}\"}} form=name=Ada&access_token={sentinel}"
        );
        let (text, report) = redact_text_with_report(&input);

        assert!(!text.contains(sentinel));
        assert!(text.contains(safe));
        assert!(text.contains("name=Ada"));
        assert!(text.contains(REDACTED));
        assert!(!report.kinds.is_empty());
    }

    #[test]
    fn lazy_redaction_preserves_masked_prefixes_delimiters_and_whitespace() {
        let reference = redaction_marker();
        for (input, expected) in [
            (
                "  safe\r\napi_key=***;token=secret&name=Ada\t尾部  ".to_owned(),
                "  safe\r\napi_key=***;token=***&name=Ada\t尾部  ".to_owned(),
            ),
            (
                "BEARER BASIC secret  public\n".to_owned(),
                "BEARER BASIC ***  public\n".to_owned(),
            ),
            (
                format!("&api_key={reference}&name=Ada&token=secret&&tail=yes&"),
                format!("&api_key={reference}&name=Ada&token=***&&tail=yes&"),
            ),
        ] {
            assert_eq!(redact_text(&input), expected);
        }
    }

    #[test]
    fn safe_diagnostic_prose_redacts_bare_credential_schemes_without_losing_suffix() {
        let (text, report) = redact_text_with_report(
            "ordinary prefix bearer candidate ordinary middle basic another ordinary suffix",
        );

        assert_eq!(
            text,
            "ordinary prefix bearer *** ordinary middle basic *** ordinary suffix"
        );
        assert_eq!(
            report.kinds.into_iter().collect::<Vec<_>>(),
            vec![RedactionKind::CredentialText]
        );
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

    fn redaction_marker() -> String {
        format!(
            "{}{}{}",
            marker::PREFIX,
            "abcdefghijklmnopqrstuvwxyzab",
            marker::SUFFIX
        )
    }

    #[test]
    fn protected_literals_preserve_only_complete_redaction_markers() {
        let marker = redaction_marker();
        let id = "abcdefghijklmnopqrstuvwxyzab";
        let legacy_id = "0123456789abcdef0123456789abcdef";
        let protected = ProtectedSecrets::default();
        protected.register(["sr", id, "stravia", legacy_id, "REAL_SECRET"]);
        let mut text = format!("before/{marker}/after sr {id} {marker}REAL_SECRET");

        protected.text(&mut text);

        assert_eq!(text, format!("before/{marker}/after *** *** {marker}***"));

        let nested_value = format!(r#"{{"message":"before/{marker}/after"}}"#);
        let mut nested = serde_json::to_string(&nested_value).unwrap();
        protected.text(&mut nested);
        assert_eq!(
            serde_json::from_str::<String>(&nested).unwrap(),
            nested_value
        );

        for candidate in [
            format!("{}{}", marker::PREFIX, id),
            format!(
                "{}{}{}",
                marker::PREFIX,
                "ABCDEFGHIJKLMNOPQRSTUVWXYZAB",
                marker::SUFFIX
            ),
            format!("<!-- stravia-redaction-marker:rm_{legacy_id} -->"),
            format!("~stravia-secret:{legacy_id}~"),
        ] {
            let mut invalid = candidate.clone();
            protected.text(&mut invalid);
            assert_ne!(invalid, candidate);
            assert!(!invalid.contains("stravia"));
        }
    }

    #[test]
    fn redaction_markers_survive_plain_nested_json_and_sse_text() {
        let marker = redaction_marker();
        let plain = format!(
            "before/{marker}/after api_key={marker}/models Authorization: Bearer {marker} https://example.test/before/{marker}/after?api_key={marker}/models"
        );
        assert_eq!(redact_text(&plain), plain);

        let mut nested = Value::String(format!(
            r#"{{"message":"before/{marker}/after","detail":"api_key={marker}/models"}}"#
        ));
        redact_value(&mut nested);
        let parsed: Value = serde_json::from_str(nested.as_str().unwrap()).unwrap();
        assert_eq!(parsed["message"], format!("before/{marker}/after"));
        assert_eq!(parsed["detail"], format!("api_key={marker}/models"));

        let sse =
            format!("data:{{\"message\":\"before/{marker}/after api_key={marker}/models\"}}\n\n");
        assert_eq!(redact_text(&sse), sse);

        let adjacent = format!(
            "api_key=LEADING{marker}/models&token={marker}TRAILING/tail Authorization: Bearer {marker}HEADER/path"
        );
        assert_eq!(
            redact_text(&adjacent),
            format!(
                "api_key=***{marker}/models&token={marker}***/tail Authorization: Bearer {marker}***/path"
            )
        );
        let encoded = format!("api%5Fkey={marker}ENCODED/end");
        assert_eq!(redact_text(&encoded), format!("api%5Fkey={marker}***/end"));
    }

    #[test]
    fn structured_credential_fields_remain_redacted_with_marker_values() {
        let marker = redaction_marker();
        let mut value = serde_json::json!({
            "api_key": marker,
            "authorization": format!("Bearer {}", redaction_marker()),
            "safe": format!("before/{}/after", redaction_marker()),
        });

        let report = redact_value(&mut value);

        assert_eq!(value["api_key"], REDACTED);
        assert_eq!(value["authorization"], REDACTED);
        assert_eq!(
            value["safe"],
            format!("before/{}/after", redaction_marker())
        );
        assert_eq!(
            report.kinds.into_iter().collect::<Vec<_>>(),
            vec![RedactionKind::CredentialField]
        );
    }

    #[test]
    fn incomplete_and_legacy_markers_receive_no_credential_exemption() {
        let id = "abcdefghijklmnopqrstuvwxyzab";
        let legacy_id = "0123456789abcdef0123456789abcdef";
        for candidate in [
            format!("{}{}", marker::PREFIX, id),
            format!("<!-- stravia-redaction-marker:rm_{legacy_id} -->"),
            format!("~stravia-secret:{legacy_id}~"),
        ] {
            let redacted = redact_text(&format!("api_key={candidate}"));
            assert_ne!(redacted, format!("api_key={candidate}"));
            assert!(redacted.contains(REDACTED));
        }
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
