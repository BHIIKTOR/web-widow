//! SSRF gate: scheme check, our own DNS resolution, and classification of every
//! resolved address.
//!
//! The allowlist is config-file only. A match is logged at info with the rule
//! that matched, so an operator can audit why a private fetch happened.

use std::future::Future;
use std::net::IpAddr;
use std::pin::Pin;
use std::sync::Arc;

use ipnet::IpNet;
use tracing::info;
use url::Url;

use crate::core::error::{Result, WidowError};
use crate::core::ip::first_blocked;
use crate::core::url::host_literal;

#[derive(Debug, Clone)]
enum AllowRule {
 Host(String),
 Net(IpNet),
}

#[derive(Debug, Clone, Default)]
pub struct AllowList {
 rules: Vec<(AllowRule, String)>,
}

impl AllowList {
 /// Entries are exact hostnames or CIDRs; anything else is a startup error
 /// rather than a silently ignored line.
 pub fn parse(entries: &[String]) -> Result<Self> {
 let mut rules = Vec::new();
 for entry in entries {
 let text = entry.trim();
 if text.is_empty() {
 continue;
 }
 if let Ok(net) = text.parse::<IpNet>() {
 rules.push((AllowRule::Net(net), text.to_string()));
 } else if let Ok(ip) = text.parse::<IpAddr>() {
 let net = IpNet::from(ip);
 rules.push((AllowRule::Net(net), text.to_string()));
 } else if is_plausible_host(text) {
 rules.push((AllowRule::Host(text.to_ascii_lowercase()), text.to_string()));
 } else {
 return Err(WidowError::InvalidUrl(format!(
 "fetch.allow_hosts entry `{text}` is neither a hostname nor a CIDR"
)));
 }
 }
 Ok(Self { rules })
 }

 pub fn is_empty(&self) -> bool {
 self.rules.is_empty()
 }

 pub fn len(&self) -> usize {
 self.rules.len()
 }

 fn host_match(&self, host: &str) -> Option<&str> {
 let host = host.trim_end_matches('.').to_ascii_lowercase();
 self.rules.iter().find_map(|(rule, text)| match rule {
 AllowRule::Host(allowed) if *allowed == host => Some(text.as_str()),
 _ => None,
 })
 }

 fn address_match(&self, address: IpAddr) -> Option<&str> {
 self.rules.iter().find_map(|(rule, text)| match rule {
 AllowRule::Net(net) if net.contains(&address) => Some(text.as_str()),
 _ => None,
 })
 }
}

fn is_plausible_host(text: &str) -> bool {
 !text.is_empty()
 && text.len() <= 253
 && text
.bytes()
.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
}

/// The peer we are about to talk to, after the gate.
#[derive(Debug, Clone)]
pub struct VettedPeer {
 pub host: String,
 pub port: u16,
 /// Empty when the whole host was allowlisted by name (no lookup was made).
 pub addresses: Vec<IpAddr>,
 pub allowed_by: Option<String>,
}

/// The DNS seam at the vet boundary (). Production uses the ambient
/// resolution in [`resolve`]; tests inject a deterministic resolver.
pub type Resolver = Arc<
 dyn Fn(String, u16) -> Pin<Box<dyn Future<Output = Result<Vec<IpAddr>>> + Send>> + Send + Sync,
>;

fn default_resolver() -> Resolver {
 Arc::new(|host: String, port: u16| Box::pin(async move { resolve(&host, port).await }))
}

#[derive(Clone)]
pub struct SsrfGate {
 allow: AllowList,
 resolver: Resolver,
}

impl std::fmt::Debug for SsrfGate {
 fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
 formatter
.debug_struct("SsrfGate")
.field("allow", &self.allow)
.finish_non_exhaustive()
 }
}

impl SsrfGate {
 pub fn new(allow: AllowList) -> Self {
 Self {
 allow,
 resolver: default_resolver(),
 }
 }

 /// A gate with an injected resolver ( tests and the seam itself).
 pub fn with_resolver(allow: AllowList, resolver: Resolver) -> Self {
 Self { allow, resolver }
 }

