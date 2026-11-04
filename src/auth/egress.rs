//! Auth-browser egress policy (v0.9).
//!
//! Two predicates, deliberately separate:
//!
//! - [`AuthEgressPolicy::check`] — **reachable**: may this authenticated browser
//!   open a connection to this URL at all? Exact binding origin (host + port),
//!   optional subdomain suffix, exact asset hosts; https only except a literal
//!   loopback host when the profile opted in; default deny.
//! - [`AuthEgressPolicy::may_bear_credential`] — **credential**: may the sealed
//!   session cookie be attached? Exact binding origin only; a subdomain or an
//!   asset is reachable but never credential-bearing.
//!
//! Every denial carries a typed [`EgressDenial`] whose strings are pinned for
//! the audit row and the harness attribution (`(class, url, reason)`), and whose
//! audit outcome is `egress_blocked`. The predicate is pure so the request
//! loop, the filtering proxy and the harness share one rule.

use std::collections::BTreeSet;
use std::net::IpAddr;

use url::Url;

use crate::core::error::{Result, WidowError};

/// The pinned audit outcome for every policy denial.
pub const AUDIT_EGRESS_BLOCKED: &str = "egress_blocked";

/// Why a URL is not allowed. The strings are stable evidence vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EgressDenial {
    /// Not `http(s)`, or plain `http` to a non-loopback/opted-out host.
    Scheme,
    /// The binding/subdomain host on a port other than the bound port.
    Port,
    /// The URL passed `check` but may not carry the session credential.
    Credential,
    /// A host outside the binding/subdomain/assets set.
    Host,
}

impl EgressDenial {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Scheme => "scheme",
            Self::Port => "port",
            Self::Credential => "credential",
            Self::Host => "host",
        }
    }

    /// The audit outcome for any denial; the reason is a separate, value-free
    /// detail.
    pub fn audit_outcome(self) -> &'static str {
        AUDIT_EGRESS_BLOCKED
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EgressDecision {
    Allow,
    Block(EgressDenial),
}

/// The per-session egress allowlist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthEgressPolicy {
    binding_host: String,
    binding_port: u16,
    allow_subdomains: bool,
    allow_insecure_loopback_http: bool,
    assets: BTreeSet<String>,
    /// C-gate drill only (feature `gate-drill`): the predicate is off, so the
    /// drill can measure that a class reaches the canary *when the policy is the
    /// only control*. No shipped build enables the feature, and [`new`] always
    /// produces an enforcing policy.
    ///
    /// [`new`]: AuthEgressPolicy::new
    #[cfg(feature = "gate-drill")]
    permissive: bool,
}

impl AuthEgressPolicy {
    /// `binding_port` is the exact port of the authenticated origin (the
    /// profile's bound origin). `assets` are exact hostnames. Plain `http` is
    /// allowed only for a literal loopback host and only when
    /// `allow_insecure_loopback_http` is set (the v0.8 `allow_loopback_http`
    /// rule).
    pub fn new(
        binding_host: &str,
        binding_port: u16,
        allow_subdomains: bool,
        assets: &[String],
        allow_insecure_loopback_http: bool,
    ) -> Result<Self> {
        if binding_port == 0 {
            return Err(WidowError::InvalidUrl(
                "binding port must be 1..=65535".to_string(),
            ));
        }
        let binding_host = normalize_host(binding_host, "binding host")?;
        let mut set = BTreeSet::new();
        for asset in assets {
            set.insert(normalize_host(asset, "assets entry")?);
        }
        Ok(Self {
            binding_host,
            binding_port,
            allow_subdomains,
            allow_insecure_loopback_http,
            assets: set,
            #[cfg(feature = "gate-drill")]
            permissive: false,
        })
    }

    /// C-gate drill only (feature `gate-drill`): a policy that allows every URL.
    ///
    /// It exists so the drill can show the canary is reachable when the
    /// policy is the only control. The feature is not in any shipped build
    /// (`default`, `http` and `static` build it off), so this constructor does
    /// not exist in the deployment artifact.
    #[cfg(feature = "gate-drill")]
    pub fn permissive_for_drill() -> Self {
        Self {
            binding_host: "drill.invalid".to_string(),
            binding_port: 1,
            allow_subdomains: false,
            allow_insecure_loopback_http: true,
            assets: BTreeSet::new(),
            permissive: true,
        }
    }

