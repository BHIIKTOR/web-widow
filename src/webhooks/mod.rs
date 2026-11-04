//! Webhook emission (v0.7) — config-named targets, a durable
//! outbox, HMAC signing and a separate bounded dispatcher.
//!
//! The egress posture inverts the SSRF gate: a target URL can only come from
//! the operator's `[webhooks.targets.<name>]` config. Tools and monitors name a
//! target, never a URL, so page content, a prompt-injected agent or a tool
//! argument cannot invent an endpoint. The URL is treated as a secret (Slack
//! and Discord embed a token in it) and is never logged or returned.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::OnceLock;
use std::time::Duration;

use chrono::Utc;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::config::{Config, WebhookKind, WebhooksConfig};
use crate::core::error::{Result, WidowError};
use crate::monitor::change_summary;
use crate::monitor::normalize::{ChangeKind, ChangeStats};
use crate::store::Store;
use crate::store::webhooks::{
    DELIVERY_PASS, DeliveryContext, DeliveryUpdate, DueDelivery, MAX_TARGETS_PER_MONITOR,
};

/// Retry backoff in seconds, clamped to the last entry.
const BACKOFF_SECONDS: [i64; 5] = [60, 300, 1_800, 7_200, 21_600];
/// Response bytes drained and discarded (never logged).
const MAX_DRAIN_BYTES: usize = 4 * 1024;
/// Bounded `last_error` length.
const MAX_ERROR_CHARS: usize = 500;
/// `webhook_status` page cap.
const MAX_STATUS_LIMIT: usize = 100;

// ---------------------------------------------------------------------------
// Tool shapes
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WebhookTargetInput {
    /// The monitor whose attachment set is replaced.
    pub monitor_id: i64,
    /// Configured target names, 0..=8; `[]` clears. Unknown names are
    /// `schema_invalid`; a URL is never accepted here.
    #[serde(default)]
    pub targets: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema, PartialEq)]
pub struct WebhookTargetPayload {
    pub monitor_id: i64,
    pub targets: Vec<String>,
    pub known_targets: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WebhookStatusInput {
    /// Include recent delivery rows (default false).
    #[serde(default)]
    pub include_deliveries: Option<bool>,
    /// Delivery page size, 1..=100 (default 20).
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema, PartialEq)]
pub struct WebhookTargetStatus {
    pub name: String,
    pub kind: String,
    /// Host only; the path (a Slack/Discord token) is never returned.
    pub host: String,
    pub has_secret: bool,
    pub has_auth: bool,
    pub include_diff: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema, PartialEq)]
pub struct WebhookMonitorStatus {
    pub monitor_id: i64,
    pub label: Option<String>,
    pub targets: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema, PartialEq)]
pub struct WebhookDeliveryStatus {
    pub id: i64,
    pub change_id: i64,
    pub monitor_id: i64,
    pub target: String,
    pub status: String,
    pub attempts: i64,
    pub last_status: Option<i64>,
    pub next_attempt_at: String,
    pub created_at: String,
    pub delivered_at: Option<String>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema, PartialEq)]
pub struct WebhookStatusPayload {
    pub enabled: bool,
    pub targets: Vec<WebhookTargetStatus>,
    pub monitors: Vec<WebhookMonitorStatus>,
    pub deliveries: Vec<WebhookDeliveryStatus>,
    pub pending: u64,
    pub dead: u64,
}

// ---------------------------------------------------------------------------
// Service
// ---------------------------------------------------------------------------

/// The webhook service as the tools and the dispatcher see it. One instance per
/// process; `serve` spawns the dispatcher, local `run-due` flushes once.
pub struct WebhookService {
    config: std::sync::Arc<Config>,
    store: std::sync::Arc<Store>,
    client: reqwest::Client,
    /// The feed URL only when the loopback listener is serving; `None` otherwise.
    feed_url: Option<String>,
}

impl std::fmt::Debug for WebhookService {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WebhookService")
            .field("enabled", &self.config.webhooks.enabled)
            .field("targets", &self.config.webhooks.targets.len())
            .finish_non_exhaustive()
    }
}

