//! v0.8 — the credential engine.
//!
//! One [`CredentialPlan`] per authenticated `fetch_page`: it carries the bound
//! host, the credential shape and the *references* (never a value). The plan
//! checks every hop against the binding before a secret is resolved, resolves
//! through the operator's manager CLI under the subprocess contract, and
//! builds the request header in memory. Values live only in [`Secret`] and the
//! constructed header; nothing here is serialisable, logged, stored or returned.
//!
//! The module is deliberately independent of the store and the pipeline: it can
//! refuse, resolve or attach, and nothing else.

pub mod egress;
pub mod gate;
pub mod login;
pub mod pipe;
pub mod proxy;
pub mod store;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use base64::Engine as _;
use reqwest::header::{HeaderName, HeaderValue};
use url::Url;
use zeroize::Zeroize;

use crate::config::{AuthConfig, AuthKind};
use crate::core::error::{Result, WidowError};

/// The pinned manager set (the reference scheme selects the provider binary).
pub const PROVIDERS: [&str; 3] = ["op", "bw", "pass"];
/// Upper bound on a manager's stdout.
pub const MAX_SECRET_BYTES: usize = 8 * 1024;

/// One resolved secret. Redacted in every formatting path, zeroized on drop, no
/// `Serialize`/`Deserialize`, never placed in an error, a log line or a payload.
pub struct Secret(String);

impl Secret {
    pub fn new(value: String) -> Self {
        Self(value)
    }

    /// The value, only for building the request header.
    fn expose(&self) -> &str {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Secret(<redacted>)")
    }
}

impl std::fmt::Display for Secret {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("<redacted>")
    }
}

/// The canonical hosts that actually received the credential on one attempt, in
/// hop order and without duplicates (F-2). The HTTP layer records a host after a
/// request was answered; the pipeline joins the list into `auth_audit.host`.
#[derive(Debug, Default)]
pub struct AuthHosts {
    hosts: std::sync::Mutex<Vec<String>>,
}

impl AuthHosts {
    pub fn record(&self, host: &str) {
        if host.is_empty() {
            return;
        }
        let mut hosts = self
            .hosts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !hosts.iter().any(|seen| seen == host) {
            hosts.push(host.to_string());
        }
    }

    pub fn list(&self) -> Vec<String> {
        self.hosts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// `Some("a,b")` when at least one host received the credential.
    pub fn joined(&self) -> Option<String> {
        let hosts = self.list();
        (!hosts.is_empty()).then(|| hosts.join(","))
    }
}

/// One configured manager CLI: an absolute path plus the only environment
/// variables the child may see.
#[derive(Debug, Clone)]
pub struct ManagerSpec {
    pub provider: &'static str,
    pub path: PathBuf,
    pub env_allow: Vec<String>,
}

/// Everything one authenticated fetch needs, resolved from `[auth]` at the
/// start of the call. Contains references, never values.
#[derive(Clone)]
pub struct CredentialPlan {
    pub profile: String,
    /// Canonical bound host (lowercase, no trailing dot).
    pub host: String,
    pub kind: AuthKind,
    pub header: Option<String>,
    pub allow_subdomains: bool,
    pub allow_loopback_http: bool,
    primary: Option<String>,
    username: Option<String>,
    password: Option<String>,
    managers: BTreeMap<String, ManagerSpec>,
    timeout: Duration,
    /// Exact credential origin port, from the profile (default 443).
    pub origin_port: u16,
    /// Time spent resolving the credential for this fetch (one resolution).
    resolution_ms: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl std::fmt::Debug for CredentialPlan {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // References, paths and values stay out of every debug rendering.
        formatter
            .debug_struct("CredentialPlan")
            .field("profile", &self.profile)
            .field("host", &self.host)
            .field("kind", &self.kind)
            .field("allow_subdomains", &self.allow_subdomains)
            .finish_non_exhaustive()
    }
}

impl CredentialPlan {
    /// Build the plan for one operator profile. An unknown name is
    /// `auth_profile_unknown` and reveals nothing about the configuration.
    pub fn from_config(config: &AuthConfig, profile: &str) -> Result<Self> {
        let entry = config
            .profiles
            .get(profile)
            .ok_or_else(|| WidowError::AuthProfileUnknown {
                profile: profile.to_string(),
            })?;
        if entry.kind == AuthKind::Session {
            // The login path delivers the login and the sealed jar only; the
            // authenticated browser path is separate, so a session profile must
            // never reach the header path.
            return Err(WidowError::AuthSessionUnsupported);
        }
        let mut managers = BTreeMap::new();
        for reference in [
            entry.reference.as_deref(),
            entry.username_reference.as_deref(),
            entry.password_reference.as_deref(),
        ]
        .into_iter()
        .flatten()
        {
            let provider =
                provider_of(reference).ok_or_else(|| WidowError::AuthSecretUnavailable {
                    provider: "unknown".to_string(),
                })?;
            if managers.contains_key(provider) {
                continue;
            }
            let manager =
                config
                    .managers
                    .get(provider)
                    .ok_or_else(|| WidowError::AuthSecretUnavailable {
                        provider: provider.to_string(),
                    })?;
            managers.insert(
                provider.to_string(),
                ManagerSpec {
                    provider: match provider {
                        "op" => "op",
                        "bw" => "bw",
                        _ => "pass",
                    },
                    path: PathBuf::from(&manager.path),
                    env_allow: manager.env_allow.clone(),
                },
            );
        }
        let host = entry
            .host
            .strip_suffix('.')
            .unwrap_or(&entry.host)
            .to_string();
        Ok(Self {
            profile: profile.to_string(),
            host,
            kind: entry.kind,
            header: entry.header.clone(),
            allow_subdomains: entry.allow_subdomains,
            allow_loopback_http: config.allow_loopback_http,
            primary: entry.reference.clone(),
            username: entry.username_reference.clone(),
            password: entry.password_reference.clone(),
            managers,
            origin_port: entry.port,
            timeout: Duration::from_millis(config.manager_timeout_ms),
            resolution_ms: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
        })
    }

