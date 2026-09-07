//! Incremental restoration. Only an incomplete reference and JSON escape are retained.
use std::collections::{BTreeMap, BTreeSet};

use super::{RedactionError, store::Mapping, text};
use crate::protocol::ir::AiStreamDelta;

#[derive(Default)]
struct ReferenceScanner {
    // Raw spelling is retained so unknown references keep their original JSON escapes.
    pending: String,
    spelling: String,
}

impl ReferenceScanner {
    fn flush(&mut self, output: &mut String) {
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
        output: &mut String,
    ) -> Result<(), RedactionError> {
        let position = self.pending.len();
        let matches = if position < text::PREFIX.len() {
            ch == char::from(text::PREFIX.as_bytes()[position])
        } else if position < text::REFERENCE_LEN - 1 {
            ch.is_ascii_digit() || ('a'..='f').contains(&ch)
        } else {
            ch == '~'
        };
        if matches {
            self.pending.push(ch);
            self.spelling.push_str(raw);
            if self.pending.len() == text::REFERENCE_LEN {
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
            // A valid unfinished reference contains no second '~', so only
            // the current character can begin an overlapping candidate.
            if ch == '~' {
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
    fn push(
        &mut self,
        input: &str,
        mappings: &[Mapping],
        used: &mut BTreeSet<String>,
    ) -> Result<String, RedactionError> {
        let mut output = String::with_capacity(input.len());
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
        Ok(output)
    }

    fn finish(&mut self) -> String {
        let mut output = String::new();
        self.references.flush(&mut output);
        output.push_str(&std::mem::take(&mut self.escape));
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
            let mut output = String::new();
            self.plain.flush(&mut output);
            output
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
            let restored = if key.kind == 5 {
                pending.json.push(input, mappings, &mut self.used)?
            } else {
                let mut restored = String::with_capacity(input.len());
                for ch in input.chars() {
                    pending.plain.push(
                        ch,
                        ch.encode_utf8(&mut [0; 4]),
                        false,
                        mappings,
                        &mut self.used,
                        &mut restored,
                    )?;
                }
                restored
            };
            *input = restored;
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