/// Validate a requested attachment set: names must already exist, at most eight,
/// deduplicated in request order.
pub fn validate_targets(config: &WebhooksConfig, raw: &[String]) -> Result<Vec<String>> {
    if raw.len() > MAX_TARGETS_PER_MONITOR {
        return Err(WidowError::SchemaInvalid {
            pointer: "/targets".to_string(),
            message: format!("at most {MAX_TARGETS_PER_MONITOR} webhook targets per monitor"),
        });
    }
    let mut seen = BTreeSet::new();
    let mut targets = Vec::new();
    for (index, name) in raw.iter().enumerate() {
        let name = name.trim();
        if !config.targets.contains_key(name) {
            return Err(WidowError::SchemaInvalid {
                pointer: format!("/targets/{index}"),
                message: format!(
                    "unknown webhook target `{name}`: configure it under [webhooks.targets] first"
                ),
            });
        }
        if seen.insert(name.to_string()) {
            targets.push(name.to_string());
        }
    }
    Ok(targets)
}

impl WebhookService {
    pub fn new(
        config: std::sync::Arc<Config>,
        store: std::sync::Arc<Store>,
        feed_url: Option<String>,
    ) -> Result<Self> {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_millis(config.webhooks.timeout_ms))
            .user_agent(format!("web-widow/{}", crate::VERSION))
            .build()
            .map_err(|error| {
                WidowError::Internal(format!("cannot build the webhook HTTP client: {error}"))
            })?;
        Ok(Self {
            config,
            store,
            client,
            feed_url,
        })
    }

    pub fn known_targets(&self) -> Vec<String> {
        self.config.webhooks.targets.keys().cloned().collect()
    }

    pub fn validate_targets(&self, raw: &[String]) -> Result<Vec<String>> {
        validate_targets(&self.config.webhooks, raw)
    }

    /// Replace one monitor's attachment set (already validated).
    pub async fn attach_validated(
        &self,
        monitor_id: i64,
        targets: Vec<String>,
    ) -> Result<WebhookTargetPayload> {
        if self.store.db.monitor(monitor_id).await?.is_none() {
            return Err(WidowError::NotFound(format!(
                "no monitor with id {monitor_id}"
            )));
        }
        self.store
            .db
            .set_monitor_webhooks(monitor_id, &targets, Utc::now())
            .await?;
        Ok(WebhookTargetPayload {
            monitor_id,
            targets,
            known_targets: self.known_targets(),
        })
    }

    pub async fn attach(&self, input: WebhookTargetInput) -> Result<WebhookTargetPayload> {
        let targets = self.validate_targets(&input.targets)?;
        self.attach_validated(input.monitor_id, targets).await
    }

    pub async fn status(&self, input: WebhookStatusInput) -> Result<WebhookStatusPayload> {
        let limit = input.limit.unwrap_or(20);
        if !(1..=MAX_STATUS_LIMIT).contains(&limit) {
            return Err(WidowError::SchemaInvalid {
                pointer: "/limit".to_string(),
                message: format!("limit must be 1..={MAX_STATUS_LIMIT}"),
            });
        }
        let targets = self
            .config
            .webhooks
            .targets
            .iter()
            .map(|(name, target)| WebhookTargetStatus {
                name: name.clone(),
                kind: kind_str(target.kind).to_string(),
                host: Url::parse(&target.url)
                    .ok()
                    .and_then(|url| url.host_str().map(str::to_string))
                    .unwrap_or_default(),
                has_secret: target.secret_env.is_some() || target.secret_file.is_some(),
                has_auth: target.auth_env.is_some() || target.auth_file.is_some(),
                include_diff: target.include_diff,
            })
            .collect();
        let monitors = self
            .store
            .db
            .webhook_attachments()
            .await?
            .into_iter()
            .map(|row| WebhookMonitorStatus {
                monitor_id: row.monitor_id,
                label: row.label,
                targets: row.targets,
            })
            .collect();
        let deliveries = if input.include_deliveries.unwrap_or(false) {
            self.store
                .db
                .webhook_delivery_page(limit)
                .await?
                .into_iter()
                .map(|row| WebhookDeliveryStatus {
                    id: row.id,
                    change_id: row.change_id,
                    monitor_id: row.monitor_id,
                    target: row.target,
                    status: row.status,
                    attempts: row.attempts,
                    last_status: row.last_status,
                    next_attempt_at: row.next_attempt_at,
                    created_at: row.created_at,
                    delivered_at: row.delivered_at,
                    last_error: row.last_error,
                })
                .collect()
        } else {
            Vec::new()
        };
        let (pending, dead) = self.store.db.webhook_delivery_counts().await?;
        Ok(WebhookStatusPayload {
            enabled: self.config.webhooks.enabled,
            targets,
            monitors,
            deliveries,
            pending,
            dead,
        })
    }

    // -- delivery ----------------------------------------------------------

    /// Attempt up to `DELIVERY_PASS` due rows (or `budget`), sequentially.
    /// A network failure updates the row, never the caller.
    pub async fn flush_due(&self, budget: usize) -> Result<usize> {
        if !self.config.webhooks.enabled {
            return Ok(0);
        }
        let limit = budget.clamp(1, DELIVERY_PASS);
        let due = self
            .store
            .db
            .due_webhook_deliveries(Utc::now(), limit)
            .await?;
        let mut attempted = 0;
        for delivery in due {
            self.attempt(delivery).await?;
            attempted += 1;
        }
        Ok(attempted)
    }

    async fn attempt(&self, delivery: DueDelivery) -> Result<()> {
        let Some(target) = self.config.webhooks.targets.get(&delivery.target) else {
            return self
                .finish(
                    &delivery,
                    Outcome::Dead {
                        status: None,
                        error: format!("target `{}` is not configured", delivery.target),
                    },
                )
                .await;
        };
        let Some(context) = self
            .store
            .db
            .webhook_delivery_context(delivery.change_id)
            .await?
        else {
            return self
                .finish(
                    &delivery,
                    Outcome::Dead {
                        status: None,
                        error: "the change no longer exists".to_string(),
                    },
                )
                .await;
        };
        if !context.monitor_active {
            return self
                .finish(
                    &delivery,
                    Outcome::Dead {
                        status: None,
                        error: "monitor is inactive".to_string(),
                    },
                )
                .await;
        }
        let secret = match resolve_secret(target, "secret") {
            Ok(secret) => secret,
            Err(message) => {
                return self
                    .finish(
                        &delivery,
                        Outcome::Dead {
                            status: None,
                            error: message,
                        },
                    )
                    .await;
            }
        };
        let auth = match resolve_auth(target) {
            Ok(auth) => auth,
            Err(message) => {
                return self
                    .finish(
                        &delivery,
                        Outcome::Dead {
                            status: None,
                            error: message,
                        },
                    )
                    .await;
            }
        };

        let body = build_body(
            target.kind,
            &context,
            delivery.id,
            delivery.monitor_id,
            delivery.change_id,
            &self.feed_url,
            target.include_diff,
        );
        let timestamp = Utc::now().timestamp().to_string();
        let signature = secret
            .as_deref()
            .map(|secret| sign(secret, &timestamp, &body));
        let mut request = self
            .client
            .post(&target.url)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header("X-Web-Widow-Timestamp", &timestamp)
            .header("X-Web-Widow-Event", "change")
            .header("X-Web-Widow-Delivery", delivery.id.to_string())
            .body(body);
        if let Some(signature) = signature {
            request = request.header("X-Web-Widow-Signature", format!("sha256={signature}"));
        }
        if let Some(auth) = auth {
            request = request.header(reqwest::header::AUTHORIZATION, auth);
        }

        let outcome = match request.send().await {
            Ok(response) => {
                let status = response.status().as_u16();
                drain(response).await;
                classify_status(status)
            }
            Err(error) => classify_transport(&error),
        };
        self.finish(&delivery, outcome).await
    }

    async fn finish(&self, delivery: &DueDelivery, outcome: Outcome) -> Result<()> {
        let now = Utc::now();
        let attempts = delivery.attempts + 1;
        let max_attempts = i64::from(self.config.webhooks.max_attempts);
        let (status, last_status, last_error, delivered_at, next_attempt_at) = match outcome {
            Outcome::Delivered { status } => ("delivered", Some(status), None, Some(now), now),
            Outcome::Dead { status, error } => {
                ("dead", status, Some(bound_error(&error)), None, now)
            }
            Outcome::Retry { status, error } if attempts >= max_attempts => {
                ("dead", status, Some(bound_error(&error)), None, now)
            }
            Outcome::Retry { status, error } => (
                "pending",
                status,
                Some(bound_error(&error)),
                None,
                now + chrono::Duration::seconds(backoff_seconds(delivery.id, attempts)),
            ),
        };
        self.store
            .db
            .update_webhook_delivery(
                delivery.id,
                DeliveryUpdate {
                    status,
                    attempts,
                    next_attempt_at,
                    last_status,
                    last_error,
                    delivered_at,
                },
            )
            .await
    }

    /// Spawn the dispatcher loop (serve only). One pass at start, then sleep to
    /// the earliest pending attempt (1..=60 s) or 30 s when idle.
    pub fn spawn_dispatcher(
        self: std::sync::Arc<Self>,
        cancel: CancellationToken,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            loop {
                let delay = match self.flush_due(DELIVERY_PASS).await {
                    Ok(_) => self.next_delay().await,
                    Err(error) => {
                        tracing::warn!(error = %error, "webhook dispatcher pass failed");
                        Duration::from_secs(30)
                    }
                };
                tokio::select! {
                    () = cancel.cancelled() => {
                        tracing::info!("webhook dispatcher stopped");
                        break;
                    }
                    _ = tokio::time::sleep(delay) => {}
                }
            }
        })
    }

    async fn next_delay(&self) -> Duration {
        match self.store.db.next_webhook_attempt_at().await {
            Ok(Some(at)) => {
                let seconds = (at - Utc::now()).num_seconds().clamp(1, 60);
                Duration::from_secs(seconds as u64)
            }
            _ => Duration::from_secs(30),
        }
    }
}

