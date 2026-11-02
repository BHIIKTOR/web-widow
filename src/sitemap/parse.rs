//! Streaming sitemap parsers: XML (`<urlset>` / `<sitemapindex>`) and the plain
//! text format.
//!
//! Bounded by construction: the URL and child caps stop the reader early and
//! set `truncated`/`limit_hit`. Malformed XML is a typed `extraction_failed`
//! with the line/column of the parser error; no partial model escapes. A
//! `DOCTYPE` is skipped and never resolved, and only the five predefined XML
//! entities are accepted, so no entity expansion or external access is possible.

use quick_xml::Reader;
use quick_xml::escape::unescape;
use quick_xml::events::{BytesRef, Event};

use super::{
    ChildSitemap, ParsedSitemap, SitemapKind, SitemapLimits, SitemapUrl, normalize_lastmod,
    valid_loc,
};
use crate::core::error::{Result, WidowError};

/// Dispatch on the first non-whitespace bytes: a leading `<` is XML, anything
/// else is the one-URL-per-line text format.
pub fn parse_text(text: &str, limits: SitemapLimits) -> Result<ParsedSitemap> {
    let trimmed = text.trim_start_matches('\u{feff}').trim_start();
    if trimmed.is_empty() {
        return Err(WidowError::Extraction(
            "empty sitemap document (no XML root and no URL lines)".to_string(),
        ));
    }
    if trimmed.starts_with('<') {
        parse_xml(trimmed, limits)
    } else {
        Ok(parse_lines(trimmed, limits))
    }
}

fn parse_lines(text: &str, limits: SitemapLimits) -> ParsedSitemap {
    let mut out = ParsedSitemap {
        kind: Some(SitemapKind::Text),
        ..ParsedSitemap::default()
    };
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match valid_loc(line) {
            Some(loc) => {
                out.urls.push(SitemapUrl { loc, lastmod: None });
                if out.urls.len() >= limits.max_urls {
                    out.truncated = true;
                    out.limit_hit = Some("max_urls");
                    break;
                }
            }
            None => out.skipped_scheme += 1,
        }
    }
    out
}

/// What the reader is collecting inside the current `<url>`/`<sitemap>` entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Capture {
    Loc,
    Lastmod,
}

#[derive(Debug)]
struct Builder<'a> {
    limits: &'a SitemapLimits,
    out: ParsedSitemap,
    kind: Option<SitemapKind>,
    in_entry: bool,
    entry_depth: usize,
    capture: Option<Capture>,
    capture_depth: usize,
    loc: Option<String>,
    lastmod: Option<String>,
    /// True when a `<lastmod>` element was present, even if it was empty.
    lastmod_seen: bool,
    stop: bool,
}

impl<'a> Builder<'a> {
    fn new(limits: &'a SitemapLimits) -> Self {
        Self {
            limits,
            out: ParsedSitemap::default(),
            kind: None,
            in_entry: false,
            entry_depth: 0,
            capture: None,
            capture_depth: 0,
            loc: None,
            lastmod: None,
            lastmod_seen: false,
            stop: false,
        }
    }

    fn start(&mut self, local: &str, depth: usize) -> Result<()> {
        match local {
            // Namespaces are ignored: 0.9 and the legacy 0.8 prefixes both
            // arrive here as the local name.
            "urlset" => {
                if self.kind.is_none() {
                    self.kind = Some(SitemapKind::Urlset);
                }
            }
            "sitemapindex" => {
                if self.kind.is_none() {
                    self.kind = Some(SitemapKind::Index);
                }
            }
            "url" if self.kind == Some(SitemapKind::Urlset) => {
                self.in_entry = true;
                self.entry_depth = depth;
                self.loc = None;
                self.lastmod = None;
                self.lastmod_seen = false;
            }
            "sitemap" if self.kind == Some(SitemapKind::Index) => {
                self.in_entry = true;
                self.entry_depth = depth;
                self.loc = None;
                self.lastmod = None;
                self.lastmod_seen = false;
            }
            "loc" if self.in_entry && depth == self.entry_depth + 1 => {
                self.capture = Some(Capture::Loc);
                self.capture_depth = depth;
            }
            "lastmod" if self.in_entry && depth == self.entry_depth + 1 => {
                self.capture = Some(Capture::Lastmod);
                self.capture_depth = depth;
                self.lastmod_seen = true;
            }
            _ => {}
        }
        Ok(())
    }

