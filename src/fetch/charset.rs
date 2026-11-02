//! Charset detection and decoding.
//!
//! Precedence: BOM, then the `Content-Type` charset parameter, then a `<meta
//! charset>`-style declaration in the first 1024 bytes, then UTF-8. `encoding_rs`
//! applies the BOM override itself when decoding.

use encoding_rs::{Encoding, UTF_8};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decoded {
    pub text: String,
    /// Canonical encoding name as reported by `encoding_rs`.
    pub charset: String,
    /// True when the decoder had to substitute replacement characters.
    pub had_errors: bool,
}

pub fn decode(body: &[u8], content_type: Option<&str>) -> Decoded {
    let candidate = content_type
        .and_then(charset_from_content_type)
        .and_then(|label| Encoding::for_label(label.as_bytes()))
        .or_else(|| charset_from_meta(body).and_then(|label| Encoding::for_label(label.as_bytes())))
        .unwrap_or(UTF_8);

    // `decode` performs its own BOM sniffing and overrides `candidate` when a
    // BOM is present, which is exactly the required precedence.
    let (text, used, had_errors) = candidate.decode(body);
    Decoded {
        text: text.into_owned(),
        charset: used.name().to_string(),
        had_errors,
    }
}

pub fn charset_from_content_type(content_type: &str) -> Option<&str> {
    for parameter in content_type.split(';').skip(1) {
        let parameter = parameter.trim();
        let (name, value) = parameter.split_once('=')?;
        if name.trim().eq_ignore_ascii_case("charset") {
            let value = value.trim().trim_matches('"').trim();
            if !value.is_empty() {
                return Some(value);
            }
        }
    }
    None
}

/// Look for `charset=<label>` in the first 1024 bytes, tolerating the usual
/// `<meta charset="...">` and `<meta http-equiv=... content="...charset=...">`
/// spellings.
pub fn charset_from_meta(body: &[u8]) -> Option<String> {
    let window = &body[..body.len().min(1024)];
    let lower: Vec<u8> = window
        .iter()
        .map(|byte| byte.to_ascii_lowercase())
        .collect();
    let needle = b"charset";
    let mut index = 0usize;
    while index + needle.len() <= lower.len() {
        if &lower[index..index + needle.len()] == needle {
            let mut cursor = index + needle.len();
            while cursor < lower.len() && lower[cursor].is_ascii_whitespace() {
                cursor += 1;
            }
            if cursor < lower.len() && lower[cursor] == b'=' {
                cursor += 1;
                while cursor < lower.len() && lower[cursor].is_ascii_whitespace() {
                    cursor += 1;
                }
                let quote = lower[cursor];
                if quote == b'"' || quote == b'\'' {
                    cursor += 1;
                    let start = cursor;
                    while cursor < lower.len() && lower[cursor] != quote {
                        cursor += 1;
                    }
                    if cursor > start {
                        return Some(String::from_utf8_lossy(&lower[start..cursor]).to_string());
                    }
                } else {
                    let start = cursor;
                    while cursor < lower.len()
                        && (lower[cursor].is_ascii_alphanumeric()
                            || matches!(lower[cursor], b'-' | b'_' | b'.' | b':'))
                    {
                        cursor += 1;
                    }
                    if cursor > start {
                        return Some(String::from_utf8_lossy(&lower[start..cursor]).to_string());
                    }
                }
            }
        }
        index += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn latin1(text: &str) -> Vec<u8> {
        text.chars()
            .map(|character| {
                if character.is_ascii() {
                    character as u8
                } else {
                    // Latin-1 byte for the character (all our samples are < U+0100).
                    character as u32 as u8
                }
            })
            .collect()
    }

    #[test]
    fn header_charset_wins_over_meta() {
        let mut body = Vec::new();
        body.extend_from_slice(br#"<html><head><meta charset="utf-8"></head><body>"#);
        body.extend_from_slice(&latin1("café"));
        body.extend_from_slice(b"</body></html>");
        let decoded = decode(&body, Some("text/html; charset=iso-8859-1"));
        assert_eq!(decoded.charset, "windows-1252");
        assert!(decoded.text.contains("café"), "{}", decoded.text);
    }

    #[test]
    fn meta_charset_used_when_header_absent() {
        let mut body = Vec::new();
        body.extend_from_slice(br#"<html><head><meta charset="iso-8859-1"></head><body>"#);
        body.extend_from_slice(&latin1("naïve café"));
        body.extend_from_slice(b"</body></html>");
        let decoded = decode(&body, Some("text/html"));
        assert_eq!(decoded.charset, "windows-1252");
        assert!(decoded.text.contains("naïve café"), "{}", decoded.text);
    }

    #[test]
    fn http_equiv_content_charset_is_recognised() {
        let body = br#"<html><head><meta http-equiv="content-type" content="text/html; charset=Shift_JIS"></head><body>x</body></html>"#;
        let decoded = decode(body, None);
        assert_eq!(decoded.charset, "Shift_JIS");
    }

    #[test]
    fn bom_beats_everything() {
        let mut body = vec![0xEF, 0xBB, 0xBF];
        body.extend_from_slice("héllo".as_bytes());
        let decoded = decode(&body, Some("text/html; charset=iso-8859-1"));
        assert_eq!(decoded.charset, "UTF-8");
        assert_eq!(decoded.text, "héllo");
        assert!(!decoded.text.starts_with('\u{feff}'));
    }

    #[test]
    fn defaults_to_utf8_and_reports_replacement() {
        let decoded = decode("plain ascii".as_bytes(), None);
        assert_eq!(decoded.charset, "UTF-8");
        assert_eq!(decoded.text, "plain ascii");
        assert!(!decoded.had_errors);

        let invalid = [0xFF, 0xFE, 0x41];
        let decoded = decode(&invalid, None);
        assert!(decoded.had_errors);
    }

    #[test]
    fn unknown_labels_fall_through() {
        assert_eq!(
            charset_from_content_type("text/html; charset=\"not-a-charset\""),
            Some("not-a-charset")
        );
        let decoded = decode(b"abc", Some("text/html; charset=not-a-charset"));
        assert_eq!(decoded.charset, "UTF-8");
    }
}
