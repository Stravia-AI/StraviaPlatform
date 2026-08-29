use std::collections::{HashMap, HashSet};
use std::net::IpAddr;

use super::{
    SearchCompletion, SearchEvidenceSet, SearchPartialCause, SearchReport, SearchTurnId,
    WebSearchError,
};

const MAX_ANSWER_BYTES: usize = 64 * 1024;
const MAX_SOURCES: usize = 20;
const MAX_LIMITATIONS: usize = 20;
const MAX_LIMITATION_BYTES: usize = 2 * 1024;
const MAX_SOURCE_URL_BYTES: usize = 8 * 1024;
const MAX_SOURCE_TITLE_BYTES: usize = 2 * 1024;
const MAX_REPORT_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone, Default)]
pub struct SearchReportValidator;

impl SearchReportValidator {
    pub async fn validate(
        &self,
        turn_id: &SearchTurnId,
        completion: SearchCompletion,
        partial_cause: Option<SearchPartialCause>,
        mut report: SearchReport,
        evidence: &SearchEvidenceSet,
    ) -> Result<SearchReport, WebSearchError> {
        if (completion == SearchCompletion::Partial) != partial_cause.is_some() {
            return Err(WebSearchError::new(
                "invalid_partial",
                "Search partial cause does not match completion state",
            ));
        }
        let answer_bytes = report.answer.len();
        if !(1..=MAX_ANSWER_BYTES).contains(&answer_bytes) {
            return Err(WebSearchError::new(
                "invalid_report",
                "Search Report answer must contain between 1 byte and 64 KiB",
            ));
        }
        if !(1..=MAX_SOURCES).contains(&report.sources.len()) {
            return Err(WebSearchError::new(
                "invalid_report",
                "Search Report must contain between 1 and 20 sources",
            ));
        }
        if report.limitations.len() > MAX_LIMITATIONS
            || report
                .limitations
                .iter()
                .any(|item| item.len() > MAX_LIMITATION_BYTES)
        {
            return Err(WebSearchError::new(
                "invalid_report",
                "Search Report limitations exceed the configured bounds",
            ));
        }
        if completion == SearchCompletion::Partial && report.limitations.is_empty() {
            return Err(WebSearchError::new(
                "invalid_partial",
                "A partial Search Report must state its budget or timeout limitation",
            ));
        }
        if serde_json::to_vec(&report)
            .map_err(|_| WebSearchError::new("invalid_report", "Search Report is invalid"))?
            .len()
            > MAX_REPORT_BYTES
        {
            return Err(WebSearchError::new(
                "invalid_report",
                "Search Report exceeds the 256 KiB encoded size limit",
            ));
        }

        let expected_prefix = format!("source-{}-", turn_id.as_str());
        let mut sources = HashMap::with_capacity(report.sources.len());
        for source in &mut report.sources {
            if !source.id.starts_with(&expected_prefix)
                || source.id[expected_prefix.len()..].parse::<u32>().is_err()
            {
                return Err(WebSearchError::new(
                    "invalid_marker",
                    "Search Source marker is not scoped to the current Turn",
                ));
            }
            if source.url.len() > MAX_SOURCE_URL_BYTES
                || source
                    .title
                    .as_ref()
                    .is_some_and(|title| title.len() > MAX_SOURCE_TITLE_BYTES)
            {
                return Err(WebSearchError::new(
                    "invalid_report",
                    "Search Source URL or title exceeds the configured bounds",
                ));
            }
            let normalized = normalize_public_url(&source.url)?;
            validate_public_dns(&normalized).await?;
            let Some(verified_title) = evidence.by_url.get(&normalized) else {
                return Err(WebSearchError::new(
                    "unverified_source",
                    "Search Source URL is not present in verified evidence",
                ));
            };
            if let Some(title) = source.title.as_ref()
                && verified_title.as_deref() != Some(title.as_str())
            {
                return Err(WebSearchError::new(
                    "unverified_source",
                    "Search Source title is not present in verified evidence",
                ));
            }
            source.url = normalized;
            if sources
                .insert(source.id.clone(), source.url.clone())
                .is_some()
            {
                return Err(WebSearchError::new(
                    "invalid_marker",
                    "Search Source IDs must be unique",
                ));
            }
        }

        let markers = answer_markers(&report.answer)?;
        if markers.is_empty() {
            return Err(WebSearchError::new(
                "invalid_marker",
                "Search Report answer must cite at least one source",
            ));
        }
        let mut cited = HashSet::new();
        for marker in markers {
            if !marker.starts_with(&expected_prefix) || !sources.contains_key(marker) {
                return Err(WebSearchError::new(
                    "invalid_marker",
                    "Search Report contains a dangling or foreign source marker",
                ));
            }
            cited.insert(marker.to_owned());
        }
        if cited.len() != sources.len() {
            return Err(WebSearchError::new(
                "unused_source",
                "Every Search Source must be cited by the answer",
            ));
        }
        Ok(report)
    }
}

