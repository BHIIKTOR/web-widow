//! PDF text-layer extraction (PDFium 8125).
//!
//! Pure parse: bytes + limits in, a bounded document model or a typed error out.
//! Fetching (SSRF, robots, politeness, the PDF-specific content scope) stays in
//! [`crate::pipeline`]; engine loading and the process-wide parse mutex live in
//! [`engine`]. The parser runs on a blocking thread behind `catch_unwind`, so a
//! Rust panic becomes `pdf_invalid`. A **native** PDFium crash is not catchable
//! (): the pinned library, the caps and process supervision are the
//! controls, and no OCR/images/forms/JavaScript are used.
//!
//! Version pinning: the bindings are `pdfium_8125`; the deployment records the
//! library file's version and SHA-256 (see the C2 card).

pub mod engine;

use pdfium_render::prelude::{
 PdfDocument, PdfDocumentMetadataTagType, Pdfium, PdfiumError, PdfiumInternalError,
};
use serde::{Deserialize, Serialize};

use crate::core::error::{Result, WidowError};

/// The accept header for a PDF request; the body is the scope decision.
pub const ACCEPT_PDF: &str = "application/pdf,*/*;q=0.1";

/// Limits for one PDF. `max_bytes` bounds the fetched body; `max_pages` and
/// `max_chars` bound the delivered markdown at a page boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PdfLimits {
 pub max_bytes: u64,
 pub max_pages: usize,
 pub max_chars: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PdfOutcome {
 /// Every declared page parsed and at least one page has text.
 Ok,
 /// Every page parsed and none has text (a successful call, no OCR).
 NoTextLayer,
 /// Extraction stopped before `declared_pages` (a page error or a cap).
 Partial,
}

impl PdfOutcome {
 pub fn as_str(self) -> &'static str {
 match self {
 Self::Ok => "ok",
 Self::NoTextLayer => "no_text_layer",
 Self::Partial => "partial",
 }
 }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PdfMetadata {
 #[serde(skip_serializing_if = "Option::is_none")]
 pub title: Option<String>,
 #[serde(skip_serializing_if = "Option::is_none")]
 pub author: Option<String>,
 #[serde(skip_serializing_if = "Option::is_none")]
 pub subject: Option<String>,
 #[serde(skip_serializing_if = "Option::is_none")]
 pub creator: Option<String>,
 #[serde(skip_serializing_if = "Option::is_none")]
 pub producer: Option<String>,
 #[serde(skip_serializing_if = "Option::is_none")]
 pub creation_date: Option<String>,
 #[serde(skip_serializing_if = "Option::is_none")]
 pub modification_date: Option<String>,
}

/// A parsed PDF. `markdown` is the delivered text (one `## Page N` section per
/// page in range); `pages` is the number of sections it contains.
#[derive(Debug, Clone)]
pub struct PdfExtracted {
 /// PDFium's page count (`pages().len()`), when the document opened.
 pub declared_pages: Option<usize>,
 pub pages: usize,
 pub pages_with_text: usize,
 pub empty_pages: Vec<usize>,
 pub empty_pages_truncated: bool,
 pub chars: usize,
 pub markdown: String,
 pub metadata: PdfMetadata,
 pub outcome: PdfOutcome,
 pub truncated: bool,
 pub limit_hit: Option<&'static str>,
}

