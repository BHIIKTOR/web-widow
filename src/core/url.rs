//! URL validation and canonicalization.
//!
//! Canonicalization is identity-only: the request still uses the URL exactly as
//! the caller wrote it. Two spellings that a server treats identically share one
//! `pages` row and one blob.

use std::net::IpAddr;

use url::Url;

use crate::core::error::{Result, WidowError};

/// Query parameters that never change server-side content and are dropped from
/// the canonical form. Deliberately conservative: a parameter that can be
/// functional (`ref`, `page`, `id`) is kept and sorted, never removed.
pub const TRACKING_PARAMS: &[&str] = &[
    "utm_source",
    "utm_medium",
    "utm_campaign",
    "utm_term",
    "utm_content",
    "utm_id",
    "utm_name",
    "utm_reader",
    "utm_referrer",
    "fbclid",
    "gclid",
    "dclid",
    "gbraid",
    "wbraid",
    "msclkid",
    "yclid",
    "igshid",
    "mc_cid",
    "mc_eid",
    "_ga",
    "_gl",
];

pub fn is_tracking_param(name: &str) -> bool {
    TRACKING_PARAMS
        .iter()
        .any(|known| known.eq_ignore_ascii_case(name))
}

/// A validated fetch target with its canonical identity.
#[derive(Debug, Clone)]
pub struct PageUrl {
    raw: String,
    url: Url,
    canonical: String,
    hash: String,
}

impl PageUrl {
    pub fn parse(input: &str) -> Result<Self> {
        let raw = input.trim();
        if raw.is_empty() {
            return Err(WidowError::InvalidUrl("empty url".to_string()));
        }
        let url = Url::parse(raw).map_err(|error| WidowError::InvalidUrl(error.to_string()))?;
        validate(&url)?;
        let canonical = canonicalize(&url);
        let hash = content_hash(canonical.as_bytes());
        Ok(Self {
            raw: raw.to_string(),
            url,
            canonical,
            hash,
        })
    }

    /// The URL exactly as supplied (trimmed), used for the outgoing request.
    pub fn raw(&self) -> &str {
        &self.raw
    }

    pub fn url(&self) -> &Url {
        &self.url
    }

    pub fn canonical(&self) -> &str {
        &self.canonical
    }

    /// BLAKE3-256 hex of the canonical URL; the `pages.url_hash` identity.
    pub fn hash(&self) -> &str {
        &self.hash
    }

    pub fn host(&self) -> &str {
        // A parsed absolute http(s) URL always has a host.
        self.url.host_str().unwrap_or_default()
    }

    /// Registrable site for grouping (`psl`), the host itself for IP literals.
    pub fn site(&self) -> String {
        let host = self.host();
        if host.parse::<IpAddr>().is_ok() {
            return host.to_string();
        }
        psl::domain_str(host).unwrap_or(host).to_string()
    }
}

pub fn validate(url: &Url) -> Result<()> {
    match url.scheme() {
        "http" | "https" => {}
        other => {
            return Err(WidowError::BlockedScheme {
                scheme: other.to_string(),
            });
        }
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(WidowError::InvalidUrl(
            "url userinfo is not supported: M01 does not perform authenticated fetching"
                .to_string(),
        ));
    }
    if url.host_str().is_none() {
        return Err(WidowError::InvalidUrl("url has no host".to_string()));
    }
    Ok(())
}

/// Canonical identity of `url`: lowercase scheme/host, no default port, no
/// fragment, empty path becomes `/`, tracking parameters dropped, remaining
/// query parameters sorted. Idempotent (see the property test).
pub fn canonicalize(url: &Url) -> String {
    let scheme = url.scheme();
    let mut out = String::with_capacity(url.as_str().len());
    out.push_str(scheme);
    out.push_str("://");

    match url.host() {
        Some(url::Host::Ipv6(address)) => {
            out.push('[');
            out.push_str(&address.to_string());
            out.push(']');
        }
        Some(url::Host::Ipv4(address)) => out.push_str(&address.to_string()),
        Some(url::Host::Domain(domain)) => out.push_str(domain.trim_end_matches('.')),
        None => {}
    }

    if let Some(port) = url.port()
        && !is_default_port(scheme, port)
    {
        out.push(':');
        out.push_str(&port.to_string());
    }

    let path = url.path();
    if path.is_empty() {
        out.push('/');
    } else {
        out.push_str(path);
    }

    let mut pairs: Vec<(String, String)> = url
        .query_pairs()
        .filter(|(name, _)| !is_tracking_param(name))
        .map(|(name, value)| (name.into_owned(), value.into_owned()))
        .collect();
    if !pairs.is_empty() {
        pairs.sort();
        let mut serializer = url::form_urlencoded::Serializer::new(String::new());
        for (name, value) in &pairs {
            serializer.append_pair(name, value);
        }
        out.push('?');
        out.push_str(&serializer.finish());
    }

    out
}

