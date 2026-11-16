//! The `hostile` site: one link to each thing that has crashed or stalled a
//! website copier. Generated in code, since most traps are about headers,
//! timing or broken bodies rather than files.

use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Path, State};
use axum::http::{StatusCode, Uri, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use futures_util::stream;

/// Requests to `/rate-limited` answered 429 before it gives in.
const RATE_LIMITED_REFUSALS: usize = 2;

/// Paths whose names are illegal or awkward as file names somewhere,
/// matched against the raw (still percent-encoded) request path.
const AWKWARD_PATHS: &[&str] = &[
    "/CON.html",
    "/aux",
    "/name%3Fwith%3Fquestion.html",
    "/trailing.",
    "/emoji-%F0%9F%98%80.html",
    "/latin1-%FF.html",
    "/Page.html",
    "/page.html",
];

/// Everything the index links to, in order.
const TRAPS: &[&str] = &[
    "loop-a",
    "calendar/1",
    "bad-gzip",
    "garbage-type",
    "no-type",
    "binary.html",
    "bad-location",
    "redirect-offsite",
    "rate-limited",
    "always-503",
    "retry-forever",
    "huge.bin",
    "slow",
    "truncated",
    "empty.html",
    "private/secret.html",
    "mailto:someone@example.invalid",
    "javascript:alert(1)",
    "data:text/plain,hello",
    "http://[broken",
    "//offsite.invalid/protocol-relative",
];

pub(crate) fn router() -> Router {
    let refusals = Arc::new(AtomicUsize::new(0));
    Router::new()
        .route("/", get(index))
        .route("/robots.txt", get(robots))
        .route("/loop-a", get(|| async { redirect("/loop-b") }))
        .route("/loop-b", get(|| async { redirect("/loop-a") }))
        .route("/calendar/{day}", get(calendar))
        .route("/long/{*rest}", get(|| async { Html("<p>long</p>") }))
        .route("/deep/{*rest}", get(|| async { Html("<p>deep</p>") }))
        .route("/bad-gzip", get(bad_gzip))
        .route("/garbage-type", get(garbage_type))
        .route("/no-type", get(no_type))
        .route("/binary.html", get(binary))
        .route("/bad-location", get(bad_location))
        .route(
            "/redirect-offsite",
            get(|| async { redirect("http://offsite.invalid/") }),
        )
        .route("/rate-limited", get(rate_limited).with_state(refusals))
        .route("/always-503", get(|| async { busy("1") }))
        .route("/retry-forever", get(|| async { busy("3600") }))
        .route("/huge.bin", get(huge))
        .route("/slow", get(slow))
        .route("/truncated", get(truncated))
        // Not linked: for resuming large downloads.
        .route("/big.bin", get(big))
        .route("/empty.html", get(|| async { Html("") }))
        .route("/ok", get(|| async { Html("<p>ok</p>") }))
        .route(
            "/private/secret.html",
            get(|| async { Html("<p>robots.txt says no</p>") }),
        )
        .fallback(awkward)
}

async fn index() -> Html<String> {
    let mut links: Vec<String> = TRAPS.iter().map(|t| (*t).to_owned()).collect();
    links.push(format!("long/{}.html", "a".repeat(300)));
    links.push(format!("deep/{}x.html", "d/".repeat(150)));
    links.extend(
        AWKWARD_PATHS
            .iter()
            .map(|p| p.trim_start_matches('/').to_owned()),
    );
    let anchors: String = links
        .iter()
        .map(|href| format!("<li><a href=\"{href}\">{href}</a></li>\n"))
        .collect();
    Html(format!(
        "<!doctype html><title>Amel fixture: hostile</title>\n<ul>\n{anchors}</ul>\n"
    ))
}

async fn robots() -> &'static str {
    include_str!("../sites/hostile/robots.txt")
}

fn redirect(to: &'static str) -> Response {
    (StatusCode::FOUND, [(header::LOCATION, to)]).into_response()
}

/// An endless chain of days, each linking to the next.
async fn calendar(Path(day): Path<u64>) -> Html<String> {
    Html(format!(
        r#"<p>Day {day}</p><a href="{}">next</a><a href="{}">previous</a>"#,
        day + 1,
        day.saturating_sub(1)
    ))
}

/// Claims gzip, isn't.
async fn bad_gzip() -> Response {
    (
        [
            (header::CONTENT_TYPE, "text/html"),
            (header::CONTENT_ENCODING, "gzip"),
        ],
        "this is not gzip",
    )
        .into_response()
}

async fn garbage_type() -> Response {
    (
        [(header::CONTENT_TYPE, "text/html; charset=\"")],
        r#"<a href="ok">ok</a>"#,
    )
        .into_response()
}

