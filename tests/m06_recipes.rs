//! v0.6 — the extraction recipe library: save → list → apply, versioning,
//! the name/description/spec validation pointers, and the FTS projection rule.
//!
//! Lane-local test file: it uses the shared loopback fixture and `TestEnv` but
//! never edits `tests/common/mod.rs`.

mod common;

use common::{TestEnv, start_fixture, test_config};
use serde_json::{Value, json};
use web_widow::ErrorCode;
use web_widow::extract::spec;
use web_widow::fts::SearchContentInput;
use web_widow::monitor::MonitorCreateInput;
use web_widow::pipeline::{ExtractInput, RenderPolicy};
use web_widow::recipes::{RecipeApplyInput, RecipeListInput, RecipeSaveInput};

/// v1: two fields from the shared `/extract/items` fixture.
fn spec_v1() -> Value {
    json!({
        "container": ".item",
        "fields": {
            "name": {"selector": "h2"},
            "price": {"selector": ".price", "transform": "number"}
        }
    })
}

/// v2: the same records plus an absolute link, so the canonical hash changes.
fn spec_v2() -> Value {
    json!({
        "container": ".item",
        "fields": {
            "name": {"selector": "h2"},
            "price": {"selector": ".price", "transform": "number"},
            "href": {"selector": "a", "transform": "absolute_url"}
        }
    })
}

async fn env() -> TestEnv {
    TestEnv::new(test_config()).await
}

fn save(name: &str, schema: Value, description: Option<&str>) -> RecipeSaveInput {
    RecipeSaveInput {
        name: name.to_string(),
        schema,
        description: description.map(str::to_string),
    }
}

fn apply(name: &str, version: Option<u32>, url: &str) -> RecipeApplyInput {
    RecipeApplyInput {
        name: name.to_string(),
        version,
        url: url.to_string(),
        render: Some(RenderPolicy::Never),
        use_cache: None,
        max_field_chars: None,
        max_chars: None,
    }
}

fn extract_input(url: &str, schema: Value) -> ExtractInput {
    ExtractInput {
        url: url.to_string(),
        schema,
        render: Some(RenderPolicy::Never),
        use_cache: None,
        max_field_chars: None,
        max_chars: None,
    }
}

fn canonical(schema: &Value) -> String {
    spec::validate(schema).expect("valid spec").canonical_json
}

// ---------------------------------------------------------------------------
// save → list → apply equals extract, record for record
// ---------------------------------------------------------------------------

#[tokio::test]
async fn save_list_and_apply_match_extract_record_for_record() {
    let fixture = start_fixture().await;
    let env = env().await;
    let url = fixture.url("127.0.0.1", "/extract/items");

    let baseline = env
        .pipeline
        .extract(extract_input(&url, spec_v1()))
        .await
        .expect("extract baseline");

    let saved = env
        .pipeline
        .recipe_save(save("fixture:items", spec_v1(), Some("fixture cards")))
        .await
        .expect("save");
    assert_eq!(saved.name, "fixture:items");
    assert_eq!(saved.version, 1);
    assert!(saved.changed);
    // The profile hash is the validator's BLAKE3 identity (64 hex chars); the
    // `spec/v1/` rule version is part of the hashed input, not the output.
    assert_eq!(saved.profile_hash.len(), 64, "{}", saved.profile_hash);
    assert!(saved.profile_hash.chars().all(|c| c.is_ascii_hexdigit()));

    let listed = env
        .pipeline
        .recipe_list(RecipeListInput {
            prefix: Some("fixture:".to_string()),
            include_schema: Some(true),
            limit: None,
        })
        .await
        .expect("list");
    assert_eq!(listed.count, 1);
    assert_eq!(listed.total, 1);
    assert!(!listed.truncated);
    assert_eq!(listed.recipes[0].name, "fixture:items");
    assert_eq!(listed.recipes[0].version, 1);
    assert_eq!(listed.recipes[0].profile_hash, saved.profile_hash);
    assert_eq!(
        listed.recipes[0].description.as_deref(),
        Some("fixture cards")
    );
    // `include_schema` round-trips the canonical spec exactly.
    let stored = listed.recipes[0].schema.as_ref().expect("schema included");
    assert_eq!(
        stored,
        &serde_json::from_str::<Value>(&canonical(&spec_v1())).unwrap()
    );

    let applied = env
        .pipeline
        .recipe_apply(apply("fixture:items", None, &url))
        .await
        .expect("apply");
    assert_eq!(applied.recipe, "fixture:items");
    assert_eq!(applied.version, 1);
    assert_eq!(applied.profile_hash, saved.profile_hash);
    assert_eq!(applied.extraction.records, baseline.records);
    assert_eq!(applied.extraction.record_count, baseline.record_count);
    assert_eq!(
        applied.extraction.field_errors.len(),
        baseline.field_errors.len()
    );
    assert_eq!(applied.extraction.content_type, "text/html");

    // One profile row for both paths: the hash identity is the spec, not the tool.
    let profiles: i64 = env
        .db()
        .query_row("SELECT COUNT(*) FROM profiles", [], |row| row.get(0))
        .unwrap();
    assert_eq!(profiles, 1);
}