    pub fn binding_host(&self) -> &str {
        &self.binding_host
    }

    pub fn binding_port(&self) -> u16 {
        self.binding_port
    }

    pub fn allow_subdomains(&self) -> bool {
        self.allow_subdomains
    }

    pub fn assets(&self) -> impl Iterator<Item = &str> {
        self.assets.iter().map(String::as_str)
    }

    /// Reachability: the network-egress predicate.
    pub fn check(&self, url: &Url) -> EgressDecision {
        #[cfg(feature = "gate-drill")]
        if self.permissive {
            return EgressDecision::Allow;
        }
        if !matches!(url.scheme(), "http" | "https") {
            return EgressDecision::Block(EgressDenial::Scheme);
        }
        let Some(host) = url_host(url) else {
            return EgressDecision::Block(EgressDenial::Host);
        };
        if !self.scheme_allowed(url, &host) {
            return EgressDecision::Block(EgressDenial::Scheme);
        }
        let port = url.port_or_known_default();
        if host == self.binding_host {
            if port != Some(self.binding_port) {
                return EgressDecision::Block(EgressDenial::Port);
            }
            return EgressDecision::Allow;
        }
        if self.allow_subdomains && host.ends_with(&format!(".{}", self.binding_host)) {
            if port != Some(self.binding_port) {
                return EgressDecision::Block(EgressDenial::Port);
            }
            return EgressDecision::Allow;
        }
        if self.assets.contains(&host) {
            // An operator-declared asset is egress on the **bound port**; v1
            // does not model per-asset ports. It is never credential-bearing
            // (see `may_bear_credential`). This keeps a declared asset from
            // opening every non-web service port on its host.
            if port != Some(self.binding_port) {
                return EgressDecision::Block(EgressDenial::Port);
            }
            return EgressDecision::Allow;
        }
        EgressDecision::Block(EgressDenial::Host)
    }

    /// Credential delivery: exact binding origin only. A reachable subdomain or
    /// asset is explicitly not credential-bearing.
    pub fn may_bear_credential(&self, url: &Url) -> EgressDecision {
        #[cfg(feature = "gate-drill")]
        if self.permissive {
            return EgressDecision::Allow;
        }
        if !matches!(url.scheme(), "http" | "https") {
            return EgressDecision::Block(EgressDenial::Scheme);
        }
        let Some(host) = url_host(url) else {
            return EgressDecision::Block(EgressDenial::Credential);
        };
        if !self.scheme_allowed(url, &host) {
            return EgressDecision::Block(EgressDenial::Scheme);
        }
        if host != self.binding_host {
            return EgressDecision::Block(EgressDenial::Credential);
        }
        if url.port_or_known_default() != Some(self.binding_port) {
            return EgressDecision::Block(EgressDenial::Credential);
        }
        EgressDecision::Allow
    }

    fn scheme_allowed(&self, url: &Url, host: &str) -> bool {
        match url.scheme() {
            "https" => true,
            "http" => {
                self.allow_insecure_loopback_http
                    && host
                        .parse::<IpAddr>()
                        .map(|address| address.is_loopback())
                        .unwrap_or(false)
            }
            _ => false,
        }
    }
}

fn url_host(url: &Url) -> Option<String> {
    url.host_str()
        .map(|host| host.trim_end_matches('.').to_ascii_lowercase())
}

