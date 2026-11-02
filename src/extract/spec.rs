//! Declarative extraction specs: the data an agent supplies to `extract`.
//!
//! The spec is data, never code: `container` (optional) plus a map of fields,
//! each with a CSS selector, an optional attribute, `all`, and one transform from
//! a closed set. Unknown keys, unknown transforms, invalid selectors and
//! `transform: attr` without `attr` are `schema_invalid` with the JSON pointer of
//! the offending node — a silently ignored field is worse than a hard failure.
//!
//! Validation runs the schema derived from these types (`schemars`) through
//! `jsonschema`, which is what supplies instance paths; the typed value is then
//! compiled once into `scraper::Selector`s so evaluation never parses CSS per
//! record or per field.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use scraper::Selector;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::core::error::{Result, WidowError};
use crate::core::url::content_hash;

/// The closed transform set (IMP-CONTRACT-001). No XPath, no regex, no JS.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Transform {
    /// Visible text of the element, trailing/leading whitespace trimmed.
    Text,
    /// Inner HTML of the element.
    Html,
    /// The attribute named by `attr` (required).
    Attr,
    /// Absolute URL resolved against the response's final URL.
    AbsoluteUrl,
    /// A number parsed out of the value; thousands separators are understood.
    Number,
    /// Visible text with internal whitespace runs collapsed to one space.
    Trim,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FieldSpec {
    /// CSS selector, evaluated against descendants of the record root.
    pub selector: String,
    /// Attribute to read. When set without `transform`, `attr` is implied;
    /// an explicit `text`/`html` transform with `attr` is `schema_invalid`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attr: Option<String>,
    /// Collect every match into an array instead of the first one.
    #[serde(default, skip_serializing_if = "is_false")]
    #[schemars(schema_with = "bool_schema")]
    pub all: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transform: Option<Transform>,
}

fn is_false(value: &bool) -> bool {
    !*value
}

fn bool_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({ "type": "boolean", "default": false })
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ExtractionSpec {
    /// CSS selector for the repeating record root. Omit for a single-record page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container: Option<String>,
    /// Field name -> field spec. At least one field is required.
    pub fields: BTreeMap<String, FieldSpec>,
}

impl FieldSpec {
    /// Effective transform: an explicit one wins, then `attr`, then `text`.
    pub fn effective_transform(&self) -> Transform {
        match (self.transform, &self.attr) {
            (Some(transform), _) => transform,
            (None, Some(_)) => Transform::Attr,
            (None, None) => Transform::Text,
        }
    }
}

/// A validated spec plus its canonical form and content hash.
#[derive(Debug, Clone)]
pub struct ValidatedSpec {
    pub spec: ExtractionSpec,
    /// Canonical JSON (sorted keys, no optional nulls): the profile identity.
    pub canonical_json: String,
    /// `spec/v1/<BLAKE3-256 of canonical_json>`; the `profiles.hash` value.
    /// The prefix is the semantic version of the spec evaluation rules (B05
    /// carry-forward): a future refinement that changes what a field means
    /// bumps the prefix instead of silently reusing an old profile row.
    pub hash: String,
}

/// Validate a raw agent-supplied spec.
///
/// Order: JSON Schema (shape, unknown keys, closed transform enum) -> typed
/// decode -> cross-field rules -> selector compilation. The first failure by
/// pointer wins, so the error is stable.
pub fn validate(raw: &Value) -> Result<ValidatedSpec> {
    let schema = serde_json::to_value(schemars::schema_for!(ExtractionSpec))
        .map_err(|error| WidowError::internal(format!("cannot render spec schema: {error}")))?;
    let validator = jsonschema::validator_for(&schema).map_err(|error| {
        WidowError::internal(format!("cannot compile the spec schema: {error}"))
    })?;

    let mut errors: Vec<(String, String)> = validator
        .iter_errors(raw)
        .map(|error| (error.instance_path().to_string(), error.to_string()))
        .collect();
    errors.sort();
    if let Some((pointer, message)) = errors.into_iter().next() {
        return Err(WidowError::SchemaInvalid { pointer, message });
    }

    let spec: ExtractionSpec =
        serde_json::from_value(raw.clone()).map_err(|error| WidowError::SchemaInvalid {
            pointer: String::new(),
            message: error.to_string(),
        })?;
    check_cross_field_rules(&spec)?;
    compile(&spec)?;

    let canonical_json = canonical_json(&spec)?;
    // Versioned profile identity. B02 shipped unversioned hashes; no snapshot
    // existed then, so there is nothing to migrate: the first B05 check writes
    // the v1 identity.
    let hash = spec_profile_hash(&canonical_json);
    Ok(ValidatedSpec {
        spec,
        canonical_json,
        hash,
    })
}

/// The profile identity for a canonical spec JSON. One definition so `extract`,
/// crawl jobs and monitor snapshots cannot disagree about it.
pub fn spec_profile_hash(canonical_json: &str) -> String {
    content_hash(format!("spec/v1/{canonical_json}").as_bytes())
}

