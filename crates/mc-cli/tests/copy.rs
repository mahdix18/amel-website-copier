//! End-to-end copies of the fixture sites over real HTTP, checked offline.

#![allow(clippy::unwrap_used)]

mod common;

use std::path::PathBuf;
use std::process::Output;
use std::time::Duration;

use common::{assert_links_resolve_offline, files, host_dir, tree};
use mc_core::{
    CrawlConfig, CrawlEvent, CrawlStatus, CrawlSummary, Crawler, REPORT_HTML, REPORT_JSON,
};
use mc_fixtures::{FixtureServer, Site};
use mc_requester::{Requester, RequesterFactory, SharedResources, Url};
use mc_requester_wreq::WreqFactory;
use mc_store::SqliteStore;
use std::sync::Arc;

fn requester() -> Arc<dyn Requester> {
    WreqFactory
        .build(&serde_json::json!({}), &SharedResources::default())
        .unwrap()
}

fn config(seed: Url, out: &std::path::Path) -> CrawlConfig {
    let mut config = CrawlConfig::new(seed, out);
    config.per_host_delay = Duration::ZERO;
    config.retry_base_delay = Duration::from_millis(10);
    config
}

async fn copy(site: Site) -> (tempfile::TempDir, PathBuf, CrawlSummary) {
    let server = FixtureServer::start(site).await.unwrap();
    let out = tempfile::tempdir().unwrap();
    let summary = Crawler::new(config(server.base_url(), out.path()), requester())
        .run()
        .await
        .unwrap();
    let root = host_dir(out.path(), server.addr().port());
    server.stop().await.unwrap();
    (out, root, summary)
}

async fn mistercopy(args: &[&str]) -> Output {
    tokio::process::Command::new(env!("CARGO_BIN_EXE_mistercopy"))
        .args(args)
        .output()
        .await
        .unwrap()
}

