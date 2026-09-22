use regex::Regex;
use serde::de::IgnoredAny;
use std::borrow::Cow;
use std::fmt::Write as _;
use std::iter::Peekable;
use std::sync::LazyLock;

static LIST_MARKER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?:[-*+]\s+|[0-9]+[.):]\s+|\[[0-9]+\]\s+)(.+)$")
        .expect("tool-description list marker compiles")
});

/// Formats a pre-cleaned description. XML escaping remains the caller's job.
pub(super) fn format_tool_description(description: &str) -> String {
    let normalized = normalize_newlines(description);
    let mut lines = normalized.split('\n').peekable();
    let mut writer = DescriptionWriter::new(normalized.len());

    while let Some(line) = lines.next() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            writer.preserve_paragraph_break();
            continue;
        }
        if is_fence_line(trimmed) {
            writer.write_fenced_block(line, &mut lines);
            continue;
        }
        if let Some(item) = list_item_text(trimmed) {
            writer.write_text(item.trim());
            continue;
        }

        let mut paragraph = None;
        while let Some(next) = lines.peek().copied() {
            let next_trimmed = next.trim();
            if next_trimmed.is_empty()
                || is_fence_line(next_trimmed)
                || list_item_text(next_trimmed).is_some()
            {
                break;
            }

            let next = lines.next().expect("peeked line remains available");
            let buffer = paragraph.get_or_insert_with(|| {
                let mut buffer = String::with_capacity(line.len() + next.len() + 1);
                buffer.push_str(line);
                buffer
            });
            buffer.push('\n');
            buffer.push_str(next);
        }

        match paragraph {
            Some(paragraph) => writer.write_paragraph(paragraph.trim()),
            None => writer.write_paragraph(trimmed),
        }
    }

    writer.finish()
}

struct DescriptionWriter {
    output: String,
    next_number: usize,
    paragraph_break_pending: bool,
}

impl DescriptionWriter {
    fn new(capacity: usize) -> Self {
        Self {
            output: String::with_capacity(capacity),
            next_number: 1,
            paragraph_break_pending: false,
        }
    }

    fn preserve_paragraph_break(&mut self) {
        if !self.output.is_empty() {
            self.paragraph_break_pending = true;
        }
    }

    fn write_paragraph(&mut self, text: &str) {
        if serde_json::from_str::<IgnoredAny>(text).is_ok() {
            self.start_block();
            self.output.push_str(text);
        } else {
            self.write_text(text);
        }
    }

    fn write_text(&mut self, text: &str) {
        for sentence in SentenceSlices::new(text) {
            self.start_block();
            write!(self.output, "{}. {sentence}", self.next_number)
                .expect("writing to a String cannot fail");
            self.next_number += 1;
        }
    }

    fn write_fenced_block<'a, I>(&mut self, opening: &str, lines: &mut Peekable<I>)
    where
        I: Iterator<Item = &'a str>,
    {
        self.start_block();
        self.output.push_str(opening);
        for line in lines.by_ref() {
            self.output.push('\n');
            self.output.push_str(line);
            if is_fence_line(line.trim()) {
                break;
            }
        }
    }

    fn start_block(&mut self) {
        if self.output.is_empty() {
            self.paragraph_break_pending = false;
            return;
        }
        self.output.push('\n');
        if self.paragraph_break_pending {
            self.output.push('\n');
            self.paragraph_break_pending = false;
        }
    }

    fn finish(mut self) -> String {
        self.output.truncate(self.output.trim_end().len());
        let leading_whitespace = self.output.len() - self.output.trim_start().len();
        if leading_whitespace != 0 {
            self.output.replace_range(..leading_whitespace, "");
        }
        self.output
    }
}

struct SentenceSlices<'a> {
    text: &'a str,
    sentence_start: usize,
    cursor: usize,
}

impl<'a> SentenceSlices<'a> {
    fn new(text: &'a str) -> Self {
        Self {
            text,
            sentence_start: 0,
            cursor: 0,
        }
    }
}

impl<'a> Iterator for SentenceSlices<'a> {
    type Item = &'a str;