/// Validate and canonicalize a configured host. A bare DNS name or IP; rejects a
/// scheme, path, query, userinfo, port, wildcard, whitespace, an empty label, a
/// leading dot and underscores (dead configuration an operator would otherwise
/// never learn about; L-2). IPv6 is not expressible (documented limitation).
fn normalize_host(raw: &str, label: &str) -> Result<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(WidowError::InvalidUrl(format!("{label} must not be empty")));
    }
    if trimmed.contains("://")
        || trimmed.contains('/')
        || trimmed.contains('?')
        || trimmed.contains('#')
        || trimmed.contains('@')
        || trimmed.contains('*')
        || trimmed.chars().any(char::is_whitespace)
    {
        return Err(WidowError::InvalidUrl(format!(
            "{label} `{trimmed}` must be a bare hostname (no scheme, path, userinfo or wildcard)"
        )));
    }
    let host = trimmed.trim_end_matches('.').to_ascii_lowercase();
    if host.is_empty() || host.starts_with('.') || host.contains("..") || host.contains('_') {
        return Err(WidowError::InvalidUrl(format!(
            "{label} `{trimmed}` is not a valid hostname (empty label, leading dot or underscore)"
        )));
    }
    // A port is not a host. IPv6 literals are bracketed and not used by the
    // session contract; refuse a colon rather than guess.
    if host.contains(':') {
        return Err(WidowError::InvalidUrl(format!(
            "{label} `{trimmed}` must be a bare hostname without a port (IPv6 bindings are not supported)"
        )));
    }
    Ok(host)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(raw: &str) -> Url {
        Url::parse(raw).unwrap()
    }

    fn policy(
        binding: &str,
        port: u16,
        subdomains: bool,
        assets: &[&str],
        loopback_http: bool,
    ) -> AuthEgressPolicy {
        AuthEgressPolicy::new(
            binding,
            port,
            subdomains,
            &assets
                .iter()
                .map(|value| value.to_string())
                .collect::<Vec<_>>(),
            loopback_http,
        )
        .unwrap()
    }

    fn http_loopback_policy() -> AuthEgressPolicy {
        policy("127.0.0.1", 8443, false, &["127.0.0.3"], true)
    }

    #[test]
    fn the_binding_origin_is_reachable_cross_host_is_not() {
        let policy = http_loopback_policy();
        assert_eq!(
            policy.check(&url("http://127.0.0.1:8443/page")),
            EgressDecision::Allow
        );
        assert_eq!(
            policy.check(&url("http://127.0.0.2:8443/page")),
            EgressDecision::Block(EgressDenial::Host)
        );
        assert_eq!(
            policy.check(&url("http://bound.test:8443/page")),
            EgressDecision::Block(EgressDenial::Scheme),
            "plain http to a non-loopback host is a scheme denial"
        );
        assert_eq!(
            policy.check(&url("https://bound.test:8443/page")),
            EgressDecision::Block(EgressDenial::Host)
        );
    }

    #[test]
    fn a_sibling_service_on_the_binding_host_is_a_port_denial() {
        // A different port on the binding host is a different service.
        let policy = http_loopback_policy();
        assert_eq!(
            policy.check(&url("http://127.0.0.1:9000/anything")),
            EgressDecision::Block(EgressDenial::Port)
        );
        // The literal review pair: an off-host/off-scheme target is never a hit.
        assert_eq!(
            policy.check(&url("http://bound.test:9000/anything")),
            EgressDecision::Block(EgressDenial::Scheme)
        );
    }

    #[test]
    fn plain_http_needs_the_literal_loopback_opt_in() {
        let strict = policy("127.0.0.1", 8443, false, &[], false);
        assert_eq!(
            strict.check(&url("http://127.0.0.1:8443/page")),
            EgressDecision::Block(EgressDenial::Scheme)
        );
        assert_eq!(
            strict.check(&url("https://127.0.0.1:8443/page")),
            EgressDecision::Allow
        );
        let named = policy("bound.test", 443, false, &[], true);
        assert_eq!(
            named.check(&url("http://bound.test/page")),
            EgressDecision::Block(EgressDenial::Scheme)
        );
    }

    #[test]
    fn subdomains_are_opt_in_and_port_bound() {
        let policy = policy("bound.test", 443, true, &[], false);
        assert_eq!(
            policy.check(&url("https://sub.bound.test/page")),
            EgressDecision::Allow
        );
        assert_eq!(
            policy.check(&url("https://sub.bound.test:8443/page")),
            EgressDecision::Block(EgressDenial::Port)
        );
        assert_eq!(
            policy.check(&url("https://evilbound.test/page")),
            EgressDecision::Block(EgressDenial::Host)
        );
        assert_eq!(
            policy.check(&url("https://bound.test.evil/page")),
            EgressDecision::Block(EgressDenial::Host)
        );
    }

    #[test]
    fn assets_are_reachable_but_never_credential_bearing() {
        let policy = policy("bound.test", 443, false, &["assets.test"], false);
        assert_eq!(
            policy.check(&url("https://assets.test/a.js")),
            EgressDecision::Allow
        );
        assert_eq!(
            policy.check(&url("https://assets.test:9000/a.js")),
            EgressDecision::Block(EgressDenial::Port),
            "v1 pins assets to the bound port ()"
        );
        assert_eq!(
            policy.may_bear_credential(&url("https://assets.test/a.js")),
            EgressDecision::Block(EgressDenial::Credential)
        );
        assert_eq!(
            policy.may_bear_credential(&url("https://bound.test/page")),
            EgressDecision::Allow
        );
        assert_eq!(
            policy.may_bear_credential(&url("https://sub.bound.test/page")),
            EgressDecision::Block(EgressDenial::Credential)
        );
        assert_eq!(
            policy.may_bear_credential(&url("https://bound.test:8443/page")),
            EgressDecision::Block(EgressDenial::Credential)
        );
    }

    #[test]
    fn only_http_and_https_are_considered() {
        let policy = http_loopback_policy();
        for raw in [
            "ws://127.0.0.1:8443/socket",
            "wss://127.0.0.1:8443/socket",
            "file:///etc/passwd",
        ] {
            assert_eq!(
                policy.check(&url(raw)),
                EgressDecision::Block(EgressDenial::Scheme),
                "{raw}"
            );
        }
    }

    #[test]
    fn hosts_are_case_folded_and_a_root_dot_is_ignored() {
        let policy = policy("127.0.0.1", 8443, false, &[], true);
        assert_eq!(
            policy.check(&url("HTTP://127.0.0.1:8443/page")),
            EgressDecision::Allow
        );
        let named = AuthEgressPolicy::new("BOUND.TEST.", 443, false, &[], false).unwrap();
        assert_eq!(named.binding_host(), "bound.test");
    }

    #[test]
    fn a_malformed_configured_host_or_port_is_refused() {
        for raw in [
            "",
            "https://bound.test",
            "bound.test/path",
            "*.bound.test",
            "bound.test@evil",
            "bound.test:443",
            ".bound.test",
            "bound..test",
            "under_score.test",
        ] {
            assert!(
                AuthEgressPolicy::new(raw, 443, false, &[], false).is_err(),
                "{raw} must be refused"
            );
        }
        assert!(AuthEgressPolicy::new("bound.test", 0, false, &[], false).is_err());
        assert!(
            AuthEgressPolicy::new("bound.test", 443, false, &["*.cdn.test".to_string()], false)
                .is_err()
        );
    }

    #[test]
    fn every_denial_has_a_pinned_reason_and_the_audit_outcome() {
        for (denial, reason) in [
            (EgressDenial::Scheme, "scheme"),
            (EgressDenial::Port, "port"),
            (EgressDenial::Credential, "credential"),
            (EgressDenial::Host, "host"),
        ] {
            assert_eq!(denial.as_str(), reason);
            assert_eq!(denial.audit_outcome(), AUDIT_EGRESS_BLOCKED);
            assert_eq!(AUDIT_EGRESS_BLOCKED, "egress_blocked");
        }
    }

    #[test]
    fn the_allow_positive_asset_and_canary_use_the_same_enforcing_policy() {
        //: one policy object; the binding and a distinct asset host are
        // reachable, the canary/port sibling is denied. The asset host is
        // distinct from the canary host by construction (127.0.0.3 vs .2).
        let policy = policy("127.0.0.1", 8443, false, &["127.0.0.3"], true);
        assert_eq!(
            policy.check(&url("http://127.0.0.1:8443/page")),
            EgressDecision::Allow
        );
        assert_eq!(
            policy.check(&url("http://127.0.0.3:8443/asset.js")),
            EgressDecision::Allow
        );
        assert_eq!(
            policy.check(&url("http://127.0.0.2:9000/canary")),
            EgressDecision::Block(EgressDenial::Host)
        );
        assert_eq!(
            policy.check(&url("http://127.0.0.1:9000/canary")),
            EgressDecision::Block(EgressDenial::Port)
        );
    }

    #[test]
    fn asset_iteration_is_deterministic() {
        let policy = policy("bound.test", 443, false, &["z.test", "a.test"], false);
        assert_eq!(
            policy.assets().collect::<Vec<_>>(),
            vec!["a.test", "z.test"]
        );
    }
}
