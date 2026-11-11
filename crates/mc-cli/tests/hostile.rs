//! The hostile fixture, end to end over real HTTP: every trap must end as a
//! recorded outcome, never a crash or a hang.

#![allow(clippy::unwrap_used)]

mod common;

use std::collections::HashMap;
use std::time::Duration;

use common::{assert_links_resolve_offline, host_dir};
use mc_core::{CrawlConfig, Crawler};
use mc_fixtures::{FixtureServer, Site};
use mc_requester::{RequesterFactory, SharedResources};
use mc_requester_wreq::WreqFactory;

#[tokio::test]
async fn survives_the_hostile_site() {
    let server = FixtureServer::start(Site::Hostile).await.unwrap();
    let out = tempfile::tempdir().unwrap();
    let shared = SharedResources {
        default_timeout: Duration::from_secs(1),
        max_body_bytes: 1024 * 1024,
        ..SharedResources::default()
    };
    let requester = WreqFactory.build(&serde_json::json!({}), &shared).unwrap();
    let mut config = CrawlConfig::new(server.base_url(), out.path());
    config.per_host_delay = Duration::ZERO;
    config.retry_base_delay = Duration::from_millis(10);
    config.max_retries = 2;
    config.max_depth = Some(3);
    config.concurrency = 8;

    let summary = tokio::time::timeout(
        Duration::from_secs(60),
        Crawler::new(config, requester).run(),
    )
    .await
    .expect("the crawl finishes")
    .unwrap();

    let failed: HashMap<String, String> = summary
        .failed
        .iter()
        .map(|(url, reason)| (url.path().to_owned(), reason.clone()))
        .collect();
    let reason = |path: &str| {
        failed
            .get(path)
            .unwrap_or_else(|| panic!("{path} should have failed; failures: {failed:#?}"))
            .as_str()
    };
    assert!(
        reason("/loop-a").contains("redirects"),
        "{}",
        reason("/loop-a")
    );
    assert!(reason("/always-503").contains("503"));
    assert!(reason("/retry-forever").contains("503"));
    assert!(reason("/huge.bin").contains("limit"));
    assert!(reason("/slow").contains("timed out"));
    assert!(reason("/bad-location").contains("302"));
    for path in ["/bad-gzip", "/truncated"] {
        assert!(
            failed.contains_key(path),
            "{path} should have failed: {failed:#?}"
        );
    }

    let requests = server.requests();
    // Disallowed by robots.txt, so never asked for.
    assert!(
        !requests.iter().any(|r| r.starts_with("/private/")),
        "{requests:#?}"
    );
    // Given up on without retrying: Retry-After is an hour.
    assert_eq!(
        requests.iter().filter(|r| *r == "/retry-forever").count(),
        1
    );
    // Retried after its two refusals, then saved.
    assert_eq!(requests.iter().filter(|r| *r == "/rate-limited").count(), 3);
    // The endless calendar stops at the depth limit.
    assert!(requests.iter().any(|r| r == "/calendar/3"));
    assert!(
        !requests.iter().any(|r| r == "/calendar/5"),
        "{requests:#?}"
    );

    let root = host_dir(out.path(), server.addr().port());
    for file in [
        "rate-limited.html",
        "garbage-type.html",
        "binary.html",
        "empty.html",
        "_CON.html",
        "_aux.html",
        "name_with_question.html",
        "emoji-😀.html",
        "Page.html",
    ] {
        assert!(root.join(file).is_file(), "missing {file}");
    }
    // A 300-character name is cut to fit one path segment…
    let long_name = std::fs::read_dir(root.join("long"))
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .next()
        .unwrap();
    assert!(
        long_name.len() <= 200 && long_name.ends_with(".html"),
        "{long_name}"
    );
    // …and a path too deep for Windows moves under `_long/`.
    assert!(root.join("_long").is_dir(), "very deep URLs are shortened");
    let index = std::fs::read_to_string(root.join("index.html")).unwrap();
    assert!(
        index.contains("http://offsite.invalid/"),
        "off-site redirect stays remote"
    );
    assert!(index.contains("mailto:someone@example.invalid"));
    assert_links_resolve_offline(&root, false);
    server.stop().await.unwrap();
}