// ---------------------------------------------------------------------------
// Attempt outcomes, payloads, signing, secrets
// ---------------------------------------------------------------------------

enum Outcome {
    Delivered { status: u16 },
    Retry { status: Option<u16>, error: String },
    Dead { status: Option<u16>, error: String },
}

fn classify_status(status: u16) -> Outcome {
    if (200..300).contains(&status) {
        return Outcome::Delivered { status };
    }
    if status == 408 || status == 429 || (500..600).contains(&status) {
        return Outcome::Retry {
            status: Some(status),
            error: format!("http {status}"),
        };
    }
    if (300..400).contains(&status) {
        return Outcome::Dead {
            status: Some(status),
            error: "redirect not followed".to_string(),
        };
    }
    Outcome::Dead {
        status: Some(status),
        error: format!("http {status}"),
    }
}

fn classify_transport(error: &reqwest::Error) -> Outcome {
    // Never render `error`: reqwest messages can include the URL (a secret).
    let (message, retryable) = if error.is_timeout() {
        ("timeout", true)
    } else if error.is_connect() {
        ("connection error", true)
    } else if error.is_redirect() {
        ("redirect not followed", false)
    } else if error.is_request() {
        ("request error", true)
    } else if error.is_body() || error.is_decode() {
        ("response body error", true)
    } else {
        ("transport error", true)
    };
    if retryable {
        Outcome::Retry {
            status: None,
            error: message.to_string(),
        }
    } else {
        Outcome::Dead {
            status: None,
            error: message.to_string(),
        }
    }
}

