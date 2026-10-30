//! Incremental restoration. Only an incomplete reference and JSON escape are retained.
use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};

use super::{RedactionError, marker, store::Mapping, text};
use stravia_runtime_contract::protocol::ir::AiStreamDelta;

trait TextOutput {
    fn push_str(&mut self, text: &str);
}

impl TextOutput for String {
    fn push_str(&mut self, text: &str) {
        String::push_str(self, text);
    }
}

// Keep the input borrowed until emitted bytes actually differ, including when a
// candidate prefix is withheld for the next fragment.
struct Output<'a> {
    input: &'a str,
    unchanged: usize,
    changed: Option<String>,
}

impl<'a> Output<'a> {
    fn new(input: &'a str) -> Self {
        Self {
            input,
            unchanged: 0,
            changed: None,
        }
    }

    fn push(&mut self, ch: char) {
        self.push_str(ch.encode_utf8(&mut [0; 4]));
    }

    fn finish(self) -> Cow<'a, str> {
        match self.changed {
            Some(output) => Cow::Owned(output),
            None if self.unchanged == self.input.len() => Cow::Borrowed(self.input),
            None => Cow::Owned(self.input[..self.unchanged].to_owned()),
        }
    }
}

impl TextOutput for Output<'_> {
    fn push_str(&mut self, text: &str) {
        if let Some(output) = &mut self.changed {
            output.push_str(text);
        } else if self.input[self.unchanged..].starts_with(text) {
            self.unchanged += text.len();
        } else {
            let mut output =
                String::with_capacity(self.input.len().max(self.unchanged + text.len()));
            output.push_str(&self.input[..self.unchanged]);
            output.push_str(text);
            self.changed = Some(output);
        }
    }
}

#[derive(Default)]
struct ReferenceScanner {
    // Raw spelling is retained so unknown references keep their original JSON escapes.
    pending: String,
    spelling: String,
}

impl ReferenceScanner {
    fn text(
        &mut self,
        input: &mut String,
        mappings: &[Mapping],
        used: &mut BTreeSet<String>,
    ) -> Result<(), RedactionError> {
        if self.pending.is_empty() && !input.contains('<') {
            return Ok(());
        }
        let mut output = Output::new(input);
        let mut position = 0;
        while position < input.len() {
            if self.pending.is_empty() {
                let rest = &input[position..];
                let end = rest
                    .find('<')
                    .map_or(input.len(), |offset| position + offset);
                output.push_str(&input[position..end]);
                position = end;
                if position == input.len() {
                    break;
                }
            }
            let ch = input[position..]
                .chars()
                .next()
                .expect("character boundary");
            let end = position + ch.len_utf8();
            self.push(
                ch,
                &input[position..end],
                false,
                mappings,
                used,
                &mut output,
            )?;
            position = end;
        }
        if let Cow::Owned(output) = output.finish() {
            *input = output;
        }
        Ok(())
    }

    fn flush(&mut self, output: &mut impl TextOutput) {
        output.push_str(&self.spelling);
        self.pending.clear();
        self.spelling.clear();
    }

    fn push(
        &mut self,
        ch: char,
        raw: &str,
        json: bool,
        mappings: &[Mapping],
        used: &mut BTreeSet<String>,
        output: &mut impl TextOutput,
    ) -> Result<(), RedactionError> {
        let position = self.pending.len();
        let matches = ch.is_ascii() && marker::matches_byte(position, ch as u8);
        if matches {
            self.pending.push(ch);
            self.spelling.push_str(raw);
            if self.pending.len() == marker::REFERENCE_LEN {
                if let Some(mapping) = mappings
                    .iter()
                    .find(|mapping| mapping.reference == self.pending && text::active(mapping))
                {
                    if json {
                        let escaped = serde_json::to_string(&mapping.secret)
                            .map_err(|_| RedactionError::InvalidText)?;
                        output.push_str(&escaped[1..escaped.len() - 1]);
                    } else {
                        output.push_str(&mapping.secret);
                    }
                    used.insert(mapping.reference.clone());
                    self.pending.clear();
                    self.spelling.clear();
                } else {
                    self.flush(output);
                }
            }
        } else {
            self.flush(output);
            // A valid unfinished reference contains no second '<', so only
            // the current character can begin an overlapping candidate.
            if ch == '<' {
                self.pending.push(ch);
                self.spelling.push_str(raw);
            } else {
                output.push_str(raw);
            }
        }
        Ok(())
    }
}

