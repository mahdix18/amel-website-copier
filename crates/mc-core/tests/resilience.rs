//! M2 behaviour against the mock requester: retries, robots.txt, stopping
//! and resuming, updates, scan mode and credentials.

#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use mc_core::{
    CrawlConfig, CrawlEvent, CrawlStatus, Crawler, Credential, EntryState, MemoryStore,
    REPORT_JSON, StopReason,
};
use mc_requester::mock::{MockFailure, MockRequester, MockRoute};
use mc_requester::{StatusCode, header};
use url::Url;

fn url(s: &str) -> Url {
    Url::parse(s).unwrap()
}

fn config(out: &Path) -> CrawlConfig {
    let mut config = CrawlConfig::new(url("https://site.test/"), out);
    config.per_host_delay = Duration::ZERO;
    config.retry_base_delay = Duration::from_millis(1);
    config
}

/// Every file under `dir` (state directory and report excluded) with its contents.
fn tree(dir: &Path) -> BTreeMap<String, Vec<u8>> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            let rel = path
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .into_owned();
            if rel.starts_with(".mistercopy") || rel.starts_with("mistercopy-report") {
                continue;
            }
            if path.is_dir() {
                walk(root, &path, out);
            } else {
                out.insert(rel, std::fs::read(&path).unwrap());
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(dir, dir, &mut out);
    out
}

/// A ten-page chain, so a crawl can be cut off part way.
fn chain() -> MockRequester {
    let mut mock = MockRequester::new()
        .route(
            url("https://site.test/style.css"),
            MockRoute::respond(
                StatusCode::OK,
                "text/css",
                "body { background: url(bg.png) }",
            ),
        )
        .route(
            url("https://site.test/bg.png"),
            MockRoute::respond(StatusCode::OK, "image/png", "PNG"),
        );
    for i in 0..10 {
        let page = if i == 0 {
            "https://site.test/".to_owned()
        } else {
            format!("https://site.test/p{i}")
        };
        let body = format!(
            r#"<link rel="stylesheet" href="/style.css"><a href="/p{}">next</a><a href="/">home</a>"#,
            i + 1
        );
        mock = mock.route(url(&page), MockRoute::html(body));
    }
    mock
}

#[tokio::test]
async fn retries_transient_failures_then_succeeds() {
    let out = tempfile::tempdir().unwrap();
    let mock = Arc::new(MockRequester::new().route_sequence(
        url("https://site.test/"),
        vec![
                MockRoute::Fail(MockFailure::Timeout),
                MockRoute::status(StatusCode::SERVICE_UNAVAILABLE, "busy")
                    .with_header(header::RETRY_AFTER, "0"),
                MockRoute::html("finally"),
            ],
    ));
    let crawler = Crawler::new(config(out.path()), mock.clone());
    let mut events = crawler.subscribe();
    let summary = crawler.run().await.unwrap();

    assert!(summary.failed.is_empty(), "{:?}", summary.failed);
    assert_eq!(summary.saved, 1);
    let mut retries = 0;
    while let Ok(event) = events.try_recv() {
        if let CrawlEvent::Retrying { attempt, .. } = event {
            retries += 1;
            assert_eq!(attempt, retries);
        }
    }
    assert_eq!(retries, 2);
}

