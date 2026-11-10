//! Error taxonomy shared by the fetch, extract, store and MCP layers.
//!
//! The codes are the MCP contract ([`ErrorCode`]); [`WidowError`] is the single
//! in-process error type so a new failure cannot be invented in one layer and
//! forgotten in another. `detail` carries machine-useful extras (observed
//! content type, blocking CIDR) without duplicating the message.

use serde::{Deserialize, Serialize};

use schemars::JsonSchema;

/// Stable MCP error codes ().
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
 InvalidUrl,
 BlockedScheme,
 BlockedPrivateAddress,
 BlockedByRobots,
 DnsError,
 TlsError,
 Timeout,
 TooLarge,
 UnsupportedContentType,
 HttpError,
 RenderUnavailable,
 RenderFailed,
 EmbeddingUnavailable,
 EmbeddingFailed,
 PdfEncrypted,
 PdfInvalid,
 ExtractionFailed,
 SchemaInvalid,
 NotFound,
 LimitExceeded,
 Cancelled,
 Busy,
 SessionBusy,
 SessionExpired,
 SessionNotFound,
 ActionFailed,
 AuthProfileUnknown,
 AuthManagerFailed,
 AuthManagerTimeout,
 AuthSecretUnavailable,
 AuthRejected,
 AuthRedirectBlocked,
 AuthHostNotAllowed,
 AuthInsecureUrl,
 AuthRenderUnsupported,
 AuthHeaderRejected,
 AuthStoreUnavailable,
 AuthStoreLocked,
 AuthSessionUnsupported,
 AuthLoginNotTerminal,
 AuthLoginOriginRefused,
 AuthLoginFinalOriginRefused,
 AuthVerifyFailed,
 AuthCaptureEmpty,
 AuthLoginBrowserFailed,
 AuthChallengeRefused,
 AuthSessionExpired,
 AuthSessionMissing,
 AuthStoreInvalid,
 AuthMethodRefused,
 EgressBlocked,
 Internal,
}

impl ErrorCode {
 pub fn as_str(self) -> &'static str {
 match self {
 Self::InvalidUrl => "invalid_url",
 Self::BlockedScheme => "blocked_scheme",
 Self::BlockedPrivateAddress => "blocked_private_address",
 Self::BlockedByRobots => "blocked_by_robots",
 Self::DnsError => "dns_error",
 Self::TlsError => "tls_error",
 Self::Timeout => "timeout",
 Self::TooLarge => "too_large",
 Self::UnsupportedContentType => "unsupported_content_type",
 Self::HttpError => "http_error",
 Self::RenderUnavailable => "render_unavailable",
 Self::RenderFailed => "render_failed",
 Self::EmbeddingUnavailable => "embedding_unavailable",
 Self::EmbeddingFailed => "embedding_failed",
 Self::PdfEncrypted => "pdf_encrypted",
 Self::PdfInvalid => "pdf_invalid",
 Self::ExtractionFailed => "extraction_failed",
 Self::SchemaInvalid => "schema_invalid",
 Self::NotFound => "not_found",
 Self::LimitExceeded => "limit_exceeded",
 Self::Cancelled => "cancelled",
 Self::Busy => "busy",
 Self::SessionBusy => "session_busy",
 Self::SessionExpired => "session_expired",
 Self::SessionNotFound => "session_not_found",
 Self::ActionFailed => "action_failed",
 Self::AuthProfileUnknown => "auth_profile_unknown",
 Self::AuthManagerFailed => "auth_manager_failed",
 Self::AuthManagerTimeout => "auth_manager_timeout",
 Self::AuthSecretUnavailable => "auth_secret_unavailable",
 Self::AuthRejected => "auth_rejected",
 Self::AuthRedirectBlocked => "auth_redirect_blocked",
 Self::AuthHostNotAllowed => "auth_host_not_allowed",
 Self::AuthInsecureUrl => "auth_insecure_url",
 Self::AuthRenderUnsupported => "auth_render_unsupported",
 Self::AuthHeaderRejected => "auth_header_rejected",
 Self::AuthStoreUnavailable => "auth_store_unavailable",
 Self::AuthStoreLocked => "auth_store_locked",
 Self::AuthSessionUnsupported => "auth_session_unsupported",
 Self::AuthLoginNotTerminal => "auth_login_not_terminal",
 Self::AuthLoginOriginRefused => "auth_login_origin_refused",
 Self::AuthLoginFinalOriginRefused => "auth_login_final_origin_refused",
 Self::AuthVerifyFailed => "auth_verify_failed",
 Self::AuthCaptureEmpty => "auth_capture_empty",
 Self::AuthLoginBrowserFailed => "auth_login_browser_failed",
 Self::AuthChallengeRefused => "auth_challenge_refused",
 Self::AuthSessionExpired => "auth_session_expired",
 Self::AuthSessionMissing => "auth_session_missing",
 Self::AuthStoreInvalid => "auth_store_invalid",
 Self::AuthMethodRefused => "auth_method_refused",
 Self::EgressBlocked => "egress_blocked",
 Self::Internal => "internal",
 }
 }
}