#[tokio::test]
async fn copies_the_static_blog() {
    let (out, root, summary) = copy(Site::StaticBlog).await;
    assert!(summary.failed.is_empty(), "{:?}", summary.failed);

    for file in [
        "index.html",
        "about/index.html",
        "posts/hello world.html",
        "posts/café.html",
        "assets/style.css",
        "assets/fonts.css",
        "images/logo.svg",
        "images/logo-2x.svg",
        "images/bg.svg",
        "images/pattern.svg",
    ] {
        assert!(root.join(file).is_file(), "missing {file}");
    }
    let scripts: Vec<_> = files(&root.join("assets"))
        .into_iter()
        .filter(|p| p.extension().is_some_and(|e| e == "js"))
        .collect();
    assert_eq!(scripts.len(), 1, "{scripts:?}");
    assert!(
        scripts[0]
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("app.")
    );

    let index = std::fs::read_to_string(root.join("index.html")).unwrap();
    assert!(
        index.contains(r#"href="posts/hello%20world.html""#),
        "{index}"
    );
    assert!(index.contains(r#"href="posts/caf%C3%A9.html""#), "{index}");
    assert!(
        index.contains(r#"href="posts/hello%20world.html#comments""#),
        "{index}"
    );
    assert!(
        index.contains(r#"<a href="about/index.html">A link that redirects</a>"#),
        "{index}"
    );
    assert!(index.contains(r#"href="https://example.com/""#), "{index}");

    assert_links_resolve_offline(&root, true);
    assert!(out.path().join(REPORT_HTML).is_file());
    let report: serde_json::Value =
        serde_json::from_slice(&std::fs::read(out.path().join(REPORT_JSON)).unwrap()).unwrap();
    assert_eq!(report["status"], "complete");
    assert_eq!(report["totals"]["saved"], summary.saved);
}

#[tokio::test]
async fn copies_the_ua_gated_site() {
    let (_out, root, summary) = copy(Site::UaGate).await;
    assert!(summary.failed.is_empty(), "{:?}", summary.failed);
    for file in ["index.html", "page2.html", "style.css"] {
        assert!(root.join(file).is_file(), "missing {file}");
    }
    assert_links_resolve_offline(&root, true);
}

#[tokio::test]
async fn interrupted_copy_resumes_to_the_same_tree() {
    let server = FixtureServer::start(Site::StaticBlog).await.unwrap();
    let full = tempfile::tempdir().unwrap();
    Crawler::new(config(server.base_url(), full.path()), requester())
        .run()
        .await
        .unwrap();

    let out = tempfile::tempdir().unwrap();
    let mut first = config(server.base_url(), out.path());
    first.concurrency = 1;
    let crawler =
        Crawler::new(first, requester()).with_store(SqliteStore::open(out.path()).unwrap());
    let stop = crawler.stop_handle();
    let mut events = crawler.subscribe();
    let stopper = tokio::spawn(async move {
        let mut saved = 0;
        while let Ok(event) = events.recv().await {
            if matches!(event, CrawlEvent::Saved { .. }) {
                saved += 1;
                if saved == 3 {
                    stop.stop();
                }
            }
        }
    });
    let partial = crawler.run().await.unwrap();
    stopper.await.unwrap();
    assert!(partial.stopped.is_some());
    assert!(partial.saved < 11, "stopped early: {}", partial.saved);
    assert_eq!(
        SqliteStore::open(out.path()).unwrap().status().unwrap(),
        Some(CrawlStatus::Stopped)
    );

    // A new process would reopen the same state file.
    let resumed = Crawler::new(config(server.base_url(), out.path()), requester())
        .with_store(SqliteStore::open(out.path()).unwrap())
        .run()
        .await
        .unwrap();
    assert!(resumed.stopped.is_none());
    assert!(resumed.failed.is_empty(), "{:?}", resumed.failed);
    assert_eq!(tree(out.path()), tree(full.path()));
    server.stop().await.unwrap();
}

#[tokio::test]
async fn cli_copies_and_reports_exit_codes() {
    let server = FixtureServer::start(Site::StaticBlog).await.unwrap();
    let out = tempfile::tempdir().unwrap();
    let dir = out.path().to_str().unwrap();
    let url = server.base_url();

    let first = mistercopy(&[
        "copy",
        url.as_str(),
        "--delay-ms",
        "0",
        "--quiet",
        "-o",
        dir,
    ])
    .await;
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert!(
        host_dir(out.path(), server.addr().port())
            .join("index.html")
            .is_file()
    );

    // A finished copy isn't overwritten by accident.
    let again = mistercopy(&[
        "copy",
        url.as_str(),
        "--delay-ms",
        "0",
        "--quiet",
        "-o",
        dir,
    ])
    .await;
    assert_eq!(again.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&again.stderr).contains("mistercopy update"));
    let fresh = mistercopy(&[
        "copy",
        url.as_str(),
        "--delay-ms",
        "0",
        "--quiet",
        "--fresh",
        "-o",
        dir,
    ])
    .await;
    assert!(fresh.status.success());

    let bad_regex =
        mistercopy(&["copy", url.as_str(), "--exclude", "(", "--fresh", "-o", dir]).await;
    assert_eq!(bad_regex.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&bad_regex.stderr).contains("invalid regex"));

    let bad_profile = mistercopy(&[
        "copy",
        url.as_str(),
        "--profile",
        "netscape_4",
        "--fresh",
        "-o",
        dir,
    ])
    .await;
    assert_eq!(bad_profile.status.code(), Some(2));

    let bad_auth = mistercopy(&[
        "copy",
        url.as_str(),
        "--auth",
        "nonsense",
        "--fresh",
        "-o",
        dir,
    ])
    .await;
    assert_eq!(bad_auth.status.code(), Some(2));

    let listed = mistercopy(&["requesters", "list"]).await;
    assert!(String::from_utf8_lossy(&listed.stdout).contains("wreq"));
    server.stop().await.unwrap();
}

#[tokio::test]
async fn cli_resumes_and_updates() {
    let server = FixtureServer::start(Site::StaticBlog).await.unwrap();
    let out = tempfile::tempdir().unwrap();
    let dir = out.path().to_str().unwrap();
    let url = server.base_url();

    // A zero time limit stops before the first request.
    let stopped = mistercopy(&[
        "copy",
        url.as_str(),
        "--delay-ms",
        "0",
        "--max-time-secs",
        "0",
        "-o",
        dir,
    ])
    .await;
    assert_eq!(
        stopped.status.code(),
        Some(3),
        "{}",
        String::from_utf8_lossy(&stopped.stderr)
    );
    assert!(String::from_utf8_lossy(&stopped.stderr).contains("mistercopy resume"));
    let update_too_early = mistercopy(&["update", dir]).await;
    assert_eq!(update_too_early.status.code(), Some(2));

    // Saved options, limits included, apply to every run: a zero time
    // limit stops the resumed run too.
    let resumed = mistercopy(&["resume", dir]).await;
    assert_eq!(resumed.status.code(), Some(3));

    let finished = mistercopy(&[
        "copy",
        url.as_str(),
        "--delay-ms",
        "0",
        "--quiet",
        "--fresh",
        "-o",
        dir,
    ])
    .await;
    assert!(finished.status.success());
    let done = mistercopy(&["resume", dir]).await;
    assert!(done.status.success());
    assert!(String::from_utf8_lossy(&done.stdout).contains("already complete"));

    let updated = mistercopy(&["update", dir]).await;
    let stdout = String::from_utf8_lossy(&updated.stdout);
    assert!(
        updated.status.success(),
        "{}",
        String::from_utf8_lossy(&updated.stderr)
    );
    assert!(stdout.contains("same    "), "{stdout}");
    assert!(stdout.contains("unchanged"), "{stdout}");
    assert_links_resolve_offline(&host_dir(out.path(), server.addr().port()), true);

    let missing = mistercopy(&["resume", out.path().join("nowhere").to_str().unwrap()]).await;
    assert_eq!(missing.status.code(), Some(2));
    server.stop().await.unwrap();
}

#[tokio::test]
async fn cli_scan_saves_only_the_report() {
    let server = FixtureServer::start(Site::StaticBlog).await.unwrap();
    let out = tempfile::tempdir().unwrap();
    let report = out.path().join("scan.json");
    let scanned = mistercopy(&[
        "scan",
        server.base_url().as_str(),
        "--delay-ms",
        "0",
        "--quiet",
        "--report",
        report.to_str().unwrap(),
    ])
    .await;
    assert!(
        scanned.status.success(),
        "{}",
        String::from_utf8_lossy(&scanned.stderr)
    );
    assert_eq!(files(out.path()), std::slice::from_ref(&report));
    let json: serde_json::Value = serde_json::from_slice(&std::fs::read(&report).unwrap()).unwrap();
    assert!(json["totals"]["saved"].as_u64().unwrap() >= 10, "{json}");
    assert!(String::from_utf8_lossy(&scanned.stdout).contains("Checked"));
    server.stop().await.unwrap();
}

#[tokio::test]
async fn cli_writes_warc_and_zip() {
    let server = FixtureServer::start(Site::StaticBlog).await.unwrap();
    let out = tempfile::tempdir().unwrap();
    let dir = out.path().join("copy");
    let warc = out.path().join("blog.warc.gz");
    let copied = mistercopy(&[
        "copy",
        server.base_url().as_str(),
        "--delay-ms",
        "0",
        "-q",
        "--warc",
        warc.to_str().unwrap(),
        "--zip",
        "-o",
        dir.to_str().unwrap(),
    ])
    .await;
    assert!(
        copied.status.success(),
        "{}",
        String::from_utf8_lossy(&copied.stderr)
    );

    let mut text = String::new();
    std::io::Read::read_to_string(
        &mut flate2::read::MultiGzDecoder::new(std::fs::File::open(&warc).unwrap()),
        &mut text,
    )
    .unwrap();
    let host = format!("127.0.0.1_{}", server.addr().port());
    let saved = files(&dir.join(&host)).len();
    assert_eq!(
        text.matches("WARC-Type: response").count(),
        saved,
        "one record per saved file"
    );
    assert!(text.contains(&format!(
        "WARC-Target-URI: {}posts/caf%C3%A9.html",
        server.base_url()
    )));
    assert!(
        text.contains("<h1>Café</h1>"),
        "bodies are stored as served"
    );

    let zip_path = dir.join(format!("{host}.zip"));
    let zip_bytes = std::fs::read(&zip_path).unwrap();
    assert!(zip_bytes.starts_with(b"PK"), "a zip file");
    let index_entry = format!("{host}/index.html");
    assert!(
        zip_bytes
            .windows(index_entry.len())
            .any(|w| w == index_entry.as_bytes())
    );
    server.stop().await.unwrap();
}