#[tokio::test]
async fn gives_up_after_max_retries_and_skips_permanent_errors() {
    let out = tempfile::tempdir().unwrap();
    let mock = Arc::new(
        MockRequester::new()
            .route(
                url("https://site.test/"),
                MockRoute::html(r#"<a href="/flaky">f</a><a href="/gone">g</a>"#),
            )
            .route(
                url("https://site.test/flaky"),
                MockRoute::status(StatusCode::BAD_GATEWAY, ""),
            )
            .route(
                url("https://site.test/gone"),
                MockRoute::status(StatusCode::NOT_FOUND, ""),
            ),
    );
    let mut config = config(out.path());
    config.max_retries = 2;
    let summary = Crawler::new(config, mock.clone()).run().await.unwrap();

    let calls = mock.calls();
    assert_eq!(calls.iter().filter(|u| u.path() == "/flaky").count(), 3);
    assert_eq!(calls.iter().filter(|u| u.path() == "/gone").count(), 1);
    let reasons: Vec<_> = summary.failed.iter().map(|(_, r)| r.as_str()).collect();
    assert_eq!(reasons, ["HTTP 502 Bad Gateway", "HTTP 404 Not Found"]);
}

#[tokio::test]
async fn honours_robots_txt() {
    let robots = "User-agent: *\nDisallow: /private/\n";
    let mock = Arc::new(
        MockRequester::new()
            .route(
                url("https://site.test/robots.txt"),
                MockRoute::respond(StatusCode::OK, "text/plain", robots),
            )
            .route(
                url("https://site.test/"),
                MockRoute::html(r#"<a href="/private/x">p</a><a href="/public">q</a>"#),
            )
            .route(url("https://site.test/public"), MockRoute::html("ok"))
            .route(
                url("https://site.test/private/x"),
                MockRoute::html("secret"),
            ),
    );
    let out = tempfile::tempdir().unwrap();
    let summary = Crawler::new(config(out.path()), mock.clone())
        .run()
        .await
        .unwrap();
    assert!(
        !mock
            .calls()
            .iter()
            .any(|u| u.path().starts_with("/private"))
    );
    assert_eq!(summary.report.skipped.get("robots"), Some(&1));
    assert_eq!(
        mock.calls()
            .iter()
            .filter(|u| u.path() == "/robots.txt")
            .count(),
        1
    );

    // Opting out fetches it.
    let out = tempfile::tempdir().unwrap();
    let mut config = config(out.path());
    config.respect_robots = false;
    Crawler::new(config, mock.clone()).run().await.unwrap();
    assert!(mock.calls().iter().any(|u| u.path() == "/private/x"));
}

#[tokio::test]
async fn unreachable_robots_txt_blocks_the_site() {
    let out = tempfile::tempdir().unwrap();
    let mock = Arc::new(
        MockRequester::new()
            .route(
                url("https://site.test/robots.txt"),
                MockRoute::status(StatusCode::INTERNAL_SERVER_ERROR, ""),
            )
            .route(url("https://site.test/"), MockRoute::html("hi")),
    );
    let summary = Crawler::new(config(out.path()), mock).run().await.unwrap();
    assert_eq!(summary.saved, 0);
    assert_eq!(summary.report.skipped.get("robots"), Some(&1));
}

#[tokio::test]
async fn stopping_and_resuming_matches_an_uninterrupted_crawl() {
    let full = tempfile::tempdir().unwrap();
    Crawler::new(config(full.path()), Arc::new(chain()))
        .run()
        .await
        .unwrap();

    let out = tempfile::tempdir().unwrap();
    let store = MemoryStore::new();
    let mut first = config(out.path());
    first.concurrency = 1;
    let crawler = Crawler::new(first, Arc::new(chain())).with_store(store.clone());
    let stop = crawler.stop_handle();
    let mut events = crawler.subscribe();
    let stopper = tokio::spawn(async move {
        let mut saved = 0;
        while let Ok(event) = events.recv().await {
            if matches!(event, CrawlEvent::Saved { .. }) {
                saved += 1;
                if saved == 4 {
                    stop.stop();
                }
            }
        }
    });
    let summary = crawler.run().await.unwrap();
    stopper.await.unwrap();
    assert_eq!(summary.stopped, Some(StopReason::Requested));
    assert_eq!(store.status(), Some(CrawlStatus::Stopped));
    assert!(summary.saved < 12, "stopped early: {}", summary.saved);
    assert!(
        store
            .entries()
            .iter()
            .any(|e| e.state == EntryState::Queued)
    );

    let resumed = Crawler::new(config(out.path()), Arc::new(chain()))
        .with_store(store.clone())
        .run()
        .await
        .unwrap();
    assert_eq!(resumed.stopped, None);
    assert_eq!(store.status(), Some(CrawlStatus::Complete));
    // 11 pages (p10 is a 404), the stylesheet and its image.
    assert_eq!(resumed.saved, 12);
    assert_eq!(tree(out.path()), tree(full.path()));
}

#[tokio::test]
async fn update_reuses_unchanged_files() {
    let page =
        |body: &str, etag: &str| MockRoute::html(body.to_owned()).with_header(header::ETAG, etag);
    let first = MockRequester::new()
        .route(
            url("https://site.test/"),
            page(r#"<a href="/a">a</a><a href="/b">b</a>"#, "\"root-1\""),
        )
        .route(url("https://site.test/a"), page("a v1", "\"a-1\""))
        .route(url("https://site.test/b"), page("b v1", "\"b-1\""));
    let out = tempfile::tempdir().unwrap();
    let store = MemoryStore::new();
    Crawler::new(config(out.path()), Arc::new(first))
        .with_store(store.clone())
        .run()
        .await
        .unwrap();

    // Second visit: root and /a unchanged, /b changed.
    let not_modified = || MockRoute::status(StatusCode::NOT_MODIFIED, "");
    let second = Arc::new(
        MockRequester::new()
            .route(url("https://site.test/"), not_modified())
            .route(url("https://site.test/a"), not_modified())
            .route(url("https://site.test/b"), page("b v2", "\"b-2\"")),
    );
    store.begin_update();
    let summary = Crawler::new(config(out.path()), second.clone())
        .with_store(store.clone())
        .run()
        .await
        .unwrap();

    assert_eq!(summary.unchanged, 2);
    assert_eq!(summary.saved, 3);
    let etags: Vec<_> = second
        .requests()
        .into_iter()
        .filter(|r| r.url.path() != "/robots.txt")
        .map(|r| r.validators.and_then(|v| v.etag))
        .collect();
    assert_eq!(etags.len(), 3);
    assert!(etags.iter().all(Option::is_some), "{etags:?}");

    let root = out.path().join("site.test");
    assert_eq!(
        std::fs::read_to_string(root.join("b.html")).unwrap(),
        "b v2"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("a.html")).unwrap(),
        "a v1"
    );
    // Links in the unchanged page still point at the local copies.
    let index = std::fs::read_to_string(root.join("index.html")).unwrap();
    assert!(
        index.contains(r#"href="a.html""#) && index.contains(r#"href="b.html""#),
        "{index}"
    );
}

#[tokio::test]
async fn scan_writes_nothing_and_checks_assets_with_head() {
    let out = tempfile::tempdir().unwrap();
    let dir = out.path().join("scan");
    let mock = Arc::new(
        MockRequester::new()
            .route(
                url("https://site.test/"),
                MockRoute::html(r#"<img src="/big.png"><img src="/broken.png"><a href="/p">p</a>"#),
            )
            .route(url("https://site.test/p"), MockRoute::html("p"))
            .route(
                url("https://site.test/big.png"),
                MockRoute::respond(StatusCode::OK, "image/png", ""),
            ),
    );
    let mut config = config(&dir);
    config.dry_run = true;
    let summary = Crawler::new(config, mock.clone()).run().await.unwrap();

    assert!(!dir.exists());
    assert_eq!(summary.saved, 3);
    assert_eq!(summary.failed.len(), 1);
    assert_eq!(summary.failed[0].0.path(), "/broken.png");
    let methods: Vec<_> = mock
        .requests()
        .into_iter()
        .map(|r| (r.url.path().to_owned(), r.method.to_string()))
        .collect();
    assert!(
        methods.contains(&("/big.png".to_owned(), "HEAD".to_owned())),
        "{methods:?}"
    );
    assert!(
        methods.contains(&("/p".to_owned(), "GET".to_owned())),
        "{methods:?}"
    );
}

#[tokio::test]
async fn credentials_and_headers_stay_with_their_hosts() {
    let out = tempfile::tempdir().unwrap();
    let mock = Arc::new(
        MockRequester::new()
            .route(
                url("https://site.test/"),
                MockRoute::html(r#"<img src="https://cdn.test/x.png">"#),
            )
            .route(
                url("https://cdn.test/x.png"),
                MockRoute::respond(StatusCode::OK, "image/png", "x"),
            ),
    );
    let mut config = config(out.path());
    config.include_external_assets = true;
    config.credentials = vec![Credential {
        host: "SITE.test".into(),
        username: "ann".into(),
        password: "pw".into(),
    }];
    config.headers.insert("x-token", "secret".parse().unwrap());
    Crawler::new(config, mock.clone()).run().await.unwrap();

    let requests = mock.requests();
    let site = requests
        .iter()
        .find(|r| r.url.as_str() == "https://site.test/")
        .unwrap();
    assert_eq!(site.headers[header::AUTHORIZATION], "Basic YW5uOnB3");
    assert_eq!(site.headers["x-token"], "secret");
    let cdn = requests
        .iter()
        .find(|r| r.url.as_str() == "https://cdn.test/x.png")
        .unwrap();
    assert!(!cdn.headers.contains_key(header::AUTHORIZATION));
    assert!(!cdn.headers.contains_key("x-token"));
}

#[tokio::test]
async fn limits_stop_resumably() {
    let out = tempfile::tempdir().unwrap();
    let store = MemoryStore::new();
    let mut config = config(out.path());
    config.concurrency = 1;
    config.max_total_bytes = Some(150);
    let summary = Crawler::new(config, Arc::new(chain()))
        .with_store(store.clone())
        .run()
        .await
        .unwrap();
    assert_eq!(summary.stopped, Some(StopReason::ByteLimit));
    assert_eq!(store.status(), Some(CrawlStatus::Stopped));

    let report: serde_json::Value =
        serde_json::from_slice(&std::fs::read(out.path().join(REPORT_JSON)).unwrap()).unwrap();
    assert_eq!(report["status"], "stopped: download size limit reached");
}
