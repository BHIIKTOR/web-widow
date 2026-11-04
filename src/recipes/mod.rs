//! Extraction recipe library: names and versions over the
//! content-addressed `profiles` table.
//!
//! A recipe is **CSS-spec only** in v1: the `schema` is exactly the `extract`
//! envelope (`{container?, fields}`), validated by the same
//! [`crate::extract::spec`] validator with the same JSON pointers. Tabular
//! (`extract_rows`) specs are not recipes; they are per-call arguments.
//!
//! Storage stays in the `recipes`/`recipe_versions` tables while the spec text
//! stays in `profiles`, keyed by the validator's canonical hash. A version row is
//! a *pointer* to that hash, so advancing a recipe never rewrites a monitor's
//! `profile_hash` and `recipe_apply` never mutates a spec.

use std::sync::OnceLock;

use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::core::error::WidowError;

/// `(description)` upper bound; the spec itself is bounded by the validator.
pub const MAX_DESCRIPTION_CHARS: usize = 200;
/// Rows returned by `recipe_list` when the caller is silent.
pub const DEFAULT_LIST_LIMIT: usize = 50;
/// Hard ceiling for `recipe_list`'s `limit`.
pub const MAX_LIST_LIMIT: usize = 200;

/// `project:slug`, lowercase, 1..=64 characters per segment.
fn name_pattern() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| {
        Regex::new(r"^[a-z0-9][a-z0-9._-]{0,63}(:[a-z0-9][a-z0-9._-]{0,63})?$")
            .expect("static recipe-name pattern")
    })
}

/// The identity rule for a recipe name. Uppercase, whitespace and slashes are
/// rejected rather than normalized: a name is a stable identity, not input to
/// tidy up.
pub fn validate_name(raw: &str) -> Result<(), WidowError> {
    if name_pattern().is_match(raw) {
        return Ok(());
    }
    Err(WidowError::SchemaInvalid {
        pointer: "/name".to_string(),
        message: format!(
            "`{raw}` is not a recipe name: use lowercase `[a-z0-9][a-z0-9._-]{{0,63}}`, optionally one `:slug` segment (for example `shop:products`)"
        ),
    })
}

/// Description: optional, but when present it is 1..=200 characters (an empty
/// string is rejected, not silently dropped).
pub fn validate_description(raw: &str) -> Result<(), WidowError> {
    let chars = raw.chars().count();
    if (1..=MAX_DESCRIPTION_CHARS).contains(&chars) {
        return Ok(());
    }
    Err(WidowError::SchemaInvalid {
        pointer: "/description".to_string(),
        message: format!(
            "description must be 1..={MAX_DESCRIPTION_CHARS} characters when present, got {chars}"
        ),
    })
}

/// `limit` is 1..=200.
pub fn check_list_limit(value: usize) -> Result<usize, WidowError> {
    if (1..=MAX_LIST_LIMIT).contains(&value) {
        Ok(value)
    } else {
        Err(WidowError::SchemaInvalid {
            pointer: "/limit".to_string(),
            message: format!("limit must be 1..={MAX_LIST_LIMIT}, got {value}"),
        })
    }
}

/// `recipe_save` input: the recipe name, the `extract` schema, optional metadata.
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct RecipeSaveInput {
    /// Recipe name (`project:slug` convention, lowercase).
    pub name: String,
    /// The extraction schema, the same shape `extract` takes: `{container?, fields}`.
    pub schema: Value,
    /// Optional human-facing description, 1..=200 characters.
    #[serde(default)]
    pub description: Option<String>,
}

/// `recipe_save` result. `changed` reports whether a new version was written; a
/// metadata-only save keeps the version.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RecipeSavePayload {
    pub name: String,
    pub version: u32,
    pub profile_hash: String,
    /// `true` when this save created or advanced the recipe's version.
    pub changed: bool,
    /// When the current version row was written.
    pub created_at: String,
}

