use super::*;
use stravia_web_access::address_policy::{allows_url, is_public_ip};

pub(super) fn validate_search_request(
    mut request: SearchRequest,
) -> Result<SearchRequest, WebAccessError> {
    request.query = request.query.trim().to_string();
    if request.query.is_empty() {
        return Err(WebAccessError::invalid("query cannot be empty"));
    }
    if request.query.chars().count() > 2_000 {
        return Err(WebAccessError::invalid(
            "query cannot exceed 2,000 characters",
        ));
    }
    if !(1..=20).contains(&request.max_results) {
        return Err(WebAccessError::invalid(
            "max_results must be between 1 and 20",
        ));
    }
    if request.allowed_domains.len() > 20 {
        return Err(WebAccessError::invalid(
            "domain filters cannot contain more than 20 entries",
        ));
    }

    request.allowed_domains = normalize_domains(request.allowed_domains)?;
    Ok(request)
}

use stravia_web_access_contract::{normalize_domains, url_matches_allowed_domains};

pub(super) fn apply_domain_filters(request: &SearchRequest, response: &mut SearchResponse) {
    response
        .results
        .retain(|result| url_matches_allowed_domains(&result.url, &request.allowed_domains));
    if let Some(citations) = response.citations.as_mut() {
        citations.retain(|citation| {
            url_matches_allowed_domains(&citation.url, &request.allowed_domains)
        });
    }
}

pub(super) async fn validate_fetch_request(
    mut request: FetchRequest,
) -> Result<FetchRequest, WebAccessError> {
    if !(1..=20).contains(&request.urls.len()) {
        return Err(WebAccessError::invalid(
            "urls must contain between 1 and 20 entries",
        ));
    }
    if !(1_000..=500_000).contains(&request.max_characters) {
        return Err(WebAccessError::invalid(
            "max_characters must be between 1,000 and 500,000",
        ));
    }
    for value in &mut request.urls {
        *value = value.trim().to_string();
        let parsed = reqwest::Url::parse(value)
            .map_err(|_| WebAccessError::invalid(format!("invalid URL: {value}")))?;
        if !allows_url(&parsed) {
            return Err(WebAccessError::invalid(format!(
                "URL must be public HTTP(S): {value}"
            )));
        }
        let hostname = parsed
            .host_str()
            .expect("host checked above")
            .trim_end_matches('.')
            .to_ascii_lowercase();
        let ip_literal = hostname
            .strip_prefix('[')
            .and_then(|hostname| hostname.strip_suffix(']'))
            .unwrap_or(&hostname);
        if ip_literal.parse::<std::net::IpAddr>().is_ok() {
            continue;
        }

        // Tokio's resolver runs through its async runtime rather than blocking
        // the request task. Every A/AAAA answer must be public; accepting any
        // private answer would let a DNS alias reach an internal service.
        let addresses = tokio::net::lookup_host((hostname.as_str(), 0))
            .await
            .map_err(|_| {
                WebAccessError::from_code(
                    WebAccessErrorCode::Unavailable,
                    format!("URL hostname could not be resolved: {hostname}"),
                )
            })?;
        let mut resolved_any = false;
        for address in addresses {
            resolved_any = true;
            if !is_public_ip(address.ip()) {
                return Err(WebAccessError::invalid(format!(
                    "URL must be public HTTP(S): {value}"
                )));
            }
        }
        if !resolved_any {
            return Err(WebAccessError::from_code(
                WebAccessErrorCode::Unavailable,
                format!("URL hostname could not be resolved: {hostname}"),
            ));
        }
    }
    Ok(request)
}
