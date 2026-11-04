//! robots.txt acquisition, caching and the policy decision.
//!
//! Enforcement has no per-call switch: the only override is
//! `robots.enabled = false`, which is announced loudly at startup. A file we
//! cannot obtain or parse yields a conservative disallow for the failure TTL.

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use texting_robots::Robot;
use tracing::{debug, info, warn};
use url::Url;

use crate::config::RobotsConfig;
use crate::core::error::Result;
use crate::core::robots::RobotsDecision;
use crate::core::robots::RobotsRules;
use crate::core::url::content_hash;
use crate::fetch::http::{
    ACCEPT_ROBOTS, Conditional, FetchRequest, HopCheckFuture, HopGuard, HttpClient,
};
use crate::store::db::{Database, HostRow};

/// The parsed policy for one host, or an explicit "allow all" when robots.txt
/// is absent or enforcement is disabled.
#[derive(Debug)]
pub enum Rules {
    Parsed(Arc<Robot>),
    AllowAll,
}

impl RobotsRules for Rules {
    fn allowed(&self, url: &str) -> bool {
        match self {
            Self::Parsed(robot) => robot.allowed(url),
            Self::AllowAll => true,
        }
    }

    fn crawl_delay(&self) -> Option<Duration> {
        match self {
            Self::Parsed(robot) => robot
                .delay
                .filter(|seconds| *seconds > 0.0)
                .map(Duration::from_secs_f32),
            Self::AllowAll => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct RobotsService {
    http: Arc<HttpClient>,
    db: Arc<Database>,
    config: RobotsConfig,
}

impl RobotsService {
    pub fn new(http: Arc<HttpClient>, db: Arc<Database>, config: RobotsConfig) -> Self {
        Self { http, db, config }
    }

    pub fn enabled(&self) -> bool {
        self.config.enabled
    }

    /// `None` means "no usable policy": the caller must apply the conservative
    /// disallow. `Some` carries the rules for this request.
    pub async fn rules(&self, url: &Url) -> Result<Option<Arc<dyn RobotsRules>>> {
        if !self.config.enabled {
            return Ok(Some(Arc::new(Rules::AllowAll)));
        }
        // robots.txt is per authority (RFC 9309), so a non-default port is part
        // of the cache key; the politeness gate stays per host.
        let host = authority_key(url);
        let now = Utc::now();

        if let Some(row) = self.db.host(&host).await?
            && let Some(expires_at) = row.robots_expires_at
            && now < expires_at
        {
            debug!(host = %host, "robots.txt served from cache");
            return Ok(decode_row(&row, &self.config.user_agent));
        }

        let robots_url = robots_url_for(url)?;
        let response = match self
            .http
            .fetch_untyped(FetchRequest {
                url: robots_url.clone(),
                conditional: Conditional::default(),
                accept: ACCEPT_ROBOTS,
                max_body_bytes: self.config.max_body_bytes,
                hop_guard: None,
                min_interval: None,
                // `fetch_untyped` skips the scope entirely; robots.txt status is
                // the policy input, so its body type is irrelevant.
                scope: crate::fetch::content::ContentScope::Page,
            })
            .await
        {
            Ok(response) => response,
            Err(error) => {
                // A blocked address is a property of the site, not of robots.txt;
                // surface it instead of turning it into a policy decision.
                if matches!(
                    error.code(),
                    crate::core::error::ErrorCode::BlockedPrivateAddress
                        | crate::core::error::ErrorCode::BlockedScheme
                        | crate::core::error::ErrorCode::InvalidUrl
                ) {
                    return Err(error);
                }
                warn!(host = %host, error = %error, "robots.txt unavailable; disallowing for the failure TTL");
                self.store_row(&host, None, None, None, self.config.failure_ttl_s)
                    .await?;
                return Ok(None);
            }
        };

        if (200..300).contains(&response.status) {
            match Robot::new(&self.config.user_agent, response.body.as_slice()) {
                Ok(robot) => {
                    let sitemaps: Vec<String> = robot.sitemaps.clone();
                    if !sitemaps.is_empty() {
                        info!(host = %host, sitemaps = ?sitemaps, "robots.txt sitemaps recorded (not acted on)");
                    }
                    let ttl = match response.cache_control_max_age {
                        Some(max_age) => {
                            max_age.min(Duration::from_secs(self.config.success_ttl_s))
                        }
                        None => Duration::from_secs(self.config.success_ttl_s),
                    };
                    let hash = content_hash(&response.body);
                    self.store_row(
                        &host,
                        Some(i64::from(response.status)),
                        Some(hash),
                        Some(response.body.clone()),
                        ttl.as_secs(),
                    )
                    .await?;
                    debug!(host = %host, bytes = response.bytes, ttl_s = ttl.as_secs(), "robots.txt fetched");
                    Ok(Some(Arc::new(Rules::Parsed(Arc::new(robot)))))
                }
                Err(error) => {
                    warn!(host = %host, error = %error, "robots.txt could not be parsed; disallowing for the failure TTL");
                    self.store_row(
                        &host,
                        Some(i64::from(response.status)),
                        None,
                        Some(response.body),
                        self.config.failure_ttl_s,
                    )
                    .await?;
                    Ok(None)
                }
            }
        } else if (400..500).contains(&response.status) {
            // RFC 9309: 4xx means "no restrictions".
            debug!(host = %host, status = response.status, "robots.txt absent; allowing");
            self.store_row(
                &host,
                Some(i64::from(response.status)),
                None,
                None,
                self.config.success_ttl_s,
            )
            .await?;
            Ok(Some(Arc::new(Rules::AllowAll)))
        } else {
            warn!(host = %host, status = response.status, "robots.txt 5xx; disallowing for the failure TTL");
            self.store_row(
                &host,
                Some(i64::from(response.status)),
                None,
                None,
                self.config.failure_ttl_s,
            )
            .await?;
            Ok(None)
        }
    }

    async fn store_row(
        &self,
        host: &str,
        status: Option<i64>,
        hash: Option<String>,
        body: Option<Vec<u8>>,
        ttl_seconds: u64,
    ) -> Result<()> {
        let now = Utc::now();
        let expires = now + chrono::Duration::seconds(ttl_seconds.min(i64::MAX as u64) as i64);
        self.db
            .save_host(HostRow {
                host: host.to_string(),
                robots_fetched_at: Some(now),
                robots_status: status,
                robots_hash: hash,
                robots_body: body.map(|bytes| {
                    let limit = self.config.max_body_bytes as usize;
                    bytes.into_iter().take(limit).collect()
                }),
                robots_expires_at: Some(expires),
                next_allowed_at: None,
                consecutive_failures: if status.is_some_and(|value| value >= 500)
                    || status.is_none()
                {
                    1
                } else {
                    0
                },
            })
            .await
    }
}

/// The per-hop guard used by page fetches: robots policy plus `Crawl-delay`.
#[derive(Debug, Clone)]
pub struct RobotsHopGuard {
    service: Arc<RobotsService>,
}

impl RobotsHopGuard {
    pub fn new(service: Arc<RobotsService>) -> Self {
        Self { service }
    }
}

impl HopGuard for RobotsHopGuard {
    fn check<'a>(&'a self, url: &'a Url) -> HopCheckFuture<'a> {
        Box::pin(async move {
            let rules = self.service.rules(url).await?;
            match crate::core::robots::decide(rules.as_deref(), url.as_str()) {
                RobotsDecision::Allowed => {
                    Ok(rules.as_deref().and_then(|rules| rules.crawl_delay()))
                }
                RobotsDecision::Disallowed { reason } => {
                    Err(crate::core::error::WidowError::BlockedByRobots { reason })
                }
            }
        })
    }
}

/// Cache key for robots.txt: host plus a non-default port.
pub fn authority_key(url: &Url) -> String {
    let host = url.host_str().unwrap_or_default().to_ascii_lowercase();
    match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host,
    }
}

pub fn robots_url_for(url: &Url) -> Result<Url> {
    let mut robots = url.clone();
    robots.set_path("/robots.txt");
    robots.set_query(None);
    robots.set_fragment(None);
    Ok(robots)
}

fn decode_row(row: &HostRow, user_agent: &str) -> Option<Arc<dyn RobotsRules>> {
    match row.robots_status {
        Some(status) if (200..300).contains(&status) => {
            let body = row.robots_body.clone().unwrap_or_default();
            match Robot::new(user_agent, body.as_slice()) {
                Ok(robot) => Some(Arc::new(Rules::Parsed(Arc::new(robot)))),
                Err(error) => {
                    debug!(host = %row.host, error = %error, "cached robots.txt unparseable");
                    None
                }
            }
        }
        Some(status) if (400..500).contains(&status) => Some(Arc::new(Rules::AllowAll)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn robots_url_is_at_the_origin_root() {
        let url = Url::parse("https://example.com:8443/a/b?q=1#frag").unwrap();
        assert_eq!(
            robots_url_for(&url).unwrap().as_str(),
            "https://example.com:8443/robots.txt"
        );
        let url = Url::parse("http://example.com").unwrap();
        assert_eq!(
            robots_url_for(&url).unwrap().as_str(),
            "http://example.com/robots.txt"
        );
    }

    #[test]
    fn authority_key_includes_non_default_ports_only() {
        let plain = Url::parse("https://example.com/x").unwrap();
        assert_eq!(authority_key(&plain), "example.com");
        let explicit_default = Url::parse("https://example.com:443/x").unwrap();
        assert_eq!(authority_key(&explicit_default), "example.com");
        let custom = Url::parse("http://127.0.0.1:8080/x").unwrap();
        assert_eq!(authority_key(&custom), "127.0.0.1:8080");
        let ipv6 = Url::parse("http://[::1]:9000/x").unwrap();
        assert_eq!(authority_key(&ipv6), "[::1]:9000");
    }

    #[test]
    fn parsed_rules_expose_crawl_delay_and_matching() {
        let text = b"User-agent: otherbot\nDisallow: /\n\nUser-agent: web-widow\nDisallow: /private\nCrawl-Delay: 7\n";
        let robot = Robot::new("web-widow", text.as_slice()).unwrap();
        let rules = Rules::Parsed(Arc::new(robot));
        assert!(rules.allowed("https://example.com/public"));
        assert!(!rules.allowed("https://example.com/private/x"));
        assert_eq!(rules.crawl_delay(), Some(Duration::from_secs(7)));
    }

    #[test]
    fn allow_all_has_no_delay() {
        let rules = Rules::AllowAll;
        assert!(rules.allowed("https://example.com/anything"));
        assert_eq!(rules.crawl_delay(), None);
    }

    #[test]
    fn ua_specific_group_is_selected_over_wildcard() {
        let text = b"User-agent: *\nDisallow: /\n\nUser-agent: web-widow\nAllow: /\n";
        let robot = Robot::new("web-widow", text.as_slice()).unwrap();
        assert!(robot.allowed("https://example.com/page"));
        let wildcard = Robot::new("another-agent", text.as_slice()).unwrap();
        assert!(!wildcard.allowed("https://example.com/page"));
    }

    #[test]
    fn cached_rows_map_status_to_policy() {
        let allow = HostRow {
            host: "example.com".to_string(),
            robots_fetched_at: None,
            robots_status: Some(404),
            robots_hash: None,
            robots_body: None,
            robots_expires_at: None,
            next_allowed_at: None,
            consecutive_failures: 0,
        };
        assert!(decode_row(&allow, "web-widow").is_some());

        let failure = HostRow {
            robots_status: Some(503),
            ..allow.clone()
        };
        assert!(decode_row(&failure, "web-widow").is_none());

        let unparseable = HostRow {
            robots_status: Some(200),
            robots_body: Some(vec![0xff, 0xfe, 0x00]),
            ..allow
        };
        // Either parses leniently or yields no policy; never panics.
        let _ = decode_row(&unparseable, "web-widow");
    }
}