    fn next(&mut self) -> Option<Self::Item> {
        while self.cursor < self.text.len() {
            let offset = self.cursor;
            let character = self.text[offset..]
                .chars()
                .next()
                .expect("cursor is before the end of the string");
            let end = offset + character.len_utf8();
            self.cursor = end;

            if !is_sentence_terminator(character) {
                continue;
            }

            let mut next_content = end;
            for (relative_offset, following) in self.text[end..].char_indices() {
                if !following.is_whitespace() {
                    break;
                }
                next_content = end + relative_offset + following.len_utf8();
            }

            let boundary = next_content > end || is_cjk_terminator(character);
            if boundary
                && next_content < self.text.len()
                && !ends_in_abbreviation(&self.text[self.sentence_start..end])
            {
                let sentence = self.text[self.sentence_start..end].trim();
                self.sentence_start = next_content;
                self.cursor = next_content;
                return Some(sentence);
            }
        }

        let sentence = self.text[self.sentence_start..].trim();
        self.sentence_start = self.text.len();
        self.cursor = self.text.len();
        (!sentence.is_empty()).then_some(sentence)
    }
}

fn list_item_text(line: &str) -> Option<&str> {
    LIST_MARKER
        .captures(line)
        .and_then(|captures| captures.get(1))
        .map(|item| item.as_str())
}

fn is_fence_line(line: &str) -> bool {
    line.starts_with("```") || line.starts_with("~~~")
}

fn is_sentence_terminator(character: char) -> bool {
    matches!(character, '.' | '!' | '?' | '。' | '！' | '？')
}

fn is_cjk_terminator(character: char) -> bool {
    matches!(character, '。' | '！' | '？')
}

fn ends_in_abbreviation(fragment: &str) -> bool {
    const ABBREVIATIONS: [&str; 9] = [
        "e.g.", "i.e.", "etc.", "vs.", "mr.", "mrs.", "dr.", "prof.", "no.",
    ];

    let Some(last_word) = fragment.split_whitespace().next_back() else {
        return false;
    };
    let last_word = last_word.trim_matches(['"', '\'', '(', ')', '[', ']', '{', '}']);
    ABBREVIATIONS
        .iter()
        .any(|abbreviation| last_word.eq_ignore_ascii_case(abbreviation))
}

fn normalize_newlines(description: &str) -> Cow<'_, str> {
    if !description.contains('\r') {
        return Cow::Borrowed(description);
    }

    let mut normalized = String::with_capacity(description.len());
    let mut unprocessed = description;
    while let Some(carriage_return) = unprocessed.find('\r') {
        normalized.push_str(&unprocessed[..carriage_return]);
        normalized.push('\n');
        unprocessed = &unprocessed[carriage_return + 1..];
        if let Some(without_line_feed) = unprocessed.strip_prefix('\n') {
            unprocessed = without_line_feed;
        }
    }
    normalized.push_str(unprocessed);
    Cow::Owned(normalized)
}

#[cfg(test)]
mod tests {
    use super::format_tool_description;

    #[test]
    fn prose_and_existing_lists_are_continuously_numbered() {
        let description = "Create a file. Then verify it!\n- 删除旧文件。重建它？\n* Keep metadata.\n+ Report status.\n42. Dot form.\n7) Finish.\n8: Archive.\n[99] Done.\n- {\"enabled\":true}";

        assert_eq!(
            format_tool_description(description),
            "1. Create a file.\n2. Then verify it!\n3. 删除旧文件。\n4. 重建它？\n5. Keep metadata.\n6. Report status.\n7. Dot form.\n8. Finish.\n9. Archive.\n10. Done.\n11. {\"enabled\":true}"
        );
    }

    #[test]
    fn fenced_code_and_complete_json_paragraphs_keep_their_structure() {
        let description = r#"Explain first.

```json
{"inside": "Do not split. Still code!"}

{"second": true}
```

{
  "description": "Keep. This!",
  "nested": [
    1,
    2
  ]
}

~~~text
Do not split this. Or this!
~~~

Afterwards?"#;

        let expected = r#"1. Explain first.

```json
{"inside": "Do not split. Still code!"}

{"second": true}
```

{
  "description": "Keep. This!",
  "nested": [
    1,
    2
  ]
}

~~~text
Do not split this. Or this!
~~~

2. Afterwards?"#;

        assert_eq!(format_tool_description(description), expected);
    }

    #[test]
    fn abbreviations_decimals_and_blank_paragraphs_keep_sentence_boundaries() {
        let description = "Compare e.g. A, i.e. B, etc. vs. C with Mr. X, Mrs. Y, Dr. Z, Prof. Q, no. 7. Value is 3.14.\r\n\r\n下一段。紧接中文！\rLast question?";

        assert_eq!(
            format_tool_description(description),
            "1. Compare e.g. A, i.e. B, etc. vs. C with Mr. X, Mrs. Y, Dr. Z, Prof. Q, no. 7.\n2. Value is 3.14.\n\n3. 下一段。\n4. 紧接中文！\n5. Last question?"
        );
    }
}