// ---------------------------------------------------------------------------
// no-op saves, version bumps, old versions, monitor isolation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_identical_save_is_a_no_op_and_a_changed_spec_bumps_to_v2() {
    let env = env().await;

    let first = env
        .pipeline
        .recipe_save(save("shop:products", spec_v1(), None))
        .await
        .unwrap();
    assert_eq!(first.version, 1);
    assert!(first.changed);

    // Byte-identical spec and metadata: nothing moves.
    let again = env
        .pipeline
        .recipe_save(save("shop:products", spec_v1(), None))
        .await
        .unwrap();
    assert_eq!(again.version, 1);
    assert!(!again.changed);
    assert_eq!(again.profile_hash, first.profile_hash);
    assert_eq!(again.created_at, first.created_at);

    // A description-only save is metadata: same version, updated description.
    let described = env
        .pipeline
        .recipe_save(save("shop:products", spec_v1(), Some("product cards")))
        .await
        .unwrap();
    assert_eq!(described.version, 1);
    assert!(!described.changed);

    // A real spec change writes version 2 and keeps version 1 resolvable.
    let bumped = env
        .pipeline
        .recipe_save(save("shop:products", spec_v2(), Some("product cards")))
        .await
        .unwrap();
    assert_eq!(bumped.version, 2);
    assert!(bumped.changed);
    assert_ne!(bumped.profile_hash, first.profile_hash);

    let listed = env
        .pipeline
        .recipe_list(RecipeListInput {
            prefix: None,
            include_schema: Some(false),
            limit: None,
        })
        .await
        .unwrap();
    assert_eq!(listed.count, 1, "one recipe, two versions");
    assert_eq!(listed.recipes[0].version, 2);
    assert_eq!(
        listed.recipes[0].description.as_deref(),
        Some("product cards")
    );

    let db = env.db();
    let recipes: i64 = db
        .query_row("SELECT COUNT(*) FROM recipes", [], |row| row.get(0))
        .unwrap();
    let versions: i64 = db
        .query_row("SELECT COUNT(*) FROM recipe_versions", [], |row| row.get(0))
        .unwrap();
    assert_eq!((recipes, versions), (1, 2));
    let current: i64 = db
        .query_row(
            "SELECT current_version FROM recipes WHERE name = 'shop:products'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(current, 2);
    // `profiles.name` stays NULL: the recipe table is the naming layer.
    let named: i64 = db
        .query_row(
            "SELECT COUNT(*) FROM profiles WHERE name IS NOT NULL",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(named, 0);
}

#[tokio::test]
async fn an_old_version_still_applies_and_a_monitor_keeps_its_profile_hash() {
    let fixture = start_fixture().await;
    let env = env().await;
    let url = fixture.url("127.0.0.1", "/extract/items");

    let v1 = env
        .pipeline
        .recipe_save(save("fixture:items", spec_v1(), None))
        .await
        .unwrap();

    // A monitor created on v1's profile hash (through the normal monitor path).
    let monitor = env
        .monitors
        .create(MonitorCreateInput {
            url: url.clone(),
            mode: None,
            schema: Some(spec_v1()),
            interval_s: None,
            ignore_selectors: Vec::new(),
            label: Some("recipe:v1".to_string()),
        })
        .await
        .expect("monitor create");
    let monitor_hash: String = env
        .db()
        .query_row(
            "SELECT profile_hash FROM monitors WHERE id = ?1",
            [monitor.monitor_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(monitor_hash, v1.profile_hash);

    // Advancing the recipe must not rewrite the monitor's identity.
    let v2 = env
        .pipeline
        .recipe_save(save("fixture:items", spec_v2(), None))
        .await
        .unwrap();
    assert_eq!(v2.version, 2);
    let after_hash: String = env
        .db()
        .query_row(
            "SELECT profile_hash FROM monitors WHERE id = ?1",
            [monitor.monitor_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(after_hash, monitor_hash);
    let profiles: i64 = env
        .db()
        .query_row("SELECT COUNT(*) FROM profiles", [], |row| row.get(0))
        .unwrap();
    assert_eq!(profiles, 2, "v1 and v2 are two distinct stored specs");

    // v1 still applies, with v1's identity and v1's records.
    let applied_v1 = env
        .pipeline
        .recipe_apply(apply("fixture:items", Some(1), &url))
        .await
        .expect("apply v1");
    assert_eq!(applied_v1.version, 1);
    assert_eq!(applied_v1.profile_hash, v1.profile_hash);
    assert!(applied_v1.extraction.records[0].get("href").is_none());

    // The current version applies with the v2 fields.
    let applied_current = env
        .pipeline
        .recipe_apply(apply("fixture:items", None, &url))
        .await
        .expect("apply current");
    assert_eq!(applied_current.version, 2);
    assert_eq!(applied_current.profile_hash, v2.profile_hash);
    assert!(applied_current.extraction.records[0].get("href").is_some());
}

// ---------------------------------------------------------------------------
// validation pointers and typed not_found
// ---------------------------------------------------------------------------

#[tokio::test]
async fn name_description_and_spec_validation_keep_their_pointers() {
    let env = env().await;

    for (name, pointer) in [
        ("Shop", "/name"),
        ("shop products", "/name"),
        ("shop/products", "/name"),
        (":shop", "/name"),
        ("", "/name"),
    ] {
        let error = env
            .pipeline
            .recipe_save(save(name, spec_v1(), None))
            .await
            .expect_err("invalid name");
        let tool = error.to_tool_error();
        assert_eq!(tool.code.as_str(), "schema_invalid", "{name:?}");
        let expected = format!("pointer={pointer}");
        assert_eq!(tool.detail.as_deref(), Some(expected.as_str()), "{name:?}");
    }

    for description in ["", &"a".repeat(201)] {
        let error = env
            .pipeline
            .recipe_save(save("shop:ok", spec_v1(), Some(description)))
            .await
            .expect_err("invalid description");
        assert_eq!(
            error.to_tool_error().detail.as_deref(),
            Some("pointer=/description")
        );
    }

    // The spec validator's own pointer survives the recipe layer unchanged.
    let bad_spec = json!({"fields": {"price": {"selector": ".p", "transform": "explode"}}});
    let error = env
        .pipeline
        .recipe_save(save("shop:ok", bad_spec, None))
        .await
        .expect_err("invalid spec");
    let tool = error.to_tool_error();
    assert_eq!(tool.code.as_str(), "schema_invalid");
    assert_eq!(
        tool.detail.as_deref(),
        Some("pointer=/fields/price/transform")
    );

    for limit in [0, 201] {
        let error = env
            .pipeline
            .recipe_list(RecipeListInput {
                prefix: None,
                include_schema: None,
                limit: Some(limit),
            })
            .await
            .expect_err("invalid limit");
        assert_eq!(
            error.to_tool_error().detail.as_deref(),
            Some("pointer=/limit")
        );
    }
}

#[tokio::test]
async fn unknown_recipes_versions_and_version_zero_are_typed() {
    let env = env().await;

    let error = env
        .pipeline
        .recipe_apply(apply("shop:missing", None, "http://127.0.0.1:1/x"))
        .await
        .expect_err("unknown recipe");
    assert_eq!(error.code(), ErrorCode::NotFound);

    env.pipeline
        .recipe_save(save("shop:here", spec_v1(), None))
        .await
        .unwrap();
    let error = env
        .pipeline
        .recipe_apply(apply("shop:here", Some(9), "http://127.0.0.1:1/x"))
        .await
        .expect_err("unknown version");
    assert_eq!(error.code(), ErrorCode::NotFound);
    assert!(error.to_string().contains("version 9"), "{error}");

    let error = env
        .pipeline
        .recipe_apply(apply("shop:here", Some(0), "http://127.0.0.1:1/x"))
        .await
        .expect_err("version zero");
    assert_eq!(error.code(), ErrorCode::SchemaInvalid);
    assert_eq!(
        error.to_tool_error().detail.as_deref(),
        Some("pointer=/version")
    );
}

// ---------------------------------------------------------------------------
// listing: prefix, pagination, literal wildcards
// ---------------------------------------------------------------------------

#[tokio::test]
async fn listing_paginates_and_treats_a_prefix_literally() {
    let env = env().await;
    for name in [
        "shop:alpha",
        "shop:beta",
        "shop:gamma",
        "other:one",
        "a_b",
        "axb",
    ] {
        env.pipeline
            .recipe_save(save(name, spec_v1(), None))
            .await
            .unwrap();
    }

    let all = env
        .pipeline
        .recipe_list(RecipeListInput {
            prefix: None,
            include_schema: None,
            limit: None,
        })
        .await
        .unwrap();
    assert_eq!(all.total, 6);
    assert_eq!(all.count, 6);
    assert_eq!(
        all.recipes
            .iter()
            .map(|recipe| recipe.name.as_str())
            .collect::<Vec<_>>(),
        vec![
            "a_b",
            "axb",
            "other:one",
            "shop:alpha",
            "shop:beta",
            "shop:gamma"
        ]
    );

    let shop = env
        .pipeline
        .recipe_list(RecipeListInput {
            prefix: Some("shop:".to_string()),
            include_schema: None,
            limit: Some(2),
        })
        .await
        .unwrap();
    assert_eq!(shop.total, 3);
    assert_eq!(shop.count, 2);
    assert!(shop.truncated);

    // `_` is a LIKE wildcard; a literal prefix must not match `axb`.
    let underscore = env
        .pipeline
        .recipe_list(RecipeListInput {
            prefix: Some("a_".to_string()),
            include_schema: None,
            limit: None,
        })
        .await
        .unwrap();
    assert_eq!(underscore.total, 1, "{:?}", underscore.recipes);
    assert_eq!(underscore.recipes[0].name, "a_b");

    // A prefix with no match is an empty page, not an error.
    let none = env
        .pipeline
        .recipe_list(RecipeListInput {
            prefix: Some("nope".to_string()),
            include_schema: None,
            limit: None,
        })
        .await
        .unwrap();
    assert_eq!((none.count, none.total, none.truncated), (0, 0, false));
}

// ---------------------------------------------------------------------------
// a fresh store starts with no recipes
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_fresh_store_has_no_recipes() {
    let env = env().await;
    let listed = env
        .pipeline
        .recipe_list(RecipeListInput {
            prefix: None,
            include_schema: Some(true),
            limit: None,
        })
        .await
        .unwrap();
    assert_eq!((listed.count, listed.total), (0, 0));
    let db = env.db();
    for table in ["recipes", "recipe_versions"] {
        let rows: i64 = db
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(rows, 0, "{table} must start empty");
    }
}

// ---------------------------------------------------------------------------
// applying stores an extraction and indexes row text fill-empty
// ---------------------------------------------------------------------------

#[tokio::test]
async fn applying_a_recipe_stores_and_indexes_row_text_fill_empty() {
    let fixture = start_fixture().await;
    let url = fixture.url("127.0.0.1", "/extract/items");

    // Only the apply runs: the record text gives the page its indexed body.
    let apply_only = env().await;
    apply_only
        .pipeline
        .recipe_save(save("fixture:items", spec_v1(), None))
        .await
        .unwrap();
    apply_only
        .pipeline
        .recipe_apply(apply("fixture:items", None, &url))
        .await
        .unwrap();
    let indexed: String = apply_only
        .db()
        .query_row("SELECT body FROM pages_fts LIMIT 1", [], |row| row.get(0))
        .unwrap();
    assert!(indexed.contains("First item"), "{indexed}");
    let hits = apply_only
        .content
        .search(SearchContentInput {
            query: "First item".to_string(),
            limit: None,
        })
        .await
        .unwrap();
    assert_eq!(hits.results.len(), 1, "{hits:?}");
    assert_eq!(hits.results[0].url, url);

    // The delivered page body wins: a later apply may not replace it.
    let page_first = env().await;
    page_first
        .pipeline
        .recipe_save(save("fixture:items", spec_v1(), None))
        .await
        .unwrap();
    page_first.fetch(&url).await.expect("page fetch");
    let before: String = page_first
        .db()
        .query_row("SELECT body FROM pages_fts LIMIT 1", [], |row| row.get(0))
        .unwrap();
    page_first
        .pipeline
        .recipe_apply(apply("fixture:items", None, &url))
        .await
        .unwrap();
    let after: String = page_first
        .db()
        .query_row("SELECT body FROM pages_fts LIMIT 1", [], |row| row.get(0))
        .unwrap();
    assert_eq!(
        after, before,
        "the projection replaced the delivered page body"
    );
    assert!(after.contains("First item"), "{after}");
}
