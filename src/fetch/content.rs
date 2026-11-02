//! Content scope (IMP-GRANT-006 / ITEM-006): what we agree to turn into text.
//!
//! A type outside this closed set is refused with `unsupported_content_type`
//! before the body is read, so a PDF is never downloaded "just to check".

use crate::core::error::WidowError;

pub const PROCESSED_TYPES: &str =
    "text/html, application/xhtml+xml, text/plain, application/json, application/xml, text/xml";

/// Media types the tabular family adds on top of [`PROCESSED_TYPES`]
/// (IMP-DEC-033). Only `extract_rows` fetches with this wider scope; every page
/// path keeps [`classify`] and still refuses them.
pub const TABULAR_TYPES: &str = "text/csv, text/tab-separated-values";

/// The full tabular allowlist as it appears in an `unsupported_content_type`
/// message: the page list plus [`TABULAR_TYPES`], in one place.
pub fn tabular_processed_types() -> String {
    format!("{PROCESSED_TYPES}, {TABULAR_TYPES}")
}

/// The same scope plus the pointer shown when a PDF is refused. `fetch_page`
/// and `extract` keep refusing PDFs; `pdf_extract` is the only PDF path.
pub const PROCESSED_TYPES_PDF_POINTER: &str = "text/html, application/xhtml+xml, text/plain, application/json, application/xml, text/xml (PDFs: use pdf_extract)";

/// The `processed` hint for a refused type: a PDF names its own tool.
pub fn processed_hint(content_type: Option<&str>) -> &'static str {
    match content_type.map(base_media_type) {
        Some(media) if media == "application/pdf" => PROCESSED_TYPES_PDF_POINTER,
        _ => PROCESSED_TYPES,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaKind {
    Html,
    Xhtml,
    Text,
    Json,
    Xml,
    /// Tabular scope only: never returned by [`classify`].
    Csv,
    /// Tabular scope only: never returned by [`classify`].
    Tsv,
}

impl MediaKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Html => "text/html",
            Self::Xhtml => "application/xhtml+xml",
            Self::Text => "text/plain",
            Self::Json => "application/json",
            Self::Xml => "application/xml",
            Self::Csv => "text/csv",
            Self::Tsv => "text/tab-separated-values",
        }
    }

    /// Markup types go through the HTML parser; the others are delivered as text.
    pub fn is_markup(self) -> bool {
        matches!(self, Self::Html | Self::Xhtml)
    }
}

/// Media type without parameters, lowercased and trimmed.
pub fn base_media_type(content_type: &str) -> String {
    content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
}

/// `None` means "outside the agreed scope"; callers turn that into the typed
/// error with the observed value.
pub fn classify(content_type: Option<&str>) -> Option<MediaKind> {
    let raw = content_type?;
    let media = base_media_type(raw);
    match media.as_str() {
        "text/html" => Some(MediaKind::Html),
        "application/xhtml+xml" => Some(MediaKind::Xhtml),
        "text/plain" => Some(MediaKind::Text),
        "application/json" | "text/json" => Some(MediaKind::Json),
        "application/xml" | "text/xml" => Some(MediaKind::Xml),
        _ => {
            if media.ends_with("+json") {
                Some(MediaKind::Json)
            } else if media.ends_with("+xml") {
                Some(MediaKind::Xml)
            } else {
                None
            }
        }
    }
}

/// Typed refusal for a type outside the scope.
pub fn require(content_type: Option<&str>) -> Result<MediaKind, WidowError> {
    ContentScope::Page.require(content_type)
}

/// Which media allowlist one acquisition applies. `Page` is every page tool;
/// `Tabular` adds CSV/TSV for `extract_rows` and nothing else (IMP-DEC-033). The
/// scope never changes the SSRF, robots, host-gate or body-cap path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentScope {
    Page,
    Tabular,
}

impl ContentScope {
    pub fn classify(self, content_type: Option<&str>) -> Option<MediaKind> {
        match self {
            Self::Page => classify(content_type),
            Self::Tabular => classify_tabular(content_type),
        }
    }

