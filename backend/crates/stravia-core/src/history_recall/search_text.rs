use std::ops::Range;
use unicode_normalization::char::{canonical_combining_class, compose, decompose_compatible};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Token {
    pub(super) text: String,
    pub(super) range: Range<usize>,
}

#[derive(Clone)]
struct MappedChar {
    ch: char,
    range: Range<usize>,
}

// 规范分解、排序及重组时保留来源；一次兼容分解产生的多个字符共享原字符范围。
fn normalized(input: &str) -> Vec<MappedChar> {
    let mut decomposed: Vec<MappedChar> = Vec::new();
    let mut pending = 0;
    for (start, ch) in input.char_indices() {
        decompose_compatible(ch, |part| {
            let class = canonical_combining_class(part);
            if class == 0 {
                decomposed[pending..].sort_by_key(|c| canonical_combining_class(c.ch));
                pending = decomposed.len() + 1;
            }
            decomposed.push(MappedChar {
                ch: part,
                range: start..start + ch.len_utf8(),
            });
        });
    }
    decomposed[pending..].sort_by_key(|c| canonical_combining_class(c.ch));
    let mut result: Vec<MappedChar> = Vec::new();
    let mut starter: Option<usize> = None;
    let mut last_class = 0;
    for item in decomposed {
        let class = canonical_combining_class(item.ch);
        if let Some(index) = starter
            && (last_class == 0 || last_class < class)
            && let Some(ch) = compose(result[index].ch, item.ch)
        {
            result[index].ch = ch;
            result[index].range.start = result[index].range.start.min(item.range.start);
            result[index].range.end = result[index].range.end.max(item.range.end);
            continue;
        }
        if class == 0 {
            starter = Some(result.len());
        }
        last_class = class;
        result.push(item);
    }
    result
        .into_iter()
        .flat_map(|item| {
            item.ch.to_lowercase().map(move |ch| MappedChar {
                ch,
                range: item.range.clone(),
            })
        })
        .collect()
}

fn is_cjk(ch: char) -> bool {
    matches!(ch as u32,
        0x3400..=0x4dbf | 0x4e00..=0x9fff | 0xf900..=0xfaff |
        0x20000..=0x2fa1f | 0x30000..=0x323af |
        0x3040..=0x30ff | 0x31f0..=0x31ff | 0x1b000..=0x1b16f |
        0x1100..=0x11ff | 0x3130..=0x318f | 0xa960..=0xa97f |
        0xac00..=0xd7ff | 0xff66..=0xff9f)
}

pub(super) fn tokenize(input: &str) -> Vec<Token> {
    let chars = normalized(input);
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let cjk = is_cjk(chars[i].ch);
        if !cjk && !chars[i].ch.is_alphanumeric() {
            i += 1;
            continue;
        }
        let start = i;
        i += 1;
        while i < chars.len()
            && if cjk {
                is_cjk(chars[i].ch)
            } else {
                !is_cjk(chars[i].ch) && chars[i].ch.is_alphanumeric()
            }
        {
            i += 1;
        }
        let end = i;
        let width = if cjk && end - start > 1 {
            2
        } else {
            end - start
        };
        for first in start..=end - width {
            let part = &chars[first..first + width];
            let text: String = part.iter().map(|c| c.ch).collect();
            if text.len() <= 64 {
                let range_start = part.iter().map(|c| c.range.start).min().unwrap();
                let range_end = part.iter().map(|c| c.range.end).max().unwrap();
                tokens.push(Token {
                    text,
                    range: range_start..range_end,
                });
            }
            if !cjk {
                break;
            }
        }
    }
    tokens
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct QueryTerm {
    pub(super) tokens: Vec<Token>,
    pub(super) prefix: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ParsedQuery {
    pub(super) terms: Vec<QueryTerm>,
}

pub(super) fn parse_query(input: &str) -> Result<ParsedQuery, &'static str> {
    if input.chars().count() > 256 {
        return Err("Search query must contain at most 256 characters.");
    }
    let mut pieces = Vec::new();
    let mut start = None;
    let mut quoted = false;
    for (index, ch) in input.char_indices() {
        if ch == '"' {
            if quoted {
                pieces.push(&input[start.take().unwrap()..index]);
                quoted = false;
            } else {
                if let Some(begin) = start.take() {
                    pieces.push(&input[begin..index]);
                }
                quoted = true;
                start = Some(index + ch.len_utf8());
            }
        } else if ch.is_whitespace() && !quoted {
            if let Some(begin) = start.take() {
                pieces.push(&input[begin..index]);
            }
        } else if start.is_none() {
            start = Some(index);
        }
    }
    if quoted {
        return Err("Search query has an unclosed quote.");
    }
    if let Some(begin) = start {
        pieces.push(&input[begin..]);
    }
    if pieces.len() > 8 {
        return Err("Search query must contain at most 8 terms.");
    }
    let mut terms = Vec::new();
    for piece in pieces {
        let tokens = tokenize(piece);
        if tokens.len() > 16 {
            return Err("Each search term must contain at most 16 tokens.");
        }
        if !tokens.is_empty() {
            let mut chars = tokens[0].text.chars();
            let prefix =
                tokens.len() == 1 && chars.next().is_some_and(is_cjk) && chars.next().is_none();
            terms.push(QueryTerm { tokens, prefix });
        }
    }
    if terms.is_empty() {
        return Err("Search query has no searchable tokens.");
    }
    Ok(ParsedQuery { terms })
}