/// Machine-readable error payload returned in `structuredContent.error`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ToolError {
 pub code: ErrorCode,
 pub message: String,
 #[serde(skip_serializing_if = "Option::is_none")]
 pub detail: Option<String>,
}

/// The single internal error type. Every variant maps to exactly one code.
#[derive(Debug, thiserror::Error)]
pub enum WidowError {
 #[error("invalid url: {0}")]
 InvalidUrl(String),

 #[error("blocked scheme `{scheme}`: only http and https are fetched")]
 BlockedScheme { scheme: String },

 #[error("blocked private address `{address}` (range {range})")]
 BlockedPrivateAddress { address: String, range: String },

 #[error("blocked by robots.txt: {reason}")]
 BlockedByRobots { reason: String },

 #[error("dns resolution failed for `{host}`: {message}")]
 Dns { host: String, message: String },

 #[error("tls error: {0}")]
 Tls(String),

 #[error("timed out: {0}")]
 Timeout(String),

 #[error("response body exceeded the {limit} byte limit")]
 TooLarge { limit: u64 },

 #[error("unsupported content type `{content_type}`: {processed}")]
 UnsupportedContentType {
 content_type: String,
 processed: String,
 },

 #[error("http error: {detail}")]
 Http { status: Option<u16>, detail: String },

 #[error("rendering unavailable: {0}")]
 RenderUnavailable(String),

 #[error("rendering failed: {0}")]
 RenderFailed(String),

 #[error("embedding unavailable: {0}")]
 EmbeddingUnavailable(String),

 #[error("embedding failed: {0}")]
 EmbeddingFailed(String),

 #[error("PDF is encrypted: this tool does not guess or use a password")]
 PdfEncrypted,

 #[error("invalid PDF: {0}")]
 PdfInvalid(String),

 #[error("extraction failed: {0}")]
 Extraction(String),

 #[error("invalid extraction spec at {pointer}: {message}")]
 SchemaInvalid { pointer: String, message: String },

 #[error("not found: {0}")]
 NotFound(String),

 #[error("limit exceeded: {0}")]
 LimitExceeded(String),

 #[error("busy: {0}")]
 Busy(String),

 #[error("a browser interaction session is already open: {session_id}")]
 SessionBusy { session_id: String },

 #[error("the browser interaction session has expired: {session_id}")]
 SessionExpired { session_id: String },

 #[error("no browser interaction session with id {session_id}")]
 SessionNotFound { session_id: String },

 #[error("action failed ({reason}): {message}")]
 ActionFailed { reason: String, message: String },

 // A1 — the credential engine. Messages name the profile, provider or
 // host only: never a reference, a header value, a secret or a URL path.
 #[error("unknown auth profile `{profile}`: the operator configures profile names")]
 AuthProfileUnknown { profile: String },

 #[error("auth manager `{provider}` could not be started")]
 AuthManagerFailed { provider: String },

 #[error("auth manager `{provider}` exceeded the {limit_ms} ms timeout")]
 AuthManagerTimeout { provider: String, limit_ms: u64 },

 #[error("auth manager `{provider}` did not return a usable value")]
 AuthSecretUnavailable { provider: String },

 #[error("the origin rejected the configured credential (HTTP {status})")]
 AuthRejected { status: u16 },

 #[error("authenticated redirect to `{host}` refused: the profile binds `{bound}`")]
 AuthRedirectBlocked { host: String, bound: String },

 #[error("host `{host}` is outside the profile binding `{bound}`")]
 AuthHostNotAllowed { host: String, bound: String },

