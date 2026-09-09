//! Web Access 的静态地址规则；不执行 DNS 或网络访问。
//!
//! 域名通过静态检查不代表解析地址安全。调用方仍须在各自的准入、
//! 重定向和浏览器出站时机检查已取得的全部地址，并保留连接地址固定与代理分工。

use std::net::IpAddr;

use url::{Host, Url};

/// 判断已解析 URL 是否符合静态 HTTP(S) 地址规则。
///
/// 不修整或重新解析原始输入，不查询 DNS，不验证域名实际目的地址。
/// 解析失败和策略拒绝的错误类型、文案由调用方映射。
#[must_use]
pub fn allows_url(url: &Url) -> bool {
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return false;
    }
    match url.host() {
        Some(Host::Ipv4(address)) => is_public_ip(IpAddr::V4(address)),
        Some(Host::Ipv6(address)) => is_public_ip(IpAddr::V6(address)),
        Some(Host::Domain(hostname)) => {
            // HTTP(S) host 已由 URL parser 规范化大小写；沿用 core 的尾随点语义。
            let hostname = hostname.trim_end_matches('.');
            !hostname.is_empty()
                && hostname != "localhost"
                && !hostname.ends_with(".localhost")
                && !hostname.ends_with(".local")
                && hostname != "home.arpa"
                && !hostname.ends_with(".home.arpa")
                && hostname.parse::<IpAddr>().map_or(true, is_public_ip)
        }
        None => false,
    }
}

/// 按 Web Access 既有范围与例外分类一个 IP 地址。
///
/// 仅判定此地址，不解析域名，也不保证实际连接使用此地址。
#[must_use]
pub fn is_public_ip(address: std::net::IpAddr) -> bool {
    match address {
        std::net::IpAddr::V4(address) => is_public_ipv4(address),
        std::net::IpAddr::V6(address) => is_public_ipv6(address),
    }
}