#[test]
fn retains_ranges_through_compatibility_and_composition() {
    let tokens = tokenize("ﬃ 가 ㍿");
    assert_eq!(
        tokens.iter().map(|t| t.text.as_str()).collect::<Vec<_>>(),
        ["ffi", "가", "株式", "式会", "会社"]
    );
    assert_eq!(
        tokens.iter().map(|t| t.range.clone()).collect::<Vec<_>>(),
        [0..3, 4..10, 11..14, 11..14, 11..14]
    );
}

#[test]
fn punctuation_does_not_disable_single_cjk_prefix() {
    let query = parse_query("中! x! 中文!").unwrap();
    assert_eq!(
        query
            .terms
            .iter()
            .map(|term| term.prefix)
            .collect::<Vec<_>>(),
        [true, false, false]
    );
}

#[test]
fn tokenization_matches_nfkc_and_lowercase() {
    use unicode_normalization::UnicodeNormalization;

    for input in [
        "ＡＢＣ ㍿ ① ｶﾞ",
        "ﬃ ﬁ ﬄ",
        "e\u{301} a\u{315}\u{300} a\u{300}\u{315}",
        "각 각 한글",
        "ا\u{0654}\u{0651} ש\u{05c1}\u{05b8}",
        "İ ẞ Σ ΟΣ",
        "👩\u{200d}💻 ✨ 中文abcかな한글123",
        "\u{301}\u{323} A\u{301}\u{327}",
    ] {
        let canonical: String = input.nfkc().flat_map(char::to_lowercase).collect();
        let actual: Vec<_> = tokenize(input)
            .into_iter()
            .map(|token| token.text)
            .collect();
        let expected: Vec<_> = tokenize(&canonical)
            .into_iter()
            .map(|token| token.text)
            .collect();
        assert_eq!(actual, expected, "input: {input:?}");
    }
}

#[test]
fn parses_phrases_and_single_character_modes() {
    let query = parse_query("\"run admitted\" 中 x run_admitted").unwrap();
    assert_eq!(
        query
            .terms
            .iter()
            .map(|term| term
                .tokens
                .iter()
                .map(|t| t.text.as_str())
                .collect::<Vec<_>>())
            .collect::<Vec<_>>(),
        [
            vec!["run", "admitted"],
            vec!["中"],
            vec!["x"],
            vec!["run", "admitted"]
        ]
    );
    assert_eq!(
        query
            .terms
            .iter()
            .map(|term| term.prefix)
            .collect::<Vec<_>>(),
        [false, true, false, false]
    );
    assert_eq!(query.terms[3].tokens, tokenize("run_admitted"));
}

#[test]
fn rejects_query_limits_and_empty_tokens() {
    assert!(parse_query("--- ___").is_err());
    assert!(parse_query(&"a ".repeat(9)).is_err());
    assert!(parse_query(&"a".repeat(257)).is_err());
    assert!(parse_query(&format!("\"{}\"", "a ".repeat(17))).is_err());
    assert!(parse_query("\"unterminated").is_err());
    assert!(parse_query(&"a ".repeat(8)).is_ok());
    assert!(parse_query(&format!("a{}", " ".repeat(255))).is_ok());
    assert!(parse_query(&format!("\"{}\"", "a ".repeat(16))).is_ok());
    assert_eq!(tokenize(&"z".repeat(64)).len(), 1);
}

#[test]
fn splits_scripts_and_drops_long_tokens() {
    let input = format!("中文a12_日本.한/カ-{} x", "z".repeat(65));
    let tokens = tokenize(&input);
    assert_eq!(
        tokens.iter().map(|t| t.text.as_str()).collect::<Vec<_>>(),
        ["中文", "a12", "日本", "한", "カ", "x"]
    );
    assert!(
        tokens
            .iter()
            .all(|t| input.is_char_boundary(t.range.start) && input.is_char_boundary(t.range.end))
    );
}

#[test]
fn normalizes_and_retains_original_ranges() {
    let tokens = tokenize("ＡＢＣ e\u{301} 中文字 X");
    assert_eq!(
        tokens.iter().map(|t| t.text.as_str()).collect::<Vec<_>>(),
        ["abc", "é", "中文", "文字", "x"]
    );
    assert_eq!(
        tokens.iter().map(|t| t.range.clone()).collect::<Vec<_>>(),
        [0..9, 10..13, 14..20, 17..23, 24..25]
    );
}