fn bound_error(message: &str) -> String {
    crate::core::truncate::truncate_chars(message, MAX_ERROR_CHARS).0
}

fn backoff_seconds(delivery_id: i64, attempts: i64) -> i64 {
    let index = attempts.saturating_sub(1).clamp(0, 4) as usize;
    let base = BACKOFF_SECONDS[index];
    let seed = (delivery_id
        .wrapping_mul(31)
        .wrapping_add(attempts.wrapping_mul(17)))
    .unsigned_abs();
    let percent = (seed % 41) as i64 - 20;
    (base * (100 + percent) / 100).max(1)
}

fn kind_str(kind: WebhookKind) -> &'static str {
    match kind {
        WebhookKind::Generic => "generic",
        WebhookKind::Slack => "slack",
        WebhookKind::Discord => "discord",
    }
}

fn change_kind(raw: &str) -> ChangeKind {
    match raw {
        "cosmetic" => ChangeKind::Cosmetic,
        "substantive" => ChangeKind::Substantive,
        _ => ChangeKind::None,
    }
}

fn build_body(
    kind: WebhookKind,
    context: &DeliveryContext,
    delivery_id: i64,
    monitor_id: i64,
    change_id: i64,
    feed_url: &Option<String>,
    include_diff: bool,
) -> String {
    let stats: ChangeStats = serde_json::from_str(&context.stats_json).unwrap_or_default();
    let summary = change_summary(&stats, change_kind(&context.kind));
    match kind {
        WebhookKind::Generic => {
            let mut object = serde_json::Map::new();
            object.insert("event".to_string(), json!("change"));
            object.insert("delivery_id".to_string(), json!(delivery_id));
            object.insert(
                "monitor".to_string(),
                json!({
                    "id": monitor_id,
                    "label": context.monitor_label,
                    "url": context.monitor_url,
                }),
            );
            object.insert(
                "change".to_string(),
                json!({
                    "id": change_id,
                    "detected_at": context.detected_at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                    "kind": context.kind,
                    "stats": stats,
                    "summary": summary,
                    "from_snapshot": context.from_snapshot_at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                    "to_snapshot": context.to_snapshot_at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                }),
            );
            object.insert("feed_url".to_string(), json!(feed_url));
            if include_diff {
                object.insert("diff".to_string(), json!(context.diff_text));
            }
            Value::Object(object).to_string()
        }
        WebhookKind::Slack => {
            let text = sanitize_mentions(&format!("{summary} — {}", context.monitor_url));
            json!({ "text": crate::core::truncate::truncate_chars(&text, 3_000).0 }).to_string()
        }
        WebhookKind::Discord => {
            let text = sanitize_mentions(&format!("{summary} — {}", context.monitor_url));
            json!({ "content": crate::core::truncate::truncate_chars(&text, 1_900).0 }).to_string()
        }
    }
}