    /// The binding id recorded in `auth_audit` (the canonical bound host).
    pub fn audit_id(&self) -> &str {
        &self.host
    }

    /// Wall time spent resolving the credential for this fetch, in milliseconds.
    pub fn resolution_ms(&self) -> u64 {
        self.resolution_ms
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Per-hop check, evaluated after redirect resolution and before any secret
    /// is resolved or attached. `redirected` distinguishes a cross-host redirect
    /// from an initial URL outside the binding, which get different codes.
    ///
    /// **Credential delivery is exact-origin:** the host must equal the
    /// bound host (a subdomain is reachable but never credential-bearing) and
    /// the port must equal the configured `origin_port` (default 443). A
    /// subdomain or port mismatch is `egress_blocked`, produced here before
    /// `resolve_header` can return a value. `allow_subdomains` no longer widens
    /// credential delivery; the browser reachability predicate has its own
    /// `AuthEgressPolicy`.
    pub fn check_url(&self, url: &Url, redirected: bool) -> Result<()> {
        match url.scheme() {
            "https" => {}
            "http" if self.allow_loopback_http && is_literal_loopback(url) => {}
            _ => return Err(WidowError::AuthInsecureUrl),
        }
        let host = canonical_url_host(url);
        if host != self.host {
            if self.allow_subdomains && is_subdomain(&host, &self.host) {
                return Err(WidowError::EgressBlocked {
                    host,
                    reason: "credential".to_string(),
                });
            }
            return Err(if redirected {
                WidowError::AuthRedirectBlocked {
                    host,
                    bound: self.host.clone(),
                }
            } else {
                WidowError::AuthHostNotAllowed {
                    host,
                    bound: self.host.clone(),
                }
            });
        }
        // The origin port is the configured one (default 443), never the
        // first caller-supplied URL's (item 2).
        if url.port_or_known_default() != Some(self.origin_port) {
            return Err(WidowError::EgressBlocked {
                host,
                reason: "credential".to_string(),
            });
        }
        Ok(())
    }

    /// Resolve the credential and build the request header. One manager
    /// invocation per reference per fetch (the caller resolves once and reuses
    /// the header across hops); the value never leaves this module.
    pub async fn resolve_header(&self) -> Result<(HeaderName, HeaderValue)> {
        let started = std::time::Instant::now();
        // The name is fixed before the value exists, so an unusable value is a
        // typed refusal that names the header and never echoes the value.
        let name = match self.kind {
            AuthKind::Bearer | AuthKind::Basic => HeaderName::from_static("authorization"),
            AuthKind::Session => return Err(WidowError::AuthSessionUnsupported),
            AuthKind::Header => {
                let configured = self.header.as_deref().unwrap_or_default();
                HeaderName::from_bytes(configured.as_bytes()).map_err(|_| {
                    WidowError::AuthHeaderRejected {
                        name: configured.to_string(),
                    }
                })?
            }
        };
        let value = match self.kind {
            AuthKind::Bearer => {
                let secret = self.resolve(self.primary.as_deref(), "reference").await?;
                let mut buffer = String::with_capacity(secret.expose().len() + 7);
                buffer.push_str("Bearer ");
                buffer.push_str(secret.expose());
                header_value(&buffer, name.clone())?
            }
            AuthKind::Basic => {
                let user = self
                    .resolve(self.username.as_deref(), "username_reference")
                    .await?;
                let password = self
                    .resolve(self.password.as_deref(), "password_reference")
                    .await?;
                // `user:password` is combined in memory and zeroized after the
                // base64 buffer exists; only `Basic <b64>` reaches the request.
                let mut joined =
                    String::with_capacity(user.expose().len() + password.expose().len() + 1);
                joined.push_str(user.expose());
                joined.push(':');
                joined.push_str(password.expose());
                let encoded = base64::engine::general_purpose::STANDARD.encode(joined.as_bytes());
                joined.zeroize();
                let mut buffer = String::with_capacity(encoded.len() + 6);
                buffer.push_str("Basic ");
                buffer.push_str(&encoded);
                header_value(&buffer, name.clone())?
            }
            AuthKind::Header => {
                let secret = self.resolve(self.primary.as_deref(), "reference").await?;
                header_value(secret.expose(), name.clone())?
            }
            AuthKind::Session => return Err(WidowError::AuthSessionUnsupported),
        };
        self.resolution_ms.store(
            started.elapsed().as_millis().min(u64::MAX as u128) as u64,
            std::sync::atomic::Ordering::Relaxed,
        );

        Ok((name, value))
    }

    async fn resolve(&self, reference: Option<&str>, field: &str) -> Result<Secret> {
        let reference = reference.ok_or_else(|| WidowError::AuthSecretUnavailable {
            provider: "unknown".to_string(),
        })?;
        let provider = provider_of(reference).ok_or_else(|| WidowError::AuthSecretUnavailable {
            provider: "unknown".to_string(),
        })?;
        let spec =
            self.managers
                .get(provider)
                .ok_or_else(|| WidowError::AuthSecretUnavailable {
                    provider: provider.to_string(),
                })?;
        let args =
            manager_args(provider, reference).map_err(|_| WidowError::AuthSecretUnavailable {
                provider: provider.to_string(),
            })?;
        let value = run_manager(spec, &args, self.timeout).await?;
        let trimmed = value.trim_end_matches(['\n', '\r']);
        if trimmed.is_empty() || trimmed.len() > MAX_SECRET_BYTES {
            return Err(WidowError::AuthSecretUnavailable {
                provider: spec.provider.to_string(),
            });
        }
        tracing::debug!(
            profile = %self.profile,
            provider = spec.provider,
            field,
            "auth reference resolved"
        );
        Ok(Secret::new(trimmed.to_string()))
    }
}

/// `provider` for a reference scheme, matching the config's manager keys.
pub fn provider_of(reference: &str) -> Option<&'static str> {
    PROVIDERS
        .iter()
        .copied()
        .find(|provider| reference.starts_with(&format!("{provider}://")))
}

/// argv for one provider reference (D2/D3): the value is only ever on stdout.
fn manager_args(provider: &str, reference: &str) -> std::result::Result<Vec<String>, ()> {
    let rest = reference
        .strip_prefix(&format!("{provider}://"))
        .ok_or(())?;
    Ok(match provider {
        "op" => vec!["read".to_string(), rest.to_string()],
        "bw" => {
            let mut parts = rest.splitn(2, '/');
            let item = parts.next().ok_or(())?;
            let field = parts.next();
            vec![
                "get".to_string(),
                match field {
                    Some("username") => "username".to_string(),
                    _ => "password".to_string(),
                },
                item.to_string(),
            ]
        }
        "pass" => vec!["show".to_string(), rest.to_string()],
        _ => return Err(()),
    })
}

/// The subprocess contract: absolute configured path, `stdin` null,
/// cleared environment plus `HOME` and the operator-named allowlist, piped
/// stderr that is never logged, bounded stdout, hard timeout, one attempt.
async fn run_manager(spec: &ManagerSpec, args: &[String], timeout: Duration) -> Result<String> {
    let mut command = tokio::process::Command::new(&spec.path);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env_clear()
        .kill_on_drop(true);
    if let Some(home) = std::env::var_os("HOME") {
        command.env("HOME", home);
    }
    for name in &spec.env_allow {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }

    let child = command.spawn().map_err(|_| WidowError::AuthManagerFailed {
        provider: spec.provider.to_string(),
    })?;
    let output = match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(output)) => output,
        Ok(Err(_)) => {
            return Err(WidowError::AuthManagerFailed {
                provider: spec.provider.to_string(),
            });
        }
        Err(_) => {
            return Err(WidowError::AuthManagerTimeout {
                provider: spec.provider.to_string(),
                limit_ms: timeout.as_millis().min(u64::MAX as u128) as u64,
            });
        }
    };
    if !output.status.success() {
        // The category only: stderr may carry provider internals or a value.
        return Err(WidowError::AuthSecretUnavailable {
            provider: spec.provider.to_string(),
        });
    }
    if output.stdout.len() > MAX_SECRET_BYTES {
        return Err(WidowError::AuthManagerFailed {
            provider: spec.provider.to_string(),
        });
    }
    String::from_utf8(output.stdout).map_err(|_| WidowError::AuthSecretUnavailable {
        provider: spec.provider.to_string(),
    })
}