/// `recipe_list` input: prefix filter, optional schema bodies, page size.
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct RecipeListInput {
    /// Literal name prefix (`shop:` lists that project's recipes).
    #[serde(default)]
    pub prefix: Option<String>,
    /// Include each recipe's stored canonical spec (default false).
    #[serde(default)]
    pub include_schema: Option<bool>,
    /// 1..=200 (default 50).
    #[serde(default)]
    pub limit: Option<usize>,
}

/// One recipe as `recipe_list` reports it. `schema` is present only when
/// `include_schema` was true.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RecipeSummary {
    pub name: String,
    pub version: u32,
    pub profile_hash: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub updated_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schema: Option<Value>,
}

/// `recipe_list` result: the page, its size, and the full match count.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RecipeListPayload {
    pub recipes: Vec<RecipeSummary>,
    pub count: usize,
    pub total: usize,
    pub truncated: bool,
}

/// `recipe_apply` input: a recipe version (default current) and the normal
/// `extract` arguments. Nothing here can widen the extraction contract.
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct RecipeApplyInput {
    /// Recipe name.
    pub name: String,
    /// Version to apply; omit for the recipe's current version.
    #[serde(default)]
    pub version: Option<u32>,
    /// Absolute http(s) URL to fetch and extract from.
    pub url: String,
    /// `auto` (default), `never` or `always`, exactly as `extract`.
    #[serde(default)]
    pub render: Option<crate::pipeline::RenderPolicy>,
    /// Prefer a stored extraction for this page and spec (default false).
    #[serde(default)]
    pub use_cache: Option<bool>,
    /// Per-string-value cap, clamped by `[extract]`, exactly as `extract`.
    #[serde(default)]
    pub max_field_chars: Option<usize>,
    /// Total character budget over the records, clamped by `[extract]`.
    #[serde(default)]
    pub max_chars: Option<usize>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_follow_the_project_slug_rule() {
        for good in ["shop", "shop:products", "a", "a.b-c_d", "x9:y0"] {
            validate_name(good).unwrap_or_else(|error| panic!("{good}: {error}"));
        }
        for bad in [
            "",
            "Shop",
            "shop products",
            "shop/products",
            ":shop",
            "shop:",
            "shop:Prod",
            "-shop",
            ".shop",
            &"a".repeat(65),
            // A 65-character *segment* fails even when the total is long.
            &format!("{}:{}", "a".repeat(65), "b"),
        ] {
            let error = validate_name(bad).unwrap_err();
            assert_eq!(error.code().as_str(), "schema_invalid", "{bad:?}");
            assert_eq!(
                error.to_tool_error().detail.as_deref(),
                Some("pointer=/name")
            );
        }
    }

    #[test]
    fn a_64_character_segment_and_two_segments_are_accepted() {
        let single = "a".repeat(64);
        validate_name(&single).unwrap();
        let two = format!("{}:{}", "a".repeat(64), "b".repeat(64));
        validate_name(&two).unwrap();
    }

    #[test]
    fn descriptions_are_bounded_and_never_silently_dropped() {
        assert!(validate_description("price list").is_ok());
        assert!(validate_description(&"é".repeat(200)).is_ok());
        for bad in ["", &"a".repeat(201)] {
            let error = validate_description(bad).unwrap_err();
            assert_eq!(
                error.to_tool_error().detail.as_deref(),
                Some("pointer=/description")
            );
        }
    }

    #[test]
    fn list_limits_are_bounded() {
        assert_eq!(check_list_limit(1).unwrap(), 1);
        assert_eq!(check_list_limit(MAX_LIST_LIMIT).unwrap(), MAX_LIST_LIMIT);
        for bad in [0, MAX_LIST_LIMIT + 1] {
            let error = check_list_limit(bad).unwrap_err();
            assert_eq!(
                error.to_tool_error().detail.as_deref(),
                Some("pointer=/limit")
            );
        }
    }
}