fn sanitize_mentions(text: &str) -> String {
    static MENTIONS: OnceLock<Regex> = OnceLock::new();
    let pattern = MENTIONS.get_or_init(|| {
        Regex::new(r"(?i)<@[^>]*>|<!(?:channel|here|everyone)>|@(?:channel|here|everyone)")
            .expect("static mention pattern")
    });
    let stripped = pattern.replace_all(text, "");
    stripped
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn sign(secret: &[u8], timestamp: &str, body: &str) -> String {
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, secret);
    let mut message = Vec::with_capacity(timestamp.len() + 1 + body.len());
    message.extend_from_slice(timestamp.as_bytes());
    message.push(b'.');
    message.extend_from_slice(body.as_bytes());
    let signature = ring::hmac::sign(&key, &message);
    hex(signature.as_ref())
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

fn resolve_secret(
    target: &crate::config::WebhookTargetConfig,
    label: &str,
) -> std::result::Result<Option<Vec<u8>>, String> {
    match (&target.secret_env, &target.secret_file) {
        (Some(variable), None) => std::env::var(variable)
            .map(|value| Some(value.trim().as_bytes().to_vec()))
            .map_err(|_| format!("{label}_env `{variable}` is not set")),
        (None, Some(path)) => read_private(path, &format!("{label}_file")).map(Some),
        (None, None) => Ok(None),
        (Some(_), Some(_)) => Err(format!("{label}: both env and file are configured")),
    }
}

fn resolve_auth(
    target: &crate::config::WebhookTargetConfig,
) -> std::result::Result<Option<String>, String> {
    match (&target.auth_env, &target.auth_file) {
        (Some(variable), None) => std::env::var(variable)
            .map(|value| Some(value.trim().to_string()))
            .map_err(|_| format!("auth_env `{variable}` is not set")),
        (None, Some(path)) => read_private(path, "auth_file")
            .map(|bytes| Some(String::from_utf8_lossy(&bytes).trim().to_string())),
        (None, None) => Ok(None),
        (Some(_), Some(_)) => Err("auth: both env and file are configured".to_string()),
    }
}

fn read_private(path: &Path, label: &str) -> std::result::Result<Vec<u8>, String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let metadata = std::fs::metadata(path)
            .map_err(|_| format!("{label} `{}` is not readable", path.display()))?;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(format!("{label} `{}` is not mode 0600", path.display()));
        }
    }
    std::fs::read(path).map_err(|_| format!("{label} `{}` is not readable", path.display()))
}