 /// Full check: scheme, then resolve, then classify every address.
 pub async fn vet(&self, url: &Url) -> Result<VettedPeer> {
 crate::core::url::validate(url)?;
 let host = url.host_str().unwrap_or_default().to_string();
 let port = url.port_or_known_default().unwrap_or(443);

 //: an allowlisted *host* is still resolved, so the address the
 // gate vetted is the address the connection is pinned to.
 let host_rule = self.allow.host_match(&host).map(str::to_string);
 let addresses = (self.resolver)(host.clone(), port).await?;
 if addresses.is_empty() {
 return Err(WidowError::Dns {
 host,
 message: "no addresses returned".to_string(),
 });
 }

 let mut allowed_by = host_rule.clone();
 let mut blocked: Option<(IpAddr, &'static str)> = None;
 for address in &addresses {
 if let Some(rule) = self.allow.address_match(*address) {
 info!(host = %host, address = %address, rule = %rule, "fetch allowed by fetch.allow_hosts");
 allowed_by.get_or_insert_with(|| rule.to_string());
 continue;
 }
 if host_rule.is_some() {
 // The host itself is allowlisted: every address it resolves to is
 // permitted, and the whole set is pinned by the caller.
 continue;
 }
 if let Some((offender, range)) = first_blocked([*address]) {
 blocked = Some((offender, range));
 break;
 }
 }
 if let Some((address, range)) = blocked {
 return Err(WidowError::BlockedPrivateAddress {
 address: address.to_string(),
 range: range.to_string(),
 });
 }

 Ok(VettedPeer {
 host,
 port,
 addresses,
 allowed_by,
 })
 }
}

/// Resolve without relying on the HTTP client's resolver, so the check and the
/// connection at least share the same input. A literal address short-circuits.
pub async fn resolve(host: &str, port: u16) -> Result<Vec<IpAddr>> {
 if let Some(address) = host_literal(host) {
 return Ok(vec![address]);
 }
 let lookup = tokio::net::lookup_host((host, port))
.await
.map_err(|error| WidowError::Dns {
 host: host.to_string(),
 message: error.to_string(),
 })?;
 let mut addresses = Vec::new();
 for address in lookup {
 if !addresses.contains(&address.ip()) {
 addresses.push(address.ip());
 }
 }
 Ok(addresses)
}

#[cfg(test)]
mod tests {
 use super::*;

 fn url(text: &str) -> Url {
 Url::parse(text).unwrap()
 }

 #[tokio::test]
 async fn blocks_loopback_literal_and_private_literal() {
 let gate = SsrfGate::new(AllowList::default());
 for target in [
 "http://127.0.0.1/",
 "http://127.0.0.1:8080/x",
 "http://[::1]/",
 "http://10.1.2.3/",
 "http://169.254.169.254/latest/meta-data/",
 "http://192.168.1.1/",
 "http://100.64.0.1/",
 "http://[::ffff:127.0.0.1]/",
 ] {
 let error = gate.vet(&url(target)).await.unwrap_err();
 assert_eq!(
 error.code(),
 crate::core::error::ErrorCode::BlockedPrivateAddress,
 "{target}"
);
 }
 }

 #[tokio::test]
 async fn blocks_non_http_schemes_before_resolving() {
 let gate = SsrfGate::new(AllowList::default());
 for target in [
 "ftp://example.com/",
 "file:///etc/passwd",
 "gopher://example.com/",
 ] {
 let error = gate.vet(&url(target)).await.unwrap_err();
 assert_eq!(
 error.code(),
 crate::core::error::ErrorCode::BlockedScheme,
 "{target}"
);
 }
 }

 #[tokio::test]
 async fn allowlist_by_host_and_by_cidr() {
 let allow = AllowList::parse(&["localhost".to_string(), "10.0.0.0/8".to_string()]).unwrap();
 let gate = SsrfGate::new(allow);
 let peer = gate.vet(&url("http://localhost:9000/x")).await.unwrap();
 assert_eq!(peer.allowed_by.as_deref(), Some("localhost"));
 assert!(
 !peer.addresses.is_empty(),
 ": an allowlisted host is resolved and pinned too"
);

 let peer = gate.vet(&url("http://10.9.9.9/x")).await.unwrap();
 assert_eq!(peer.allowed_by.as_deref(), Some("10.0.0.0/8"));
 assert_eq!(peer.addresses, vec!["10.9.9.9".parse::<IpAddr>().unwrap()]);

 // A different private address is still blocked.
 let error = gate.vet(&url("http://192.168.0.1/x")).await.unwrap_err();
 assert_eq!(
 error.code(),
 crate::core::error::ErrorCode::BlockedPrivateAddress
);
 }

 #[test]
 fn malformed_allowlist_entries_are_rejected() {
 let error = AllowList::parse(&["http://example.com".to_string()]).unwrap_err();
 assert!(error.to_string().contains("neither a hostname nor a CIDR"));
 assert!(AllowList::parse(&["".to_string()]).unwrap().is_empty());
 assert_eq!(
 AllowList::parse(&["example.com".to_string()])
.unwrap()
.len(),
 1
);
 }

 #[tokio::test]
 async fn dns_names_resolving_to_a_public_address_are_not_blocked() {
 // Uses the machine resolver; a failure to resolve is an environmental
 // limitation of the test host, not a gate decision.
 let gate = SsrfGate::new(AllowList::default());
 match gate.vet(&url("https://example.com/")).await {
 Ok(peer) => assert!(!peer.addresses.is_empty()),
 Err(error) => {
 assert_eq!(error.code(), crate::core::error::ErrorCode::DnsError);
 }
 }
 }
}