#[derive(Default)]
struct JsonScanner {
    inside: bool,
    key: bool,
    expect_key: bool,
    // JSON nesting is syntax state, not buffered document content. Match serde_json's limit.
    objects: Vec<bool>,
    escape: String,
    references: ReferenceScanner,
}

impl JsonScanner {
    fn push<'a>(
        &mut self,
        input: &'a str,
        mappings: &[Mapping],
        used: &mut BTreeSet<String>,
    ) -> Result<Cow<'a, str>, RedactionError> {
        let mut output = Output::new(input);
        for ch in input.chars() {
            if !self.inside {
                match ch {
                    '"' => {
                        self.inside = true;
                        self.key = self.expect_key;
                    }
                    '{' | '[' => {
                        if self.objects.len() >= 128 {
                            return Err(RedactionError::InvalidText);
                        }
                        self.objects.push(ch == '{');
                        self.expect_key = ch == '{';
                    }
                    '}' | ']' => {
                        self.objects.pop();
                        self.expect_key = false;
                    }
                    ',' => self.expect_key = self.objects.last().copied().unwrap_or(false),
                    ':' => self.expect_key = false,
                    _ => {}
                }
                output.push(ch);
                continue;
            }
            if !self.escape.is_empty() {
                if !ch.is_ascii() {
                    return Err(RedactionError::InvalidText);
                }
                self.escape.push(ch);
                let bytes = self.escape.as_bytes();
                let expected = if bytes.get(1) == Some(&b'u') { 6 } else { 2 };
                if bytes.len() < expected {
                    continue;
                }
                if expected == 6 {
                    let high = u16::from_str_radix(&self.escape[2..6], 16)
                        .map_err(|_| RedactionError::InvalidText)?;
                    if (0xd800..=0xdbff).contains(&high) && bytes.len() < 12 {
                        continue;
                    }
                }
                let token = format!("\"{}\"", self.escape);
                let decoded: String =
                    serde_json::from_str(&token).map_err(|_| RedactionError::InvalidText)?;
                let decoded = decoded.chars().next().ok_or(RedactionError::InvalidText)?;
                if self.key {
                    output.push_str(&self.escape);
                } else {
                    self.references.push(
                        decoded,
                        &self.escape,
                        true,
                        mappings,
                        used,
                        &mut output,
                    )?;
                }
                self.escape.clear();
                continue;
            }
            match ch {
                '\\' => self.escape.push(ch),
                '"' => {
                    self.references.flush(&mut output);
                    self.inside = false;
                    output.push(ch);
                }
                _ => {
                    if ch.is_control() {
                        return Err(RedactionError::InvalidText);
                    }
                    if self.key {
                        output.push(ch);
                    } else {
                        self.references.push(
                            ch,
                            ch.encode_utf8(&mut [0; 4]),
                            true,
                            mappings,
                            used,
                            &mut output,
                        )?;
                    }
                }
            }
        }
        Ok(output.finish())
    }

    fn finish(&mut self) -> String {
        self.references.pending.clear();
        let mut output = std::mem::take(&mut self.references.spelling);
        if output.is_empty() {
            return std::mem::take(&mut self.escape);
        }
        output.push_str(&self.escape);
        self.escape.clear();
        output
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Field {
    kind: u8,
    output: Option<usize>,
    content: Option<usize>,
}

impl Field {
    fn unindexed(self) -> bool {
        self.kind != 5 && self.output.is_none()
    }
}

struct PendingField {
    plain: ReferenceScanner,
    json: JsonScanner,
    template: AiStreamDelta,
}

impl PendingField {
    fn finish(mut self) -> Option<AiStreamDelta> {
        let output = if matches!(self.template, AiStreamDelta::ToolCallDelta { .. }) {
            self.json.finish()
        } else {
            std::mem::take(&mut self.plain.spelling)
        };
        if output.is_empty() {
            return None;
        }
        *delta_text(&mut self.template).expect("readable template") = output;
        Some(self.template)
    }
}

fn field(delta: &AiStreamDelta) -> Option<Field> {
    let (kind, output, content) = match delta {
        AiStreamDelta::TextDelta(_) => (0, None, None),
        AiStreamDelta::TextDeltaWithMetadata {
            output_index,
            content_index,
            ..
        } => (0, *output_index, *content_index),
        AiStreamDelta::RefusalDelta(_) => (1, None, None),
        AiStreamDelta::RefusalDeltaWithIndex {
            output_index,
            content_index,
            ..
        } => (1, Some(*output_index), Some(*content_index)),
        AiStreamDelta::ThinkingDelta(_) => (2, None, None),
        AiStreamDelta::ThinkingDeltaWithMetadata {
            output_index,
            content_index,
            ..
        } => (3, *output_index, *content_index),
        AiStreamDelta::ReasoningSummaryDelta {
            output_index,
            content_index,
            ..
        } => (4, *output_index, *content_index),
        AiStreamDelta::ToolCallDelta { index, .. } => (5, Some(*index), None),
        _ => return None,
    };
    Some(Field {
        kind,
        output,
        content,
    })
}

fn delta_text(delta: &mut AiStreamDelta) -> Option<&mut String> {
    match delta {
        AiStreamDelta::TextDelta(text)
        | AiStreamDelta::RefusalDelta(text)
        | AiStreamDelta::ThinkingDelta(text)
        | AiStreamDelta::TextDeltaWithMetadata { text, .. }
        | AiStreamDelta::RefusalDeltaWithIndex { text, .. }
        | AiStreamDelta::ThinkingDeltaWithMetadata { text, .. }
        | AiStreamDelta::ReasoningSummaryDelta { text, .. } => Some(text),
        AiStreamDelta::ToolCallDelta { arguments, .. } => Some(arguments),
        _ => None,
    }
}

fn empty_template(delta: &AiStreamDelta) -> AiStreamDelta {
    // Do not retain chunk-sized text/logprobs, or replay dated token metadata on a flush.
    match delta {
        AiStreamDelta::TextDelta(_) => AiStreamDelta::TextDelta(String::new()),
        AiStreamDelta::RefusalDelta(_) => AiStreamDelta::RefusalDelta(String::new()),
        AiStreamDelta::ThinkingDelta(_) => AiStreamDelta::ThinkingDelta(String::new()),
        AiStreamDelta::TextDeltaWithMetadata {
            output_index,
            content_index,
            ..
        } => AiStreamDelta::TextDeltaWithMetadata {
            text: String::new(),
            logprobs: Vec::new(),
            obfuscation: None,
            output_index: *output_index,
            content_index: *content_index,
        },
        AiStreamDelta::RefusalDeltaWithIndex {
            output_index,
            content_index,
            ..
        } => AiStreamDelta::RefusalDeltaWithIndex {
            text: String::new(),
            output_index: *output_index,
            content_index: *content_index,
        },
        AiStreamDelta::ThinkingDeltaWithMetadata {
            output_index,
            content_index,
            ..
        } => AiStreamDelta::ThinkingDeltaWithMetadata {
            text: String::new(),
            obfuscation: None,
            output_index: *output_index,
            content_index: *content_index,
        },
        AiStreamDelta::ReasoningSummaryDelta {
            output_index,
            content_index,
            ..
        } => AiStreamDelta::ReasoningSummaryDelta {
            text: String::new(),
            obfuscation: None,
            output_index: *output_index,
            content_index: *content_index,
        },
        AiStreamDelta::ToolCallDelta { index, .. } => AiStreamDelta::ToolCallDelta {
            index: *index,
            arguments: String::new(),
        },
        _ => unreachable!("only readable deltas have field templates"),
    }
}

#[derive(Default)]
pub struct StreamRestorer {
    fields: BTreeMap<Field, PendingField>,
    unindexed: Option<Field>,
    used: BTreeSet<String>,
    tool_ids: BTreeMap<usize, String>,
}

impl StreamRestorer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn take_restored_references(&mut self) -> Vec<String> {
        std::mem::take(&mut self.used).into_iter().collect()
    }

    fn flush_where(&mut self, predicate: impl Fn(Field) -> bool, output: &mut Vec<AiStreamDelta>) {
        let keys: Vec<_> = self
            .fields
            .keys()
            .copied()
            .filter(|key| predicate(*key))
            .collect();
        for key in keys {
            if let Some(pending) = self.fields.remove(&key).and_then(PendingField::finish) {
                output.push(pending);
            }
            if self.unindexed == Some(key) {
                self.unindexed = None;
            }
        }
    }

    pub fn push(
        &mut self,
        mut delta: AiStreamDelta,
        mappings: &[Mapping],
    ) -> Result<Vec<AiStreamDelta>, RedactionError> {
        let mut output = Vec::new();
        if let Some(key) = field(&delta) {
            // Legacy unindexed blocks are ordered runs, not one turn-wide text bucket.
            if let Some(previous) = self.unindexed
                && previous != key
            {
                self.flush_where(|field| field == previous, &mut output);
            }
            if key.unindexed() {
                self.unindexed = Some(key);
            }
            let pending = self.fields.entry(key).or_insert_with(|| PendingField {
                plain: ReferenceScanner::default(),
                json: JsonScanner::default(),
                template: empty_template(&delta),
            });
            let input = delta_text(&mut delta).expect("readable delta");
            if key.kind == 5 {
                if let Cow::Owned(restored) = pending.json.push(input, mappings, &mut self.used)? {
                    *input = restored;
                }
            } else {
                pending.plain.text(input, mappings, &mut self.used)?;
            }
            // Even an empty readable delta can carry logprobs or obfuscation.
            output.push(delta);
            return Ok(output);
        }
        match &mut delta {
            AiStreamDelta::ToolCallStart { index, id, .. } => {
                self.flush_where(|key| key.unindexed(), &mut output);
                if self
                    .tool_ids
                    .get(index)
                    .is_some_and(|previous| !id.is_empty() && previous != id)
                {
                    self.flush_where(
                        |key| key.kind == 5 && key.output == Some(*index),
                        &mut output,
                    );
                }
                if !id.is_empty() {
                    self.tool_ids.insert(*index, id.clone());
                }
            }
            AiStreamDelta::ToolCallComplete { index, tool_call } => {
                self.flush_where(
                    |key| key.kind == 5 && key.output == Some(*index),
                    &mut output,
                );
                text::restore_arguments(&mut tool_call.arguments, mappings, &mut self.used)?;
                self.tool_ids.remove(index);
            }
            AiStreamDelta::ItemDone { index, item } => {
                let calls: BTreeSet<_> = item
                    .tool_calls
                    .iter()
                    .flatten()
                    .map(|call| call.id.as_str())
                    .collect();
                let tool_indices: BTreeSet<_> = self
                    .tool_ids
                    .iter()
                    .filter_map(|(index, id)| calls.contains(id.as_str()).then_some(*index))
                    .collect();
                self.flush_where(
                    |key| {
                        key.unindexed()
                            || (key.kind != 5 && key.output == Some(*index))
                            || (key.kind == 5
                                && key
                                    .output
                                    .is_some_and(|index| tool_indices.contains(&index)))
                    },
                    &mut output,
                );
                for index in tool_indices {
                    self.tool_ids.remove(&index);
                }
                text::restore_item(item, mappings, &mut self.used)?;
            }
            AiStreamDelta::Unknown { raw } => {
                if text::restore_unknown(raw, mappings, &mut self.used)? {
                    self.flush_where(|key| key.unindexed(), &mut output);
                }
            }
            AiStreamDelta::ThinkingSignature(_) | AiStreamDelta::ProtectedThinkingStart { .. } => {
                self.flush_where(|key| key.unindexed(), &mut output)
            }
            AiStreamDelta::Done { .. }
            | AiStreamDelta::ResponseTerminal { .. }
            | AiStreamDelta::StreamError { .. }
            | AiStreamDelta::UnexpectedEof
            | AiStreamDelta::MessageStart { .. } => {
                self.flush_where(|_| true, &mut output);
                self.tool_ids.clear();
            }
            _ => {}
        }
        output.push(delta);
        Ok(output)
    }

    pub fn finish(&mut self, _mappings: &[Mapping]) -> Result<Vec<AiStreamDelta>, RedactionError> {
        let mut output = Vec::new();
        self.flush_where(|_| true, &mut output);
        self.tool_ids.clear();
        Ok(output)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn json_scanner_keeps_unknown_escaped_atoms_and_restores_across_every_boundary() {
        let reference = "<!--sr:abcdefghijklmnopqrstuvwxyzab-->";
        let unknown = "<!--sr:zzzzzzzzzzzzzzzzzzzzzzzzzzzz-->";
        let secret = "密钥\"\\\n";
        let mappings = [Mapping {
            reference: reference.into(),
            secret: secret.into(),
            expires_at: i64::MAX,
            rule_ids: Default::default(),
        }];
        let input = format!(
            "{{\"{reference}\":\"前缀\\u003c!--sr:abcdefghijklmnopqrstuvwxyzab-->尾部\",\"other\":\"\\u003c{}\"}}",
            &unknown[1..],
        );
        let encoded = serde_json::to_string(secret).unwrap();
        let expected = input.replace(
            "\\u003c!--sr:abcdefghijklmnopqrstuvwxyzab-->",
            &encoded[1..encoded.len() - 1],
        );
        for split in input
            .char_indices()
            .map(|(index, _)| index)
            .chain([input.len()])
        {
            let mut scanner = JsonScanner::default();
            let mut used = BTreeSet::new();
            let mut output = String::new();
            for chunk in [&input[..split], &input[split..]] {
                output.push_str(&scanner.push(chunk, &mappings, &mut used).unwrap());
            }
            output.push_str(&scanner.finish());
            assert_eq!(output, expected, "split at {split}");
            assert_eq!(used, BTreeSet::from([reference.to_owned()]));
        }
    }

    use super::*;

    #[test]
    fn gemini_response_metadata_preserves_fragmented_client_text() {
        let reference = "<!--sr:abcdefghijklmnopqrstuvwxyzab-->";
        let mappings = [Mapping {
            reference: reference.into(),
            secret: "restored-credential".into(),
            expires_at: i64::MAX,
            rule_ids: Default::default(),
        }];
        for split in 1..reference.len() {
            let mut restorer = StreamRestorer::new();
            let metadata = AiStreamDelta::Unknown {
                raw: serde_json::json!({
                    "__google_response_metadata": {
                        "responseId": "gemini-response",
                        "modelVersion": "gemini-model"
                    }
                })
                .to_string(),
            };
            let mut client_text = String::new();
            let mut completed_text = None;
            let mut saw_metadata = false;
            for delta in [
                AiStreamDelta::TextDelta(format!("before {}", &reference[..split])),
                metadata.clone(),
                AiStreamDelta::TextDelta(format!("{} after", &reference[split..])),
                AiStreamDelta::ItemDone {
                    index: 0,
                    item: stravia_runtime_contract::protocol::ir::AiItem::output_text(format!(
                        "before {reference} after"
                    )),
                },
                AiStreamDelta::Done {
                    stop_reason: "stop".into(),
                },
            ] {
                for restored in restorer.push(delta, &mappings).unwrap() {
                    match restored {
                        AiStreamDelta::TextDelta(text) => client_text.push_str(&text),
                        AiStreamDelta::ItemDone { item, .. } => {
                            completed_text = item.output_text_ref().map(str::to_owned);
                        }
                        AiStreamDelta::Unknown { raw } => {
                            let AiStreamDelta::Unknown { raw: expected } = &metadata else {
                                unreachable!()
                            };
                            assert_eq!(&raw, expected);
                            saw_metadata = true;
                        }
                        _ => {}
                    }
                }
            }
            assert!(saw_metadata);
            assert_eq!(client_text, "before restored-credential after");
            assert_eq!(completed_text.as_deref(), Some(client_text.as_str()));
            assert_eq!(restorer.take_restored_references(), vec![reference]);
            assert!(restorer.finish(&mappings).unwrap().is_empty());
        }
    }

    #[test]
    fn inline_marker_restoration_preserves_paths_at_every_split() {
        let reference = "<!--sr:abcdefghijklmnopqrstuvwxyzab-->";
        let unknown = "<!--sr:zzzzzzzzzzzzzzzzzzzzzzzzzzzz-->";
        let legacy = "~stravia-secret:0123456789abcdef0123456789abcdef~";
        let mappings = [Mapping {
            reference: reference.into(),
            secret: "stravia".into(),
            expires_at: i64::MAX,
            rule_ids: Default::default(),
        }];
        let input = format!("前缀<<backend/crates/{reference}-core/src {unknown} {legacy} <!--sr:");
        let expected = input.replace(reference, "stravia");
        for split in input
            .char_indices()
            .map(|(index, _)| index)
            .chain(std::iter::once(input.len()))
        {
            let mut restorer = StreamRestorer::new();
            let mut output = String::new();
            for chunk in [&input[..split], &input[split..]] {
                for delta in restorer
                    .push(AiStreamDelta::TextDelta(chunk.into()), &mappings)
                    .unwrap()
                {
                    let AiStreamDelta::TextDelta(text) = delta else {
                        panic!("unexpected delta");
                    };
                    output.push_str(&text);
                }
            }
            for delta in restorer.finish(&mappings).unwrap() {
                let AiStreamDelta::TextDelta(text) = delta else {
                    panic!("unexpected trailing delta");
                };
                output.push_str(&text);
            }
            assert_eq!(output, expected, "split at {split}");
            assert_eq!(restorer.take_restored_references(), [reference]);
        }
    }
}
