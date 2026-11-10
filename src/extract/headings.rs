//! Restore section headings and structural gaps that readability's assembly drops.
//!
//! `dom_smoothie` 0.18.2 builds the article from the top candidate plus those
//! *siblings* whose stored content score passes a threshold; headings score
//! `-5`, so a page that uses `<h1>`/`<h2>` for section headings between content
//! blocks loses them while its paragraphs survive (observed on
//! sqlite.org/whentouse.html). The library exposes no option for this.
//!
//! Two fail-closed repairs live here, applied in this order:
//!
//! 1. [`repair_structural_gaps`] restores content blocks (and their headings)
//! that sit *between* two already-delivered blocks, following.
//! It never re-selects the article, never inserts at the edges, and skips
//! the whole page when the 25 % bound would be exceeded.
//! 2. [`restore_section_headings`] puts back a heading whose following content
//! block is already in the Markdown.
//!
//! Both are deliberately surgical: anything ambiguous (no anchor, repeated
//! anchor, short anchor, hidden subtree) is skipped, so the worst case is the
//! previous behaviour.
//!
//! `h1` is emitted as level 2, matching `dom_smoothie`'s own `h1 -> h2` rename
//! for headings that do reach the article, so a restored heading cannot look
//! different from one that was kept.

use std::collections::HashSet;

use scraper::node::Node;
use scraper::{ElementRef, Html};

/// Subtree tags that never contribute an anchor, a gap candidate or a heading.
const SKIP_TAGS: &[&str] = &[
 "head", "script", "style", "noscript", "template", "svg", "iframe", "object", "embed",
 "canvas", "nav", "footer", "form", "aside", "header",
];

const HEADING_TAGS: &[&str] = &["h1", "h2", "h3", "h4", "h5", "h6"];

/// Block-level elements that carry delivered prose. Their text is the anchor a
/// heading must precede; nested blocks are not descended into, so an anchor is
/// always an outermost block.
const CONTENT_TAGS: &[&str] = &[
 "p",
 "ul",
 "ol",
 "pre",
 "table",
 "blockquote",
 "dl",
 "figure",
];

/// Below this many normalized characters an anchor is too ambiguous to place a
/// heading reliably.
const MIN_ANCHOR_CHARS: usize = 40;

/// How much leading source text is collected as an anchor candidate. Enough to
/// be distinctive, short enough to precede the first inline link or emphasis.
const ANCHOR_WINDOW_CHARS: usize = 200;

/// A gap repair may never insert more than this share of the delivered text.
const MAX_REPAIR_PERCENT: usize = 25;

/// A candidate whose link text is more than this share of its text is chrome.
const MAX_LINK_DENSITY_PERCENT: usize = 50;

/// The chrome-phrase deny applies only to blocks shorter than this; long prose
/// is never filtered by phrase.
const CHROME_DENY_MAX_CHARS: usize = 200;

