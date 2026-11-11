//! robots.txt, read the way RFC 9309 asks.

use std::time::Duration;

use mc_requester::{Body, FetchError, FetchResponse};
use texting_robots::Robot;
use url::Url;

/// The product token matched against `User-agent` lines. Groups for
/// `MisterCopy` apply, then `*`.
pub(crate) const AGENT: &str = "MisterCopy";

/// Longest `Crawl-delay` honoured, so one odd robots.txt can't stall a crawl.
const MAX_CRAWL_DELAY: Duration = Duration::from_secs(60);

pub(crate) enum RobotsRules {
    AllowAll,
    DisallowAll,
    Parsed(Box<Robot>),
}

impl RobotsRules {
    /// Interprets the fetch of `/robots.txt`. Returns the rules and any
    /// `Crawl-delay`.
    pub(crate) fn from_fetch(
        result: &Result<FetchResponse, FetchError>,
    ) -> (Self, Option<Duration>) {
        let response = match result {
            Ok(response) => response,
            // Unreachable: RFC 9309 says to assume everything is disallowed.
            Err(_) => return (Self::DisallowAll, None),
        };
        let status = response.status.as_u16();
        match status {
            200..=299 => {
                let Body::Bytes(body) = &response.body else {
                    return (Self::AllowAll, None);
                };
                match Robot::new(AGENT, body) {
                    Ok(robot) => {
                        let delay = robot
                            .delay
                            .filter(|seconds| seconds.is_finite() && *seconds > 0.0)
                            .map(|seconds| Duration::from_secs_f32(seconds).min(MAX_CRAWL_DELAY));
                        (Self::Parsed(Box::new(robot)), delay)
                    }
                    Err(_) => (Self::AllowAll, None),
                }
            }
            // Missing or forbidden robots.txt means no restrictions.
            400..=499 if status != 429 => (Self::AllowAll, None),
            _ => (Self::DisallowAll, None),
        }
    }

    pub(crate) fn allows(&self, url: &Url) -> bool {
        match self {
            Self::AllowAll => true,
            Self::DisallowAll => false,
            Self::Parsed(robot) => robot.allowed(url.as_str()),
        }
    }
}

/// One robots.txt per origin.
pub(crate) fn origin_key(url: &Url) -> String {
    url.origin().ascii_serialization()
}

pub(crate) fn robots_url(url: &Url) -> Option<Url> {
    url.join("/robots.txt").ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use mc_requester::{HeaderMap, StatusCode};

    fn fetched(status: u16, body: &str) -> Result<FetchResponse, FetchError> {
        Ok(FetchResponse::new(
            Url::parse("https://s.test/robots.txt").unwrap(),
            StatusCode::from_u16(status).unwrap(),
            HeaderMap::new(),
            Body::Bytes(body.as_bytes().to_vec().into()),
        ))
    }

    fn url(path: &str) -> Url {
        Url::parse(&format!("https://s.test{path}")).unwrap()
    }

    #[test]
    fn applies_our_group_then_the_wildcard() {
        let body = "User-agent: *\nDisallow: /private/\n\nUser-agent: MisterCopy\nDisallow: /no-copies/\nCrawl-delay: 2\n";
        let (rules, delay) = RobotsRules::from_fetch(&fetched(200, body));
        assert!(!rules.allows(&url("/no-copies/x")));
        // Our own group replaces `*` entirely.
        assert!(rules.allows(&url("/private/x")));
        assert_eq!(delay, Some(Duration::from_secs(2)));

        let (rules, _) =
            RobotsRules::from_fetch(&fetched(200, "User-agent: *\nDisallow: /private/\n"));
        assert!(!rules.allows(&url("/private/x")));
        assert!(rules.allows(&url("/public")));
    }

    #[test]
    fn status_codes_follow_rfc_9309() {
        assert!(
            RobotsRules::from_fetch(&fetched(404, ""))
                .0
                .allows(&url("/a"))
        );
        assert!(
            !RobotsRules::from_fetch(&fetched(503, ""))
                .0
                .allows(&url("/a"))
        );
        assert!(
            !RobotsRules::from_fetch(&fetched(429, ""))
                .0
                .allows(&url("/a"))
        );
        assert!(
            !RobotsRules::from_fetch(&Err(FetchError::Timeout))
                .0
                .allows(&url("/a"))
        );
    }
}