pub fn is_default_port(scheme: &str, port: u16) -> bool {
    matches!((scheme, port), ("http", 80) | ("https", 443))
}

/// BLAKE3-256 of arbitrary bytes as lowercase hex.
pub fn content_hash(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

/// Host literal without IPv6 brackets, for address parsing.
pub fn host_literal(host: &str) -> Option<IpAddr> {
    host.trim_start_matches('[')
        .trim_end_matches(']')
        .parse::<IpAddr>()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn canon(input: &str) -> String {
        let url = Url::parse(input).expect("test url");
        canonicalize(&url)
    }

    #[test]
    fn known_variants_share_one_identity() {
        let expected = "https://example.com/a/b?q=1&z=2";
        for variant in [
            "https://example.com/a/b?q=1&z=2",
            "https://EXAMPLE.com/a/b?z=2&q=1",
            "https://example.com:443/a/b?q=1&z=2#section",
            "https://example.com/a/b?q=1&z=2&utm_source=news",
            "https://example.com/a/b?utm_campaign=x&q=1&z=2#top",
            "https://example.com/./a/b?q=1&z=2",
            "https://example.com/a/b?q=1&z=2&fbclid=abc",
        ] {
            assert_eq!(canon(variant), expected, "variant {variant}");
        }
    }

    #[test]
    fn distinct_resources_keep_distinct_identities() {
        // Different resources must not collapse: a trailing slash, a functional
        // query parameter, and the scheme are all meaningful.
        let a = canon("https://example.com/a");
        let b = canon("https://example.com/a/");
        let c = canon("https://example.com/a?page=2");
        let d = canon("http://example.com/a");
        let mut seen = std::collections::HashSet::new();
        for value in [a, b, c, d] {
            assert!(seen.insert(value.clone()), "{value} collided");
        }
    }

    #[test]
    fn default_ports_are_elided_and_others_kept() {
        assert_eq!(canon("http://example.com:80/x"), "http://example.com/x");
        assert_eq!(canon("https://example.com:443/x"), "https://example.com/x");
        assert_eq!(
            canon("https://example.com:8443/x"),
            "https://example.com:8443/x"
        );
        assert_eq!(
            canon("http://example.com:443/x"),
            "http://example.com:443/x"
        );
    }

    #[test]
    fn empty_path_and_trailing_dot_host() {
        assert_eq!(canon("https://example.com"), "https://example.com/");
        assert_eq!(canon("https://example.com."), "https://example.com/");
    }

    #[test]
    fn ipv6_hosts_are_bracketed_and_lowercased() {
        assert_eq!(
            canon("http://[2001:DB8::1]:8080/x"),
            "http://[2001:db8::1]:8080/x"
        );
    }

    #[test]
    fn tracking_parameters_are_removed_case_insensitively() {
        let value = canon("https://example.com/?UTM_Source=a&keep=1");
        assert_eq!(value, "https://example.com/?keep=1");
        // The parameter name is preserved when it is not tracking.
        assert_eq!(
            canon("https://example.com/?ref=sidebar"),
            "https://example.com/?ref=sidebar"
        );
    }

    #[test]
    fn page_url_rejects_non_http_schemes_and_userinfo() {
        let error = PageUrl::parse("ftp://example.com/x").unwrap_err();
        assert_eq!(error.code(), crate::core::error::ErrorCode::BlockedScheme);
        let error = PageUrl::parse("file:///etc/passwd").unwrap_err();
        assert_eq!(error.code(), crate::core::error::ErrorCode::BlockedScheme);
        let error = PageUrl::parse("https://user:pass@example.com/").unwrap_err();
        assert_eq!(error.code(), crate::core::error::ErrorCode::InvalidUrl);
        let error = PageUrl::parse("not a url").unwrap_err();
        assert_eq!(error.code(), crate::core::error::ErrorCode::InvalidUrl);
        let error = PageUrl::parse("   ").unwrap_err();
        assert_eq!(error.code(), crate::core::error::ErrorCode::InvalidUrl);
    }

    #[test]
    fn site_groups_subdomains_by_registrable_domain() {
        let page = PageUrl::parse("https://docs.example.co.uk/a").unwrap();
        assert_eq!(page.site(), "example.co.uk");
        let page = PageUrl::parse("http://127.0.0.1:8080/a").unwrap();
        assert_eq!(page.site(), "127.0.0.1");
    }

    #[test]
    fn hash_is_stable_for_equivalent_input() {
        let a = PageUrl::parse("https://example.com/x?utm_source=a").unwrap();
        let b = PageUrl::parse("HTTPS://Example.com:443/x#frag").unwrap();
        assert_eq!(a.hash(), b.hash());
        assert_ne!(
            a.hash(),
            PageUrl::parse("https://example.com/y").unwrap().hash()
        );
    }
}