/// A usable header value, or a typed refusal (`auth_header_rejected`). The
/// value is never echoed: it is the credential or a fragment of it.
fn header_value(value: &str, name: HeaderName) -> Result<HeaderValue> {
    HeaderValue::from_str(value).map_err(|_| WidowError::AuthHeaderRejected {
        name: name.as_str().to_string(),
    })
}

/// Literal loopback hosts only: a name that resolves to loopback is not
/// enough, because the destination's DNS answer must not widen the exception.
fn is_literal_loopback(url: &Url) -> bool {
    // A literal IP only: a name that resolves to loopback is not enough
    // (drops `localhost` for fail-closed alignment).
    matches!(
        url.host(),
        Some(url::Host::Ipv4(address)) if address.is_loopback()
    ) || matches!(url.host(), Some(url::Host::Ipv6(address)) if address.is_loopback())
}

fn is_subdomain(host: &str, bound: &str) -> bool {
    host.strip_suffix(bound)
        .is_some_and(|prefix| prefix.ends_with('.'))
}

/// Canonical host of a URL, comparable with the config's canonical `host`.
pub fn canonical_url_host(url: &Url) -> String {
    match url.host() {
        Some(url::Host::Domain(domain)) => domain.trim_end_matches('.').to_ascii_lowercase(),
        Some(url::Host::Ipv4(address)) => address.to_string(),
        Some(url::Host::Ipv6(address)) => address.to_string(),
        None => String::new(),
    }
}

