#![allow(clippy::unwrap_used)]

use mc_fixtures::{FixtureServer, Site};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// A bare HTTP/1.1 GET, so the fixture tests don't depend on any requester.
async fn get(server: &FixtureServer, path: &str) -> (u16, String) {
    let mut stream = TcpStream::connect(server.addr()).await.unwrap();
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
        server.addr()
    );
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await.unwrap();

    let text = String::from_utf8_lossy(&raw).into_owned();
    let status = text.split_whitespace().nth(1).unwrap().parse().unwrap();
    let body = text
        .split_once("\r\n\r\n")
        .map(|(_, body)| body.to_owned())
        .unwrap_or_default();
    (status, body)
}

#[tokio::test]
async fn serves_the_static_blog() {
    let server = FixtureServer::start(Site::StaticBlog).await.unwrap();

    let (status, body) = get(&server, "/").await;
    assert_eq!(status, 200);
    assert!(body.contains("MisterCopy fixture: static-blog"));

    for path in [
        "/posts/hello%20world.html",
        "/posts/caf%C3%A9.html",
        "/about/",
        "/assets/style.css",
        "/assets/fonts.css",
        "/assets/app.js?v=3",
        "/images/logo.svg",
        "/images/logo-2x.svg",
        "/images/bg.svg",
        "/images/pattern.svg",
    ] {
        assert_eq!(get(&server, path).await.0, 200, "{path}");
    }

    assert_eq!(get(&server, "/missing.html").await.0, 404);
    assert_eq!(get(&server, "/old-about").await.0, 308);
    server.stop().await.unwrap();
}

#[tokio::test]
async fn ua_gate_turns_away_plain_clients() {
    let server = FixtureServer::start(Site::UaGate).await.unwrap();
    // The bare request `get` sends has no browser headers.
    let (status, body) = get(&server, "/").await;
    assert_eq!(status, 403);
    assert_eq!(body, "browser required\n");
    server.stop().await.unwrap();
}

#[test]
fn site_names_round_trip() {
    for site in Site::ALL {
        assert_eq!(site.name().parse::<Site>().unwrap(), *site);
        assert!(site.root().is_dir(), "{} has no directory", site);
    }
    let err = "nope".parse::<Site>().unwrap_err();
    assert_eq!(
        err.to_string(),
        "unknown fixture site `nope` (available: static-blog, ua-gate, hostile, spa, auth)"
    );
}
