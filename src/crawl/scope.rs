//! Crawl scope predicate.
//!
//! Pure: the frontier decides admission here, so the rule is testable without a
//! server and cannot drift between the worker and a tool handler.
//!
//! Seeds are enqueued as given (the operator asked for them); the predicate
//! applies to **discovered links**: depth, off-site, excluded patterns and
//! include prefixes.

use std::collections::HashSet;

use regex::Regex;
use url::Url;

use crate::core::error::{Result, WidowError};
use crate::core::url::PageUrl;

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ScopeSpec {
    /// Path prefixes a discovered link must start with (empty = every path).
    pub include_paths: Vec<String>,
    /// Regexes matched against `path?query`; a match excludes the link.
    pub exclude_patterns: Vec<String>,
    pub max_depth: Option<u8>,
    pub allow_offsite: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    Depth,
    Offsite,
    ExcludedPattern,
    OutsideIncludePaths,
}

impl SkipReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Depth => "depth",
            Self::Offsite => "offsite",
            Self::ExcludedPattern => "excluded_pattern",
            Self::OutsideIncludePaths => "outside_include_paths",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Enqueue,
    Skip(SkipReason),
}

#[derive(Debug)]
pub struct Scope {
    include: Vec<String>,
    exclude: Vec<Regex>,
    max_depth: u8,
    allow_offsite: bool,
    sites: HashSet<String>,
}

impl Scope {
    /// Compile a scope. The start sites are the registrable domains (`psl`) of the
    /// seeds, so `www.example.com` and `docs.example.com` count as the same site.
    pub fn compile(spec: &ScopeSpec, seeds: &[PageUrl], max_depth_ceiling: u8) -> Result<Self> {
        let mut exclude = Vec::with_capacity(spec.exclude_patterns.len());
        for (index, pattern) in spec.exclude_patterns.iter().enumerate() {
            let regex = Regex::new(pattern).map_err(|error| WidowError::SchemaInvalid {
                pointer: format!("/scope/exclude_patterns/{index}"),
                message: format!("invalid regular expression `{pattern}`: {error}"),
            })?;
            exclude.push(regex);
        }
        let include: Vec<String> = spec
            .include_paths
            .iter()
            .map(|path| {
                if path.starts_with('/') {
                    path.clone()
                } else {
                    format!("/{path}")
                }
            })
            .collect();
        let sites = seeds.iter().map(PageUrl::site).collect();
        Ok(Self {
            include,
            exclude,
            max_depth: spec
                .max_depth
                .unwrap_or(max_depth_ceiling)
                .min(max_depth_ceiling),
            allow_offsite: spec.allow_offsite,
            sites,
        })
    }

    pub fn max_depth(&self) -> u8 {
        self.max_depth
    }

    pub fn admit(&self, url: &PageUrl, depth: u8) -> Decision {
        if depth > self.max_depth {
            return Decision::Skip(SkipReason::Depth);
        }
        if !self.allow_offsite && !self.sites.contains(&url.site()) {
            return Decision::Skip(SkipReason::Offsite);
        }
        let target = request_target(url.url());
        if self.exclude.iter().any(|regex| regex.is_match(&target)) {
            return Decision::Skip(SkipReason::ExcludedPattern);
        }
        if !self.include.is_empty()
            && !self
                .include
                .iter()
                .any(|prefix| url.url().path().starts_with(prefix.as_str()))
        {
            return Decision::Skip(SkipReason::OutsideIncludePaths);
        }
        Decision::Enqueue
    }
}

