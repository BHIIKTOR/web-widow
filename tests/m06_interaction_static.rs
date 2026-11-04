//! v0.6-11 — the static-only build has no browser, so every `interact_*`
//! operation is a typed `render_unavailable` and no session or row is created.

#![cfg(not(feature = "render"))]

mod common;

use std::sync::Arc;

use web_widow::core::ErrorCode;
use web_widow::interaction::{
    InteractActInput, InteractCloseInput, InteractOpenInput, InteractStateInput, InteractionManager,
};

fn manager(env: &common::TestEnv) -> InteractionManager {
    InteractionManager::new(Arc::clone(&env.config), Arc::clone(&env.pipeline))
}

#[tokio::test]
async fn every_interaction_tool_is_render_unavailable_and_creates_no_session() {
    let env = common::TestEnv::new(common::test_config()).await;
    let manager = manager(&env);

    let error = manager
        .open(InteractOpenInput {
            url: "http://127.0.0.1:1/static-only".to_string(),
            render: None,
            viewport_width: None,
            viewport_height: None,
        })
        .await
        .expect_err("the static build must not open a session");
    assert_eq!(error.code(), ErrorCode::RenderUnavailable);
    assert!(error.to_string().contains("render"), "{error}");

    let id = "00000000-0000-7000-8000-000000000000".to_string();
    for error in [
        manager
            .act(InteractActInput {
                session_id: id.clone(),
                action: serde_json::json!({"kind": "scroll", "to": "top"}),
            })
            .await
            .expect_err("act"),
        manager
            .state(InteractStateInput {
                session_id: id.clone(),
                screenshot: Some(false),
                console_limit: None,
            })
            .await
            .expect_err("state"),
        manager
            .close(InteractCloseInput { session_id: id })
            .await
            .expect_err("close"),
    ] {
        assert_eq!(error.code(), ErrorCode::RenderUnavailable);
    }

    let db = env.db();
    let screenshots: i64 = db
        .query_row("SELECT COUNT(*) FROM screenshots", [], |row| row.get(0))
        .unwrap();
    assert_eq!(screenshots, 0);
    assert_eq!(env.blob_count(), 0);
}
