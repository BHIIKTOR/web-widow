//! RSS 2.0 rendering for the change feed.
//!
//! Newest first, at most 100 items, every text node XML-escaped and stripped of
//! control characters. The document is validated with `feed-rs` in tests (the
//! same parser `fetch_feed` uses), which is how "consumable by our own reader"
//! is proven without an SSRF exception. The full diff is never included.

use super::{EventItem, RSS_MAX_ITEMS};
use crate::monitor::normalize::ChangeStats;

/// Render the feed. `feed_url` is the served endpoint when the HTTP listener is
/// active; stdio has none and the channel link falls back to the first item.
pub fn render(items: &[EventItem], feed_url: Option<&str>) -> String {
 let items: &[EventItem] = &items[..items.len().min(RSS_MAX_ITEMS)];
 let channel_link = feed_url
.map(str::to_string)
.or_else(|| items.first().map(|item| item.url.clone()))
.unwrap_or_else(|| "about:blank".to_string());
 let last_build = items
.iter()
.filter_map(|item| chrono::DateTime::parse_from_rfc3339(&item.detected_at).ok())
.map(|value| value.to_rfc2822())
.next()
.unwrap_or_else(|| chrono::Utc::now().to_rfc2822());

 let mut xml = String::with_capacity(256 + items.len() * 320);
 xml.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
 xml.push_str("<rss version=\"2.0\">\n<channel>\n");
 xml.push_str(&format!(
 "<title>{}</title>\n<link>{}</link>\n<description>{}</description>\n<lastBuildDate>{}</lastBuildDate>\n",
 escape("web-widow changes"),
 escape(&channel_link),
 escape("Change feed from the local web-widow store"),
 escape(&last_build)
));
 for item in items {
 let title = item
.monitor_label
.as_deref()
.or(item.title.as_deref())
.unwrap_or(&item.url);
 xml.push_str("<item>\n");
 xml.push_str(&format!("<title>{}</title>\n", escape(title)));
 xml.push_str(&format!("<link>{}</link>\n", escape(&item.url)));
 xml.push_str(&format!(
 "<guid isPermaLink=\"false\">{}</guid>\n",
 escape(&format!("change-{}", item.change_id))
));
 xml.push_str(&format!(
 "<pubDate>{}</pubDate>\n",
 escape(&rfc2822(&item.detected_at))
));
 xml.push_str(&format!(
 "<description>{}</description>\n",
 escape(&summary(&item.kind, &item.stats))
));
 xml.push_str("</item>\n");
 }
 xml.push_str("</channel>\n</rss>\n");
 xml
}

fn rfc2822(timestamp: &str) -> String {
 chrono::DateTime::parse_from_rfc3339(timestamp)
.map(|value| value.to_rfc2822())
.unwrap_or_else(|_| timestamp.to_string())
}

/// A short description: the kind and the full stats counts (never the diff).
pub fn summary(kind: &str, stats: &ChangeStats) -> String {
 let mut text = format!(
 "{kind}: +{}/-{} lines, +{}/-{} words, {} changed lines",
 stats.lines_added,
 stats.lines_removed,
 stats.words_added,
 stats.words_removed,
 stats.changed_lines
);
 if !stats.changed_fields.is_empty() {
 text.push_str("; fields: ");
 text.push_str(&stats.changed_fields.join(", "));
 }
 if stats.previous_unavailable {
 text.push_str("; previous text unavailable");
 }
 text
}

/// XML text escaping plus control-character stripping (tab/newline kept).
fn escape(value: &str) -> String {
 let mut out = String::with_capacity(value.len());
 for character in value.chars() {
 match character {
 '&' => out.push_str("&amp;"),
 '<' => out.push_str("&lt;"),
 '>' => out.push_str("&gt;"),
 '"' => out.push_str("&quot;"),
 '\'' => out.push_str("&apos;"),
 '\n' => out.push('\n'),
 '\t' => out.push('\t'),
 character if character.is_control() => {}
 character => out.push(character),
 }
 }
 out
}

#[cfg(test)]
mod tests {
 use super::*;

 #[test]
 fn escaping_covers_the_five_entities_and_controls() {
 assert_eq!(escape("<a>&\"'"), "&lt;a&gt;&amp;&quot;&apos;");
 assert_eq!(escape("a\u{0}b\u{1}c\td\ne"), "abc\td\ne");
 }

 #[test]
 fn the_summary_carries_stats_and_fields_not_a_diff() {
 let stats = ChangeStats {
 lines_added: 2,
 lines_removed: 1,
 changed_lines: 3,
 words_added: 4,
 words_removed: 5,
 changed_fields: vec!["price".to_string()],
 previous_unavailable: false,
 };
 let text = summary("substantive", &stats);
 assert!(text.contains("+2/-1 lines"));
 assert!(text.contains("price"));
 assert!(!text.contains("---"));
 }
}
