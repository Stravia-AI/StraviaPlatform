use super::ssrf::is_public_ip;
use super::*;

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
    if request.allowed_domains.len() > 20 || request.blocked_domains.len() > 20 {
        return Err(WebAccessError::invalid(
            "domain filters cannot contain more than 20 entries",
        ));
    }

    request.allowed_domains = normalize_domains(request.allowed_domains)?;
    request.blocked_domains = normalize_domains(request.blocked_domains)?;
    let blocked: HashSet<&str> = request.blocked_domains.iter().map(String::as_str).collect();
    if let Some(conflict) = request
        .allowed_domains
        .iter()
        .find(|domain| blocked.contains(domain.as_str()))
    {
        return Err(WebAccessError::invalid(format!(
            "domain appears in allowed_domains and blocked_domains: {conflict}"
        )));
    }
    Ok(request)
}

pub(crate) fn normalize_domains(domains: Vec<String>) -> Result<Vec<String>, WebAccessError> {
    let mut seen = HashSet::new();
    let mut normalized = Vec::with_capacity(domains.len());
    for domain in domains {
        let candidate = domain.trim();
        if candidate.is_empty()
            || candidate.contains('/')
            || candidate.contains('?')
            || candidate.contains('#')
            || candidate.contains('@')
            || candidate.contains(':')
        {
            return Err(WebAccessError::invalid(format!(
                "invalid domain filter: {domain}"
            )));
        }
        let parsed = reqwest::Url::parse(&format!("https://{candidate}/"))
            .map_err(|_| WebAccessError::invalid(format!("invalid domain filter: {domain}")))?;
        let hostname = parsed
            .host_str()
            .ok_or_else(|| WebAccessError::invalid(format!("invalid domain filter: {domain}")))?
            .trim_end_matches('.')
            .to_ascii_lowercase();
        if hostname.is_empty() || !seen.insert(hostname.clone()) {
            continue;
        }
        normalized.push(hostname);
    }
    Ok(normalized)
}

pub(super) fn apply_domain_filters(request: &SearchRequest, response: &mut SearchResponse) {
    response.results.retain(|result| {
        url_matches_domain_filters(
            &result.url,
            &request.allowed_domains,
            &request.blocked_domains,
        )
    });
    if let Some(citations) = response.citations.as_mut() {
        citations.retain(|citation| {
            url_matches_domain_filters(
                &citation.url,
                &request.allowed_domains,
                &request.blocked_domains,
            )
        });
    }
}

fn url_matches_domain_filters(url: &str, allowed: &[String], blocked: &[String]) -> bool {
    let Ok(parsed) = reqwest::Url::parse(url) else {
        return false;
    };
    let Some(hostname) = parsed.host_str() else {
        return false;
    };
    let hostname = hostname.to_ascii_lowercase();
    if blocked
        .iter()
        .any(|domain| hostname == *domain || hostname.ends_with(&format!(".{domain}")))
    {
        return false;
    }
    allowed.is_empty()
        || allowed
            .iter()
            .any(|domain| hostname == *domain || hostname.ends_with(&format!(".{domain}")))
}

pub(super) async fn validate_fetch_request(
    mut request: FetchRequest,
) -> Result<FetchRequest, WebAccessError> {
    if !(1..=20).contains(&request.urls.len()) {
        return Err(WebAccessError::invalid(
            "urls must contain between 1 and 20 entries",
        ));
    }
    if !(1_000..=50_000).contains(&request.max_characters) {
        return Err(WebAccessError::invalid(
            "max_characters must be between 1,000 and 50,000",
        ));
    }
    for value in &mut request.urls {
        *value = value.trim().to_string();
        let parsed = reqwest::Url::parse(value)
            .map_err(|_| WebAccessError::invalid(format!("invalid URL: {value}")))?;
        if !matches!(parsed.scheme(), "http" | "https")
            || parsed.host_str().is_none()
            || !parsed.username().is_empty()
            || parsed.password().is_some()
        {
            return Err(WebAccessError::invalid(format!(
                "URL must be public HTTP(S): {value}"
            )));
        }
        let hostname = parsed
            .host_str()
            .expect("host checked above")
            .trim_end_matches('.')
            .to_ascii_lowercase();
        if hostname.is_empty() {
            return Err(WebAccessError::invalid(format!(
                "URL must be public HTTP(S): {value}"
            )));
        }
        if hostname == "localhost"
            || hostname.ends_with(".localhost")
            || hostname.ends_with(".local")
            || hostname == "home.arpa"
            || hostname.ends_with(".home.arpa")
        {
            return Err(WebAccessError::invalid(format!(
                "URL must be public HTTP(S): {value}"
            )));
        }

        let ip_literal = hostname
            .strip_prefix('[')
            .and_then(|hostname| hostname.strip_suffix(']'))
            .unwrap_or(&hostname);
        if let Ok(address) = ip_literal.parse::<std::net::IpAddr>() {
            if !is_public_ip(address) {
                return Err(WebAccessError::invalid(format!(
                    "URL must be public HTTP(S): {value}"
                )));
            }
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
