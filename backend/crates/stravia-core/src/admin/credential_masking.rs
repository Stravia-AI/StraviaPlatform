//! 命中正文的读时遮罩：把检测器已经算出的匹配区间替换为定长占位符，并重算匹配位置。

use stravia_credential_protection::{CredentialTextMatch, advance_position};

/// 固定长度的遮蔽占位符：六个 U+2022，不随秘密长度变化。
pub(super) const MASK_TOKEN: &str = "\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}";

pub(super) fn mask_matches(
    text: &str,
    matches: Vec<CredentialTextMatch>,
) -> (String, Vec<CredentialTextMatch>) {
    let mut offsets: Vec<usize> = matches
        .iter()
        .flat_map(|found| [found.start, found.end])
        .collect();
    offsets.sort_unstable();
    offsets.dedup();
    let bytes = boundary_bytes(text, &offsets);
    let boundary = |offset: usize| -> Option<usize> { bytes[offsets.binary_search(&offset).ok()?] };

    // 零长度（含反向区间）、越界与落在代理对内部的匹配都无法在 UTF-8 上切分，直接丢弃。
    let mut valid = Vec::with_capacity(matches.len());
    let mut retained = vec![false; matches.len()];
    for (index, found) in matches.iter().enumerate() {
        if found.start >= found.end {
            continue;
        }
        let (Some(start_byte), Some(end_byte)) = (boundary(found.start), boundary(found.end))
        else {
            continue;
        };
        retained[index] = true;
        valid.push((index, found.start, found.end, start_byte, end_byte));
    }
    valid.sort_unstable_by_key(|&(index, start, end, _, _)| (start, end, index));

    struct Span {
        end: usize,
        start_byte: usize,
        end_byte: usize,
        members: Vec<usize>,
    }
    // 重叠与相邻（`start == 上一个 end`）的区间合并成一个占位符。
    let mut spans: Vec<Span> = Vec::new();
    for (index, start, end, start_byte, end_byte) in valid {
        match spans.last_mut() {
            Some(span) if start <= span.end => {
                if end > span.end {
                    span.end = end;
                    span.end_byte = end_byte;
                }
                span.members.push(index);
            }
            _ => spans.push(Span {
                end,
                start_byte,
                end_byte,
                members: vec![index],
            }),
        }
    }

    // 单趟构建遮蔽文本：行列按遮蔽后的文本推进，匹配区间里的换行不再影响后续位置。
    let mut masked = String::with_capacity(text.len());
    let mut position = (0usize, 1usize, 1usize);
    let mut cursor = 0usize;
    let mut tokens = Vec::with_capacity(spans.len());
    for span in &spans {
        masked.push_str(&text[cursor..span.start_byte]);
        position = advance_position(&text[cursor..span.start_byte], position);
        let token_start = position;
        masked.push_str(MASK_TOKEN);
        position = advance_position(MASK_TOKEN, position);
        tokens.push((token_start, position));
        cursor = span.end_byte;
    }
    masked.push_str(&text[cursor..]);

    let mut remapped: Vec<Option<CredentialTextMatch>> = matches
        .into_iter()
        .zip(retained)
        .map(|(found, retained)| retained.then_some(found))
        .collect();
    for (span, (token_start, token_end)) in spans.iter().zip(tokens) {
        for &index in &span.members {
            let Some(mut found) = remapped[index].take() else {
                continue;
            };
            found.start = token_start.0;
            found.end = token_end.0;
            found.start_line = token_start.1;
            found.start_column = token_start.2;
            found.end_line = token_end.1;
            found.end_column = token_end.2;
            remapped[index] = Some(found);
        }
    }

    (masked, remapped.into_iter().flatten().collect())
}

/// 把有序的 UTF-16 偏移映射为字节偏移；落在代理对内部或超出文本长度时为 `None`。
fn boundary_bytes(text: &str, targets: &[usize]) -> Vec<Option<usize>> {
    let mut resolved = vec![None; targets.len()];
    let mut target = 0usize;
    let mut utf16 = 0usize;
    for (byte, character) in text.char_indices() {
        while let Some(&wanted) = targets.get(target) {
            if wanted > utf16 {
                break;
            }
            if wanted == utf16 {
                resolved[target] = Some(byte);
            }
            target += 1;
        }
        utf16 += character.len_utf16();
    }
    while let Some(&wanted) = targets.get(target) {
        if wanted == utf16 {
            resolved[target] = Some(text.len());
        }
        target += 1;
    }
    resolved
}

#[cfg(test)]
mod tests {
    use super::*;

    fn match_at(rule_id: &str, start: usize, end: usize, hit: bool) -> CredentialTextMatch {
        CredentialTextMatch {
            rule_id: rule_id.to_owned(),
            start,
            end,
            start_line: 0,
            start_column: 0,
            end_line: 0,
            end_column: 0,
            hit,
        }
    }

    #[test]
    fn token_is_six_identical_bullets() {
        assert_eq!(MASK_TOKEN.chars().count(), 6);
        assert!(MASK_TOKEN.chars().all(|character| character == '\u{2022}'));
        assert_eq!(MASK_TOKEN.encode_utf16().count(), 6);
    }