fn is_public_ipv4(address: std::net::Ipv4Addr) -> bool {
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
        && !(octets[0] == 198 && (octets[1] == 18 || octets[1] == 19))
        && octets[0] < 240
        && octets[0] != 0
}
fn is_public_ipv6(address: std::net::Ipv6Addr) -> bool {
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
    // IANA special-purpose ranges that are not globally reachable.
    // Well-known NAT64 64:ff9b::/96 and local-use 64:ff9b:1::/48.
    if (segments[0] == 0x0064
        && segments[1] == 0xff9b
        && segments[2] == 0
        && segments[3] == 0
        && segments[4] == 0
        && segments[5] == 0)
        || (segments[0] == 0x0064 && segments[1] == 0xff9b && segments[2] == 1)
        // 6to4 transition addresses can embed private IPv4 destinations.
        || segments[0] == 0x2002
        // Discard-only 100::/64 and dummy 100:0:0:1::/64.
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

    // 2001::/23 is reserved for IETF assignments. Permit only the
    // specifically allocated globally reachable entries in that block.
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

#[cfg(test)]
mod tests {
    use super::{allows_url, is_public_ip};
    use url::Url;

    #[test]
    fn ipv4_special_ranges_keep_their_boundaries_and_exceptions() {
        for (address, allowed) in [
            ("0.255.255.255", false),
            ("1.0.0.0", true),
            ("10.0.0.0", false),
            ("100.63.255.255", true),
            ("100.64.0.0", false),
            ("100.127.255.255", false),
            ("100.128.0.0", true),
            ("127.0.0.1", false),
            ("169.254.0.1", false),
            ("172.16.0.0", false),
            ("192.0.0.8", false),
            ("192.0.0.9", true),
            ("192.0.0.10", true),
            ("192.0.0.11", false),
            ("192.0.2.1", false),
            ("192.88.98.255", true),
            ("192.88.99.0", false),
            ("192.88.99.255", false),
            ("192.88.100.0", true),
            ("192.168.1.1", false),
            ("198.17.255.255", true),
            ("198.18.0.0", false),
            ("198.19.255.255", false),
            ("198.20.0.0", true),
            ("198.51.100.1", false),
            ("203.0.113.1", false),
            ("223.255.255.255", true),
            ("224.0.0.0", false),
            ("239.255.255.255", false),
            ("240.0.0.0", false),
            ("255.255.255.255", false),
        ] {
            assert_eq!(is_public_ip(address.parse().unwrap()), allowed, "{address}");
        }
    }

    #[test]
    fn ipv6_special_ranges_keep_their_boundaries_and_exceptions() {
        for (address, allowed) in [
            ("::", false),
            ("::1", false),
            ("64:ff9b::808:808", false),
            ("64:ff9b:1::1", false),
            ("100::1", false),
            ("100:0:0:1::1", false),
            ("1fff:ffff::1", false),
            ("2000::1", true),
            ("2001::1", false),
            ("2001:1::", false),
            ("2001:1::1", true),
            ("2001:1::3", true),
            ("2001:1::4", false),
            ("2001:2:ffff::1", false),
            ("2001:3::1", true),
            ("2001:4:111:ffff::1", false),
            ("2001:4:112::1", true),
            ("2001:4:112:ffff::1", true),
            ("2001:4:113::1", false),
            ("2001:1f:ffff::1", false),
            ("2001:20::1", true),
            ("2001:2f:ffff::1", true),
            ("2001:30::1", true),
            ("2001:3f:ffff::1", true),
            ("2001:40::1", false),
            ("2001:1ff:ffff::1", false),
            ("2001:200::1", true),
            ("2001:db8::1", false),
            ("2001:ffff::1", true),
            ("2002::1", false),
            ("2002:ffff::1", false),
            ("2003::1", true),
            ("3ffe:ffff::1", true),
            ("3fff::1", false),
            ("3fff:fff:ffff::1", false),
            ("3fff:1000::1", true),
            ("3fff:ffff::1", true),
            ("4000::1", false),
            ("5f00::1", false),
            ("fc00::1", false),
            ("fe80::1", false),
            ("fec0::1", false),
            ("ff00::1", false),
        ] {
            assert_eq!(is_public_ip(address.parse().unwrap()), allowed, "{address}");
        }
    }

    #[test]
    fn ipv4_embedded_forms_retain_the_ipv4_decision() {
        for (address, allowed) in [
            ("::ffff:127.0.0.1", false),
            ("::127.0.0.1", false),
            ("::ffff:192.168.1.1", false),
            ("::192.168.1.1", false),
            ("::ffff:192.0.0.9", true),
            ("::192.0.0.9", true),
            ("::ffff:8.8.8.8", true),
            ("::8.8.8.8", true),
        ] {
            assert_eq!(is_public_ip(address.parse().unwrap()), allowed, "{address}");
        }
    }

    #[test]
    fn url_static_acceptance_does_not_require_dns() {
        for (value, allowed) in [
            ("file:///fixture", false),
            ("ftp://example.com/", false),
            ("http://user:fake@example.com/", false),
            ("http://:fake@example.com/", false),
            ("http://LOCALHOST./", false),
            ("http://sub.localhost../", false),
            ("http://sub.local./", false),
            ("http://HOME.ARPA../", false),
            ("http://sub.home.arpa./", false),
            ("http://./", false),
            ("http://127.1/", false),
            ("http://0x7f000001/", false),
            ("http://127.0.0.1../", false),
            ("http://192.168.1.1../", false),
            ("http://192.0.0.9../", true),
            ("http://[::ffff:127.0.0.1]/", false),
            ("http://[::ffff:8.8.8.8]/", true),
            ("HTTPS://EXAMPLE.COM./", true),
            ("https://does-not-exist.invalid/", true),
        ] {
            assert_eq!(allows_url(&Url::parse(value).unwrap()), allowed, "{value}");
        }
    }
}