async fn drain(mut response: reqwest::Response) {
    let mut drained = 0usize;
    while let Ok(Some(chunk)) = response.chunk().await {
        drained += chunk.len();
        if drained >= MAX_DRAIN_BYTES {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context() -> DeliveryContext {
        DeliveryContext {
            monitor_url: "https://example.com/page".to_string(),
            monitor_label: Some("ops".to_string()),
            monitor_active: true,
            detected_at: Utc::now(),
            kind: "substantive".to_string(),
            stats_json: r#"{"changed_lines":3,"lines_added":2,"lines_removed":1}"#.to_string(),
            diff_text: "-old\n+new".to_string(),
            truncated: false,
            from_snapshot_at: Utc::now(),
            to_snapshot_at: Utc::now(),
        }
    }

    #[test]
    fn hmac_matches_the_documented_receiver_recipe() {
        // RFC 4231 test case 2: key "Jefe", data "what do ya want for nothing?"
        let signature = sign(b"Jefe", "1", "what do ya want for nothing?");
        assert_eq!(signature.len(), 64);
        assert!(signature.chars().all(|c| c.is_ascii_hexdigit()));
        // The signed message is timestamp + "." + body.
        assert_ne!(
            signature,
            sign(b"Jefe", "2", "what do ya want for nothing?")
        );
    }

    #[test]
    fn backoff_is_bounded_and_deterministic() {
        for attempts in 1..=8 {
            let first = backoff_seconds(42, attempts);
            assert_eq!(first, backoff_seconds(42, attempts));
            assert!(first > 0);
        }
        // The first attempt is around 60 s, the last is around 21 600 s.
        assert!((48..=72).contains(&backoff_seconds(1, 1)));
        assert!((17_280..=25_920).contains(&backoff_seconds(1, 8)));
    }

    #[test]
    fn mentions_and_control_characters_are_stripped() {
        let text = "hi <@U123> <!channel> @everyone\t@here\u{0007} done";
        let clean = sanitize_mentions(text);
        assert!(!clean.contains("<@"));
        assert!(!clean.contains("<!"));
        assert!(!clean.to_lowercase().contains("@everyone"));
        assert!(!clean.contains('\u{0007}'));
        assert!(clean.contains("hi"));
        assert!(clean.contains("done"));
    }

    #[test]
    fn generic_payload_includes_the_diff_only_when_opted_in() {
        let without = build_body(WebhookKind::Generic, &context(), 7, 3, 11, &None, false);
        let with = build_body(WebhookKind::Generic, &context(), 7, 3, 11, &None, true);
        assert!(!without.contains("\"diff\""));
        assert!(with.contains("\"diff\""));
        assert!(without.contains("summary"));
    }

    #[test]
    fn slack_and_discord_bodies_are_capped() {
        let context = DeliveryContext {
            diff_text: String::new(),
            ..context()
        };
        let slack = build_body(WebhookKind::Slack, &context, 1, 3, 11, &None, false);
        let discord = build_body(WebhookKind::Discord, &context, 1, 3, 11, &None, false);
        let slack: Value = serde_json::from_str(&slack).unwrap();
        let discord: Value = serde_json::from_str(&discord).unwrap();
        assert!(slack["text"].as_str().unwrap().chars().count() <= 3_000);
        assert!(discord["content"].as_str().unwrap().chars().count() <= 1_900);
    }

    #[test]
    fn unknown_target_names_are_schema_invalid() {
        let config = WebhooksConfig::default();
        let error = validate_targets(&config, &["ops".to_string()]).unwrap_err();
        assert!(
            matches!(&error, WidowError::SchemaInvalid { pointer, .. } if pointer == "/targets/0")
        );
        let many = vec!["x".to_string(); MAX_TARGETS_PER_MONITOR + 1];
        assert!(validate_targets(&config, &many).is_err());
    }
}