    fn text(&mut self, value: &str) -> Result<()> {
        let Some(capture) = self.capture else {
            return Ok(());
        };
        // Text events can still carry escaped references depending on the
        // reader mode; decode only the predefined set.
        let decoded = unescape(value).map_err(|error| {
            WidowError::Extraction(format!(
                "unknown entity inside the sitemap text: {error} (DTD entities are never expanded)"
            ))
        })?;
        self.append(capture, decoded.as_ref());
        Ok(())
    }

    fn reference(&mut self, reference: BytesRef<'_>) -> Result<()> {
        let Some(capture) = self.capture else {
            return Ok(());
        };
        let text = reference.clone().into_inner();
        let resolved = if text.starts_with('#') {
            match reference.resolve_char_ref() {
                Ok(Some(character)) => character.to_string(),
                Ok(None) => {
                    return Err(WidowError::Extraction(format!(
                        "invalid character reference `&{text};`"
                    )));
                }
                Err(error) => {
                    return Err(WidowError::Extraction(format!(
                        "invalid character reference `&{text};`: {error}"
                    )));
                }
            }
        } else {
            match text.as_ref() {
                "amp" => "&".to_string(),
                "lt" => "<".to_string(),
                "gt" => ">".to_string(),
                "apos" => "'".to_string(),
                "quot" => "\"".to_string(),
                other => {
                    return Err(WidowError::Extraction(format!(
                        "unknown entity `&{other};` (DTD entities are never expanded)"
                    )));
                }
            }
        };
        self.append(capture, &resolved);
        Ok(())
    }

    fn append(&mut self, capture: Capture, value: &str) {
        let slot = match capture {
            Capture::Loc => &mut self.loc,
            Capture::Lastmod => &mut self.lastmod,
        };
        match slot {
            Some(existing) => existing.push_str(value),
            None => *slot = Some(value.to_string()),
        }
    }

    fn end(&mut self, local: &str, depth: usize) -> Result<()> {
        if let Some(capture) = self.capture
            && depth == self.capture_depth
            && matches!(
                (capture, local),
                (Capture::Loc, "loc") | (Capture::Lastmod, "lastmod")
            )
        {
            self.capture = None;
        }
        let entry = match self.kind {
            Some(SitemapKind::Urlset) => "url",
            Some(SitemapKind::Index) => "sitemap",
            _ => "",
        };
        if local == entry && self.in_entry && depth == self.entry_depth {
            self.finish_entry();
        }
        Ok(())
    }

    fn finish_entry(&mut self) {
        self.in_entry = false;
        self.capture = None;
        let loc = self.loc.take();
        let lastmod = self.lastmod.take();

        let Some(loc) = loc.as_deref().and_then(valid_loc) else {
            self.out.skipped_scheme += 1;
            return;
        };
        let lastmod = match lastmod {
            Some(value) => match normalize_lastmod(&value) {
                Some(normalized) => Some(normalized),
                None => {
                    self.out.invalid_lastmod += 1;
                    None
                }
            },
            None if self.lastmod_seen => {
                self.out.invalid_lastmod += 1;
                None
            }
            None => None,
        };

        match self.kind {
            Some(SitemapKind::Index) => {
                self.out.children.push(ChildSitemap { loc });
                if self.out.children.len() >= self.limits.max_children {
                    self.out.truncated = true;
                    self.out.limit_hit = Some("max_children");
                    self.stop = true;
                }
            }
            _ => {
                self.out.urls.push(SitemapUrl { loc, lastmod });
                if self.out.urls.len() >= self.limits.max_urls {
                    self.out.truncated = true;
                    self.out.limit_hit = Some("max_urls");
                    self.stop = true;
                }
            }
        }
    }
}

