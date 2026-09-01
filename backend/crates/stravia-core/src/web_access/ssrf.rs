pub(crate) fn is_public_ip(address: std::net::IpAddr) -> bool {
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
