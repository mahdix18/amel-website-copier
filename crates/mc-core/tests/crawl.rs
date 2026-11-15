#![allow(clippy::unwrap_used)]

use std::sync::Arc;
use std::time::Duration;

use mc_core::{CrawlConfig, CrawlEvent, Crawler, Rule, RuleAction};
use mc_requester::StatusCode;
use mc_requester::mock::{MockFailure, MockRequester, MockRoute};
use url::Url;

fn url(s: &str) -> Url {
    Url::parse(s).unwrap()
}

fn site() -> MockRequester {
    MockRequester::new()
        .route(
            url("https://site.test/"),
            MockRoute::html(
                r#"<link rel="stylesheet" href="/css/main.css">
                <a href="about">About</a>
                <a href="old-link">Moved</a>
                <a href="posts/hello%20world.html#c">Hello</a>
                <a href="missing.html">Missing</a>
                <a href="down.html">Down</a>
                <a href="admin/">Admin</a>
                <a href="https://elsewhere.test/">Elsewhere</a>
                <img src="logo.png?v=2">"#,
            ),
        )
        .route(url("https://site.test/about"), MockRoute::html(r#"<a href="/">Home</a>"#))
        .route(url("https://site.test/old-link"), MockRoute::redirect(url("https://site.test/about")))
        .route(
            url("https://site.test/posts/hello%20world.html"),
            MockRoute::html(r#"<a href="../">Home</a><img src="/logo.png?v=2"><a href="deeper.html">Deeper</a>"#),
        )
        .route(url("https://site.test/posts/deeper.html"), MockRoute::html("deep"))
        .route(
            url("https://site.test/css/main.css"),
            MockRoute::respond(StatusCode::OK, "text/css", r#"body { background: url("../img/bg.svg") }"#),
        )
        .route(url("https://site.test/img/bg.svg"), MockRoute::respond(StatusCode::OK, "image/svg+xml", "<svg/>"))
        .route(url("https://site.test/logo.png?v=2"), MockRoute::respond(StatusCode::OK, "image/png", "PNG"))
        .route(url("https://site.test/down.html"), MockRoute::Fail(MockFailure::Connect))
        .route(url("https://site.test/admin/"), MockRoute::html("secret"))
}

fn config(out: &std::path::Path) -> CrawlConfig {
    let mut config = CrawlConfig::new(url("https://site.test/"), out);
    config.per_host_delay = Duration::ZERO;
    config.retry_base_delay = Duration::from_millis(1);
    config.rules = vec![Rule::new("/admin/", RuleAction::Exclude).unwrap()];
    config
}

/// Calls other than robots.txt.
fn page_calls(requester: &MockRequester) -> Vec<Url> {
    requester
        .calls()
        .into_iter()
        .filter(|u| u.path() != "/robots.txt")
        .collect()
}

fn read(out: &std::path::Path, rel: &str) -> String {
    std::fs::read_to_string(out.join(rel)).unwrap_or_else(|e| panic!("{rel}: {e}"))
}

#[tokio::test]
async fn copies_and_rewrites_a_site() {
    let out = tempfile::tempdir().unwrap();
    let requester = Arc::new(site());
    let summary = Crawler::new(config(out.path()), requester.clone())
        .run()
        .await
        .unwrap();

    let root = out.path().join("site.test");
    let index = read(&root, "index.html");
    assert!(index.contains(r#"href="css/main.css""#), "{index}");
    assert!(index.contains(r#"href="about.html""#), "{index}");
    // The redirect resolves to the page it lands on.
    assert!(
        index.contains(r#"<a href="about.html">Moved</a>"#),
        "{index}"
    );
    assert!(
        index.contains(r#"href="posts/hello%20world.html#c""#),
        "{index}"
    );
    // Not copied: links stay pointed at the live site.
    assert!(
        index.contains(r#"href="https://site.test/missing.html""#),
        "{index}"
    );
    assert!(
        index.contains(r#"href="https://site.test/admin/""#),
        "{index}"
    );
    assert!(
        index.contains(r#"href="https://elsewhere.test/""#),
        "{index}"
    );

    let logo = std::fs::read_dir(&root)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .find(|name| name.starts_with("logo.") && name.ends_with(".png"))
        .expect("query-string image saved with a hashed name");
    assert!(index.contains(&format!(r#"src="{logo}""#)), "{index}");

    let post = read(&root, "posts/hello world.html");
    assert!(post.contains(r#"href="../index.html""#), "{post}");
    assert!(post.contains(&format!(r#"src="../{logo}""#)), "{post}");
    assert!(post.contains(r#"href="deeper.html""#), "{post}");

    assert_eq!(
        read(&root, "css/main.css"),
        r#"body { background: url("../img/bg.svg") }"#
    );
    assert_eq!(read(&root, "img/bg.svg"), "<svg/>");
    assert!(!root.join("admin").exists());

    let failed: Vec<_> = summary.failed.iter().map(|(u, _)| u.path()).collect();
    assert!(
        failed.contains(&"/missing.html") && failed.contains(&"/down.html"),
        "{failed:?}"
    );
    // 4 pages, 1 stylesheet, 2 images; the redirect is an alias, not a copy.
    assert_eq!(summary.saved, 7);
    assert!(
        !requester
            .calls()
            .iter()
            .any(|u| u.host_str() == Some("elsewhere.test"))
    );
    assert!(
        !requester
            .calls()
            .iter()
            .any(|u| u.path().starts_with("/admin"))
    );
}

#[tokio::test]
async fn depth_limit_stops_page_links_but_not_assets() {
    let out = tempfile::tempdir().unwrap();
    let mut config = config(out.path());
    config.max_depth = Some(1);
    let requester = Arc::new(site());
    Crawler::new(config, requester.clone()).run().await.unwrap();

    let root = out.path().join("site.test");
    assert!(root.join("posts/hello world.html").exists());
    assert!(!root.join("posts/deeper.html").exists());
    assert!(root.join("img/bg.svg").exists());
    let post = read(&root, "posts/hello world.html");
    assert!(
        post.contains(r#"href="https://site.test/posts/deeper.html""#),
        "{post}"
    );
}

#[tokio::test]
async fn url_limit_caps_fetches() {
    let out = tempfile::tempdir().unwrap();
    let mut config = config(out.path());
    config.max_urls = Some(3);
    config.concurrency = 1;
    let requester = Arc::new(site());
    let summary = Crawler::new(config, requester.clone()).run().await.unwrap();
    assert_eq!(page_calls(&requester).len(), 3);
    assert!(summary.skipped > 0);
}

#[tokio::test]
async fn reports_progress_events() {
    let out = tempfile::tempdir().unwrap();
    let crawler = Crawler::new(config(out.path()), Arc::new(site()));
    let mut events = crawler.subscribe();
    let summary = crawler.run().await.unwrap();

    let mut saved = 0;
    let mut rewriting = false;
    while let Ok(event) = events.try_recv() {
        match event {
            CrawlEvent::Saved { .. } => saved += 1,
            CrawlEvent::Rewriting { documents } => rewriting = documents == 5,
            _ => {}
        }
    }
    assert_eq!(saved, summary.saved);
    assert!(rewriting);
}

#[tokio::test]
async fn rejects_bad_input_before_crawling() {
    let out = tempfile::tempdir().unwrap();
    let mut config = CrawlConfig::new(url("ftp://site.test/"), out.path());
    assert!(
        Crawler::new(config.clone(), Arc::new(site()))
            .run()
            .await
            .is_err()
    );

    config.seeds = vec![url("https://site.test/")];
    config.concurrency = 0;
    assert!(
        Crawler::new(config.clone(), Arc::new(site()))
            .run()
            .await
            .is_err()
    );

    // The output path exists but is a file.
    let file = out.path().join("file");
    std::fs::write(&file, "x").unwrap();
    let config = CrawlConfig::new(url("https://site.test/"), &file);
    let err = Crawler::new(config, Arc::new(site()))
        .run()
        .await
        .err()
        .unwrap();
    assert!(
        err.to_string().contains("cannot use output directory"),
        "{err}"
    );
}

#[tokio::test]
async fn reports_workers_and_progress() {
    let out = tempfile::tempdir().unwrap();
    let mut config = config(out.path());
    config.concurrency = 3;
    let crawler = Crawler::new(config, Arc::new(site()));
    let mut events = crawler.subscribe();
    let summary = crawler.run().await.unwrap();

    let (mut fetching, mut idle, mut last) = (0, 0, None);
    let mut previous_done = 0;
    while let Ok(event) = events.try_recv() {
        match event {
            CrawlEvent::Fetching { worker, .. } => {
                assert!(worker < 3, "worker {worker} beyond the concurrency");
                fetching += 1;
            }
            CrawlEvent::WorkerIdle { worker } => {
                assert!(worker < 3);
                idle += 1;
            }
            CrawlEvent::Progress {
                done,
                total,
                active,
            } => {
                assert!(done >= previous_done, "progress went backwards");
                assert!(done <= total && active <= 3);
                previous_done = done;
                last = Some((done, total, active));
            }
            _ => {}
        }
    }
    assert_eq!(fetching, idle, "every request ends with its worker idle");
    assert!(fetching >= summary.saved);
    let (done, total, active) = last.unwrap();
    assert_eq!((done, active), (total, 0), "finished with nothing left");
}

#[tokio::test]
async fn sitemaps_seed_pages_nothing_links_to() {
    let out = tempfile::tempdir().unwrap();
    let requester = MockRequester::new()
        .route(url("https://site.test/"), MockRoute::html("<p>no links</p>"))
        .route(
            url("https://site.test/robots.txt"),
            MockRoute::respond(
                StatusCode::OK,
                "text/plain",
                "User-agent: *\nDisallow: /private/\nSitemap: https://site.test/index.xml\n",
            ),
        )
        .route(
            url("https://site.test/index.xml"),
            MockRoute::respond(
                StatusCode::OK,
                "application/xml",
                "<sitemapindex><sitemap><loc>https://site.test/pages.xml</loc></sitemap></sitemapindex>",
            ),
        )
        .route(
            url("https://site.test/pages.xml"),
            MockRoute::respond(
                StatusCode::OK,
                "application/xml",
                "<urlset><url><loc>https://site.test/orphan</loc></url>\
                 <url><loc>https://site.test/private/x</loc></url>\
                 <url><loc>https://other.test/away</loc></url></urlset>",
            ),
        )
        .route(url("https://site.test/orphan"), MockRoute::html("found via sitemap"));
    let requester = Arc::new(requester);
    let mut config = config(out.path());
    config.use_sitemaps = true;
    let crawler = Crawler::new(config, requester.clone());
    let mut events = crawler.subscribe();
    let summary = crawler.run().await.unwrap();

    assert!(
        out.path().join("site.test/orphan.html").is_file(),
        "{:?}",
        summary.failed
    );
    let calls = requester.calls();
    assert!(
        !calls.iter().any(|u| u.path() == "/private/x"),
        "robots.txt still applies"
    );
    assert!(
        !calls.iter().any(|u| u.host_str() == Some("other.test")),
        "scope still applies"
    );
    assert_eq!(
        calls.iter().filter(|u| u.path() == "/robots.txt").count(),
        1,
        "robots.txt is fetched once"
    );
    let mut read = Vec::new();
    while let Ok(event) = events.try_recv() {
        if let CrawlEvent::SitemapRead { url, pages } = event {
            read.push((url.path().to_owned(), pages));
        }
    }
    assert_eq!(
        read,
        [("/index.xml".to_owned(), 0), ("/pages.xml".to_owned(), 3)]
    );
}

#[tokio::test]
async fn rate_limit_slows_downloads() {
    let out = tempfile::tempdir().unwrap();
    let body = "x".repeat(10_000);
    let links: String = (0..4)
        .map(|i| format!(r#"<a href="/f{i}.txt">f</a>"#))
        .collect();
    let mut requester =
        MockRequester::new().route(url("https://site.test/"), MockRoute::html(links));
    for i in 0..4 {
        requester = requester.route(
            url(&format!("https://site.test/f{i}.txt")),
            MockRoute::respond(StatusCode::OK, "text/plain", body.clone()),
        );
    }
    let mut config = config(out.path());
    config.respect_robots = false;
    config.concurrency = 4;
    // About 40 KB at 100 KB/s: at least 0.3 s however many workers.
    config.max_rate = Some(100_000);
    let started = std::time::Instant::now();
    let summary = Crawler::new(config, Arc::new(requester))
        .run()
        .await
        .unwrap();
    assert_eq!(summary.saved, 5);
    assert!(
        started.elapsed() >= Duration::from_millis(300),
        "{:?}",
        started.elapsed()
    );
}