    #[test]
    fn overlapping_adjacent_and_nested_matches_share_one_token_per_merged_span() {
        let text = "abcdefghij";
        let matches = vec![
            match_at("overlap-a", 0, 3, true),
            match_at("overlap-b", 2, 5, false),
            match_at("adjacent", 5, 6, false),
            match_at("outer", 7, 10, true),
            match_at("nested", 8, 9, false),
        ];
        let (masked, remapped) = mask_matches(text, matches);
        assert_eq!(masked, format!("{MASK_TOKEN}g{MASK_TOKEN}"));
        assert_eq!(remapped.len(), 5);
        for (match_, expected) in remapped.iter().zip([
            (0, 6, 1, 1, 1, 7),
            (0, 6, 1, 1, 1, 7),
            (0, 6, 1, 1, 1, 7),
            (7, 13, 1, 8, 1, 14),
            (7, 13, 1, 8, 1, 14),
        ]) {
            assert_eq!(
                (
                    match_.start,
                    match_.end,
                    match_.start_line,
                    match_.start_column,
                    match_.end_line,
                    match_.end_column,
                ),
                expected
            );
        }
        assert_eq!(
            remapped
                .iter()
                .map(|found| found.rule_id.as_str())
                .collect::<Vec<_>>(),
            ["overlap-a", "overlap-b", "adjacent", "outer", "nested"]
        );
        assert_eq!(
            remapped.iter().map(|found| found.hit).collect::<Vec<_>>(),
            [true, false, false, true, false]
        );
        assert!(!masked.contains("abcdef"));
    }

    #[test]
    fn multibyte_text_keeps_utf16_offsets_but_counts_columns_per_character() {
        let text = "中文😀 prefix\n前é s3cr3t tail\nend";
        let start = "中文😀 prefix\n前é ".encode_utf16().count();
        let end = start + "s3cr3t".len();
        let (masked, remapped) = mask_matches(text, vec![match_at("secret", start, end, true)]);
        assert_eq!(masked, format!("中文😀 prefix\n前é {MASK_TOKEN} tail\nend"));
        let found = &remapped[0];
        assert_eq!((found.start, found.end), (start, end));
        assert_eq!((found.start_line, found.start_column), (2, 4));
        assert_eq!((found.end_line, found.end_column), (2, 10));
        assert!(!masked.contains("s3cr3t"));

        // 代理对在偏移上占两个 code unit，在列上仍是一个字符。
        let text = "😀😀key😀";
        let (masked, remapped) = mask_matches(text, vec![match_at("astral", 4, 7, false)]);
        assert_eq!(masked, format!("😀😀{MASK_TOKEN}😀"));
        assert_eq!(
            (
                remapped[0].start,
                remapped[0].end,
                remapped[0].start_column,
                remapped[0].end_column
            ),
            (4, 10, 3, 9)
        );
    }

    #[test]
    fn crlf_lines_advance_after_lf_only_and_matches_at_both_edges_stay_visible() {
        let text = "aa\r\nbb\r\ncc";
        let (masked, remapped) = mask_matches(
            text,
            vec![
                match_at("first", 0, 1, true),
                match_at("middle", 4, 6, false),
                match_at("last", 8, 10, false),
            ],
        );
        assert_eq!(
            masked,
            format!("{MASK_TOKEN}a\r\n{MASK_TOKEN}\r\n{MASK_TOKEN}")
        );
        assert_eq!(
            remapped
                .iter()
                .map(|found| (
                    found.start,
                    found.end,
                    found.start_line,
                    found.start_column,
                    found.end_column
                ))
                .collect::<Vec<_>>(),
            [(0, 6, 1, 1, 7), (9, 15, 2, 1, 7), (17, 23, 3, 1, 7),]
        );
    }

    #[test]
    fn a_match_crossing_a_newline_takes_the_token_position_of_its_start() {
        let text = "a\nb";
        let (masked, remapped) = mask_matches(text, vec![match_at("multiline", 1, 3, true)]);
        assert_eq!(masked, format!("a{MASK_TOKEN}"));
        assert_eq!(
            (
                remapped[0].start,
                remapped[0].end,
                remapped[0].start_line,
                remapped[0].start_column,
                remapped[0].end_line,
                remapped[0].end_column,
            ),
            (1, 7, 1, 2, 1, 8)
        );
    }

    #[test]
    fn zero_length_out_of_range_inverted_and_surrogate_splitting_matches_are_dropped() {
        let text = "😀ab";
        let (masked, remapped) = mask_matches(
            text,
            vec![
                match_at("zero", 2, 2, false),
                match_at("past-end", 3, 5, false),
                match_at("inverted", 3, 2, false),
                match_at("splits-surrogate-pair", 1, 4, false),
                match_at("valid", 2, 3, true),
            ],
        );
        assert_eq!(masked, format!("😀{MASK_TOKEN}b"));
        assert_eq!(remapped.len(), 1);
        assert_eq!(remapped[0].rule_id, "valid");
        assert!(remapped[0].hit);
        assert_eq!((remapped[0].start, remapped[0].end), (2, 8));
        assert_eq!((remapped[0].start_column, remapped[0].end_column), (2, 8));
    }

    #[test]
    fn token_length_does_not_depend_on_secret_length_and_leaves_no_plaintext() {
        let text = "key=abc\nkey=0123456789abcdef";
        let (masked, remapped) = mask_matches(
            text,
            vec![
                match_at("short", 4, 7, true),
                match_at("long", 12, 28, false),
            ],
        );
        assert_eq!(masked, format!("key={MASK_TOKEN}\nkey={MASK_TOKEN}"));
        assert_eq!(
            remapped
                .iter()
                .map(|found| found.end - found.start)
                .collect::<Vec<_>>(),
            [6, 6]
        );
        assert!(!masked.contains("abc") && !masked.contains("0123456789abcdef"));
        let (masked, remapped) =
            mask_matches("0123456789abcdef", vec![match_at("all", 0, 16, false)]);
        assert_eq!(masked, MASK_TOKEN);
        assert_eq!((remapped[0].start, remapped[0].end), (0, 6));
    }

    #[test]
    fn text_without_matches_is_returned_unchanged() {
        let (masked, remapped) = mask_matches("nothing to hide", Vec::new());
        assert_eq!(masked, "nothing to hide");
        assert!(remapped.is_empty());
    }
}
