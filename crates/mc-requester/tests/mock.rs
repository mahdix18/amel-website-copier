#![allow(clippy::unwrap_used)]

use mc_requester::mock::{MockFailure, MockRequester, MockRoute};
use mc_requester::{FetchContext, FetchError, FetchRequest, Requester, StatusCode, Url};

fn url(s: &str) -> Url {
    Url::parse(s).unwrap()
}

async fn get(mock: &MockRequester, s: &str) -> Result<mc_requester::FetchResponse, FetchError> {
    mock.fetch(FetchRequest::get(url(s)), &FetchContext::default())
        .await
}

#[tokio::test]
async fn serves_routes_with_content_type() {
    let mock = MockRequester::new().route(url("https://a.test/"), MockRoute::html("<p>hi</p>"));
    let response = get(&mock, "https://a.test/").await.unwrap();

    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.mime_type().as_deref(), Some("text/html"));
    assert_eq!(response.body.as_bytes().unwrap().as_ref(), b"<p>hi</p>");
    assert!(response.redirects.is_empty());
}

#[tokio::test]
async fn unknown_urls_are_404_responses_not_errors() {
    let mock = MockRequester::new();
    let response = get(&mock, "https://a.test/missing").await.unwrap();
    assert_eq!(response.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn follows_and_records_redirects() {
    let mock = MockRequester::new()
        .route(
            url("http://a.test/old"),
            MockRoute::redirect(url("https://a.test/old")),
        )
        .route(
            url("https://a.test/old"),
            MockRoute::redirect(url("https://a.test/new")),
        )
        .route(url("https://a.test/new"), MockRoute::html("new"));

    let response = get(&mock, "http://a.test/old").await.unwrap();

    assert_eq!(response.final_url, url("https://a.test/new"));
    let hops: Vec<_> = response.redirects.iter().map(|r| r.to.as_str()).collect();
    assert_eq!(hops, ["https://a.test/old", "https://a.test/new"]);
    assert_eq!(mock.calls(), [url("http://a.test/old")]);
}

#[tokio::test]
async fn redirect_loops_are_errors() {
    let mock = MockRequester::new()
        .route(
            url("https://a.test/a"),
            MockRoute::redirect(url("https://a.test/b")),
        )
        .route(
            url("https://a.test/b"),
            MockRoute::redirect(url("https://a.test/a")),
        );
    let err = get(&mock, "https://a.test/a").await.unwrap_err();
    assert!(err.to_string().contains("redirects"));
}

#[tokio::test]
async fn simulates_transport_failures() {
    let mock = MockRequester::new()
        .route(
            url("https://a.test/slow"),
            MockRoute::Fail(MockFailure::Timeout),
        )
        .route(
            url("https://a.test/down"),
            MockRoute::Fail(MockFailure::Connect),
        );

    assert!(matches!(
        get(&mock, "https://a.test/slow").await,
        Err(FetchError::Timeout)
    ));
    assert!(matches!(
        get(&mock, "https://a.test/down").await,
        Err(FetchError::Connect(_))
    ));
}

#[test]
fn mime_type_strips_parameters_and_lowercases() {
    use mc_requester::{Body, FetchResponse, HeaderMap, header};
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        "Text/CSS ; charset=utf-8".parse().unwrap(),
    );
    let response = FetchResponse::new(
        url("https://a.test/s.css"),
        StatusCode::OK,
        headers,
        Body::empty(),
    );
    assert_eq!(response.mime_type().as_deref(), Some("text/css"));
}
