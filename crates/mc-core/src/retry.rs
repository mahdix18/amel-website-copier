//! When a failed fetch is worth another try, and how long to wait.

use std::time::{Duration, SystemTime};

use mc_requester::{FetchError, HeaderMap, StatusCode, header};

use crate::path::fnv1a;

/// Upper bound for a computed backoff.
const MAX_BACKOFF: Duration = Duration::from_secs(120);

/// A `Retry-After` longer than this is treated as "not today": the URL
/// fails instead of stalling the crawl.
pub(crate) const MAX_RETRY_AFTER: Duration = Duration::from_secs(300);

pub(crate) fn retryable_status(status: StatusCode) -> bool {
    matches!(status.as_u16(), 408 | 425 | 429 | 500 | 502 | 503 | 504)
}

/// Transport failures that tend to go away: not TLS errors, oversized
/// bodies or bad URLs, which fail the same way every time.
pub(crate) fn retryable_error(error: &FetchError) -> bool {
    matches!(
        error,
        FetchError::Timeout | FetchError::Connect(_) | FetchError::Decode(_)
    )
}

/// The `Retry-After` header as a delay, in either of its forms.
pub(crate) fn retry_after(headers: &HeaderMap, now: SystemTime) -> Option<Duration> {
    let value = headers.get(header::RETRY_AFTER)?.to_str().ok()?.trim();
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    let at = httpdate::parse_http_date(value).ok()?;
    Some(at.duration_since(now).unwrap_or(Duration::ZERO))
}

/// Exponential backoff with jitter: `base · 2^attempt` plus up to one `base`.
/// The jitter is derived from `salt` rather than a random source, so runs are
/// reproducible while retries of different URLs still spread out.
pub(crate) fn backoff(base: Duration, attempt: u32, salt: &str) -> Duration {
    let exponential = base.saturating_mul(1u32 << attempt.min(16));
    let fraction = (fnv1a(format!("{salt}#{attempt}").as_bytes()) % 1000) as u32;
    let jitter = base.saturating_mul(fraction) / 1000;
    (exponential + jitter).min(MAX_BACKOFF)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_statuses() {
        for code in [408, 429, 500, 503] {
            assert!(
                retryable_status(StatusCode::from_u16(code).unwrap()),
                "{code}"
            );
        }
        for code in [200, 301, 304, 400, 403, 404, 501] {
            assert!(
                !retryable_status(StatusCode::from_u16(code).unwrap()),
                "{code}"
            );
        }
    }

    #[test]
    fn parses_retry_after() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let mut headers = HeaderMap::new();
        headers.insert(header::RETRY_AFTER, "7".parse().unwrap());
        assert_eq!(retry_after(&headers, now), Some(Duration::from_secs(7)));

        let later = httpdate::fmt_http_date(now + Duration::from_secs(30));
        headers.insert(header::RETRY_AFTER, later.parse().unwrap());
        assert_eq!(retry_after(&headers, now), Some(Duration::from_secs(30)));

        headers.insert(header::RETRY_AFTER, "soon".parse().unwrap());
        assert_eq!(retry_after(&headers, now), None);
    }

    #[test]
    fn backoff_grows_and_is_capped() {
        let base = Duration::from_millis(100);
        let first = backoff(base, 0, "u");
        let third = backoff(base, 2, "u");
        assert!(first >= base && first < base * 2, "{first:?}");
        assert!(third >= base * 4 && third < base * 5, "{third:?}");
        assert_eq!(backoff(base, 2, "u"), third);
        assert_eq!(backoff(Duration::from_secs(10), 30, "u"), MAX_BACKOFF);
    }
}