 #[error(
 "authenticated fetch requires https (plain HTTP only for literal loopback hosts with auth.allow_loopback_http)"
)]
 AuthInsecureUrl,

 #[error(
 "render=always is not supported with an auth profile; the authenticated path is static only"
)]
 AuthRenderUnsupported,
 #[error("header `{name}` cannot be set from a profile")]
 AuthHeaderRejected { name: String },

 // — the sealed session store. Fail closed: no plaintext fallback,
 // nothing written. `detail` is a sanitised platform reason, never a value.
 #[error("the sealed session store is unavailable: {detail}")]
 AuthStoreUnavailable { detail: String },

 #[error("the OS keyring collection is locked: {detail}")]
 AuthStoreLocked { detail: String },

 // — the assisted-login boundary. Every message is value-free: a
 // host, a status, or a bounded detail string; never a cookie or a path.
 #[error(
 "a session profile is resolved by the authenticated browser path, not as a request header"
)]
 AuthSessionUnsupported,

 #[error("`auth login` needs an interactive terminal: it is an operator command, never a tool")]
 AuthLoginNotTerminal,

 #[error(
 "login window refused navigation to `{host}`: only the declared login origins are allowed"
)]
 AuthLoginOriginRefused { host: String },

 #[error(
 "the login window ended on `{host}`, not the binding host `{expected}`; nothing was captured"
)]
 AuthLoginFinalOriginRefused { host: String, expected: String },

 #[error(
 "the captured session did not verify against `{verify_url}` (HTTP {status}, {reason}); nothing was sealed"
)]
 AuthVerifyFailed {
 verify_url: String,
 status: u16,
 reason: String,
 },

 #[error("the capture held no cookie for the binding host; nothing was sealed")]
 AuthCaptureEmpty,

 #[error("the login browser failed: {detail}")]
 AuthLoginBrowserFailed { detail: String },

 // — the authenticated browser path. Value-free by construction.
 #[error("the authenticated origin asked for an HTTP credential; refusal ()")]
 AuthChallengeRefused,

 #[error("the sealed session for `{site}` has expired; run `auth login {site}` again")]
 AuthSessionExpired { site: String },

 #[error("no sealed session for `{site}`; run `auth login {site}` first")]
 AuthSessionMissing { site: String },

 #[error(
 "the sealed session for `{site}` is not valid for its configured host; run `auth login {site}`"
)]
 AuthStoreInvalid { site: String },

 #[error("the authenticated browser is read-only: only GET and HEAD are allowed")]
 AuthMethodRefused,

 #[error("egress blocked for host `{host}`: {reason}")]
 EgressBlocked { host: String, reason: String },

 #[error("internal error: {0}")]
 Internal(String),
}

