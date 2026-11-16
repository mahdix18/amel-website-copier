#![allow(clippy::unwrap_used)]

use mc_fixtures::{FixtureServer, Site};
use mc_requester::{
    FetchContext, FetchError, FetchRequest, RequesterFactory, SharedResources, StatusCode,
};
use mc_requester_wreq::WreqFactory;
use serde_json::json;

async fn get(
    config: serde_json::Value,
    server: &FixtureServer,
    path: &str,
) -> Result<mc_requester::FetchResponse, FetchError> {
    let requester = WreqFactory
        .build(&config, &SharedResources::default())
        .unwrap();
    requester
        .fetch(
            FetchRequest::get(server.url(path)),
            &FetchContext::default(),
        )
        .await
}

#[tokio::test]
async fn fetches_a_page() {
    let server = FixtureServer::start(Site::StaticBlog).await.unwrap();
    let response = get(json!(null), &server, "/").await.unwrap();
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.mime_type().as_deref(), Some("text/html"));
    let body = response.body.as_bytes().unwrap();
    assert!(String::from_utf8_lossy(body).contains("Amel fixture: static-blog"));
}

#[tokio::test]
async fn records_redirect_hops() {
    let server = FixtureServer::start(Site::StaticBlog).await.unwrap();
    let response = get(json!({}), &server, "/old-about").await.unwrap();
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.final_url, server.url("/about/"));
    assert_eq!(response.redirects.len(), 1);
    assert_eq!(response.redirects[0].from, server.url("/old-about"));
}

#[tokio::test]
async fn http_errors_are_responses() {
    let server = FixtureServer::start(Site::StaticBlog).await.unwrap();
    let response = get(json!({}), &server, "/missing.html").await.unwrap();
    assert_eq!(response.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn passes_the_browser_gate() {
    let server = FixtureServer::start(Site::UaGate).await.unwrap();
    for profile in ["chrome_149", "firefox_151", "safari_26.4"] {
        let response = get(json!({ "profile": profile }), &server, "/")
            .await
            .unwrap();
        assert_eq!(response.status, StatusCode::OK, "{profile}");
    }
}

#[tokio::test]
async fn enforces_the_body_limit() {
    let server = FixtureServer::start(Site::StaticBlog).await.unwrap();
    let err = get(json!({ "max_body_bytes": 10 }), &server, "/")
        .await
        .unwrap_err();
    assert!(matches!(err, FetchError::TooLarge { limit: 10 }), "{err}");
}

#[tokio::test]
async fn connection_failures_are_errors() {
    let server = FixtureServer::start(Site::StaticBlog).await.unwrap();
    let url = server.url("/");
    server.stop().await.unwrap();
    let requester = WreqFactory
        .build(&json!({}), &SharedResources::default())
        .unwrap();
    let err = requester
        .fetch(FetchRequest::get(url), &FetchContext::default())
        .await
        .unwrap_err();
    assert!(matches!(err, FetchError::Connect(_)), "{err:?}");
}

#[test]
fn rejects_bad_config() {
    for config in [
        json!({ "profile": "netscape_4" }),
        json!({ "bogus": true }),
        json!({ "platform": "amiga" }),
    ] {
        let err = WreqFactory
            .build(&config, &SharedResources::default())
            .err()
            .unwrap();
        assert!(
            err.to_string()
                .starts_with("invalid configuration for requester `wreq`"),
            "{err}"
        );
    }
}

#[tokio::test]
async fn resumes_a_large_download_after_the_connection_drops() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    let server = FixtureServer::start(Site::Hostile).await.unwrap();
    let partial = tempfile::tempdir().unwrap();
    let requester = WreqFactory
        .build(&json!(null), &SharedResources::default())
        .unwrap();
    let seen = Arc::new(AtomicU64::new(0));
    let progress = {
        let seen = Arc::clone(&seen);
        mc_requester::ProgressSink(Arc::new(move |received, _| {
            seen.store(received, Ordering::SeqCst);
        }))
    };
    let context = FetchContext {
        partial_dir: Some(partial.path().to_path_buf()),
        progress: Some(progress),
        ..FetchContext::default()
    };
    let request = || FetchRequest::get(server.url("/big.bin"));

    // The first try breaks off halfway; what arrived stays on disk.
    let first = requester.fetch(request(), &context).await;
    assert!(first.is_err(), "the server cuts the first download");
    let halfway = seen.load(Ordering::SeqCst);
    assert!(halfway >= 3 * 1024 * 1024, "{halfway}");
    let parts: Vec<_> = std::fs::read_dir(partial.path()).unwrap().collect();
    assert_eq!(parts.len(), 2, "the partial body and its validator");

    // The retry asks for the rest only, and gets a whole, correct file.
    let second = requester.fetch(request(), &context).await.unwrap();
    assert_eq!(second.status, StatusCode::OK);
    let mc_requester::Body::File(path) = &second.body else {
        panic!("a large body is handed over as a file");
    };
    let body = std::fs::read(path).unwrap();
    assert_eq!(body.len(), mc_fixtures::BIG_BYTES);
    assert!(
        body.iter()
            .enumerate()
            .all(|(i, b)| *b == mc_fixtures::big_byte(i)),
        "the resumed part joins the first exactly"
    );
    assert_eq!(seen.load(Ordering::SeqCst), mc_fixtures::BIG_BYTES as u64);
}
