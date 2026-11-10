//! The `render = auto` heuristic ().
//!
//! Pure and testable: the pipeline supplies the main-content word count and the
//! raw HTML, and this module returns the decision plus the rule name that goes
//! into the log. Keeping the thresholds here means the decision cannot drift
//! between `fetch_page` and `extract`.

/// Thresholds from. They are deliberately small and explainable;
/// any site-specific tuning is a later decision, not a hidden constant.
pub const THIN_CONTENT_WORDS: usize = 200;
pub const NOSCRIPT_CONTENT_WORDS: usize = 100;
pub const SCRIPT_ELEMENT_THRESHOLD: usize = 3;

pub const RULE_THIN_SCRIPTS: &str = "thin_content_with_scripts_or_app_root";
pub const RULE_NOSCRIPT: &str = "noscript_javascript_required";
pub const RULE_SUFFICIENT: &str = "static_content_sufficient";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContentSignals {
 /// Words in the main content, as produced by the extraction mode in use.
 pub main_words: usize,
 /// Total `<script>` elements, including external and inline scripts.
 pub script_count: usize,
 /// `#root`, `#app`, `[data-reactroot]` or `ng-app` markers.
 pub app_root: bool,
 /// A `<noscript>` element is present.
 pub noscript: bool,
 /// An "enable JavaScript"-class message is visible in the markup.
 pub javascript_required: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutoDecision {
 pub render: bool,
 /// Stable rule identifier recorded in the decision log and asserted by tests.
 pub rule: &'static str,
}

pub fn signals_from_html(html: &str, main_words: usize) -> ContentSignals {
 let lower = html.to_ascii_lowercase();
 ContentSignals {
 main_words,
 script_count: count_script_elements(&lower),
 app_root: has_app_root(&lower),
 noscript: lower.contains("<noscript"),
 javascript_required: has_javascript_required_message(&lower),
 }
}

pub fn decide(signals: &ContentSignals) -> AutoDecision {
 if signals.main_words < THIN_CONTENT_WORDS
 && (signals.script_count > SCRIPT_ELEMENT_THRESHOLD || signals.app_root)
 {
 return AutoDecision {
 render: true,
 rule: RULE_THIN_SCRIPTS,
 };
 }
 if signals.main_words < NOSCRIPT_CONTENT_WORDS
 && (signals.noscript || signals.javascript_required)
 {
 return AutoDecision {
 render: true,
 rule: RULE_NOSCRIPT,
 };
 }
 AutoDecision {
 render: false,
 rule: RULE_SUFFICIENT,
 }
}

/// Approximate visible-text words without a DOM parse. Used by `extract` for
/// its auto decision, where a second parse purely for the heuristic would
/// violate the single-parse design. `fetch_page` uses the extractor's own word
/// count instead.
pub fn rough_visible_word_count(html: &str) -> usize {
 let bytes = html.as_bytes();
 let mut index = 0usize;
 let mut words = 0usize;
 let mut in_word = false;
 let mut skip_until: Option<&[u8]> = None;

 while index < bytes.len() {
 if let Some(close) = skip_until {
 if starts_with_ignore_ascii_case(&bytes[index..], close) {
 index = tag_end(bytes, index);
 skip_until = None;
 in_word = false;
 } else {
 index += 1;
 }
 continue;
 }

 if bytes[index] == b'<' {
 if starts_with_ignore_ascii_case(&bytes[index..], b"<script") {
 index = tag_end(bytes, index);
 skip_until = Some(b"</script");
 continue;
 }
 if starts_with_ignore_ascii_case(&bytes[index..], b"<style") {
 index = tag_end(bytes, index);
 skip_until = Some(b"</style");
 continue;
 }
 index = tag_end(bytes, index);
 if in_word {
 in_word = false;
 }
 continue;
 }

 if bytes[index].is_ascii_whitespace() {
 in_word = false;
 } else if !in_word {
 in_word = true;
 words += 1;
 }
 index += 1;
 }
 words
}

fn count_script_elements(lower: &str) -> usize {
 lower.match_indices("<script").count()
}

fn has_app_root(lower: &str) -> bool {
 [
 "id=\"root\"",
 "id='root'",
 "id=\"app\"",
 "id='app'",
 "data-reactroot",
 "ng-app",
 ]
.iter()
.any(|needle| lower.contains(needle))
}

fn has_javascript_required_message(lower: &str) -> bool {
 [
 "enable javascript",
 "javascript is required",
 "requires javascript",
 "javascript is disabled",
 "you need to enable javascript",
 ]
.iter()
.any(|needle| lower.contains(needle))
}

fn starts_with_ignore_ascii_case(haystack: &[u8], needle: &[u8]) -> bool {
 haystack.len() >= needle.len() && haystack[..needle.len()].eq_ignore_ascii_case(needle)
}

fn tag_end(bytes: &[u8], start: usize) -> usize {
 bytes[start..]
.iter()
.position(|byte| *byte == b'>')
.map(|offset| start + offset + 1)
.unwrap_or(bytes.len())
}

#[cfg(test)]
mod tests {
 use super::*;

 fn signals(
 main_words: usize,
 script_count: usize,
 app_root: bool,
 noscript: bool,
 javascript_required: bool) -> ContentSignals {
 ContentSignals {
 main_words,
 script_count,
 app_root,
 noscript,
 javascript_required,
 }
 }

 #[test]
 fn thin_page_with_four_scripts_renders_first_rule() {
 let decision = decide(&signals(80, 4, false, false, false));
 assert!(decision.render);
 assert_eq!(decision.rule, RULE_THIN_SCRIPTS);
 }

 #[test]
 fn thin_page_with_app_root_renders_first_rule() {
 let decision = decide(&signals(10, 1, true, false, false));
 assert!(decision.render);
 assert_eq!(decision.rule, RULE_THIN_SCRIPTS);
 }

 #[test]
 fn noscript_shell_renders_second_rule() {
 let decision = decide(&signals(20, 1, false, true, false));
 assert!(decision.render);
 assert_eq!(decision.rule, RULE_NOSCRIPT);
 }

 #[test]
 fn javascript_required_message_renders_second_rule() {
 let decision = decide(&signals(50, 1, false, false, true));
 assert!(decision.render);
 assert_eq!(decision.rule, RULE_NOSCRIPT);
 }

 #[test]
 fn substantial_static_content_does_not_render() {
 let decision = decide(&signals(900, 8, true, false, false));
 assert!(!decision.render);
 assert_eq!(decision.rule, RULE_SUFFICIENT);
 }

 #[test]
 fn five_fixture_shapes_produce_three_renders_and_two_static_results() {
 let fixtures = [
 signals(40, 5, false, false, false),
 signals(12, 2, true, false, false),
 signals(0, 1, false, true, false),
 signals(500, 12, true, false, false),
 signals(300, 1, false, false, false),
 ];
 let decisions: Vec<AutoDecision> = fixtures.iter().map(decide).collect();
 assert_eq!(
 decisions.iter().filter(|decision| decision.render).count(),
 3
);
 assert_eq!(
 decisions.iter().filter(|decision| !decision.render).count(),
 2
);
 assert_eq!(
 decisions
.iter()
.map(|decision| decision.rule)
.collect::<Vec<_>>(),
 vec![
 RULE_THIN_SCRIPTS,
 RULE_THIN_SCRIPTS,
 RULE_NOSCRIPT,
 RULE_SUFFICIENT,
 RULE_SUFFICIENT,
 ]
);
 }

 #[test]
 fn signals_read_scripts_markers_and_noscript_messages() {
 let html = r#"<html><body><div id="app"></div><noscript>Enable JavaScript</noscript>
 <script></script><script></script></body></html>"#;
 let signals = signals_from_html(html, 30);
 assert_eq!(signals.script_count, 2);
 assert!(signals.app_root);
 assert!(signals.noscript);
 assert!(signals.javascript_required);
 }

 #[test]
 fn rough_word_count_ignores_tags_scripts_and_styles() {
 let html = r#"<html><head><style>.x{}</style><script>var notAWord = 1;</script></head>
 <body><h1>Four</h1><p>visible words here</p></body></html>"#;
 assert_eq!(rough_visible_word_count(html), 4);
 }

 #[test]
 fn rough_word_count_handles_uppercase_tags() {
 let html = "<HTML><BODY><P>one two</P><SCRIPT>three four</SCRIPT></BODY></HTML>";
 assert_eq!(rough_visible_word_count(html), 2);
 }
}