impl WidowError {
 pub fn code(&self) -> ErrorCode {
 match self {
 Self::InvalidUrl(_) => ErrorCode::InvalidUrl,
 Self::BlockedScheme {.. } => ErrorCode::BlockedScheme,
 Self::BlockedPrivateAddress {.. } => ErrorCode::BlockedPrivateAddress,
 Self::BlockedByRobots {.. } => ErrorCode::BlockedByRobots,
 Self::Dns {.. } => ErrorCode::DnsError,
 Self::Tls(_) => ErrorCode::TlsError,
 Self::Timeout(_) => ErrorCode::Timeout,
 Self::TooLarge {.. } => ErrorCode::TooLarge,
 Self::UnsupportedContentType {.. } => ErrorCode::UnsupportedContentType,
 Self::Http {.. } => ErrorCode::HttpError,
 Self::RenderUnavailable(_) => ErrorCode::RenderUnavailable,
 Self::RenderFailed(_) => ErrorCode::RenderFailed,
 Self::EmbeddingUnavailable(_) => ErrorCode::EmbeddingUnavailable,
 Self::EmbeddingFailed(_) => ErrorCode::EmbeddingFailed,
 Self::PdfEncrypted => ErrorCode::PdfEncrypted,
 Self::PdfInvalid(_) => ErrorCode::PdfInvalid,
 Self::Extraction(_) => ErrorCode::ExtractionFailed,
 Self::SchemaInvalid {.. } => ErrorCode::SchemaInvalid,
 Self::NotFound(_) => ErrorCode::NotFound,
 Self::LimitExceeded(_) => ErrorCode::LimitExceeded,
 Self::Busy(_) => ErrorCode::Busy,
 Self::SessionBusy {.. } => ErrorCode::SessionBusy,
 Self::SessionExpired {.. } => ErrorCode::SessionExpired,
 Self::SessionNotFound {.. } => ErrorCode::SessionNotFound,
 Self::ActionFailed {.. } => ErrorCode::ActionFailed,
 Self::AuthProfileUnknown {.. } => ErrorCode::AuthProfileUnknown,
 Self::AuthManagerFailed {.. } => ErrorCode::AuthManagerFailed,
 Self::AuthManagerTimeout {.. } => ErrorCode::AuthManagerTimeout,
 Self::AuthSecretUnavailable {.. } => ErrorCode::AuthSecretUnavailable,
 Self::AuthRejected {.. } => ErrorCode::AuthRejected,
 Self::AuthRedirectBlocked {.. } => ErrorCode::AuthRedirectBlocked,
 Self::AuthHostNotAllowed {.. } => ErrorCode::AuthHostNotAllowed,
 Self::AuthInsecureUrl => ErrorCode::AuthInsecureUrl,
 Self::AuthRenderUnsupported => ErrorCode::AuthRenderUnsupported,
 Self::AuthHeaderRejected {.. } => ErrorCode::AuthHeaderRejected,
 Self::AuthStoreUnavailable {.. } => ErrorCode::AuthStoreUnavailable,
 Self::AuthStoreLocked {.. } => ErrorCode::AuthStoreLocked,
 Self::AuthSessionUnsupported => ErrorCode::AuthSessionUnsupported,
 Self::AuthLoginNotTerminal => ErrorCode::AuthLoginNotTerminal,
 Self::AuthLoginOriginRefused {.. } => ErrorCode::AuthLoginOriginRefused,
 Self::AuthLoginFinalOriginRefused {.. } => ErrorCode::AuthLoginFinalOriginRefused,
 Self::AuthVerifyFailed {.. } => ErrorCode::AuthVerifyFailed,
 Self::AuthCaptureEmpty => ErrorCode::AuthCaptureEmpty,
 Self::AuthLoginBrowserFailed {.. } => ErrorCode::AuthLoginBrowserFailed,
 Self::AuthChallengeRefused => ErrorCode::AuthChallengeRefused,
 Self::AuthSessionExpired {.. } => ErrorCode::AuthSessionExpired,
 Self::AuthSessionMissing {.. } => ErrorCode::AuthSessionMissing,
 Self::AuthStoreInvalid {.. } => ErrorCode::AuthStoreInvalid,
 Self::AuthMethodRefused => ErrorCode::AuthMethodRefused,
 Self::EgressBlocked {.. } => ErrorCode::EgressBlocked,
 Self::Internal(_) => ErrorCode::Internal,
 }
 }

 /// Extra machine-readable context, kept separate from the message.
 pub fn detail(&self) -> Option<String> {
 match self {
 Self::BlockedScheme { scheme } => Some(format!("scheme={scheme}")),
 Self::BlockedPrivateAddress { address, range } => {
 Some(format!("address={address} range={range}"))
 }
 Self::UnsupportedContentType { content_type,.. } => {
 Some(format!("content_type={content_type}"))
 }
 Self::Http {
 status: Some(status),
..
 }
 | Self::AuthRejected { status } => Some(format!("status={status}")),
 Self::TooLarge { limit } => Some(format!("limit_bytes={limit}")),
 Self::Dns { host,.. } => Some(format!("host={host}")),
 Self::SchemaInvalid { pointer,.. } => Some(format!("pointer={pointer}")),
 Self::SessionBusy { session_id }
 | Self::SessionExpired { session_id }
 | Self::SessionNotFound { session_id } => Some(format!("session_id={session_id}")),
 Self::ActionFailed { reason,.. } => Some(format!("reason={reason}")),
 Self::EgressBlocked { host, reason } => Some(format!("host={host} reason={reason}")),
 _ => None,
 }
 }

 /// HTTP status the origin returned, when the failure carries one.
 pub fn http_status(&self) -> Option<u16> {
 match self {
 Self::Http { status,.. } => *status,
 Self::AuthRejected { status } => Some(*status),
 _ => None,
 }
 }

 pub fn to_tool_error(&self) -> ToolError {
 ToolError {
 code: self.code(),
 message: self.to_string(),
 detail: self.detail(),
 }
 }

 pub fn internal(source: impl std::fmt::Display) -> Self {
 Self::Internal(source.to_string())
 }
}

pub type Result<T> = std::result::Result<T, WidowError>;

#[cfg(test)]
mod tests {
 use super::*;

