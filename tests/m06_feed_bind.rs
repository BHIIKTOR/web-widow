#![cfg(feature = "http")]

//! C3 rework — `http::serve` refuses a non-loopback bind when the feed is on.
//!
//! Binds `0.0.0.0:0` (no external traffic: `serve` returns before accepting)
//! and checks the listener is dropped, so the guard cannot be bypassed by a
//! library caller that does not go through `main`.

mod common;

use std::net::TcpListener;
use std::sync::Arc;

use common::{TestEnv, test_config};
use tokio_util::sync::CancellationToken;
use web_widow::events::{EventsService, FeedRuntime};
use web_widow::mcp::WebWidowServer;
use web_widow::mcp::http::{FeedRoute, serve};
use web_widow::robots_cache::RobotsCacheService;

#[tokio::test]
async fn serve_refuses_a_non_loopback_bind_with_the_feed_enabled() {
 let env = TestEnv::new(test_config()).await;
 let runtime = Arc::new(FeedRuntime::default());
 let events = Arc::new(EventsService::new(
 Arc::clone(&env.store),
 env.config.events.clone(),
 Arc::clone(&runtime),
 false));
 let interactions = Arc::new(web_widow::interaction::InteractionManager::new(
 Arc::clone(&env.config),
 Arc::clone(&env.pipeline)));
 let robots_cache = Arc::new(RobotsCacheService::new(
 Arc::clone(&env.store),
 env.config.robots.user_agent.clone()));
 let server = WebWidowServer::new(
 Arc::clone(&env.pipeline),
 Arc::clone(&env.crawler),
 Arc::clone(&env.monitors),
 Arc::clone(&env.content),
 interactions,
 robots_cache,
 Arc::clone(&events),
 Arc::new(web_widow::embeddings::EmbeddingService::new(
 Arc::clone(&env.config),
 Arc::clone(&env.store))),
 Arc::new(
 web_widow::webhooks::WebhookService::new(
 Arc::clone(&env.config),
 Arc::clone(&env.store),
 None)
.unwrap()));

 // Pick a free port, release it, then ask `serve` to bind 0.0.0.0 on it.
 let probe = TcpListener::bind("0.0.0.0:0").unwrap();
 let port = probe.local_addr().unwrap().port();
 drop(probe);
 let addr = format!("0.0.0.0:{port}").parse().unwrap();

 let error = serve(
 server,
 addr,
 "b".repeat(64),
 FeedRoute {
 enabled: true,
 token: Some("f".repeat(64)),
 events,
 },
 CancellationToken::new())
.await
.expect_err("a non-loopback feed bind must be refused");
 let message = error.to_string();
 assert!(
 message.contains("[events].feed_enabled"),
 "the refusal names the setting: {message}"
);
 assert!(
 message.contains("non-loopback"),
 "the refusal names the reason: {message}"
);

 // The listener was dropped without serving: the port is free again.
 let rebind = TcpListener::bind(("0.0.0.0", port));
 assert!(
 rebind.is_ok(),
 "the refused listener must be released: {rebind:?}"
);
}
