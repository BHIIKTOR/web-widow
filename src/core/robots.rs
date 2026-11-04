//! Robots decisions as a pure function over a rules port.
//!
//! `texting_robots` is the parser behind the port; an unavailable policy is
//! represented as `None` so the conservative decision lives in one place and is
//! testable without a server.

use std::time::Duration;

/// The decision-relevant surface of a parsed robots.txt.
pub trait RobotsRules: Send + Sync {
    fn allowed(&self, url: &str) -> bool;
    fn crawl_delay(&self) -> Option<Duration>;
}

/// Rules that allow everything (robots disabled by config, or an absent file).
#[derive(Debug, Clone, Copy, Default)]
pub struct AllowAll;

impl RobotsRules for AllowAll {
    fn allowed(&self, _url: &str) -> bool {
        true
    }

    fn crawl_delay(&self) -> Option<Duration> {
        None
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RobotsDecision {
    Allowed,
    Disallowed { reason: String },
}

impl RobotsDecision {
    pub fn is_allowed(&self) -> bool {
        matches!(self, Self::Allowed)
    }

    pub fn reason(&self) -> Option<&str> {
        match self {
            Self::Allowed => None,
            Self::Disallowed { reason } => Some(reason),
        }
    }
}

/// `None` means "no authoritative policy" (transport failure, 5xx, unparseable
/// file): that is a conservative disallow for the cache window.
pub fn decide(rules: Option<&dyn RobotsRules>, url: &str) -> RobotsDecision {
    match rules {
        None => RobotsDecision::Disallowed {
            reason: "no usable robots.txt (transport failure, 5xx or unparseable); conservative disallow"
                .to_string(),
        },
        Some(rules) if rules.allowed(url) => RobotsDecision::Allowed,
        Some(_) => RobotsDecision::Disallowed {
            reason: format!("path disallowed for our user-agent: {url}"),
        },
    }
}

/// `Crawl-delay` may only raise the interval, never lower it.
pub fn effective_interval(base: Duration, crawl_delay: Option<Duration>) -> Duration {
    match crawl_delay {
        Some(delay) if delay > base => delay,
        _ => base,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixed {
        allow: bool,
        delay: Option<Duration>,
    }

    impl RobotsRules for Fixed {
        fn allowed(&self, _url: &str) -> bool {
            self.allow
        }

        fn crawl_delay(&self) -> Option<Duration> {
            self.delay
        }
    }

    #[test]
    fn missing_policy_is_a_conservative_disallow() {
        let decision = decide(None, "https://example.com/x");
        assert!(!decision.is_allowed());
        assert!(decision.reason().unwrap().contains("conservative disallow"));
    }

    #[test]
    fn allowed_and_disallowed_follow_the_rules() {
        let allowed = Fixed {
            allow: true,
            delay: None,
        };
        let disallowed = Fixed {
            allow: false,
            delay: None,
        };
        assert!(decide(Some(&allowed), "https://example.com/x").is_allowed());
        let decision = decide(Some(&disallowed), "https://example.com/private");
        assert!(!decision.is_allowed());
        assert!(decision.reason().unwrap().contains("/private"));
    }

    #[test]
    fn crawl_delay_only_raises_the_interval() {
        let base = Duration::from_secs(1);
        assert_eq!(effective_interval(base, None), base);
        assert_eq!(
            effective_interval(base, Some(Duration::from_millis(200))),
            base
        );
        assert_eq!(
            effective_interval(base, Some(Duration::from_secs(7))),
            Duration::from_secs(7)
        );
    }
}
