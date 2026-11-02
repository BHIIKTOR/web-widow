//! Truncation and text metrics with one invariant: a function never returns
//! more than the cap it was given, and it says when it clipped.

/// Truncate on a character boundary. Returns the (possibly clipped) text and
/// whether clipping happened. `max_chars == 0` yields an empty string and
/// `truncated == true` for non-empty input.
pub fn truncate_chars(input: &str, max_chars: usize) -> (String, bool) {
    for (count, (offset, _)) in input.char_indices().enumerate() {
        if count == max_chars {
            return (input[..offset].to_string(), true);
        }
    }
    (input.to_string(), false)
}

/// Whitespace-delimited word count of the delivered text.
pub fn word_count(input: &str) -> usize {
    input.split_whitespace().count()
}

/// Normalize whitespace for plain-text delivery: strip trailing spaces per
/// line, collapse runs of blank lines to one, trim the whole string.
pub fn normalize_plain_text(input: &str) -> String {
    let mut lines: Vec<&str> = Vec::new();
    let mut blank_run = 0usize;
    for line in input.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            blank_run += 1;
            if blank_run > 1 {
                continue;
            }
            lines.push("");
        } else {
            blank_run = 0;
            lines.push(trimmed);
        }
    }
    lines.join("\n").trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncates_at_a_character_boundary() {
        let (text, truncated) = truncate_chars("héllo wörld", 5);
        assert_eq!(text, "héllo");
        assert!(truncated);
        // Byte length differs from char length; the slice stays valid UTF-8.
        assert_eq!(text.chars().count(), 5);
    }

    #[test]
    fn exact_fit_is_not_truncation() {
        let (text, truncated) = truncate_chars("abc", 3);
        assert_eq!(text, "abc");
        assert!(!truncated);
        let (text, truncated) = truncate_chars("abc", 10);
        assert_eq!(text, "abc");
        assert!(!truncated);
    }

    #[test]
    fn zero_cap_clips_any_content() {
        let (text, truncated) = truncate_chars("abc", 0);
        assert_eq!(text, "");
        assert!(truncated);
        let (text, truncated) = truncate_chars("", 0);
        assert_eq!(text, "");
        assert!(!truncated);
    }

    #[test]
    fn word_count_ignores_extra_whitespace() {
        assert_eq!(word_count("one  two\nthree\tfour"), 4);
        assert_eq!(word_count(""), 0);
        assert_eq!(word_count("   \n "), 0);
    }

    #[test]
    fn plain_text_normalization_is_stable() {
        let input = "a   \n\n\n\nb\n ";
        assert_eq!(normalize_plain_text(input), "a\n\nb");
        let once = normalize_plain_text(input);
        assert_eq!(normalize_plain_text(&once), once);
    }
}