fn parse_xml(text: &str, limits: SitemapLimits) -> Result<ParsedSitemap> {
    let mut reader = Reader::from_str(text);
    reader.config_mut().trim_text(true);
    let mut builder = Builder::new(&limits);
    let mut depth = 0usize;
    let mut root_seen = false;

    loop {
        let event = match reader.read_event() {
            Ok(event) => event,
            Err(error) => return Err(parser_error(text, reader.error_position(), &error)),
        };
        match event {
            Event::Start(element) => {
                let local = element.local_name().into_inner().to_string();
                if !root_seen {
                    if local != "urlset" && local != "sitemapindex" {
                        return Err(WidowError::Extraction(format!(
                            "root element `<{local}>` is not a sitemap document (urlset or sitemapindex)"
                        )));
                    }
                    root_seen = true;
                }
                depth += 1;
                builder.start(&local, depth)?;
            }
            Event::End(element) => {
                let local = element.local_name().into_inner().to_string();
                builder.end(&local, depth)?;
                depth = depth.saturating_sub(1);
                if builder.stop {
                    break;
                }
            }
            Event::Empty(element) => {
                let local = element.local_name().into_inner().to_string();
                if !root_seen {
                    if local != "urlset" && local != "sitemapindex" {
                        return Err(WidowError::Extraction(format!(
                            "root element `<{local}>` is not a sitemap document (urlset or sitemapindex)"
                        )));
                    }
                    root_seen = true;
                }
                depth += 1;
                builder.start(&local, depth)?;
                builder.end(&local, depth)?;
                depth = depth.saturating_sub(1);
                if builder.stop {
                    break;
                }
            }
            Event::Text(value) => {
                builder.text(value.into_inner().as_ref())?;
            }
            Event::CData(value) => {
                if let Some(capture) = builder.capture {
                    builder.append(capture, value.into_inner().as_ref());
                }
            }
            Event::GeneralRef(reference) => builder.reference(reference)?,
            Event::Eof => break,
            // Decl, DocType, Comment, PI: skipped, never resolved.
            _ => {}
        }
    }

    if !root_seen {
        return Err(WidowError::Extraction(
            "document has no root element (not a sitemap)".to_string(),
        ));
    }
    if builder.in_entry {
        return Err(WidowError::Extraction(format!(
            "document ended inside an open `<{}>` entry",
            match builder.kind {
                Some(SitemapKind::Index) => "sitemap",
                _ => "url",
            }
        )));
    }
    builder.out.kind = builder.kind;
    Ok(builder.out)
}