 #[test]
 fn every_variant_has_a_distinct_code() {
 let cases: Vec<(WidowError, ErrorCode)> = vec![
 (WidowError::InvalidUrl("x".into()), ErrorCode::InvalidUrl),
 (
 WidowError::BlockedScheme {
 scheme: "ftp".into(),
 },
 ErrorCode::BlockedScheme),
 (
 WidowError::BlockedPrivateAddress {
 address: "127.0.0.1".into(),
 range: "127.0.0.0/8".into(),
 },
 ErrorCode::BlockedPrivateAddress),
 (
 WidowError::BlockedByRobots {
 reason: "no".into(),
 },
 ErrorCode::BlockedByRobots),
 (
 WidowError::Dns {
 host: "x".into(),
 message: "y".into(),
 },
 ErrorCode::DnsError),
 (WidowError::Tls("x".into()), ErrorCode::TlsError),
 (WidowError::Timeout("x".into()), ErrorCode::Timeout),
 (WidowError::TooLarge { limit: 1 }, ErrorCode::TooLarge),
 (
 WidowError::UnsupportedContentType {
 content_type: "application/pdf".into(),
 processed: "text/html".into(),
 },
 ErrorCode::UnsupportedContentType),
 (
 WidowError::Http {
 status: Some(500),
 detail: "x".into(),
 },
 ErrorCode::HttpError),
 (
 WidowError::RenderUnavailable("x".into()),
 ErrorCode::RenderUnavailable),
 (
 WidowError::RenderFailed("x".into()),
 ErrorCode::RenderFailed),
 (
 WidowError::EmbeddingUnavailable("x".into()),
 ErrorCode::EmbeddingUnavailable),
 (
 WidowError::EmbeddingFailed("x".into()),
 ErrorCode::EmbeddingFailed),
 (WidowError::PdfEncrypted, ErrorCode::PdfEncrypted),
 (WidowError::PdfInvalid("x".into()), ErrorCode::PdfInvalid),
 (
 WidowError::Extraction("x".into()),
 ErrorCode::ExtractionFailed),
 (WidowError::NotFound("x".into()), ErrorCode::NotFound),
 (
 WidowError::LimitExceeded("x".into()),
 ErrorCode::LimitExceeded),
 (WidowError::Busy("x".into()), ErrorCode::Busy),
 (
 WidowError::SessionBusy {
 session_id: "id".into(),
 },
 ErrorCode::SessionBusy),
 (
 WidowError::SessionExpired {
 session_id: "id".into(),
 },
 ErrorCode::SessionExpired),
 (
 WidowError::SessionNotFound {
 session_id: "id".into(),
 },
 ErrorCode::SessionNotFound),
 (
 WidowError::ActionFailed {
 reason: "timeout".into(),
 message: "x".into(),
 },
 ErrorCode::ActionFailed),
 (
 WidowError::AuthHeaderRejected {
 name: "Host".into(),
 },
 ErrorCode::AuthHeaderRejected),
 (
 WidowError::AuthStoreUnavailable {
 detail: "no session bus".into(),
 },
 ErrorCode::AuthStoreUnavailable),
 (
 WidowError::AuthStoreLocked {
 detail: "locked".into(),
 },
 ErrorCode::AuthStoreLocked),
 (
 WidowError::EgressBlocked {
 host: "x".into(),
 reason: "host".into(),
 },
 ErrorCode::EgressBlocked),
 (WidowError::Internal("x".into()), ErrorCode::Internal),
 ];
 for (error, code) in cases {
 assert_eq!(error.code(), code, "{error}");
 // The wire form of the code must match the documented taxonomy.
 let wire = serde_json::to_string(&code).unwrap();
 assert_eq!(wire, format!("\"{}\"", code.as_str()));
 }
 }

 #[test]
 fn schema_invalid_reports_the_json_pointer() {
 let error = WidowError::SchemaInvalid {
 pointer: "/fields/price/transform".to_string(),
 message: "unknown transform".to_string(),
 };
 assert_eq!(error.code(), ErrorCode::SchemaInvalid);
 let tool = error.to_tool_error();
 assert_eq!(
 tool.detail.as_deref(),
 Some("pointer=/fields/price/transform")
);
 assert!(tool.message.contains("/fields/price/transform"));
 }

 #[test]
 fn tool_error_serializes_with_optional_detail() {
 let error = WidowError::BlockedPrivateAddress {
 address: "10.0.0.1".into(),
 range: "10.0.0.0/8".into(),
 };
 let value = serde_json::to_value(error.to_tool_error()).unwrap();
 assert_eq!(value["code"], "blocked_private_address");
 assert!(value["detail"].as_str().unwrap().contains("10.0.0.0/8"));
 }
}