/// Unmistakable chrome phrases for short candidate blocks ( §5).
const CHROME_PHRASES: &[&str] = &[
 "cookie",
 "subscribe to",
 "sign up for",
 "advertisement",
 "all rights reserved",
 "privacy policy",
 "terms of service",
 "share this",
 "follow us",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BlockKind {
 Heading(u8),
 Content,
}

/// One outermost heading or content block of the source document.
#[derive(Debug, Clone)]
struct Block {
 kind: BlockKind,
 /// Source element tag, used to re-locate the same element in the article.
 tag: String,
 /// Normalized full text: comparison form and the page-bound measure.
 text: String,
 /// The cleaned source text, used when a block is rendered back.
 display: String,
 /// Normalized placement anchor: the leading window for content, the whole
 /// text for a heading.
 anchor: String,
 /// Characters of text inside `<a>` descendants.
 link_chars: usize,
 /// Alphanumeric duplicate key: whitespace, Markdown syntax and table pipes
 /// are irrelevant, so a delivered block is recognised in any rendering.
 key: String,
 /// Source node identity, used to render the admitted block.
 node_id: ego_tree::NodeId,
 /// Parent element identity, used by the section-initiation gate.
 parent_id: Option<ego_tree::NodeId>,
}

/// Restore non-title section headings into `markdown`, in source order.
pub fn restore_section_headings(document: &Html, markdown: &str, title: Option<&str>) -> String {
 let blocks = collect_blocks(document.tree.root());
 if blocks.is_empty() {
 return markdown.to_string();
 }
 let haystack = Normalized::new(markdown);
 let title = title.map(normalize);
 let mut insertions: Vec<(usize, String)> = Vec::new();
 let mut restored: HashSet<String> = HashSet::new();

 for (index, block) in blocks.iter().enumerate() {
 if !matches!(block.kind, BlockKind::Heading(_)) {
 continue;
 }
 let heading = &block.text;
 if heading.chars().count() < 2 {
 continue;
 }
 if title
.as_deref()
.is_some_and(|title| is_title_heading(heading, title))
 {
 tracing::debug!(heading = %heading, "heading skipped: duplicates the page title");
 continue;
 }
 // Already delivered, or restored once already: nothing to do.
 if haystack.contains(heading) || !restored.insert(heading.clone()) {
 tracing::debug!(heading = %heading, "heading skipped: already in the Markdown");
 continue;
 }
 let Some(anchor) = blocks[index + 1..]
.iter()
.find_map(|block| match block.kind {
 BlockKind::Content => Some(&block.anchor),
 BlockKind::Heading(_) => None,
 })
 else {
 tracing::debug!(heading = %heading, "heading skipped: no following content block");
 continue;
 };
 if anchor.chars().count() < MIN_ANCHOR_CHARS {
 tracing::debug!(heading = %heading, anchor = %anchor, "heading skipped: anchor too short");
 continue;
 }
 let Some(offset) = haystack.unique_offset(anchor) else {
 tracing::debug!(heading = %heading, anchor = %anchor, "heading skipped: anchor absent or ambiguous");
 continue;
 };
 let line_start = markdown[..offset]
.rfind('\n')
.map(|index| index + 1)
.unwrap_or(0);
 // `h1` becomes `#`, then is raised to level 2 below, matching the
 // library's own rename for headings it keeps.
 let markdown_level = match block.kind {
 BlockKind::Heading(level) => level.max(2),
 BlockKind::Content => 2,
 };
 insertions.push((
 line_start,
 format!(
 "{} {}\n\n",
 "#".repeat(usize::from(markdown_level)),
 block.display.trim()
)));
 }

 if insertions.is_empty() {
 return markdown.to_string();
 }
 insertions.sort_by_key(|(offset, _)| *offset);
 let mut out = String::with_capacity(markdown.len() + insertions.len() * 32);
 let mut cursor = 0usize;
 for (offset, text) in insertions {
 if offset < cursor {
 // Two headings for one line cannot be placed in order; skip the
 // later one rather than guess.
 continue;
 }
 out.push_str(&markdown[cursor..offset]);
 out.push_str(&text);
 cursor = offset;
 }
 out.push_str(&markdown[cursor..]);
 out
}

/// Restore content blocks that readability dropped *between* two delivered
/// blocks (). Returns `None` when nothing qualifies, in which case
/// the caller must keep the original article bytes byte-for-byte.
///
/// Insertion happens in the article's own DOM serialization, immediately before
/// the following anchor element, which is the same position as that anchor's
/// Markdown line start — and keeps `:main` records and the Markdown consistent.
pub fn repair_structural_gaps(document: &Html, article_content: &str) -> Option<String> {
 let blocks = collect_blocks(document.tree.root());
 if blocks.len() < 3 {
 return None;
 }
 let markdown = super::html_to_markdown(article_content).ok()?;
 let haystack = Normalized::new(&markdown);
 if haystack.text.is_empty() {
 return None;
 }

 let anchors = delivered_anchors(&blocks, &haystack);
 if anchors.len() < 2 {
 return None;
 }

 let delivered_chars = haystack.text.chars().count();
 let anchor_indices: HashSet<usize> = anchors.iter().map(|(index, _)| *index).collect();
 let mut gaps: Vec<(usize, Vec<usize>)> = Vec::new();
 for pair in anchors.windows(2) {
 let (start, start_offset) = pair[0];
 let (end, end_offset) = pair[1];
 if end <= start + 1 || end_offset <= start_offset {
 continue;
 }
 if !section_initiation(&blocks, &anchor_indices, &haystack, start, end) {
 continue;
 }
 // Headings are not admitted here: they are placed by
 // `restore_section_headings` once their body exists in the Markdown,
 // which keeps their exact historical rendering (no re-escaping).
 let candidates: Vec<usize> = (start + 1..end)
.filter(|index| matches!(blocks[*index].kind, BlockKind::Content))
.filter(|index| admissible(&blocks[*index], &haystack))
.collect();
 if !candidates.is_empty() {
 gaps.push((end, candidates));
 }
 }
 if gaps.is_empty() {
 return None;
 }

 let repaired_chars: usize = gaps
.iter()
.flat_map(|(_, candidates)| candidates.iter())
.map(|index| blocks[*index].text.chars().count())
.sum();
 if repaired_chars
.saturating_mul(100)
.gt(&delivered_chars.saturating_mul(MAX_REPAIR_PERCENT))
 {
 tracing::debug!(
 repaired = repaired_chars,
 delivered = delivered_chars,
 "gap repair skipped: page bound would be exceeded"
);
 return None;
 }

 // Only pages that actually repair pay for the article parse and
 // re-serialization; every other page keeps readability's exact bytes.
 let article = Html::parse_document(article_content);
 let serialized = article.html();
 let mut insertions: Vec<(usize, String)> = Vec::new();
 for (end, candidates) in gaps {
 let Some(offset) = locate_anchor(&article, &serialized, &blocks[end]) else {
 tracing::debug!(anchor = %blocks[end].anchor, "gap repair skipped: anchor not locatable");
 continue;
 };
 let mut fragment = String::new();
 for index in candidates {
 fragment.push_str(&render_block(document, &blocks[index]));
 fragment.push('\n');
 }
 insertions.push((offset, fragment));
 }
 if insertions.is_empty() {
 return None;
 }
 insertions.sort_by_key(|(offset, _)| *offset);
 let mut out = String::with_capacity(serialized.len() + 1024);
 let mut cursor = 0usize;
 for (offset, fragment) in insertions {
 if offset < cursor {
 // Overlapping insertions cannot be placed in order; fail closed.
 return None;
 }
 out.push_str(&serialized[cursor..offset]);
 out.push_str(&fragment);
 cursor = offset;
 }
 out.push_str(&serialized[cursor..]);
 Some(out)
}

/// `(source_index, markdown_offset)` for every block that is uniquely delivered.
fn delivered_anchors(blocks: &[Block], haystack: &Normalized) -> Vec<(usize, usize)> {
 blocks
.iter()
.enumerate()
.filter_map(|(index, block)| {
 let needle = match block.kind {
 BlockKind::Heading(_) => {
 if block.text.chars().count() < 2 {
 return None;
 }
 &block.text
 }
 BlockKind::Content => {
 if block.text.chars().count() < MIN_ANCHOR_CHARS {
 return None;
 }
 &block.anchor
 }
 };
 haystack.unique_offset(needle).map(|offset| (index, offset))
 })
.collect()
}

/// The section-initiation gate ( §5, revised 2026-10-31). A gap is
/// eligible only when it starts a new section the same way its neighbours do:
///
/// 1. the first candidate is an **undelivered heading** (not an anchor, not a
/// duplicate of delivered text);
/// 2. at least one content block follows it inside the gap;
/// 3. its parent element is the same as the nearest delivered heading adjacent
/// to the gap (following first, else preceding);
/// 4. its level is no deeper than that heading's level.
///
/// This is structural (parent identity and heading level), never class- or
/// site-based. An unheaded run — a card paragraph, a landmark note, a teaser —
/// is not a section, so it is never admitted.
fn section_initiation(
 blocks: &[Block],
 anchors: &HashSet<usize>,
 haystack: &Normalized,
 start: usize,
 end: usize) -> bool {
 let first = start + 1;
 let BlockKind::Heading(level) = blocks[first].kind else {
 return false;
 };
 if anchors.contains(&first) || haystack.contains(&blocks[first].text) {
 return false;
 }
 if !(first + 1..end).any(|index| matches!(blocks[index].kind, BlockKind::Content)) {
 return false;
 }
 let Some(reference) = nearest_adjacent_heading(blocks, anchors, start, end) else {
 return false;
 };
 if blocks[first].parent_id != blocks[reference].parent_id {
 return false;
 }
 let BlockKind::Heading(reference_level) = blocks[reference].kind else {
 return false;
 };
 level <= reference_level
}

/// Nearest heading outside the gap that the delivered span implies: a heading
/// anchor itself, or one whose following content block is delivered (the
/// existing heading restore will place it). Following headings win over
/// preceding ones.
fn nearest_adjacent_heading(
 blocks: &[Block],
 anchors: &HashSet<usize>,
 start: usize,
 end: usize) -> Option<usize> {
 let delivered = |index: usize| {
 anchors.contains(&index)
 || (index + 1..blocks.len())
.find(|next| matches!(blocks[*next].kind, BlockKind::Content))
.is_some_and(|next| anchors.contains(&next))
 };
 (end + 1..blocks.len())
.find(|index| matches!(blocks[*index].kind, BlockKind::Heading(_)) && delivered(*index))
.or_else(|| {
 (0..start).rev().find(|index| {
 matches!(blocks[*index].kind, BlockKind::Heading(_)) && delivered(*index)
 })
 })
}

/// All gates from §5, applied to a content candidate.
fn admissible(block: &Block, haystack: &Normalized) -> bool {
 debug_assert!(matches!(block.kind, BlockKind::Content));
 let chars = block.text.chars().count();
 if chars < MIN_ANCHOR_CHARS {
 return false;
 }
 // The alphanumeric key is compared even when it is short: skipping a
 // candidate is the safe direction, and an exact key match over a block that
 // already passed the substance gate is a duplicate.
 if !block.key.is_empty() && haystack.key.contains(&block.key) {
 return false;
 }
 if chars == 0 || block.link_chars.saturating_mul(100) > chars * MAX_LINK_DENSITY_PERCENT {
 return false;
 }
 if chars < CHROME_DENY_MAX_CHARS {
 let lower = block.text.to_ascii_lowercase();
 if CHROME_PHRASES.iter().any(|phrase| lower.contains(phrase)) {
 return false;
 }
 }
 true
}

/// Byte offset in `serialized` where the anchor element starts. The element is
/// the innermost one whose normalized text starts with the anchor; anything
/// ambiguous or absent fails closed.
fn locate_anchor(article: &Html, serialized: &str, block: &Block) -> Option<usize> {
 let mut matches: Vec<(ego_tree::NodeId, ElementRef<'_>)> = Vec::new();
 for element in article
.tree
.root()
.descendants()
.filter_map(ElementRef::wrap)
 {
 if element.value().name() != block.tag {
 continue;
 }
 let text = normalize(&full_text(*element));
 if text.starts_with(&block.anchor) {
 matches.push((element.id(), element));
 }
 }
 // Keep only the innermost matches so a wrapper that happens to start at the
 // same text cannot win.
 let ids: HashSet<_> = matches.iter().map(|(id, _)| *id).collect();
 let mut deepest: Vec<ElementRef<'_>> = matches
.into_iter()
.filter(|(_, element)| !has_matching_descendant(element, &ids))
.map(|(_, element)| element)
.collect();
 if deepest.len() != 1 {
 return None;
 }
 let outer = deepest.pop()?.html();
 let first = serialized.find(&outer)?;
 if serialized[first + outer.len()..].contains(&outer) {
 return None;
 }
 Some(first)
}

/// Whether any proper descendant of `element` is in `ids` (wrapper elements
/// lose to the innermost match).
fn has_matching_descendant(element: &ElementRef<'_>, ids: &HashSet<ego_tree::NodeId>) -> bool {
 element
.children()
.filter_map(ElementRef::wrap)
.any(|child| ids.contains(&child.id()) || has_matching_descendant(&child, ids))
}

/// Render an admitted source block as HTML for insertion. Headings are renamed
/// to `h2` exactly as `dom_smoothie` renames the headings it keeps.
fn render_block(document: &Html, block: &Block) -> String {
 let Some(node) = document.tree.get(block.node_id) else {
 return String::new();
 };
 let Some(element) = ElementRef::wrap(node) else {
 return String::new();
 };
 match block.kind {
 BlockKind::Heading(_) => format!("<h2>{}</h2>", element.inner_html()),
 BlockKind::Content => element.html(),
 }
}

fn collect_blocks(node: ego_tree::NodeRef<'_, Node>) -> Vec<Block> {
 let mut blocks = Vec::new();
 walk(node, &mut blocks);
 blocks
}

fn walk(node: ego_tree::NodeRef<'_, Node>, blocks: &mut Vec<Block>) {
 for child in node.children() {
 let Some(element) = ElementRef::wrap(child) else {
 continue;
 };
 let name = element.value().name();
 if SKIP_TAGS.contains(&name) || is_hidden(&element) {
 continue;
 }
 if HEADING_TAGS.contains(&name) {
 let display = clean_text(&element.text().collect::<String>());
 let text = normalize(&display);
 blocks.push(Block {
 kind: BlockKind::Heading(name[1..].parse().unwrap_or(2)),
 tag: name.to_string(),
 text: text.clone(),
 display: display.clone(),
 anchor: text,
 link_chars: link_text_chars(child),
 key: duplicate_key(&display),
 node_id: child.id(),
 parent_id: child.parent().map(|parent| parent.id()),
 });
 continue;
 }
 if CONTENT_TAGS.contains(&name) {
 let display = full_text(child);
 blocks.push(Block {
 kind: BlockKind::Content,
 tag: name.to_string(),
 text: normalize(&display),
 anchor: normalize(&leading_text(child)),
 key: duplicate_key(&display),
 display,
 link_chars: link_text_chars(child),
 node_id: child.id(),
 parent_id: child.parent().map(|parent| parent.id()),
 });
 continue;
 }
 walk(child, blocks);
 }
}

/// Characters of text inside `<a>` descendants of `node`.
fn link_text_chars(node: ego_tree::NodeRef<'_, Node>) -> usize {
 let mut total = 0usize;
 for child in node.children() {
 let Some(element) = ElementRef::wrap(child) else {
 continue;
 };
 let name = element.value().name();
 if SKIP_TAGS.contains(&name) {
 continue;
 }
 if name == "a" {
 total += clean_text(&element.text().collect::<String>())
.chars()
.count();
 } else {
 total += link_text_chars(child);
 }
 }
 total
}

/// Normalized text of the whole block, including nested inline elements.
fn full_text(node: ego_tree::NodeRef<'_, Node>) -> String {
 let mut out = String::new();
 collect_full_text(node, &mut out);
 clean_text(&out)
}

fn collect_full_text(node: ego_tree::NodeRef<'_, Node>, out: &mut String) {
 for child in node.children() {
 match child.value() {
 Node::Text(text) => {
 if !out.is_empty() {
 out.push(' ');
 }
 out.push_str(&text.text);
 }
 Node::Element(element) => {
 if SKIP_TAGS.contains(&element.name()) {
 continue;
 }
 collect_full_text(child, out);
 }
 Node::Document | Node::Fragment => collect_full_text(child, out),
 _ => {}
 }
 }
}

/// Plain leading text of a block: text nodes in document order, joined with a
/// space, capped at [`ANCHOR_WINDOW_CHARS`] and trimmed at a word boundary.
fn leading_text(node: ego_tree::NodeRef<'_, Node>) -> String {
 let mut out = String::new();
 collect_text(node, &mut out);
 let collapsed = clean_text(&out);
 if collapsed.chars().count() <= ANCHOR_WINDOW_CHARS {
 return collapsed;
 }
 let clipped: String = collapsed.chars().take(ANCHOR_WINDOW_CHARS).collect();
 match clipped.rfind(' ') {
 Some(index) if index > 20 => clipped[..index].to_string(),
 _ => clipped,
 }
}

fn collect_text(node: ego_tree::NodeRef<'_, Node>, out: &mut String) {
 for child in node.children() {
 if out.len() >= ANCHOR_WINDOW_CHARS * 2 {
 return;
 }
 match child.value() {
 Node::Text(text) => {
 if !out.is_empty() {
 out.push(' ');
 }
 out.push_str(&text.text);
 }
 Node::Element(element) => {
 let name = element.name();
 if SKIP_TAGS.contains(&name) {
 continue;
 }
 collect_text(child, out);
 }
 Node::Document | Node::Fragment => collect_text(child, out),
 _ => {}
 }
 }
}

fn is_hidden(element: &ElementRef<'_>) -> bool {
 if element.value().attr("hidden").is_some() {
 return true;
 }
 if element.value().attr("aria-hidden") == Some("true") {
 return true;
 }
 element
.value()
.attr("style")
.map(|style| style.to_ascii_lowercase().replace(' ', ""))
.is_some_and(|style| style.contains("display:none") || style.contains("visibility:hidden"))
}

/// Canonical form for duplicate detection: letters and digits only, lowercased.
/// Whitespace, Markdown syntax and table pipes cannot hide a delivered block.
fn duplicate_key(text: &str) -> String {
 text.chars()
.filter(|character| character.is_alphanumeric())
.flat_map(char::to_lowercase)
.collect()
}

fn clean_text(text: &str) -> String {
 text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Comparison form: case-folded, whitespace-collapsed, with the Markdown escapes
/// and link brackets a converter introduces removed from both sides.
fn normalize(text: &str) -> String {
 let mut out = String::with_capacity(text.len());
 let chars: Vec<char> = text.chars().collect();
 let mut index = 0usize;
 while index < chars.len() {
 match chars[index] {
 '\\' => {
 // Unescape, then apply the same rules as for a literal character
 // so both sides of a comparison stay symmetric.
 if let Some(next) = chars.get(index + 1)
 && !is_stripped(*next)
 {
 out.push(next.to_ascii_lowercase());
 }
 index += 2;
 }
 '*' | '_' | '`' | '[' | ']' => index += 1,
 '(' if starts_link_target(&chars[index + 1..]) => {
 // `[text](url)` becomes `text`: drop the target, not the label.
 index += 1;
 while index < chars.len() && chars[index] != ')' {
 index += 1;
 }
 index += 1;
 }
 other => {
 out.push(other.to_ascii_lowercase());
 index += 1;
 }
 }
 }
 clean_text(&out)
}

/// A heading that repeats the page title must not be restored; readability
/// deliberately removed it (page-title de-duplication stays in force).
fn is_title_heading(heading: &str, title: &str) -> bool {
 if heading == title || heading.contains(title) || title.contains(heading) {
 return true;
 }
 let heading_words: HashSet<&str> = heading.split(' ').collect();
 let title_words: HashSet<&str> = title.split(' ').collect();
 !heading_words.is_empty() && heading_words.is_subset(&title_words)
}

/// Markdown syntax that is irrelevant to a text comparison.
fn is_stripped(character: char) -> bool {
 matches!(character, '*' | '_' | '`' | '[' | ']')
}

fn starts_link_target(rest: &[char]) -> bool {
 let head: String = rest.iter().take(8).collect::<String>().to_ascii_lowercase();
 head.starts_with("http")
 || head.starts_with('/')
 || head.starts_with('#')
 || head.starts_with("mailto:")
}

/// The Markdown with a byte-offset map back into the original string.
struct Normalized {
 text: String,
 /// `offsets[i]` is the original byte offset of `text`'s i-th character.
 offsets: Vec<usize>,
 /// Alphanumeric-only form for duplicate detection across renderings.
 key: String,
}

impl Normalized {
 fn new(source: &str) -> Self {
 let mut text = String::with_capacity(source.len());
 let mut offsets = Vec::with_capacity(source.len());
 let mut pending_space: Option<usize> = None;
 let chars: Vec<(usize, char)> = source.char_indices().collect();
 let mut index = 0usize;
 while index < chars.len() {
 let (offset, character) = chars[index];
 match character {
 '\\' => {
 // Keep the escaped character (unless it is syntactic), drop
 // the escape itself, exactly as `normalize` does.
 if let Some((next_offset, next)) = chars.get(index + 1)
 && !is_stripped(*next)
 {
 if let Some(space) = pending_space.take() {
 text.push(' ');
 offsets.push(space);
 }
 text.push(next.to_ascii_lowercase());
 offsets.push(*next_offset);
 }
 index += 2;
 }
 '*' | '_' | '`' | '[' | ']' => index += 1,
 '(' => {
 let rest: Vec<char> = chars[index + 1..].iter().map(|(_, c)| *c).collect();
 if starts_link_target(&rest) {
 while index < chars.len() && chars[index].1 != ')' {
 index += 1;
 }
 index += 1;
 } else {
 if let Some(space) = pending_space.take() {
 text.push(' ');
 offsets.push(space);
 }
 text.push('(');
 offsets.push(offset);
 index += 1;
 }
 }
 character if character.is_whitespace() => {
 if pending_space.is_none() && !text.is_empty() {
 pending_space = Some(offset);
 }
 index += 1;
 }
 character => {
 if let Some(space) = pending_space.take() {
 text.push(' ');
 offsets.push(space);
 }
 text.push(character.to_ascii_lowercase());
 offsets.push(offset);
 index += 1;
 }
 }
 }
 let key = duplicate_key(&text);
 Self { text, offsets, key }
 }

 fn contains(&self, needle: &str) -> bool {
 !needle.is_empty() && self.text.contains(needle)
 }

 /// Original byte offset of the only occurrence of `needle`, or `None` when
 /// it is absent or ambiguous.
 fn unique_offset(&self, needle: &str) -> Option<usize> {
 let first = self.text.find(needle)?;
 if self.text[first + needle.len()..].contains(needle) {
 return None;
 }
 self.offsets.get(first).copied()
 }
}

#[cfg(test)]
mod tests {
 use super::*;

 fn html(body: &str) -> Html {
 Html::parse_document(body)
 }

 #[test]
 fn restores_section_headings_between_blocks() {
 let document = html(
 r#"<html><head><title>Choosing a store</title></head><body>
 <div class="body">
 <p>Opening paragraph that readability keeps because it is long enough to matter.</p>
 <h1>1. When a file is enough</h1>
 <p>A single file removes administration from the operator completely.</p>
 <h1>2. When a server earns its keep</h1>
 <p>Many concurrent writers need a process that arbitrates between them.</p>
 </div></body></html>"#);
 let markdown = "Opening paragraph that readability keeps because it is long enough to matter.\n\nA single file removes administration from the operator completely.\n\nMany concurrent writers need a process that arbitrates between them.\n";
 let restored = restore_section_headings(&document, markdown, Some("Choosing a store"));
 assert!(
 restored.contains("## 1. When a file is enough\n\nA single file"),
 "{restored}"
);
 assert!(
 restored.contains("## 2. When a server earns its keep\n\nMany concurrent writers"),
 "{restored}"
);
 let first = restored.find("## 1.").unwrap();
 let second = restored.find("## 2.").unwrap();
 assert!(first < second, "source order must be preserved");
 }

 #[test]
 fn title_headings_are_not_restored() {
 let document = html(
 r#"<html><head><title>Release notes for version four</title></head><body>
 <h1>Release notes</h1>
 <p>Version four fixes a rounding error in the invoice total and adds a shortcut.</p>
 </body></html>"#);
 let markdown =
 "Version four fixes a rounding error in the invoice total and adds a shortcut.\n";
 let restored =
 restore_section_headings(&document, markdown, Some("Release notes for version four"));
 assert!(!restored.contains('#'), "{restored}");
 }

 #[test]
 fn headings_already_present_are_left_alone() {
 let document = html(
 r#"<html><head><title>Notes</title></head><body>
 <h2>Working section</h2>
 <p>A paragraph long enough to serve as an anchor for the heading above it.</p>
 </body></html>"#);
 let markdown = "## Working section\n\nA paragraph long enough to serve as an anchor for the heading above it.\n";
 let restored = restore_section_headings(&document, markdown, Some("Notes"));
 assert_eq!(restored, markdown);
 }

 #[test]
 fn boilerplate_headings_without_a_delivered_anchor_are_skipped() {
 let document = html(
 r#"<html><head><title>Article</title></head><body>
 <aside><h1>Share this story</h1><p>Share links for every social network you can name.</p></aside>
 <article><p>The article body that readability selected as the main content region.</p></article>
 </body></html>"#);
 let markdown = "The article body that readability selected as the main content region.\n";
 let restored = restore_section_headings(&document, markdown, Some("Article"));
 assert_eq!(restored, markdown);
 }

 #[test]
 fn hidden_or_ambiguous_headings_are_skipped() {
 let document = html(
 r#"<html><head><title>Article</title></head><body>
 <div style="display: none"><h1>Hidden section</h1></div>
 <h1>Ambiguous section</h1>
 <p>Same sentence. Same sentence. Same sentence. Same sentence.</p>
 </body></html>"#);
 let markdown = "Same sentence. Same sentence. Same sentence. Same sentence.\n\nSame sentence. Same sentence. Same sentence. Same sentence.\n";
 let restored = restore_section_headings(&document, markdown, Some("Article"));
 assert!(!restored.contains("Hidden section"), "{restored}");
 assert!(!restored.contains("# Ambiguous section"), "{restored}");
 }

 #[test]
 fn a_heading_whose_section_body_was_not_selected_is_not_restored() {
 // Mirrors sqlite.org/whentouse.html: readability drops the whole second
 // section body, so placing its heading before the next section's content
 // would misreport the document. Fail closed instead.
 let document = html(
 r#"<html><head><title>Storage notes</title></head><body>
 <h1>1. First section</h1>
 <ul><li>The first section body that readability actually keeps for the page.</li></ul>
 <h1>2. Second section</h1>
 <ul><li>Short body that never reaches the assembled article content.</li></ul>
 <h1>3. Third section</h1>
 <ol><li>The third section body that readability keeps for the page as well.</li></ol>
 </body></html>"#);
 let markdown = "The first section body that readability actually keeps for the page.

- The third section body that readability keeps for the page as well.
";
 let restored = restore_section_headings(&document, markdown, Some("Storage notes"));
 assert!(restored.contains("## 1. First section"), "{restored}");
 assert!(restored.contains("## 3. Third section"), "{restored}");
 assert!(
 !restored.contains("2. Second section"),
 "a heading without its body must not be invented: {restored}"
);
 }

 #[test]
 fn short_anchors_do_not_place_a_heading() {
 let document = html(
 r#"<html><head><title>Article</title></head><body>
 <h1>Section</h1><p>Too short.</p>
 </body></html>"#);
 let restored = restore_section_headings(&document, "Too short.\n", Some("Article"));
 assert_eq!(restored, "Too short.\n");
 }

 #[test]
 fn escaping_differences_still_match_an_existing_heading() {
 let document = html(
 r#"<html><head><title>Article</title></head><body>
 <h2>sqlite3_open() and friends</h2>
 <p>The call attaches a database file to the connection handle for later use.</p>
 </body></html>"#);
 let markdown = "## sqlite3\\_open() and friends\n\nThe call attaches a database file to the connection handle for later use.\n";
 let restored = restore_section_headings(&document, markdown, Some("Article"));
 assert_eq!(restored, markdown);
 }

 // -----------------------------------------------------------------------
 // B02 — structural-gap repair ()
 // -----------------------------------------------------------------------

 /// Ten delivered paragraphs give the 25 % page bound a realistic base, so a
 /// fixture can exercise a gate without tripping the bound by accident.
 fn delivered_paragraphs() -> String {
 (1..=10)
.map(|index| {
 format!(
 "<p>Delivered paragraph number {index} records a routine observation in the station ledger before the crew changes shift.</p>"
)
 })
.collect()
 }

 fn gap_source(middle: &str) -> Html {
 html(&format!(
 r#"<html><head><title>Gap notes</title></head><body><article>
 <h1>1. Delivered section</h1>
 {}
 {}
 <ol><li>The kept third section body explains the checklist before migrating.</li></ol>
 </article></body></html>"#,
 delivered_paragraphs(),
 middle
))
 }

 fn gap_article() -> String {
 format!(
 "<div>{}<ol><li>The kept third section body explains the checklist before migrating.</li></ol></div>",
 delivered_paragraphs()
)
 }

 const DROPPED_SECTION: &str = "<h1>2. The dropped section</h1><ul><li>The dropped section body states that the second engine needs an arbiter.</li></ul>";

 #[test]
 fn restores_a_dropped_section_between_delivered_blocks() {
 let document = gap_source(DROPPED_SECTION);
 let repaired = repair_structural_gaps(&document, &gap_article()).expect("repair");
 assert!(
 repaired.contains("<ul><li>The dropped section body states"),
 "{repaired}"
);
 // Inserted before the following anchor, not at the page edges.
 let section = repaired.find("The dropped section body").unwrap();
 let following = repaired.find("The kept third section").unwrap();
 assert!(section < following, "source order must be preserved");

 // The heading is placed by the existing heading restore, now that its
 // body exists, and lands directly above the restored section.
 let markdown = super::super::html_to_markdown(&repaired).unwrap();
 let final_markdown = restore_section_headings(&document, &markdown, Some("Gap notes"));
 let heading = final_markdown.find("## 2. The dropped section").unwrap();
 let body = final_markdown.find("The dropped section body").unwrap();
 let next = final_markdown.find("The kept third").unwrap();
 assert!(heading < body && body < next, "{final_markdown}");

 // Idempotence: a second pass sees the restored block as an anchor.
 assert_eq!(repair_structural_gaps(&document, &repaired), None);
 }

 #[test]
 fn link_dense_candidates_are_rejected() {
 let document = gap_source(
 r#"<h1>2. Related reading</h1><ul><li><a href="/a">Related story about the winter flood</a> <a href="/b">Related story about rebuilding the pier</a></li></ul>"#);
 assert_eq!(repair_structural_gaps(&document, &gap_article()), None);
 }

 #[test]
 fn short_chrome_candidates_are_rejected_but_long_prose_is_not_filtered() {
 let short = gap_source(
 "<h1>2. Notice</h1><p>Subscribe to our newsletter for weekly station updates and alerts.</p>");
 assert_eq!(repair_structural_gaps(&short, &gap_article()), None);

 let long_prose = gap_source(
 "<h1>2. Long prose</h1><p>Subscribe to the idea that a long paragraph is still prose: this sentence runs well past the short-block threshold so the phrase deny cannot hide a real section, and the inspector kept it in the ledger because the readings matched the float gauge.</p>");
 let repaired =
 repair_structural_gaps(&long_prose, &gap_article()).expect("long prose kept");
 assert!(repaired.contains("Subscribe to the idea"), "{repaired}");
 }

 #[test]
 fn a_candidate_already_in_the_markdown_is_not_duplicated() {
 let document = gap_source(
 "<h1>2. Duplicate</h1><p>Delivered paragraph number 5 records a routine observation in the station ledger before the crew changes shift.</p>");
 assert_eq!(repair_structural_gaps(&document, &gap_article()), None);
 }

 #[test]
 fn the_page_bound_skips_the_whole_repair() {
 let document = gap_source(
 "<h1>2. Bound</h1><p>The dropped section body states that the second engine needs an arbiter and then repeats the same point at length so that the page bound is exceeded by this single candidate block on the fixture page, and it keeps going with more observations about the workload, the file locking behaviour, the network filesystem latency, the maintenance window, and the operator schedule so the candidate is unambiguously larger than a quarter of the delivered article text.</p>");
 assert_eq!(repair_structural_gaps(&document, &gap_article()), None);
 }

 // ---- section-initiation fixtures ( §5, revised) -------------

 fn gap_fixture(name: &str) -> &'static str {
 match name {
 "eligible" => include_str!("../../tests/fixtures/m04/gap/eligible.html"),
 "different-parent" => {
 include_str!("../../tests/fixtures/m04/gap/different-parent.html")
 }
 "deeper-heading" => include_str!("../../tests/fixtures/m04/gap/deeper-heading.html"),
 "no-content" => include_str!("../../tests/fixtures/m04/gap/no-content.html"),
 "paragraph-first" => {
 include_str!("../../tests/fixtures/m04/gap/paragraph-first.html")
 }
 other => panic!("unknown fixture {other}"),
 }
 }

 /// The simulated delivered selection: the fixture's `data-delivered` blocks
 /// only (headings and the marked drop are not in the article).
 fn fixture_article(name: &str) -> String {
 let document = Html::parse_document(gap_fixture(name));
 let selector = scraper::Selector::parse("[data-delivered]").expect("static data selector");
 let mut out = String::from("<div>");
 for element in document.select(&selector) {
 out.push_str(&element.html());
 }
 out.push_str("</div>");
 out
 }

 #[test]
 fn an_eligible_heading_marked_sibling_is_repaired() {
 let document = Html::parse_document(gap_fixture("eligible"));
 let repaired = repair_structural_gaps(&document, &fixture_article("eligible"))
.expect("eligible section repair");
 assert!(repaired.contains("DROPPED_MARKER"), "{repaired}");
 assert!(
 repaired.contains("The dropped section body states"),
 "{repaired}"
);
 assert_eq!(
 repair_structural_gaps(&document, &repaired),
 None,
 "second pass must be a no-op"
);
 }

 #[test]
 fn a_heading_in_a_different_parent_is_not_admitted() {
 let document = Html::parse_document(gap_fixture("different-parent"));
 assert_eq!(
 repair_structural_gaps(&document, &fixture_article("different-parent")),
 None
);
 }

 #[test]
 fn a_deeper_heading_is_not_admitted() {
 let document = Html::parse_document(gap_fixture("deeper-heading"));
 assert_eq!(
 repair_structural_gaps(&document, &fixture_article("deeper-heading")),
 None
);
 }

 #[test]
 fn a_gap_without_following_content_is_not_admitted() {
 let document = Html::parse_document(gap_fixture("no-content"));
 assert_eq!(
 repair_structural_gaps(&document, &fixture_article("no-content")),
 None
);
 }

 #[test]
 fn a_paragraph_first_gap_is_not_admitted() {
 let document = Html::parse_document(gap_fixture("paragraph-first"));
 assert_eq!(
 repair_structural_gaps(&document, &fixture_article("paragraph-first")),
 None
);
 }

 #[test]
 fn edges_are_never_repaired() {
 let document = html(&format!(
 r#"<html><head><title>Edges</title></head><body><article>
 <p>Leading chrome that is long enough to look like a content block but sits before the article.</p>
 <h1>1. Delivered section</h1>
 {}
 <ol><li>The kept third section body explains the checklist before migrating.</li></ol>
 <p>Trailing chrome that is long enough to look like a content block after the article.</p>
 </article></body></html>"#,
 delivered_paragraphs()
));
 assert_eq!(repair_structural_gaps(&document, &gap_article()), None);
 }
}
