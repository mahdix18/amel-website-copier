//! Copies through real proxies: rotation over a list, credentials in the
//! URL, bypass rules, and a clear failure when the proxy refuses.

#![allow(clippy::unwrap_used)]

use mc_fixtures::{FixtureServer, ProxyServer, Site};

async fn mistercopy(args: &[&str]) -> std::process::Output {
    tokio::process::Command::new(env!("CARGO_BIN_EXE_mistercopy"))
        .args(args)
        .output()
        .await
        .unwrap()
}

#[tokio::test]
async fn rotates_over_proxies_with_and_without_credentials() {
    let site = FixtureServer::start(Site::StaticBlog).await.unwrap();
    let open = ProxyServer::start(false).await.unwrap();
    let locked = ProxyServer::start(true).await.unwrap();
    let out = tempfile::tempdir().unwrap();
    let output = mistercopy(&[
        "copy",
        site.base_url().as_str(),
        "-o",
        out.path().to_str().unwrap(),
        "-j",
        "4",
        "--delay-ms",
        "0",
        "--proxy",
        &open.url(false),
        "--proxy",
        &locked.url(true),
        "-q",
    ])
    .await;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    // 11 files and robots.txt, shared between the two proxies.
    let (a, b) = (open.carried(), locked.carried());
    assert!(a >= 4 && b >= 4, "round-robin uses both: {a} and {b}");
    assert!(a + b >= 12, "{a} + {b}");
    assert_eq!(locked.rejected(), 0, "credentials were sent up front");
}

#[tokio::test]
async fn bypassed_hosts_go_direct_and_refusals_are_reported() {
    let site = FixtureServer::start(Site::StaticBlog).await.unwrap();
    let proxy = ProxyServer::start(false).await.unwrap();
    let out = tempfile::tempdir().unwrap();
    let output = mistercopy(&[
        "copy",
        site.base_url().as_str(),
        "-o",
        out.path().to_str().unwrap(),
        "--delay-ms",
        "0",
        "--proxy",
        &proxy.url(false),
        "--no-proxy",
        "localhost,127.0.0.0/8",
        "-q",
    ])
    .await;
    assert!(output.status.success());
    assert_eq!(proxy.carried(), 0, "127.0.0.1 is in the bypass range");

    // A proxy that wants credentials and gets none: every fetch fails.
    let locked = ProxyServer::start(true).await.unwrap();
    let out = tempfile::tempdir().unwrap();
    let output = mistercopy(&[
        "copy",
        site.base_url().as_str(),
        "-o",
        out.path().to_str().unwrap(),
        "--delay-ms",
        "0",
        "--retries",
        "0",
        "--proxy",
        &locked.url(false),
    ])
    .await;
    assert!(!output.status.success());
    assert!(locked.rejected() > 0);
    assert_eq!(locked.carried(), 0);

    let empty = tempfile::tempdir().unwrap();
    let bad = mistercopy(&[
        "copy",
        "http://x.test/",
        "-o",
        empty.path().to_str().unwrap(),
        "--proxy",
        "ftp://p.test:21",
    ])
    .await;
    assert_eq!(bad.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&bad.stderr).contains("must start with"));
    assert_eq!(
        std::fs::read_dir(empty.path()).unwrap().count(),
        0,
        "nothing is written before the options are checked"
    );
}

#[tokio::test]
async fn a_dead_proxy_in_the_list_fails_nothing() {
    let site = FixtureServer::start(Site::StaticBlog).await.unwrap();
    let live = ProxyServer::start(false).await.unwrap();
    // Nothing listens on port 1.
    let dead = "http://127.0.0.1:1";
    let out = tempfile::tempdir().unwrap();
    let output = mistercopy(&[
        "copy",
        site.base_url().as_str(),
        "-o",
        out.path().to_str().unwrap(),
        "-j",
        "4",
        "--delay-ms",
        "0",
        "--retries",
        "0",
        "--proxy",
        dead,
        "--proxy",
        &live.url(false),
    ])
    .await;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        live.carried() >= 12,
        "the live proxy took over: {}",
        live.carried()
    );
}
