use std::net::ToSocketAddrs;

use url::{Host, Url};

use super::FetchError;
use crate::address_policy::{allows_url, is_public_ip};

pub(crate) fn validate_url(value: &str) -> Result<Url, FetchError> {
    let url = Url::parse(value).map_err(|_| FetchError::invalid_url(value))?;
    if !allows_url(&url) {
        return Err(FetchError::invalid_url(value));
    }
    Ok(url)
}

pub(super) fn validate_parsed_url(url: &Url) -> Result<(), FetchError> {
    if !allows_url(url) {
        return Err(FetchError::invalid_url(url.as_str()));
    }
    Ok(())
}

pub(super) fn is_public_browser_request(value: &str) -> bool {
    let Ok(url) = Url::parse(value) else {
        return false;
    };
    if matches!(url.scheme(), "about" | "blob" | "data") {
        return true;
    }
    if !allows_url(&url) {
        return false;
    }
    match url.host() {
        Some(Host::Ipv4(_) | Host::Ipv6(_)) => true,
        Some(Host::Domain(hostname)) => {
            let port = url.port_or_known_default().unwrap_or(0);
            let Ok(addresses) = (hostname, port).to_socket_addrs() else {
                return false;
            };
            let addresses = addresses.collect::<Vec<_>>();
            !addresses.is_empty() && addresses.iter().all(|address| is_public_ip(address.ip()))
        }
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::is_public_browser_request;

    #[test]
    fn browser_guard_keeps_internal_schemes_separate_from_http_policy() {
        for url in [
            "about:blank",
            "blob:https://example.com/id",
            "data:text/plain,fixture",
        ] {
            assert!(is_public_browser_request(url), "{url}");
        }
        for url in [
            "invalid",
            "file:///fixture",
            "ftp://example.com/",
            "http://127.0.0.1/",
            "http://127.0.0.1../",
        ] {
            assert!(!is_public_browser_request(url), "{url}");
        }
        assert!(is_public_browser_request("https://192.0.0.9/"));
    }
}
