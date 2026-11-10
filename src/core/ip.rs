//! SSRF address classification ().
//!
//! Pure and table-driven: one list feeds both the gate and the test that
//! asserts every listed range is blocked, so a new range cannot be added to the
//! prose without a failing test.

use std::net::IpAddr;
use std::sync::LazyLock;

use ipnet::IpNet;

/// Ranges that a fetch may never reach (), plus IPv4-mapped IPv6
/// which is unmapped before matching.
pub const BLOCKED_CIDRS: &[&str] = &[
 // IPv4 special purpose / private / metadata
 "0.0.0.0/8", // "this network" / unspecified
 "10.0.0.0/8", // private
 "100.64.0.0/10", // CGNAT (incl. 100.100.100.200 metadata)
 "127.0.0.0/8", // loopback
 "169.254.0.0/16", // link-local (incl. 169.254.169.254 metadata)
 "172.16.0.0/12", // private
 "192.168.0.0/16", // private
 "192.0.0.0/24", // IETF protocol assignments (incl. 192.0.0.192 metadata)
 "192.0.2.0/24", // TEST-NET-1
 "198.18.0.0/15", // benchmarking
 "198.51.100.0/24", // TEST-NET-2
 "203.0.113.0/24", // TEST-NET-3
 "224.0.0.0/4", // multicast
 "240.0.0.0/4", // reserved + broadcast
 // IPv6
 "::/128", // unspecified
 "::1/128", // loopback
 "fc00::/7", // unique local
 "fe80::/10", // link-local
 "ff00::/8", // multicast
 "64:ff9b::/96", // NAT64
 "::ffff:0:0/96", // IPv4-mapped (belt and braces; normally unmapped first)
];

static PARSED: LazyLock<Vec<(IpNet, &'static str)>> = LazyLock::new(|| {
 BLOCKED_CIDRS
.iter()
.map(|text| {
 let net: IpNet = text.parse().expect("blocked CIDR table must parse");
 (net, *text)
 })
.collect()
});

/// If `ip` falls in a blocked range, return the matching CIDR as written in
///. IPv4-mapped IPv6 addresses are unmapped first, so
/// `::ffff:127.0.0.1` is reported as `127.0.0.0/8`.
pub fn blocked_range(ip: IpAddr) -> Option<&'static str> {
 let canonical = ip.to_canonical();
 PARSED
.iter()
.find(|(net, _)| net.contains(&canonical))
.map(|(_, text)| *text)
}

pub fn is_public(ip: IpAddr) -> bool {
 blocked_range(ip).is_none()
}

/// Classify every resolved address; the first blocked one is fatal.
pub fn first_blocked<I: IntoIterator<Item = IpAddr>>(
 addresses: I) -> Option<(IpAddr, &'static str)> {
 addresses
.into_iter()
.find_map(|ip| blocked_range(ip).map(|range| (ip, range)))
}

#[cfg(test)]
mod tests {
 use super::*;

 fn ip(text: &str) -> IpAddr {
 text.parse().expect("test address")
 }

 #[test]
 fn every_listed_cidr_blocks_a_member_address() {
 for text in BLOCKED_CIDRS {
 let net: IpNet = text.parse().unwrap();
 // `IpNet::hosts()` skips the network address, exercising a real host.
 let member = net
.hosts()
.next()
.or_else(|| Some(net.network()))
.expect("cidr has a member");
 assert!(
 blocked_range(member).is_some(),
 "{member} should be blocked by {text}"
);
 }
 }

 #[test]
 fn public_addresses_pass() {
 for text in [
 "1.1.1.1",
 "8.8.8.8",
 "93.184.216.34",
 "2606:2800:220:1:248:1893:25c8:1946",
 "172.32.0.1",
 "192.169.0.1",
 "100.128.0.1",
 "198.20.0.1",
 "223.255.255.255",
 ] {
 assert!(blocked_range(ip(text)).is_none(), "{text} should be public");
 }
 }

 #[test]
 fn boundary_addresses_are_classified_strictly() {
 // Just inside and just outside each interesting edge.
 let cases = [
 ("9.255.255.255", true),
 ("10.0.0.0", false),
 ("10.255.255.255", false),
 ("11.0.0.0", true),
 ("126.255.255.255", true),
 ("127.0.0.1", false),
 ("128.0.0.0", true),
 ("100.63.255.255", true),
 ("100.64.0.0", false),
 ("100.127.255.255", false),
 ("100.128.0.0", true),
 ("169.253.255.255", true),
 ("169.254.169.254", false),
 ("172.15.255.255", true),
 ("172.16.0.0", false),
 ("172.31.255.255", false),
 ("172.32.0.0", true),
 ("192.0.0.192", false),
 ("192.0.1.1", true),
 ("192.0.2.1", false),
 ("198.17.255.255", true),
 ("198.18.0.0", false),
 ("198.19.255.255", false),
 ("198.20.0.0", true),
 ("198.51.100.7", false),
 ("203.0.113.9", false),
 ("223.255.255.255", true),
 ("224.0.0.1", false),
 ("239.255.255.255", false),
 ("240.0.0.1", false),
 ("255.255.255.255", false),
 ];
 for (text, public) in cases {
 assert_eq!(is_public(ip(text)), public, "classification of {text}");
 }
 }

 #[test]
 fn ipv6_special_ranges_are_blocked() {
 for text in [
 "::",
 "::1",
 "fc00::1",
 "fdff:ffff::1",
 "fe80::1",
 "ff02::1",
 "64:ff9b::7f00:1",
 ] {
 assert!(
 blocked_range(ip(text)).is_some(),
 "{text} should be blocked"
);
 }
 for text in ["2001:4860:4860::8888", "2a00:1450:4001:82f::200e"] {
 assert!(blocked_range(ip(text)).is_none(), "{text} should be public");
 }
 }

 #[test]
 fn ipv4_mapped_ipv6_reports_the_ipv4_range() {
 assert_eq!(blocked_range(ip("::ffff:127.0.0.1")), Some("127.0.0.0/8"));
 assert_eq!(
 blocked_range(ip("::ffff:169.254.169.254")),
 Some("169.254.0.0/16")
);
 assert!(blocked_range(ip("::ffff:8.8.8.8")).is_none());
 }

 #[test]
 fn first_blocked_reports_the_first_offending_address() {
 let addresses = vec![ip("8.8.8.8"), ip("10.1.2.3"), ip("127.0.0.1")];
 let (address, range) = first_blocked(addresses).unwrap();
 assert_eq!(address, ip("10.1.2.3"));
 assert_eq!(range, "10.0.0.0/8");
 assert!(first_blocked(vec![ip("8.8.8.8")]).is_none());
 }
}