/// The audit outcome vocabulary. Every typed refusal maps to exactly one
/// pinned string; anything else is a transport failure by definition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditOutcome {
    Ok,
    Rejected,
    RedirectBlocked,
    SecretUnavailable,
    ManagerTimeout,
    InsecureUrl,
    HostNotAllowed,
    ProfileUnknown,
    RobotsBlocked,
    EgressBlocked,
    TransportError,
    SessionExpired,
    SessionMissing,
    ChallengeRefused,
    AuthStoreInvalid,
    AuthMethodRefused,
}

impl AuditOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Rejected => "auth_rejected",
            Self::RedirectBlocked => "auth_redirect_blocked",
            Self::SecretUnavailable => "auth_secret_unavailable",
            Self::ManagerTimeout => "auth_manager_timeout",
            Self::InsecureUrl => "auth_insecure_url",
            Self::HostNotAllowed => "auth_host_not_allowed",
            Self::ProfileUnknown => "auth_profile_unknown",
            Self::RobotsBlocked => "robots_blocked",
            Self::EgressBlocked => "egress_blocked",
            Self::TransportError => "transport_error",
            Self::SessionExpired => "session_expired",
            Self::SessionMissing => "session_missing",
            Self::ChallengeRefused => "challenge_refused",
            Self::AuthStoreInvalid => "auth_store_invalid",
            Self::AuthMethodRefused => "egress_blocked",
        }
    }

    pub fn from_error(error: &WidowError) -> Self {
        match error {
            WidowError::AuthRejected { .. } => Self::Rejected,
            WidowError::AuthRedirectBlocked { .. } => Self::RedirectBlocked,
            WidowError::AuthSecretUnavailable { .. }
            | WidowError::AuthManagerFailed { .. }
            | WidowError::AuthHeaderRejected { .. } => Self::SecretUnavailable,
            WidowError::AuthManagerTimeout { .. } => Self::ManagerTimeout,
            WidowError::AuthInsecureUrl => Self::InsecureUrl,
            WidowError::AuthHostNotAllowed { .. } => Self::HostNotAllowed,
            WidowError::AuthProfileUnknown { .. } => Self::ProfileUnknown,
            WidowError::EgressBlocked { .. } => Self::EgressBlocked,
            WidowError::AuthSessionExpired { .. } => Self::SessionExpired,
            WidowError::AuthSessionMissing { .. } => Self::SessionMissing,
            WidowError::AuthChallengeRefused => Self::ChallengeRefused,
            WidowError::AuthStoreInvalid { .. } => Self::AuthStoreInvalid,
            WidowError::AuthMethodRefused => Self::AuthMethodRefused,
            // F-2: a robots refusal is not a transport failure; it happens before
            // any credential is resolved.
            WidowError::BlockedByRobots { .. } => Self::RobotsBlocked,
            _ => Self::TransportError,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(value: &str) -> Url {
        Url::parse(value).unwrap()
    }

    fn plan(
        profile: &str,
        kind: AuthKind,
        host: &str,
        based: bool,
        loopback: bool,
    ) -> CredentialPlan {
        plan_at(profile, kind, host, based, loopback, 443)
    }

    fn plan_at(
        profile: &str,
        kind: AuthKind,
        host: &str,
        based: bool,
        loopback: bool,
        port: u16,
    ) -> CredentialPlan {
        let mut config = AuthConfig {
            allow_loopback_http: loopback,
            ..AuthConfig::default()
        };
        config.managers.insert(
            "op".to_string(),
            crate::config::AuthManagerConfig {
                path: "/bin/sh".to_string(),
                env_allow: Vec::new(),
            },
        );
        let mut entry = crate::config::AuthProfileConfig {
            kind,
            host: host.to_string(),
            reference: Some("op://V/i/f".to_string()),
            allow_subdomains: based,
            port,
            ..Default::default()
        };
        if kind == AuthKind::Header {
            entry.header = Some("X-Api-Key".to_string());
        }
        config.profiles.insert(profile.to_string(), entry);
        CredentialPlan::from_config(&config, profile).unwrap()
    }

    #[test]
    fn an_unknown_profile_is_typed() {
        let error = CredentialPlan::from_config(&AuthConfig::default(), "nope").unwrap_err();
        assert!(matches!(error, WidowError::AuthProfileUnknown { .. }));
        assert_eq!(error.code().as_str(), "auth_profile_unknown");
    }

    #[test]
    fn credential_delivery_is_exact_origin() {
        let exact = plan("p", AuthKind::Bearer, "github.com", false, false);
        assert!(exact.check_url(&url("https://github.com/a"), false).is_ok());
        assert!(matches!(
            exact.check_url(&url("https://api.github.com/a"), false),
            Err(WidowError::AuthHostNotAllowed { .. })
        ));
        assert!(matches!(
            exact.check_url(&url("https://github.com.evil/a"), false),
            Err(WidowError::AuthHostNotAllowed { .. })
        ));

        //: allow_subdomains still permits reachability but no longer widens
        // credential delivery; the refusal is egress_blocked, not host-not-allowed.
        let widened = plan("p", AuthKind::Bearer, "github.com", true, false);
        let error = widened
            .check_url(&url("https://api.github.com/a"), false)
            .unwrap_err();
        assert!(
            matches!(error, WidowError::EgressBlocked { .. }),
            "{error:?}"
        );
        assert_eq!(error.code().as_str(), "egress_blocked");
        assert_eq!(AuditOutcome::from_error(&error).as_str(), "egress_blocked");
        assert!(
            widened
                .check_url(&url("https://github.com.evil/a"), false)
                .is_err()
        );
    }

    #[test]
    fn credential_delivery_is_bound_to_the_configured_origin_port() {
        //  item 2: the origin port is the configured one, never the
        // first caller-supplied URL's.
        let pinned = plan("p", AuthKind::Bearer, "example.com", false, false);
        assert!(
            pinned
                .check_url(&url("https://example.com/a"), false)
                .is_ok(),
            "the default origin port is 443"
        );
        let error = pinned
            .check_url(&url("https://example.com:8443/b"), true)
            .unwrap_err();
        assert!(
            matches!(error, WidowError::EgressBlocked { .. }),
            "{error:?}"
        );
        assert_eq!(error.code().as_str(), "egress_blocked");
        assert_eq!(AuditOutcome::from_error(&error).as_str(), "egress_blocked");
        assert!(
            pinned
                .check_url(&url("https://example.com:8443/a"), false)
                .is_err(),
            "another port on the bound host is not the origin"
        );

        // A profile that declares a non-default port binds to that port only.
        let custom = plan_at("p", AuthKind::Bearer, "example.com", false, false, 8443);
        assert!(
            custom
                .check_url(&url("https://example.com:8443/a"), false)
                .is_ok()
        );
        assert!(matches!(
            custom.check_url(&url("https://example.com/a"), false),
            Err(WidowError::EgressBlocked { .. })
        ));
    }

    #[test]
    fn a_cross_host_redirect_is_reported_as_such() {
        let plan = plan("p", AuthKind::Bearer, "github.com", false, false);
        assert!(matches!(
            plan.check_url(&url("https://idp.example/login"), true),
            Err(WidowError::AuthRedirectBlocked { .. })
        ));
    }

    #[test]
    fn http_is_refused_except_for_literal_loopback_when_allowed() {
        let strict = plan_at("p", AuthKind::Bearer, "127.0.0.1", false, false, 8080);
        assert!(matches!(
            strict.check_url(&url("http://127.0.0.1:8080/a"), false),
            Err(WidowError::AuthInsecureUrl)
        ));

        let loopback = plan_at("p", AuthKind::Bearer, "127.0.0.1", false, true, 8080);
        assert!(
            loopback
                .check_url(&url("http://127.0.0.1:8080/a"), false)
                .is_ok()
        );
        // `localhost` is a different binding, not a free pass.
        assert!(matches!(
            loopback.check_url(&url("http://localhost:8080/a"), false),
            Err(WidowError::AuthInsecureUrl)
        ));
        //: the plain-http exception is a *literal* loopback address, so
        // a `localhost` binding no longer gets it; the scheme refusal fires.
        let named = plan_at("p", AuthKind::Bearer, "localhost", false, true, 8080);
        assert!(matches!(
            named.check_url(&url("http://localhost:8080/a"), false),
            Err(WidowError::AuthInsecureUrl)
        ));
        // A name that merely resolves to loopback is not a literal loopback host:
        // the scheme refusal fires first for any non-literal-http destination.
        assert!(matches!(
            loopback.check_url(&url("http://loopback.example/a"), false),
            Err(WidowError::AuthInsecureUrl)
        ));
        assert!(matches!(
            loopback.check_url(&url("https://unbound.example/a"), false),
            Err(WidowError::AuthHostNotAllowed { .. })
        ));
        // And the exception never widens a non-loopback destination.
        assert!(matches!(
            loopback.check_url(&url("http://example.com/a"), false),
            Err(WidowError::AuthInsecureUrl)
        ));
        assert!(matches!(
            loopback.check_url(&url("ftp://127.0.0.1/a"), false),
            Err(WidowError::AuthInsecureUrl)
        ));
    }

    #[test]
    fn manager_argv_matches_each_provider() {
        assert_eq!(
            manager_args("op", "op://V/i/f").unwrap(),
            vec!["read", "V/i/f"]
        );
        assert_eq!(
            manager_args("bw", "bw://item").unwrap(),
            vec!["get", "password", "item"]
        );
        assert_eq!(
            manager_args("bw", "bw://item/username").unwrap(),
            vec!["get", "username", "item"]
        );
        assert_eq!(
            manager_args("pass", "pass://store/a/b").unwrap(),
            vec!["show", "store/a/b"]
        );
        assert!(manager_args("op", "bw://x").is_err());
    }

    #[test]
    fn the_secret_is_redacted_in_every_formatting_path() {
        let secret = Secret::new("canary-value".to_string());
        assert_eq!(format!("{secret:?}"), "Secret(<redacted>)");
        assert_eq!(format!("{secret}"), "<redacted>");
        assert!(!format!("{secret:?}{secret}").contains("canary"));
    }

    #[tokio::test]
    async fn a_control_character_in_the_resolved_value_is_a_typed_refusal() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("op");
        // The provider emits a value with a control character: unusable in a
        // header, and it must be refused before any request is built.
        std::fs::write(&script, "#!/bin/sh\nprintf 'bad\\001value\\n'\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();

        let mut config = AuthConfig::default();
        config.managers.insert(
            "op".to_string(),
            crate::config::AuthManagerConfig {
                path: script.to_string_lossy().to_string(),
                env_allow: Vec::new(),
            },
        );
        config.profiles.insert(
            "p".to_string(),
            crate::config::AuthProfileConfig {
                kind: AuthKind::Bearer,
                host: "example.com".to_string(),
                reference: Some("op://V/i/f".to_string()),
                ..Default::default()
            },
        );
        let plan = CredentialPlan::from_config(&config, "p").unwrap();
        let error = plan.resolve_header().await.unwrap_err();
        assert!(
            matches!(error, WidowError::AuthHeaderRejected { .. }),
            "{error:?}"
        );
        assert_eq!(error.code().as_str(), "auth_header_rejected");
        assert!(
            !error.to_string().contains("bad"),
            "the value is not echoed: {error}"
        );
        assert_eq!(
            AuditOutcome::from_error(&error).as_str(),
            "auth_secret_unavailable",
            "the audit vocabulary has no header-rejected outcome"
        );
    }

    #[test]
    fn the_hop_host_list_is_ordered_and_deduplicated() {
        let hosts = AuthHosts::default();
        assert_eq!(hosts.joined(), None);
        hosts.record("a.example.com");
        hosts.record("a.example.com");
        hosts.record("b.example.com");
        hosts.record("");
        assert_eq!(hosts.list(), vec!["a.example.com", "b.example.com"]);
        assert_eq!(
            hosts.joined().as_deref(),
            Some("a.example.com,b.example.com")
        );
    }

    #[test]
    fn a_hop_in_the_same_binding_is_accepted_for_every_bearing_host() {
        let widened = plan("p", AuthKind::Bearer, "example.com", true, false);
        assert!(
            widened
                .check_url(&url("https://example.com/x"), true)
                .is_ok(),
            "the exact binding origin is credential-bearing"
        );
        for host in ["a.example.com", "b.example.com"] {
            let error = widened
                .check_url(&url(&format!("https://{host}/x")), true)
                .unwrap_err();
            assert!(
                matches!(error, WidowError::EgressBlocked { .. }),
                "{host} is reachable but not credential-bearing: {error:?}"
            );
        }
        // The accumulator the audit row is built from names the answered hosts.
        let hosts = AuthHosts::default();
        hosts.record("a.example.com");
        hosts.record("b.example.com");
        assert_eq!(
            hosts.joined().as_deref(),
            Some("a.example.com,b.example.com")
        );
    }

    #[test]
    fn audit_outcomes_are_the_pinned_vocabulary() {
        for (error, expected) in [
            (WidowError::AuthRejected { status: 401 }, "auth_rejected"),
            (
                WidowError::AuthRedirectBlocked {
                    host: "x".into(),
                    bound: "y".into(),
                },
                "auth_redirect_blocked",
            ),
            (
                WidowError::AuthManagerTimeout {
                    provider: "op".into(),
                    limit_ms: 10,
                },
                "auth_manager_timeout",
            ),
            (
                WidowError::AuthSecretUnavailable {
                    provider: "op".into(),
                },
                "auth_secret_unavailable",
            ),
            (WidowError::AuthInsecureUrl, "auth_insecure_url"),
            (
                WidowError::AuthHostNotAllowed {
                    host: "x".into(),
                    bound: "y".into(),
                },
                "auth_host_not_allowed",
            ),
            (
                WidowError::AuthProfileUnknown {
                    profile: "p".into(),
                },
                "auth_profile_unknown",
            ),
            (
                WidowError::EgressBlocked {
                    host: "x".into(),
                    reason: "host".into(),
                },
                "egress_blocked",
            ),
            (
                WidowError::Http {
                    status: Some(500),
                    detail: "x".into(),
                },
                "transport_error",
            ),
        ] {
            assert_eq!(AuditOutcome::from_error(&error).as_str(), expected);
        }
        assert_eq!(AuditOutcome::Ok.as_str(), "ok");
    }
}
