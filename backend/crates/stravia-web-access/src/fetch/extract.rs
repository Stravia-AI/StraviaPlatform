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
    } else {
        ContentKind::Unsupported
    }
}

pub(super) fn decode(body: &[u8], content_type: &str) -> String {
    let charset = content_type
        .split(';')
        .skip(1)
        .find_map(|parameter| {
            let (name, value) = parameter.trim().split_once('=')?;
            name.eq_ignore_ascii_case("charset")
                .then(|| value.trim_matches(['\'', '"']).to_ascii_lowercase())
        })
        .or_else(|| sniff_html_charset(body));
    match charset.as_deref() {
        Some("iso-8859-1" | "latin1" | "latin-1") => decode_latin1(body),
        Some("windows-1252" | "cp1252") => decode_windows_1252(body),
        Some("utf-16le") => decode_utf16(body, u16::from_le_bytes),
        Some("utf-16be") => decode_utf16(body, u16::from_be_bytes),
        _ => String::from_utf8_lossy(body).into_owned(),
    }
}

pub(super) fn extract_html(html: &str, base_url: &Url) -> Result<HtmlExtract, FetchError> {
    let fallback_title = html_title(html);
    match Readability::new(html, Some(base_url.as_str()), None)
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

fn decode_utf16(body: &[u8], decode: fn([u8; 2]) -> u16) -> String {
    let units = body
        .chunks_exact(2)
        .map(|chunk| decode([chunk[0], chunk[1]]))
        .collect::<Vec<_>>();
    String::from_utf16_lossy(&units)
}
