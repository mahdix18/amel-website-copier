//! JavaScript-built pages, end to end with real Chromium. Needs the
//! `browser` feature and a Chromium from `mistercopy browsers install`;
//! the tests skip, saying so, when no browser is installed.

#![cfg(feature = "browser")]
#![allow(clippy::unwrap_used)]

mod common;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use common::{assert_links_resolve_offline, host_dir};
use mc_core::{CrawlConfig, CrawlSummary, Crawler};
use mc_fixtures::{FixtureServer, Site};
use mc_requester::{HybridRequester, Requester, RequesterFactory, SharedResources};
use mc_requester_playwright::{PlaywrightFactory, installed_chromium};
use mc_requester_wreq::WreqFactory;

fn browser_available() -> bool {
    let found = !installed_chromium().is_empty();
    if !found {
        eprintln!("skipping: no Chromium installed (run `mistercopy browsers install`)");
    }
    found
}

fn requester(id: &str, shared: &SharedResources) -> Arc<dyn Requester> {
    let config = serde_json::json!({});
    match id {
        "wreq" => WreqFactory.build(&config, shared).unwrap(),
        "playwright" => PlaywrightFactory.build(&config, shared).unwrap(),
        _ => Arc::new(HybridRequester::new(
            WreqFactory.build(&config, shared).unwrap(),
            PlaywrightFactory.build(&config, shared).unwrap(),
        )),
    }
}

struct Copy {
    _out: tempfile::TempDir,
    root: PathBuf,
    summary: CrawlSummary,
    requests: Vec<String>,
}

async fn copy(site: Site, requester_id: &str) -> Copy {
    let server = FixtureServer::start(site).await.unwrap();
    let out = tempfile::tempdir().unwrap();
    let shared = SharedResources {
        default_timeout: Duration::from_secs(20),
        ..SharedResources::default()
    };
    let mut config = CrawlConfig::new(server.base_url(), out.path());
    config.per_host_delay = Duration::ZERO;
    let summary = Crawler::new(config, requester(requester_id, &shared))
        .run()
        .await
        .unwrap();
    let copy = Copy {
        root: host_dir(out.path(), server.addr().port()),
        requests: server.requests(),
        summary,
        _out: out,
    };
    server.stop().await.unwrap();
    copy
}

fn read(copy: &Copy, rel: &str) -> String {
    std::fs::read_to_string(copy.root.join(rel)).unwrap_or_else(|e| panic!("{rel}: {e}"))
}

#[tokio::test]
async fn playwright_renders_routes_and_lazy_images() {
    if !browser_available() {
        return;
    }
    let copy = copy(Site::Spa, "playwright").await;
    assert!(copy.summary.failed.is_empty(), "{:?}", copy.summary.failed);

    let index = read(&copy, "index.html");
    assert!(index.contains("Welcome home"), "{index}");
    assert!(
        !index.contains("<script"),
        "scripts are stripped from snapshots: {index}"
    );
    assert!(index.contains(r#"href="about.html""#), "{index}");
    assert!(index.contains(r#"src="img/lazy-1.svg""#), "{index}");

    // A route with no file on the server, reached by a client-side link.
    assert!(read(&copy, "about.html").contains("About us"));
    for file in [
        "img/lazy-1.svg",
        "img/lazy-2.svg",
        "img/lazy-3.svg",
        "api/home.json",
        "style.css",
    ] {
        assert!(copy.root.join(file).is_file(), "missing {file}");
    }
    // Captured while rendering, so never fetched a second time.
    let lazy_hits = copy
        .requests
        .iter()
        .filter(|r| *r == "/img/lazy-1.svg")
        .count();
    assert_eq!(lazy_hits, 1, "{:#?}", copy.requests);
    assert_links_resolve_offline(&copy.root, true);
}

#[tokio::test]
async fn http_alone_sees_only_the_shell() {
    let copy = copy(Site::Spa, "wreq").await;
    let index = read(&copy, "index.html");
    assert!(!index.contains("Welcome home"), "{index}");
    assert!(!copy.root.join("img").exists());
}

#[tokio::test]
async fn auto_uses_the_browser_only_where_needed() {
    if !browser_available() {
        return;
    }
    let spa = copy(Site::Spa, "auto").await;
    assert!(read(&spa, "index.html").contains("Welcome home"));
    assert!(spa.root.join("img/lazy-3.svg").is_file());

    // Static pages have real text, so they stay on plain HTTP: the saved
    // page still has its script, which the browser would have stripped.
    let blog = copy(Site::StaticBlog, "auto").await;
    assert!(blog.summary.failed.is_empty(), "{:?}", blog.summary.failed);
    assert!(read(&blog, "index.html").contains("<script"));
}