/// `path` plus `?query` when present: the form the contract's exclusion patterns
/// are written against (`\?page=`).
fn request_target(url: &Url) -> String {
    match url.query() {
        Some(query) => format!("{}?{query}", url.path()),
        None => url.path().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(text: &str) -> PageUrl {
        PageUrl::parse(text).expect("valid url")
    }

    fn scope(spec: ScopeSpec, seeds: &[&str]) -> Scope {
        let seeds: Vec<PageUrl> = seeds.iter().map(|seed| url(seed)).collect();
        Scope::compile(&spec, &seeds, 5).expect("scope compiles")
    }

    #[test]
    fn depth_is_bounded_and_the_ceiling_wins() {
        let scope = scope(
            ScopeSpec {
                max_depth: Some(9),
                ..ScopeSpec::default()
            },
            &["https://example.com/"],
        );
        assert_eq!(scope.max_depth(), 5);
        assert_eq!(
            scope.admit(&url("https://example.com/a"), 5),
            Decision::Enqueue
        );
        assert_eq!(
            scope.admit(&url("https://example.com/a"), 6),
            Decision::Skip(SkipReason::Depth)
        );
    }

    #[test]
    fn offsite_links_are_skipped_unless_allowed() {
        let strict = scope(ScopeSpec::default(), &["https://example.com/"]);
        assert_eq!(
            strict.admit(&url("https://other.example/x"), 1),
            Decision::Skip(SkipReason::Offsite)
        );
        let permissive = scope(
            ScopeSpec {
                allow_offsite: true,
                ..ScopeSpec::default()
            },
            &["https://example.com/"],
        );
        assert_eq!(
            permissive.admit(&url("https://other.example/x"), 1),
            Decision::Enqueue
        );
    }

    #[test]
    fn subdomains_of_a_seed_site_are_onsite() {
        let scope = scope(ScopeSpec::default(), &["https://docs.example.com/"]);
        assert_eq!(
            scope.admit(&url("https://www.example.com/page"), 1),
            Decision::Enqueue
        );
        assert_eq!(
            scope.admit(&url("https://example.org/page"), 1),
            Decision::Skip(SkipReason::Offsite)
        );
    }

    #[test]
    fn include_paths_are_prefixes_on_the_path() {
        let scope = scope(
            ScopeSpec {
                include_paths: vec!["docs".to_string()],
                ..ScopeSpec::default()
            },
            &["https://example.com/"],
        );
        assert_eq!(
            scope.admit(&url("https://example.com/docs/a"), 1),
            Decision::Enqueue
        );
        assert_eq!(
            scope.admit(&url("https://example.com/docsx/a"), 1),
            Decision::Enqueue,
            "prefix semantics, not a path segment match"
        );
        assert_eq!(
            scope.admit(&url("https://example.com/blog/a"), 1),
            Decision::Skip(SkipReason::OutsideIncludePaths)
        );
    }

    #[test]
    fn exclude_patterns_see_the_query_string() {
        let scope = scope(
            ScopeSpec {
                exclude_patterns: vec![r"\?page=".to_string()],
                ..ScopeSpec::default()
            },
            &["https://example.com/"],
        );
        assert_eq!(
            scope.admit(&url("https://example.com/list?page=2"), 1),
            Decision::Skip(SkipReason::ExcludedPattern)
        );
        assert_eq!(
            scope.admit(&url("https://example.com/list"), 1),
            Decision::Enqueue
        );
    }

    #[test]
    fn invalid_patterns_are_schema_invalid_with_a_pointer() {
        let error = Scope::compile(
            &ScopeSpec {
                exclude_patterns: vec!["(".to_string()],
                ..ScopeSpec::default()
            },
            &[url("https://example.com/")],
            5,
        )
        .unwrap_err();
        let tool = error.to_tool_error();
        assert_eq!(tool.code.as_str(), "schema_invalid");
        assert!(
            tool.detail
                .unwrap_or_default()
                .contains("/scope/exclude_patterns/0")
        );
    }

    #[test]
    fn reasons_are_stable_strings() {
        assert_eq!(SkipReason::Depth.as_str(), "depth");
        assert_eq!(SkipReason::Offsite.as_str(), "offsite");
        assert_eq!(SkipReason::ExcludedPattern.as_str(), "excluded_pattern");
        assert_eq!(
            SkipReason::OutsideIncludePaths.as_str(),
            "outside_include_paths"
        );
    }
}
