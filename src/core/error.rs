//! Error taxonomy shared by the fetch, extract, store and MCP layers.
//!
//! The codes are the MCP contract ([`ErrorCode`]); [`WidowError`] is the single
//! in-process error type so a new failure cannot be invented in one layer and
//! forgotten in another. `detail` carries machine-useful extras (observed
//! content type, blocking CIDR) without duplicating the message.

use serde::{Deserialize, Serialize};

use schemars::JsonSchema;

/// Stable MCP error codes.
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

    #[error("internal error: {0}")]
    Internal(String),
}

impl WidowError {
    pub fn code(&self) -> ErrorCode {
        match self {
            Self::InvalidUrl(_) => ErrorCode::InvalidUrl,
            Self::BlockedScheme { .. } => ErrorCode::BlockedScheme,
            Self::BlockedPrivateAddress { .. } => ErrorCode::BlockedPrivateAddress,
            Self::BlockedByRobots { .. } => ErrorCode::BlockedByRobots,
            Self::Dns { .. } => ErrorCode::DnsError,
            Self::Tls(_) => ErrorCode::TlsError,
            Self::Timeout(_) => ErrorCode::Timeout,
            Self::TooLarge { .. } => ErrorCode::TooLarge,
            Self::UnsupportedContentType { .. } => ErrorCode::UnsupportedContentType,
            Self::Http { .. } => ErrorCode::HttpError,
            Self::RenderUnavailable(_) => ErrorCode::RenderUnavailable,
            Self::RenderFailed(_) => ErrorCode::RenderFailed,
            Self::EmbeddingUnavailable(_) => ErrorCode::EmbeddingUnavailable,
            Self::EmbeddingFailed(_) => ErrorCode::EmbeddingFailed,
            Self::PdfEncrypted => ErrorCode::PdfEncrypted,
            Self::PdfInvalid(_) => ErrorCode::PdfInvalid,
            Self::Extraction(_) => ErrorCode::ExtractionFailed,
            Self::SchemaInvalid { .. } => ErrorCode::SchemaInvalid,
            Self::NotFound(_) => ErrorCode::NotFound,
            Self::LimitExceeded(_) => ErrorCode::LimitExceeded,
            Self::Busy(_) => ErrorCode::Busy,
            Self::SessionBusy { .. } => ErrorCode::SessionBusy,
            Self::SessionExpired { .. } => ErrorCode::SessionExpired,
            Self::SessionNotFound { .. } => ErrorCode::SessionNotFound,
            Self::ActionFailed { .. } => ErrorCode::ActionFailed,
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
            Self::UnsupportedContentType { content_type, .. } => {
                Some(format!("content_type={content_type}"))
            }
            Self::Http {
                status: Some(status),
                ..
            } => Some(format!("status={status}")),
            Self::TooLarge { limit } => Some(format!("limit_bytes={limit}")),
            Self::Dns { host, .. } => Some(format!("host={host}")),
            Self::SchemaInvalid { pointer, .. } => Some(format!("pointer={pointer}")),
            Self::SessionBusy { session_id }
            | Self::SessionExpired { session_id }
            | Self::SessionNotFound { session_id } => Some(format!("session_id={session_id}")),
            Self::ActionFailed { reason, .. } => Some(format!("reason={reason}")),
            _ => None,
        }
    }

    /// HTTP status the origin returned, when the failure carries one.
    pub fn http_status(&self) -> Option<u16> {
        match self {
            Self::Http { status, .. } => *status,
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
                ErrorCode::BlockedScheme,
            ),
            (
                WidowError::BlockedPrivateAddress {
                    address: "127.0.0.1".into(),
                    range: "127.0.0.0/8".into(),
                },
                ErrorCode::BlockedPrivateAddress,
            ),
            (
                WidowError::BlockedByRobots {
                    reason: "no".into(),
                },
                ErrorCode::BlockedByRobots,
            ),
            (
                WidowError::Dns {
                    host: "x".into(),
                    message: "y".into(),
                },
                ErrorCode::DnsError,
            ),
            (WidowError::Tls("x".into()), ErrorCode::TlsError),
            (WidowError::Timeout("x".into()), ErrorCode::Timeout),
            (WidowError::TooLarge { limit: 1 }, ErrorCode::TooLarge),
            (
                WidowError::UnsupportedContentType {
                    content_type: "application/pdf".into(),
                    processed: "text/html".into(),
                },
                ErrorCode::UnsupportedContentType,
            ),
            (
                WidowError::Http {
                    status: Some(500),
                    detail: "x".into(),
                },
                ErrorCode::HttpError,
            ),
            (
                WidowError::RenderUnavailable("x".into()),
                ErrorCode::RenderUnavailable,
            ),
            (
                WidowError::RenderFailed("x".into()),
                ErrorCode::RenderFailed,
            ),
            (
                WidowError::EmbeddingUnavailable("x".into()),
                ErrorCode::EmbeddingUnavailable,
            ),
            (
                WidowError::EmbeddingFailed("x".into()),
                ErrorCode::EmbeddingFailed,
            ),
            (WidowError::PdfEncrypted, ErrorCode::PdfEncrypted),
            (WidowError::PdfInvalid("x".into()), ErrorCode::PdfInvalid),
            (
                WidowError::Extraction("x".into()),
                ErrorCode::ExtractionFailed,
            ),
            (WidowError::NotFound("x".into()), ErrorCode::NotFound),
            (
                WidowError::LimitExceeded("x".into()),
                ErrorCode::LimitExceeded,
            ),
            (WidowError::Busy("x".into()), ErrorCode::Busy),
            (
                WidowError::SessionBusy {
                    session_id: "id".into(),
                },
                ErrorCode::SessionBusy,
            ),
            (
                WidowError::SessionExpired {
                    session_id: "id".into(),
                },
                ErrorCode::SessionExpired,
            ),
            (
                WidowError::SessionNotFound {
                    session_id: "id".into(),
                },
                ErrorCode::SessionNotFound,
            ),
            (
                WidowError::ActionFailed {
                    reason: "timeout".into(),
                    message: "x".into(),
                },
                ErrorCode::ActionFailed,
            ),
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