fn answer_markers(answer: &str) -> Result<Vec<&str>, WebSearchError> {
    let mut markers = Vec::new();
    let mut rest = answer;
    while let Some(start) = rest.find("[source-") {
        let marker_start = start + 1;
        let tail = &rest[marker_start..];
        let Some(end) = tail.find(']') else {
            return Err(WebSearchError::new(
                "invalid_marker",
                "Search Report contains an unterminated source marker",
            ));
        };
        let marker = &tail[..end];
        if marker.chars().any(char::is_whitespace) {
            return Err(WebSearchError::new(
                "invalid_marker",
                "Search Source markers cannot contain whitespace",
            ));
        }
        markers.push(marker);
        rest = &tail[end + 1..];
    }
    Ok(markers)
}

pub(crate) fn normalize_public_url(value: &str) -> Result<String, WebSearchError> {
    let mut url = reqwest::Url::parse(value.trim()).map_err(|_| {
        WebSearchError::new(
            "invalid_source_url",
            "Search Source URL must be a valid public HTTP(S) URL",
        )
    })?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(WebSearchError::new(
            "invalid_source_url",
            "Search Source URL must be a valid public HTTP(S) URL",
        ));
    }
    let host = url
        .host_str()
        .expect("host checked above")
        .trim_end_matches('.')
        .to_ascii_lowercase();
    if host.is_empty()
        || host == "localhost"
        || host.ends_with(".localhost")
        || host.ends_with(".local")
        || host == "home.arpa"
        || host.ends_with(".home.arpa")
        || (!host.contains('.') && host.parse::<IpAddr>().is_err())
        || host
            .parse::<IpAddr>()
            .is_ok_and(|address| !crate::web_access::is_public_ip(address))
    {
        return Err(WebSearchError::new(
            "invalid_source_url",
            "Search Source URL must be a valid public HTTP(S) URL",
        ));
    }
    url.set_fragment(None);
    Ok(url.to_string())
}

async fn validate_public_dns(value: &str) -> Result<(), WebSearchError> {
    let url = reqwest::Url::parse(value).map_err(|_| {
        WebSearchError::new(
            "invalid_source_url",
            "Search Source URL must be a valid public HTTP(S) URL",
        )
    })?;
    let host = url
        .host_str()
        .expect("normalized URL has a host")
        .trim_end_matches('.');
    if host.parse::<IpAddr>().is_ok() {
        return Ok(());
    }
    let addresses = tokio::net::lookup_host((host, 0)).await.map_err(|_| {
        WebSearchError::new(
            "invalid_source_url",
            "Search Source hostname could not be resolved",
        )
    })?;
    let mut resolved_any = false;
    for address in addresses {
        resolved_any = true;
        if !crate::web_access::is_public_ip(address.ip()) {
            return Err(WebSearchError::new(
                "invalid_source_url",
                "Search Source hostname resolves to a non-public address",
            ));
        }
    }
    if !resolved_any {
        return Err(WebSearchError::new(
            "invalid_source_url",
            "Search Source hostname could not be resolved",
        ));
    }
    Ok(())
}