async fn no_type() -> Response {
    Response::new(Body::from("<p>no content type</p>"))
}

/// Declared HTML, full of NUL bytes and invalid UTF-8 around a real link.
async fn binary() -> Response {
    let mut body = vec![0u8, 0x9f, 0x92, 0x96, 0xff, 0];
    body.extend_from_slice(br#"<a href="ok">ok</a>"#);
    body.extend_from_slice(&[0, 0xc3, 0x28]);
    ([(header::CONTENT_TYPE, "text/html")], body).into_response()
}

async fn bad_location() -> Response {
    (StatusCode::FOUND, [(header::LOCATION, "http://[::1")]).into_response()
}

async fn rate_limited(State(refusals): State<Arc<AtomicUsize>>) -> Response {
    if refusals.fetch_add(1, Ordering::SeqCst) < RATE_LIMITED_REFUSALS {
        (
            StatusCode::TOO_MANY_REQUESTS,
            [(header::RETRY_AFTER, "1")],
            "slow down",
        )
            .into_response()
    } else {
        Html("<p>let in at last</p>").into_response()
    }
}

fn busy(retry_after: &'static str) -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        [(header::RETRY_AFTER, retry_after)],
        "busy",
    )
        .into_response()
}

/// 8 MiB, streamed without a Content-Length so the limit must be enforced
/// while reading.
async fn huge() -> Response {
    let chunk = Bytes::from(vec![b'x'; 64 * 1024]);
    let chunks = stream::iter((0..128).map(move |_| Ok::<_, io::Error>(chunk.clone())));
    (
        [(header::CONTENT_TYPE, "application/octet-stream")],
        Body::from_stream(chunks),
    )
        .into_response()
}

async fn slow() -> Html<&'static str> {
    tokio::time::sleep(Duration::from_secs(5)).await;
    Html("<p>too late</p>")
}

/// Size of `/big.bin`, and where it breaks off when asked for whole.
pub const BIG_BYTES: usize = 6 * 1024 * 1024;
const BIG_BREAK: usize = 3 * 1024 * 1024;
const BIG_ETAG: &str = "\"big-v1\"";

/// The byte at `index` of `/big.bin`, so a test can check the whole body.
pub fn big_byte(index: usize) -> u8 {
    (index % 251) as u8
}

/// A 6 MiB file that, asked for whole, breaks off halfway; only a `Range`
/// request with a matching `If-Range` gets the rest (206). So it can only be
/// downloaded by resuming.
async fn big(headers: axum::http::HeaderMap) -> Response {
    let range_start = headers
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("bytes="))
        .and_then(|v| v.strip_suffix('-'))
        .and_then(|v| v.parse::<usize>().ok());
    let same = headers.get(header::IF_RANGE).and_then(|v| v.to_str().ok()) == Some(BIG_ETAG);
    let bytes = |from: usize, to: usize| Bytes::from((from..to).map(big_byte).collect::<Vec<u8>>());
    match range_start {
        Some(start) if same && start < BIG_BYTES => (
            StatusCode::PARTIAL_CONTENT,
            [
                (header::CONTENT_TYPE, "application/octet-stream".to_owned()),
                (header::ETAG, BIG_ETAG.to_owned()),
                (
                    header::CONTENT_RANGE,
                    format!("bytes {start}-{}/{BIG_BYTES}", BIG_BYTES - 1),
                ),
                (header::CONTENT_LENGTH, (BIG_BYTES - start).to_string()),
            ],
            bytes(start, BIG_BYTES),
        )
            .into_response(),
        _ => {
            let parts = stream::iter([
                Ok(bytes(0, BIG_BREAK)),
                Err(io::Error::other("connection cut halfway")),
            ]);
            (
                [
                    (header::CONTENT_TYPE, "application/octet-stream".to_owned()),
                    (header::ETAG, BIG_ETAG.to_owned()),
                    (header::CONTENT_LENGTH, BIG_BYTES.to_string()),
                ],
                Body::from_stream(parts),
            )
                .into_response()
        }
    }
}

/// Starts a body, then drops the connection.
async fn truncated() -> Response {
    let parts = stream::iter([
        Ok(Bytes::from_static(b"<p>the start of a page")),
        Err(io::Error::other("connection cut")),
    ]);
    (
        [(header::CONTENT_TYPE, "text/html")],
        Body::from_stream(parts),
    )
        .into_response()
}

async fn awkward(uri: Uri) -> Response {
    let path = uri.path();
    if AWKWARD_PATHS.contains(&path) {
        Html(format!("<p>{path}</p>")).into_response()
    } else {
        StatusCode::NOT_FOUND.into_response()
    }
}