/// `pdf_extract` tool input (). The URL is fetched fresh (no
/// conditional GET); `fetch_page`/`extract` keep refusing PDFs.
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PdfExtractInput {
 /// Absolute http(s) URL of the PDF.
 pub url: String,
 /// Pages to extract (1..=`[pdf].max_pages`, default the server ceiling).
 #[serde(default)]
 pub max_pages: Option<usize>,
 /// Markdown character ceiling (1..=`[pdf].max_chars`).
 #[serde(default)]
 pub max_chars: Option<usize>,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct PdfExtractPayload {
 pub url: String,
 pub final_url: String,
 pub status: u16,
 pub content_type: String,
 /// Pages represented in `markdown` (the cap-clamped range).
 pub pages: usize,
 /// PDFium's `pages().len()`.
 pub declared_pages: Option<usize>,
 pub pages_with_text: usize,
 /// Pages in range with no text (bounded to 20).
 pub empty_pages: Vec<usize>,
 pub empty_pages_truncated: bool,
 pub chars: usize,
 pub markdown: String,
 pub metadata: PdfMetadata,
 pub outcome: PdfOutcome,
 pub truncated: bool,
 #[serde(skip_serializing_if = "Option::is_none")]
 pub limit_hit: Option<String>,
 pub fetched_at: String,
}

/// Parse a PDF body with an already-loaded engine. `bytes` is the fetched
/// response; the caller enforces the body cap, the PDF content scope and
/// [`engine::ensure`] first. Serialized by the process-wide parse mutex.
pub fn extract(bytes: &[u8], limits: PdfLimits) -> Result<PdfExtracted> {
 let _guard = engine::lock_parse();
 let pdfium = Pdfium::default();
 let document = pdfium
.load_pdf_from_byte_slice(bytes, None)
.map_err(map_load_error)?;
 let metadata = read_metadata(&document);
 let declared = document.pages().len().max(0) as usize;
 let range = declared.min(limits.max_pages);
 let mut truncated = range < declared;
 let mut limit_hit = (range < declared).then_some("max_pages");

 let mut markdown = String::new();
 let mut pages = 0usize;
 let mut pages_with_text = 0usize;
 let mut empty_pages = Vec::new();
 let mut empty_pages_truncated = false;
 let mut chars = 0usize;
 let mut stopped_early = truncated;

 for index in 0..range {
 let page = match document.pages().get(index as i32) {
 Ok(page) => page,
 Err(_) => {
 stopped_early = true;
 break;
 }
 };
 let text = match page.text() {
 Ok(text) => text.all(),
 Err(_) => {
 stopped_early = true;
 break;
 }
 };
 let page_number = index + 1;
 let text = normalize_text(&text);
 let section = if text.is_empty() {
 format!("## Page {page_number}\n\n")
 } else {
 format!("## Page {page_number}\n\n{text}\n\n")
 };
 let section_chars = section.chars().count();
 if chars + section_chars > limits.max_chars {
 if pages == 0 {
 // A single oversized first page: keep its heading and the
 // prefix that fits, so the delivered markdown is bounded.
 let heading = format!("## Page {page_number}\n\n");
 let budget = limits.max_chars.saturating_sub(heading.chars().count());
 let mut section = heading;
 section.extend(text.chars().take(budget));
 let section: String = section.chars().take(limits.max_chars).collect();
 markdown.push_str(&section);
 pages = 1;
 if text.is_empty() {
 if empty_pages.len() < MAX_EMPTY_PAGES {
 empty_pages.push(page_number);
 } else {
 empty_pages_truncated = true;
 }
 } else {
 pages_with_text = 1;
 }
 }
 truncated = true;
 limit_hit = Some("max_chars");
 stopped_early = true;
 break;
 }
 if text.is_empty() {
 if empty_pages.len() < MAX_EMPTY_PAGES {
 empty_pages.push(page_number);
 } else {
 empty_pages_truncated = true;
 }
 } else {
 pages_with_text += 1;
 }
 markdown.push_str(&section);
 chars += section_chars;
 pages += 1;
 }

 let outcome = if stopped_early {
 PdfOutcome::Partial
 } else if pages_with_text == 0 {
 PdfOutcome::NoTextLayer
 } else {
 PdfOutcome::Ok
 };
 let markdown = markdown.trim_end().to_string();
 let chars = markdown.chars().count();
 Ok(PdfExtracted {
 declared_pages: Some(declared),
 pages,
 pages_with_text,
 empty_pages,
 empty_pages_truncated,
 chars,
 markdown,
 metadata,
 outcome,
 truncated,
 limit_hit,
 })
}

/// Bounded `empty_pages` list (the payload contract).
const MAX_EMPTY_PAGES: usize = 20;

/// Trim and collapse page text: trailing spaces removed, at most one blank line
/// between blocks, no leading/trailing blanks.
fn normalize_text(text: &str) -> String {
 let mut lines: Vec<&str> = Vec::new();
 for line in text.lines() {
 let line = line.trim();
 if line.is_empty() {
 if matches!(lines.last(), Some(last) if last.is_empty()) {
 continue;
 }
 lines.push("");
 } else {
 lines.push(line);
 }
 }
 while matches!(lines.last(), Some(last) if last.is_empty()) {
 lines.pop();
 }
 lines.join("\n")
}

/// PDFium's document metadata. An absent tag is `None`; an empty value is not
/// invented into text.
fn read_metadata(document: &PdfDocument<'_>) -> PdfMetadata {
 let read = |tag: PdfDocumentMetadataTagType| {
 document
.metadata()
.get(tag)
.map(|value| value.value().trim().to_string())
.filter(|value| !value.is_empty())
 };
 PdfMetadata {
 title: read(PdfDocumentMetadataTagType::Title),
 author: read(PdfDocumentMetadataTagType::Author),
 subject: read(PdfDocumentMetadataTagType::Subject),
 creator: read(PdfDocumentMetadataTagType::Creator),
 producer: read(PdfDocumentMetadataTagType::Producer),
 creation_date: read(PdfDocumentMetadataTagType::CreationDate),
 modification_date: read(PdfDocumentMetadataTagType::ModificationDate),
 }
}

fn map_load_error(error: PdfiumError) -> WidowError {
 if let PdfiumError::PdfiumLibraryInternalError(inner) = &error
 && matches!(
 inner,
 PdfiumInternalError::PasswordError | PdfiumInternalError::SecurityError
)
 {
 return WidowError::PdfEncrypted;
 }
 WidowError::PdfInvalid(bounded(format!("cannot load PDF: {error}")))
}

fn bounded(message: String) -> String {
 const MAX: usize = 300;
 if message.chars().count() <= MAX {
 return message;
 }
 let mut out: String = message.chars().take(MAX).collect();
 out.push('…');
 out
}

#[cfg(test)]
mod tests {
 use super::*;
 use std::path::PathBuf;

 fn limits() -> PdfLimits {
 PdfLimits {
 max_bytes: 64 * 1024 * 1024,
 max_pages: 200,
 max_chars: 1_000_000,
 }
 }

 fn fixture(name: &str) -> Vec<u8> {
 std::fs::read(
 std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
.join("tests/fixtures/m06/pdf")
.join(name))
.expect("fixture")
 }

 /// The engine is process-wide; without the provisioned library the parse
 /// tests skip with an explicit marker (the Chrome precedent).
 fn engine_ready(test: &str) -> bool {
 let Some(path) = std::env::var_os(engine::ENV_PDFIUM_PATH) else {
 eprintln!(
 "SKIP {test}: no PDFium library (set {} to the provisioned libpdfium.so)",
 engine::ENV_PDFIUM_PATH
);
 return false;
 };
 let spec = engine::EngineSpec {
 pdfium_path: Some(PathBuf::from(path)),
 discovery: false,
 env_path: None,
 exe_dir: None,
 data_dir: None,
 };
 if let Err(error) = engine::ensure(&spec) {
 eprintln!("SKIP {test}: {error}");
 return false;
 }
 true
 }

 #[test]
 fn a_two_page_fixture_extracts_markers_metadata_and_page_headings() {
 if !engine_ready("a_two_page_fixture_extracts_markers_metadata_and_page_headings") {
 return;
 }
 let parsed = extract(&fixture("simple.pdf"), limits()).expect("parses");
 assert_eq!(parsed.declared_pages, Some(2));
 assert_eq!(parsed.pages, 2);
 assert_eq!(parsed.outcome, PdfOutcome::Ok);
 assert!(parsed.markdown.contains("## Page 1"));
 assert!(parsed.markdown.contains("## Page 2"));
 assert!(parsed.markdown.contains("PAGE_ONE_MARKER"));
 assert!(parsed.markdown.contains("PAGE_TWO_MARKER"));
 assert_eq!(
 parsed.metadata.title.as_deref(),
 Some(" C2 simple fixture")
);
 assert_eq!(parsed.metadata.author.as_deref(), Some("Fixture Author"));
 assert_eq!(
 parsed.metadata.subject.as_deref(),
 Some("Two-page text-layer PDF")
);
 assert_eq!(parsed.metadata.creator.as_deref(), Some("make_fixtures.py"));
 assert_eq!(parsed.metadata.producer.as_deref(), Some("web-widow tests"));
 assert_eq!(
 parsed.metadata.creation_date.as_deref(),
 Some("D:20260101120000Z")
);
 assert_eq!(
 parsed.metadata.modification_date.as_deref(),
 Some("D:20260102120000Z")
);
 assert!(parsed.empty_pages.is_empty());
 assert!(!parsed.truncated);
 assert_eq!(parsed.chars, parsed.markdown.chars().count());
 }

 #[test]
 fn a_scanned_page_is_no_text_layer_not_fabricated_text() {
 if !engine_ready("a_scanned_page_is_no_text_layer_not_fabricated_text") {
 return;
 }
 let parsed = extract(&fixture("scanned.pdf"), limits()).expect("parses");
 assert_eq!(parsed.outcome, PdfOutcome::NoTextLayer);
 assert_eq!(parsed.pages, 1);
 assert_eq!(parsed.pages_with_text, 0);
 assert_eq!(parsed.empty_pages, vec![1]);
 assert!(parsed.markdown.contains("## Page 1"));
 assert!(!parsed.truncated);
 }

 #[test]
 fn encryption_maps_empty_user_password_to_ok_and_a_real_password_to_encrypted() {
 if !engine_ready(
 "encryption_maps_empty_user_password_to_ok_and_a_real_password_to_encrypted") {
 return;
 }
 let parsed = extract(&fixture("encrypted-empty-password.pdf"), limits()).expect("opens");
 assert_eq!(parsed.outcome, PdfOutcome::Ok);
 assert!(parsed.markdown.contains("PAGE_ONE_MARKER"));

 let error = extract(&fixture("encrypted-user-password.pdf"), limits()).unwrap_err();
 assert_eq!(error.code(), crate::core::error::ErrorCode::PdfEncrypted);
 }

 #[test]
 fn malformed_is_pdf_invalid() {
 if !engine_ready("malformed_is_pdf_invalid") {
 return;
 }
 let error = extract(&fixture("malformed.pdf"), limits()).unwrap_err();
 assert_eq!(error.code(), crate::core::error::ErrorCode::PdfInvalid);
 }

 #[test]
 fn max_pages_stops_at_a_page_boundary_and_reports_partial() {
 if !engine_ready("max_pages_stops_at_a_page_boundary_and_reports_partial") {
 return;
 }
 let parsed = extract(
 &fixture("three-pages.pdf"),
 PdfLimits {
 max_pages: 2,
..limits()
 })
.expect("parses");
 assert_eq!(parsed.declared_pages, Some(3));
 assert_eq!(parsed.pages, 2);
 assert_eq!(parsed.outcome, PdfOutcome::Partial);
 assert!(parsed.truncated);
 assert_eq!(parsed.limit_hit, Some("max_pages"));
 assert!(parsed.markdown.contains("PAGE_TWO_MARKER"));
 assert!(!parsed.markdown.contains("PAGE_THREE_MARKER"));
 }

 #[test]
 fn max_chars_stops_at_a_page_boundary() {
 if !engine_ready("max_chars_stops_at_a_page_boundary") {
 return;
 }
 let full = extract(&fixture("simple.pdf"), limits()).expect("parses");
 let first_page = full.markdown.find("## Page 2").expect("two pages");
 let parsed = extract(
 &fixture("simple.pdf"),
 PdfLimits {
 max_chars: first_page,
..limits()
 })
.expect("parses");
 assert_eq!(parsed.pages, 1);
 assert_eq!(parsed.outcome, PdfOutcome::Partial);
 assert!(parsed.truncated);
 assert_eq!(parsed.limit_hit, Some("max_chars"));
 assert!(!parsed.markdown.contains("PAGE_TWO_MARKER"));
 assert!(parsed.chars <= first_page);
 }

 #[test]
 fn text_normalisation_collapses_blank_runs() {
 assert_eq!(normalize_text(" a \n\n\n\n b\n\n"), "a\n\nb");
 assert_eq!(normalize_text("\n\n \n"), "");
 }
}