/// `extraction_failed` with the parser line/column derived from its byte offset.
fn parser_error(text: &str, offset: u64, error: &quick_xml::Error) -> WidowError {
    let offset = (offset as usize).min(text.len());
    let consumed = &text.as_bytes()[..offset];
    let line = consumed.iter().filter(|byte| **byte == b'\n').count() + 1;
    let line_start = consumed
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map(|index| index + 1)
        .unwrap_or(0);
    // The offset points at the start of markup, so this slice is on a char
    // boundary; counting chars (not bytes) keeps the column meaningful.
    let column = text[line_start..offset].chars().count() + 1;
    WidowError::Extraction(format!(
        "malformed sitemap XML at line {line}, column {column}: {error}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIMITS: SitemapLimits = SitemapLimits {
        max_bytes: 1024 * 1024,
        max_urls: 10,
        max_children: 2,
    };

    fn parse(body: &str) -> Result<ParsedSitemap> {
        parse_text(body, LIMITS)
    }

    #[test]
    fn urlset_parses_loc_lastmod_and_skips_non_http_entries() {
        let parsed = parse(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">
  <url><loc>https://example.com/a</loc><lastmod>2026-01-02</lastmod></url>
  <url><loc>  https://example.com/b  </loc></url>
  <url><loc>ftp://example.com/c</loc></url>
  <url><loc>mailto:someone@example.com</loc></url>
</urlset>"#,
        )
        .unwrap();
        assert_eq!(parsed.kind, Some(SitemapKind::Urlset));
        assert_eq!(parsed.urls.len(), 2);
        assert_eq!(parsed.urls[0].loc, "https://example.com/a");
        assert_eq!(parsed.urls[0].lastmod.as_deref(), Some("2026-01-02"));
        assert_eq!(parsed.urls[1].loc, "https://example.com/b");
        assert_eq!(parsed.skipped_scheme, 2);
        assert!(!parsed.truncated);
    }

    #[test]
    fn legacy_08_and_prefixed_namespaces_parse_by_local_name() {
        for body in [
            r#"<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.8"><url><loc>https://example.com/a</loc></url></urlset>"#,
            r#"<sm:urlset xmlns:sm="http://www.sitemaps.org/schemas/sitemap/0.9"><sm:url><sm:loc>https://example.com/a</sm:loc></sm:url></sm:urlset>"#,
        ] {
            let parsed = parse(body).unwrap();
            assert_eq!(parsed.kind, Some(SitemapKind::Urlset));
            assert_eq!(parsed.urls.len(), 1, "{body}");
        }
    }

    #[test]
    fn cdata_and_escaped_entities_decode_and_unknown_entities_fail() {
        let parsed = parse(
            r#"<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">
  <url><loc><![CDATA[https://example.com/a?x=1&y=2]]></loc></url>
  <url><loc>https://example.com/b?x=1&amp;y=2</loc></url>
  <url><loc>https://example.com/c?a=&#38;&amp;</loc></url>
</urlset>"#,
        )
        .unwrap();
        assert_eq!(parsed.urls[0].loc, "https://example.com/a?x=1&y=2");
        assert_eq!(parsed.urls[1].loc, "https://example.com/b?x=1&y=2");
        assert_eq!(parsed.urls[2].loc, "https://example.com/c?a=&&");

        let error = parse(r#"<urlset><url><loc>https://example.com/&custom;</loc></url></urlset>"#)
            .unwrap_err();
        assert_eq!(
            error.code(),
            crate::core::error::ErrorCode::ExtractionFailed
        );
    }

    #[test]
    fn indexing_elements_inside_url_do_not_steal_the_entry_loc() {
        let parsed = parse(
            r#"<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9" xmlns:image="http://www.google.com/schemas/sitemap-image/1.1">
  <url>
    <loc>https://example.com/page</loc>
    <image:image><image:loc>https://cdn.example.com/p.png</image:loc></image:image>
  </url>
</urlset>"#,
        )
        .unwrap();
        assert_eq!(parsed.urls.len(), 1);
        assert_eq!(parsed.urls[0].loc, "https://example.com/page");
    }

    #[test]
    fn index_parses_children_and_reports_the_child_cap() {
        let parsed = parse(
            r#"<sitemapindex xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">
  <sitemap><loc>https://example.com/one.xml</loc><lastmod>2026-01-02</lastmod></sitemap>
  <sitemap><loc>https://example.com/two.xml</loc></sitemap>
  <sitemap><loc>https://example.com/three.xml</loc></sitemap>
</sitemapindex>"#,
        )
        .unwrap();
        assert_eq!(parsed.kind, Some(SitemapKind::Index));
        assert!(parsed.urls.is_empty());
        assert_eq!(parsed.children.len(), 2);
        assert_eq!(parsed.children[1].loc, "https://example.com/two.xml");
        assert!(parsed.truncated);
        assert_eq!(parsed.limit_hit, Some("max_children"));
    }

    #[test]
    fn the_url_cap_stops_the_reader_and_marks_truncation() {
        let mut body = String::from("<urlset>");
        for index in 0..25 {
            body.push_str(&format!(
                "<url><loc>https://example.com/{index}</loc></url>"
            ));
        }
        body.push_str("</urlset>");
        let parsed = parse(&body).unwrap();
        assert_eq!(parsed.urls.len(), LIMITS.max_urls);
        assert!(parsed.truncated);
        assert_eq!(parsed.limit_hit, Some("max_urls"));
    }

    #[test]
    fn invalid_lastmod_is_null_and_counted() {
        let parsed = parse(
            r#"<urlset>
  <url><loc>https://example.com/a</loc><lastmod>not-a-date</lastmod></url>
  <url><loc>https://example.com/b</loc><lastmod>2026-01-02T03:04:05Z</lastmod></url>
</urlset>"#,
        )
        .unwrap();
        assert_eq!(parsed.invalid_lastmod, 1);
        assert_eq!(parsed.urls[0].lastmod, None);
        assert_eq!(
            parsed.urls[1].lastmod.as_deref(),
            Some("2026-01-02T03:04:05Z")
        );
    }

    #[test]
    fn plain_text_is_one_url_per_line() {
        let parsed =
            parse("https://example.com/a\n\n  https://example.com/b  \nftp://example.com/c\n")
                .unwrap();
        assert_eq!(parsed.kind, Some(SitemapKind::Text));
        assert_eq!(parsed.urls.len(), 2);
        assert_eq!(parsed.urls[1].loc, "https://example.com/b");
        assert_eq!(parsed.skipped_scheme, 1);
    }

    #[test]
    fn malformed_xml_is_extraction_failed_with_line_and_column() {
        let error =
            parse("<urlset>\n  <url><loc>https://example.com/a</loc>\n</urlset>").unwrap_err();
        assert_eq!(
            error.code(),
            crate::core::error::ErrorCode::ExtractionFailed
        );
        let message = error.to_string();
        assert!(message.contains("line 3"), "{message}");
        assert!(message.contains("column"), "{message}");

        let error = parse("<rss><channel></channel></rss>").unwrap_err();
        assert!(error.to_string().contains("not a sitemap"));
    }

    #[test]
    fn a_doctype_is_skipped_and_never_expanded() {
        let parsed = parse(
            r#"<?xml version="1.0"?>
<!DOCTYPE urlset [ <!ENTITY ext SYSTEM "file:///etc/passwd"> ]>
<urlset><url><loc>https://example.com/a</loc></url></urlset>"#,
        )
        .unwrap();
        assert_eq!(parsed.urls.len(), 1);
        assert_eq!(parsed.urls[0].loc, "https://example.com/a");
    }

    #[test]
    fn an_empty_document_is_not_a_sitemap() {
        let error = parse("   ").unwrap_err();
        assert_eq!(
            error.code(),
            crate::core::error::ErrorCode::ExtractionFailed
        );
    }
}
