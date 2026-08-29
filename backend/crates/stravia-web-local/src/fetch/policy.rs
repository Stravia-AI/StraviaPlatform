use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, ToSocketAddrs};

use url::{Host, Url};

use super::FetchError;

pub(super) fn validate_url(value: &str) -> Result<Url, FetchError> {
    let url = Url::parse(value).map_err(|_| FetchError::invalid_url(value))?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(FetchError::invalid_url(value));
    }

    let hostname = url
        .host_str()
        .expect("host checked above")
        .trim_end_matches('.')
        .to_ascii_lowercase();
    if hostname.is_empty()
        || hostname == "localhost"
        || hostname.ends_with(".localhost")
        || hostname.ends_with(".local")
        || hostname == "home.arpa"
        || hostname.ends_with(".home.arpa")
    {
        return Err(FetchError::invalid_url(value));
    }
    if matches!(url.host(), Some(Host::Ipv4(address)) if !is_public_ipv4(address))
        || matches!(url.host(), Some(Host::Ipv6(address)) if !is_public_ipv6(address))
    {
        return Err(FetchError::invalid_url(value));
    }
    Ok(url)
}

pub(super) fn is_public_ip(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => is_public_ipv4(address),
        IpAddr::V6(address) => is_public_ipv6(address),
    }
}

pub(super) fn is_public_browser_request(value: &str) -> bool {
    let Ok(parsed) = Url::parse(value) else {
        return false;
    };
    if matches!(parsed.scheme(), "about" | "blob" | "data") {
        return true;
    }
    let Ok(url) = validate_url(value) else {
        return false;
    };
    match url.host() {
        Some(Host::Ipv4(address)) => is_public_ipv4(address),
        Some(Host::Ipv6(address)) => is_public_ipv6(address),
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

fn is_public_ipv4(address: Ipv4Addr) -> bool {
    let octets = address.octets();
    !address.is_private()
        && !address.is_loopback()
        && !address.is_link_local()
        && !address.is_unspecified()
        && !address.is_multicast()
        && !address.is_broadcast()
        && !address.is_documentation()
        && !(octets[0] == 100 && (64..=127).contains(&octets[1]))
        && !(octets[0] == 192 && octets[1] == 0 && octets[2] == 0 && !matches!(octets[3], 9 | 10))
        && !(octets[0] == 192 && octets[1] == 88 && octets[2] == 99)
        && !(octets[0] == 198 && matches!(octets[1], 18 | 19))
        && octets[0] < 240
        && octets[0] != 0
}

fn is_public_ipv6(address: Ipv6Addr) -> bool {
    if let Some(address) = address.to_ipv4() {
        return is_public_ipv4(address);
    }
    let segments = address.segments();
    (0x2000..=0x3fff).contains(&segments[0])
        && !address.is_loopback()
        && !address.is_unspecified()
        && !address.is_multicast()
        && !address.is_unique_local()
        && !address.is_unicast_link_local()
        && !(segments[0] == 0x2001 && segments[1] == 0x0db8)
        && !(segments[0] == 0x2001 && segments[1] == 0)
        && !(segments[0] == 0x2001 && (segments[1] & 0xfff0) == 0x0010)
        && is_global_ipv6_special(&segments)
}

fn is_global_ipv6_special(segments: &[u16; 8]) -> bool {
    // Keep this denylist synchronized with Web Access fetch validation.
    if (segments[0] == 0x0064
        && segments[1] == 0xff9b
        && segments[2] == 0
        && segments[3] == 0
        && segments[4] == 0
        && segments[5] == 0)
        || (segments[0] == 0x0064 && segments[1] == 0xff9b && segments[2] == 1)
        || segments[0] == 0x2002
        || (segments[0] == 0x0100
            && segments[1] == 0
            && segments[2] == 0
            && (segments[3] == 0 || segments[3] == 1))
        || (segments[0] == 0x3fff && (segments[1] & 0xf000) == 0)
        || segments[0] == 0x5f00
        || (segments[0] & 0xffc0) == 0xfec0
    {
        return false;
    }

    if segments[0] == 0x2001 && (segments[1] & 0xfe00) == 0 {
        return (segments[1] == 3)
            || (segments[1] == 4 && segments[2] == 0x0112)
            || (segments[1] == 1
                && segments[2..7].iter().all(|segment| *segment == 0)
                && matches!(segments[7], 1..=3))
            || (segments[1] & 0xfff0) == 0x0020
            || (segments[1] & 0xfff0) == 0x0030;
    }
    true
}
