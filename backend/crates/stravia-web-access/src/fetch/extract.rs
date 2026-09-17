use std::sync::LazyLock;

use dom_smoothie::Readability;
use htmd::HtmlToMarkdown;
use regex::Regex;
use scraper::{Html, Selector};
use url::Url;

use super::{FetchError, FetchErrorCode};

static MARKDOWN_CONVERTER: LazyLock<HtmlToMarkdown> = LazyLock::new(|| {
    HtmlToMarkdown::builder()
        .skip_tags(vec!["script", "style", "template", "svg"])
        .build()
});
static META_CHARSET: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)<meta[^>]+charset\s*=\s*['\"]?\s*([^\s'\"/>;]+)"#)
        .expect("meta charset regex is valid")
});

#[derive(Clone, Copy)]
pub(super) enum ContentKind {
    Html,
    Markdown,
    Plain,
    Json,
    Xml,
    Unsupported,
}

pub(super) struct HtmlExtract {
    pub title: Option<String>,
    pub markdown: String,
}

pub(super) fn classify(content_type: &str, decoded: &str) -> ContentKind {
    let mime = content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    let prefix = decoded.trim_start().to_ascii_lowercase();
    let looks_html = prefix.starts_with("<!doctype html")
        || prefix.starts_with("<html")
        || prefix.starts_with("<head")
        || prefix.starts_with("<body");
    if matches!(mime.as_str(), "text/html" | "application/xhtml+xml") || looks_html {
        ContentKind::Html
    } else if matches!(mime.as_str(), "text/markdown" | "text/x-markdown") {
        ContentKind::Markdown
    } else if mime == "text/plain" || mime.is_empty() {
        ContentKind::Plain
    } else if matches!(mime.as_str(), "application/json" | "text/json") || mime.ends_with("+json") {
        ContentKind::Json
    } else if mime.starts_with("image/") {
        ContentKind::Unsupported
    } else if matches!(mime.as_str(), "application/xml" | "text/xml") || mime.ends_with("+xml") {
        ContentKind::Xml
    } else if mime.starts_with("text/") {
        ContentKind::Plain
    } else {
        ContentKind::Unsupported
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum TextCharset {
    Utf8,
    Latin1,
    Windows1252,
    Utf16Le,
    Utf16Be,
}

pub(super) struct DecodedText {
    pub text: String,
    pub lossy: bool,
}

pub(super) fn decode_lossy(body: &[u8], content_type: &str) -> DecodedText {
    let resolved = resolve_charset(body, content_type);
    let unknown_charset = resolved.is_err();
    let (charset, bom) = resolved.unwrap_or((TextCharset::Utf8, 0));
    let payload = &body[bom..];
    let mut decoded = match charset {
        TextCharset::Utf8 => match std::str::from_utf8(payload) {
            Ok(text) => DecodedText {
                text: text.to_owned(),
                lossy: false,
            },
            Err(_) => DecodedText {
                text: String::from_utf8_lossy(payload).into_owned(),
                lossy: true,
            },
        },
        TextCharset::Latin1 => DecodedText {
            text: decode_latin1(payload),
            lossy: false,
        },
        TextCharset::Windows1252 => DecodedText {
            text: decode_windows_1252(payload),
            lossy: false,
        },
        TextCharset::Utf16Le => utf16_text(payload, u16::from_le_bytes),
        TextCharset::Utf16Be => utf16_text(payload, u16::from_be_bytes),
    };
    decoded.lossy |= unknown_charset;
    decoded
}

pub(super) fn decode_strict(body: &[u8], content_type: &str) -> Result<String, FetchError> {
    let (charset, bom) =
        resolve_charset(body, content_type).map_err(|charset| unsupported_charset(&charset))?;
    let payload = &body[bom..];
    match charset {
        TextCharset::Utf8 => std::str::from_utf8(payload)
            .map(|text| text.to_owned())
            .map_err(|_| invalid_charset_bytes("utf-8")),
        TextCharset::Latin1 => Ok(decode_latin1(payload)),
        TextCharset::Windows1252 => Ok(decode_windows_1252(payload)),
        TextCharset::Utf16Le => strict_utf16(payload, u16::from_le_bytes),
        TextCharset::Utf16Be => strict_utf16(payload, u16::from_be_bytes),
    }
}

fn resolve_charset(body: &[u8], content_type: &str) -> Result<(TextCharset, usize), String> {
    if body.starts_with(&[0xEF, 0xBB, 0xBF]) {
        return Ok((TextCharset::Utf8, 3));
    }
    if body.starts_with(&[0xFF, 0xFE]) {
        return Ok((TextCharset::Utf16Le, 2));
    }
    if body.starts_with(&[0xFE, 0xFF]) {
        return Ok((TextCharset::Utf16Be, 2));
    }
    let declared = content_type_charset(content_type).or_else(|| sniff_html_charset(body));
    match declared.as_deref() {
        None | Some("utf-8" | "utf8" | "us-ascii" | "ascii") => Ok((TextCharset::Utf8, 0)),
        Some("iso-8859-1" | "latin1" | "latin-1") => Ok((TextCharset::Latin1, 0)),
        Some("windows-1252" | "cp1252") => Ok((TextCharset::Windows1252, 0)),
        Some("utf-16le") => Ok((TextCharset::Utf16Le, 0)),
        Some("utf-16be") => Ok((TextCharset::Utf16Be, 0)),
        Some(unknown) => Err(unknown.to_string()),
    }
}

fn content_type_charset(content_type: &str) -> Option<String> {
    content_type.split(';').skip(1).find_map(|parameter| {
        let (name, value) = parameter.trim().split_once('=')?;
        name.eq_ignore_ascii_case("charset")
            .then(|| value.trim_matches(['\'', '"']).to_ascii_lowercase())
    })
}

fn utf16_text(payload: &[u8], decode: fn([u8; 2]) -> u16) -> DecodedText {
    let units = payload
        .as_chunks::<2>().0.iter()
        .map(|chunk| decode([chunk[0], chunk[1]]))
        .collect::<Vec<_>>();
    if payload.len().is_multiple_of(2) {
        if let Ok(text) = String::from_utf16(&units) {
            return DecodedText { text, lossy: false };
        }
    }
    DecodedText {
        text: String::from_utf16_lossy(&units),
        lossy: true,
    }
}

fn strict_utf16(payload: &[u8], decode: fn([u8; 2]) -> u16) -> Result<String, FetchError> {
    if !payload.len().is_multiple_of(2) {
        return Err(invalid_charset_bytes("utf-16"));
    }
    let units = payload
        .as_chunks::<2>().0.iter()
        .map(|chunk| decode([chunk[0], chunk[1]]))
        .collect::<Vec<_>>();
    String::from_utf16(&units).map_err(|_| invalid_charset_bytes("utf-16"))
}

fn unsupported_charset(charset: &str) -> FetchError {
    FetchError::new(
        FetchErrorCode::UnsupportedMediaType,
        format!("unsupported source character set: {charset}"),
    )
}

fn invalid_charset_bytes(charset: &str) -> FetchError {
    FetchError::new(
        FetchErrorCode::UnsupportedMediaType,
        format!("source bytes are not valid {charset}"),
    )
}

pub(super) fn extract_html(html: &str, base_url: Option<&Url>) -> Result<HtmlExtract, FetchError> {
    let fallback_title = html_title(html);
    match Readability::new(html, base_url.map(Url::as_str), None)
        .and_then(|mut reader| reader.parse())
    {
        Ok(article) => {
            let markdown = MARKDOWN_CONVERTER
                .convert(article.content.as_ref())
                .map_err(|error| {
                    FetchError::unavailable(format!("HTML to Markdown conversion failed: {error}"))
                })?;
            Ok(HtmlExtract {
                title: nonempty(article.title.as_ref()).or(fallback_title),
                markdown: markdown.trim().to_string(),
            })
        }
        Err(_) => {
            let markdown = MARKDOWN_CONVERTER.convert(html).map_err(|error| {
                FetchError::unavailable(format!("HTML to Markdown conversion failed: {error}"))
            })?;
            Ok(HtmlExtract {
                title: fallback_title,
                markdown: markdown.trim().to_string(),
            })
        }
    }
}

pub(super) fn json_markdown(decoded: &str) -> String {
    let pretty = serde_json::from_str::<serde_json::Value>(decoded)
        .and_then(|value| serde_json::to_string_pretty(&value))
        .unwrap_or_else(|_| decoded.trim().to_string());
    format!("```json\n{pretty}\n```")
}

pub(super) fn xml_markdown(decoded: &str) -> String {
    format!("```xml\n{}\n```", decoded.trim())
}

pub(super) fn unsupported(content_type: &str) -> FetchError {
    let mime = content_type
        .split(';')
        .next()
        .unwrap_or(content_type)
        .trim();
    FetchError::new(
        FetchErrorCode::UnsupportedMediaType,
        format!("unsupported response media type: {mime}"),
    )
}

pub(super) fn is_low_quality(markdown: &str) -> bool {
    let non_whitespace = markdown
        .chars()
        .filter(|character| !character.is_whitespace())
        .count();
    if non_whitespace <= 100 {
        return true;
    }
    let lowercase = markdown.to_ascii_lowercase();
    let javascript_gate = [
        "enable javascript",
        "javascript required",
        "turn on javascript",
        "please enable javascript",
        "browser not supported",
    ]
    .iter()
    .any(|phrase| lowercase.contains(phrase));
    if markdown.chars().count() < 1024 && javascript_gate {
        return true;
    }
    let lines = markdown
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    lines.len() > 10
        && lines
            .iter()
            .filter(|line| line.chars().count() < 40)
            .count()
            * 10
            > lines.len() * 7
}

pub(super) fn score(markdown: &str) -> usize {
    markdown
        .chars()
        .filter(|character| !character.is_whitespace())
        .count()
}

fn html_title(html: &str) -> Option<String> {
    let document = Html::parse_document(html);
    for selector in ["article h1", "main h1", "h1", "title"] {
        if let Some(element) = document
            .select(&Selector::parse(selector).expect("static selector is valid"))
            .next()
        {
            if let Some(title) = nonempty(&element.text().collect::<String>()) {
                return Some(title);
            }
        }
    }
    None
}

fn nonempty(value: &str) -> Option<String> {
    let value = value.split_whitespace().collect::<Vec<_>>().join(" ");
    (!value.is_empty()).then_some(value)
}

fn sniff_html_charset(body: &[u8]) -> Option<String> {
    let prefix = String::from_utf8_lossy(&body[..body.len().min(4096)]);
    META_CHARSET
        .captures(&prefix)
        .and_then(|captures| captures.get(1))
        .map(|value| value.as_str().to_ascii_lowercase())
}

fn decode_latin1(body: &[u8]) -> String {
    body.iter().map(|byte| char::from(*byte)).collect()
}

fn decode_windows_1252(body: &[u8]) -> String {
    const REPLACEMENTS: [char; 32] = [
        '€', '\u{0081}', '‚', 'ƒ', '„', '…', '†', '‡', 'ˆ', '‰', 'Š', '‹', 'Œ', '\u{008d}', 'Ž',
        '\u{008f}', '\u{0090}', '‘', '’', '“', '”', '•', '–', '—', '˜', '™', 'š', '›', 'œ',
        '\u{009d}', 'ž', 'Ÿ',
    ];
    body.iter()
        .map(|byte| match byte {
            0x80..=0x9f => REPLACEMENTS[(byte - 0x80) as usize],
            _ => char::from(*byte),
        })
        .collect()
}