fn check_cross_field_rules(spec: &ExtractionSpec) -> Result<()> {
    if spec.fields.is_empty() {
        return Err(WidowError::SchemaInvalid {
            pointer: "/fields".to_string(),
            message: "a spec needs at least one field".to_string(),
        });
    }
    for (name, field) in &spec.fields {
        if field.effective_transform() == Transform::Attr && field.attr.is_none() {
            return Err(WidowError::SchemaInvalid {
                pointer: format!("/fields/{name}/attr"),
                message: "transform `attr` requires an `attr` name".to_string(),
            });
        }
        // `attr` is only meaningful for the transforms that read a value from the
        // element; combining it with `text` or `html` would silently ignore it.
        if field.attr.is_some()
            && let Some(transform @ (Transform::Text | Transform::Html)) = field.transform
        {
            return Err(WidowError::SchemaInvalid {
                pointer: format!("/fields/{name}/attr"),
                message: format!(
                    "`attr` is not used by transform `{}`; use one of attr, absolute_url, number or trim, or drop `attr`",
                    match transform {
                        Transform::Text => "text",
                        Transform::Html => "html",
                        _ => unreachable!("pattern is limited to text and html"),
                    }
                ),
            });
        }
        if name.trim().is_empty() {
            return Err(WidowError::SchemaInvalid {
                pointer: "/fields".to_string(),
                message: "field names must not be empty".to_string(),
            });
        }
    }
    Ok(())
}

fn canonical_json(spec: &ExtractionSpec) -> Result<String> {
    // BTreeMap fields + serde_json's sorted maps + skipped None/defaults give a
    // stable byte string for the same spec written in any key order.
    serde_json::to_string(spec)
        .map_err(|error| WidowError::internal(format!("cannot serialize spec: {error}")))
}

/// The container scope: a CSS selector, or the readability article (`:main`).
#[derive(Debug, Clone)]
pub enum Container {
    Css(Selector),
    Main,
}

/// A spec with its CSS selectors parsed once.
#[derive(Debug, Clone)]
pub struct CompiledSpec {
    pub container: Option<Container>,
    pub fields: Vec<(String, CompiledField)>,
}

#[derive(Debug, Clone)]
pub struct CompiledField {
    /// The selector text as the agent wrote it, for error details.
    pub source: String,
    pub selector: Selector,
    pub transform: Transform,
    pub attr: Option<String>,
    pub all: bool,
}

pub fn compile(spec: &ExtractionSpec) -> Result<CompiledSpec> {
    let container = match spec.container.as_deref().map(str::trim) {
        Some(":main") => Some(Container::Main),
        Some(selector) => Some(Container::Css(parse_selector(selector, "/container")?)),
        None => None,
    };
    let mut fields = Vec::with_capacity(spec.fields.len());
    for (name, field) in &spec.fields {
        fields.push((
            name.clone(),
            CompiledField {
                source: field.selector.clone(),
                selector: parse_selector(&field.selector, &format!("/fields/{name}/selector"))?,
                transform: field.effective_transform(),
                attr: field.attr.clone(),
                all: field.all,
            },
        ));
    }
    Ok(CompiledSpec { container, fields })
}