    pub fn processed_types(self) -> String {
        match self {
            Self::Page => PROCESSED_TYPES.to_string(),
            Self::Tabular => tabular_processed_types(),
        }
    }

    /// The typed refusal for a type outside this scope. A PDF names its own
    /// tool (`pdf_extract`); the tabular scope names the tabular allowlist.
    pub fn require(self, content_type: Option<&str>) -> Result<MediaKind, WidowError> {
        self.classify(content_type)
            .ok_or_else(|| WidowError::UnsupportedContentType {
                content_type: content_type.unwrap_or("(none)").to_string(),
                processed: match self {
                    Self::Page => processed_hint(content_type).to_string(),
                    Self::Tabular => tabular_processed_types(),
                },
            })
    }
}

/// [`classify`] plus the two tabular media types. This is the only classifier
/// that admits CSV/TSV, and only the `extract_rows` acquisition uses it: an
/// `application/octet-stream` body is still `None`, so it is refused rather than
/// guessed from the bytes.
pub fn classify_tabular(content_type: Option<&str>) -> Option<MediaKind> {
    match classify(content_type) {
        Some(media) => Some(media),
        None => match base_media_type(content_type?).as_str() {
            "text/csv" => Some(MediaKind::Csv),
            "text/tab-separated-values" => Some(MediaKind::Tsv),
            _ => None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_the_agreed_scope() {
        for (header, expected) in [
            ("text/html", MediaKind::Html),
            ("text/html; charset=utf-8", MediaKind::Html),
            ("TEXT/HTML", MediaKind::Html),
            ("application/xhtml+xml", MediaKind::Xhtml),
            ("text/plain", MediaKind::Text),
            ("application/json", MediaKind::Json),
            ("application/ld+json", MediaKind::Json),
            ("application/xml", MediaKind::Xml),
            ("text/xml", MediaKind::Xml),
            ("application/rss+xml", MediaKind::Xml),
            ("application/atom+xml", MediaKind::Xml),
        ] {
            assert_eq!(classify(Some(header)), Some(expected), "{header}");
        }
    }

    #[test]
    fn refuses_types_outside_the_scope() {
        for header in [
            "application/pdf",
            "image/png",
            "application/octet-stream",
            "application/zip",
            "text/csv",
            "video/mp4",
            "application/msword",
        ] {
            assert_eq!(classify(Some(header)), None, "{header}");
        }
        assert_eq!(classify(None), None);
        let error = require(Some("application/pdf")).unwrap_err();
        let tool = error.to_tool_error();
        assert_eq!(tool.code.as_str(), "unsupported_content_type");
        assert_eq!(tool.detail.as_deref(), Some("content_type=application/pdf"));
    }

    #[test]
    fn base_type_strips_parameters() {
        assert_eq!(base_media_type(" Text/HTML ; charset=UTF-8"), "text/html");
        assert_eq!(base_media_type("application/json"), "application/json");
    }

    #[test]
    fn the_tabular_classifier_adds_only_csv_and_tsv() {
        for (header, expected) in [
            ("text/csv", MediaKind::Csv),
            ("text/csv; charset=utf-8", MediaKind::Csv),
            ("TEXT/CSV", MediaKind::Csv),
            ("text/tab-separated-values", MediaKind::Tsv),
            ("text/html", MediaKind::Html),
            ("application/json", MediaKind::Json),
            ("text/plain", MediaKind::Text),
        ] {
            assert_eq!(classify_tabular(Some(header)), Some(expected), "{header}");
        }
        // `application/octet-stream` is never guessed from the body, and the
        // page classifier still refuses both tabular types.
        for header in ["application/octet-stream", "application/pdf"] {
            assert_eq!(classify_tabular(Some(header)), None, "{header}");
        }
        assert_eq!(classify_tabular(None), None);
        assert_eq!(classify(Some("text/csv")), None);
        assert_eq!(classify(Some("text/tab-separated-values")), None);
        // The wider message lists both allowlists.
        let processed = tabular_processed_types();
        assert!(processed.contains("text/csv"));
        assert!(processed.contains("text/tab-separated-values"));
        assert!(processed.starts_with(PROCESSED_TYPES));
    }
}