fn parse_selector(text: &str, pointer: &str) -> Result<Selector> {
    if text.trim().is_empty() {
        return Err(WidowError::SchemaInvalid {
            pointer: pointer.to_string(),
            message: "selector must not be empty".to_string(),
        });
    }
    Selector::parse(text).map_err(|error| WidowError::SchemaInvalid {
        pointer: pointer.to_string(),
        message: format!("invalid CSS selector `{text}`: {error:?}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn pointer_of(error: WidowError) -> String {
        match error {
            WidowError::SchemaInvalid { pointer, .. } => pointer,
            other => panic!("expected schema_invalid, got {other}"),
        }
    }

    #[test]
    fn valid_spec_is_canonical_and_hashed() {
        let first = validate(&json!({
            "container": ".card",
            "fields": {
                "name": { "selector": "h2" },
                "href": { "selector": "a", "attr": "href" }
            }
        }))
        .unwrap();
        assert_eq!(first.spec.fields.len(), 2);
        assert_eq!(first.hash.len(), 64);

        // Key order in the input does not change the identity.
        let second = validate(&json!({
            "fields": {
                "href": { "selector": "a", "attr": "href" },
                "name": { "selector": "h2" }
            },
            "container": ".card"
        }))
        .unwrap();
        assert_eq!(first.canonical_json, second.canonical_json);
        assert_eq!(first.hash, second.hash);

        let third = validate(&json!({
            "container": ".card",
            "fields": { "name": { "selector": "h3" } }
        }))
        .unwrap();
        assert_ne!(first.hash, third.hash);
    }

    #[test]
    fn transform_defaults_follow_attr() {
        let spec = validate(&json!({
            "fields": {
                "a": { "selector": "a", "attr": "href" },
                "b": { "selector": "p" },
                "c": { "selector": "b", "transform": "trim" }
            }
        }))
        .unwrap();
        assert_eq!(spec.spec.fields["a"].effective_transform(), Transform::Attr);
        assert_eq!(spec.spec.fields["b"].effective_transform(), Transform::Text);
        assert_eq!(spec.spec.fields["c"].effective_transform(), Transform::Trim);
    }

    #[test]
    fn unknown_keys_are_rejected_with_a_pointer() {
        let error = validate(&json!({
            "fields": { "name": { "selector": "h2", "regex": "^x$" } }
        }))
        .unwrap_err();
        assert_eq!(error.code(), crate::core::error::ErrorCode::SchemaInvalid);
        let pointer = pointer_of(error);
        assert!(pointer.contains("/fields/name"), "{pointer}");
    }

    #[test]
    fn unknown_transform_is_rejected_with_a_pointer() {
        let error = validate(&json!({
            "fields": { "price": { "selector": ".price", "transform": "xpath" } }
        }))
        .unwrap_err();
        let pointer = pointer_of(error);
        assert!(pointer.contains("/fields/price/transform"), "{pointer}");
    }

    #[test]
    fn missing_selector_is_rejected_with_a_pointer() {
        let error = validate(&json!({ "fields": { "name": {} } })).unwrap_err();
        let pointer = pointer_of(error);
        assert!(pointer.contains("/fields/name"), "{pointer}");
    }

    #[test]
    fn attr_is_allowed_with_the_value_transforms() {
        for transform in ["attr", "absolute_url", "number", "trim"] {
            let spec = validate(&json!({
                "fields": {
                    "x": { "selector": "span", "attr": "data-x", "transform": transform }
                }
            }))
            .unwrap_or_else(|error| panic!("attr + {transform} must validate: {error}"));
            assert_eq!(
                spec.spec.fields["x"].effective_transform(),
                match transform {
                    "attr" => Transform::Attr,
                    "absolute_url" => Transform::AbsoluteUrl,
                    "number" => Transform::Number,
                    "trim" => Transform::Trim,
                    _ => unreachable!(),
                }
            );
        }
    }

    #[test]
    fn attr_with_text_or_html_is_rejected() {
        for transform in ["text", "html"] {
            let error = validate(&json!({
                "fields": {
                    "n": { "selector": "h2", "attr": "href", "transform": transform }
                }
            }))
            .expect_err("attr with a text/html transform must be refused");
            assert!(matches!(error, WidowError::SchemaInvalid { .. }));
            assert_eq!(pointer_of(error), "/fields/n/attr");
        }
    }

    #[test]
    fn attr_transform_requires_an_attribute() {
        let error = validate(&json!({
            "fields": { "href": { "selector": "a", "transform": "attr" } }
        }))
        .unwrap_err();
        assert_eq!(pointer_of(error), "/fields/href/attr");
    }

    #[test]
    fn invalid_css_and_empty_fields_are_rejected() {
        let error = validate(&json!({
            "fields": { "x": { "selector": "div[[" } }
        }))
        .unwrap_err();
        assert_eq!(pointer_of(error), "/fields/x/selector");

        let error = validate(&json!({ "fields": {} })).unwrap_err();
        assert_eq!(pointer_of(error), "/fields");

        let error = validate(&json!({ "container": "", "fields": { "x": { "selector": "p" } } }))
            .unwrap_err();
        assert_eq!(pointer_of(error), "/container");
    }

    #[test]
    fn container_is_optional() {
        let spec = validate(&json!({ "fields": { "title": { "selector": "h1" } } })).unwrap();
        assert!(spec.spec.container.is_none());
        let compiled = compile(&spec.spec).unwrap();
        assert!(compiled.container.is_none());
        assert_eq!(compiled.fields.len(), 1);
    }

    #[test]
    fn main_is_a_reserved_container_and_not_a_field_selector() {
        let spec = validate(&json!({
            "container": "  :main  ",
            "fields": { "title": { "selector": "h1" } }
        }))
        .unwrap();
        let compiled = compile(&spec.spec).unwrap();
        assert!(matches!(compiled.container, Some(Container::Main)));
        assert_eq!(compiled.fields.len(), 1);

        let error = validate(&json!({
            "fields": { "main": { "selector": ":main" } }
        }))
        .unwrap_err();
        assert_eq!(pointer_of(error), "/fields/main/selector");
    }

    #[test]
    fn non_object_specs_are_schema_invalid() {
        for raw in [json!("selector"), json!([]), json!(42), json!(null)] {
            let error = validate(&raw).unwrap_err();
            assert_eq!(error.code(), crate::core::error::ErrorCode::SchemaInvalid);
        }
    }
}
