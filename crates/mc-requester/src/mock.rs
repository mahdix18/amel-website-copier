//! An in-memory [`Requester`] that serves canned responses.
//!
//! Lets the crawler be tested without a network, and doubles as a check that
//! the trait is enough to write a requester outside this workspace.

use std::collections::HashMap;
use std::sync::{Mutex, PoisonError};

use bytes::Bytes;
use http::header::HeaderName;
use http::{HeaderMap, HeaderValue, StatusCode, header};
use url::Url;

use crate::{
    Body, Capabilities, FetchContext, FetchError, FetchRequest, FetchResponse, Redirect, Requester,
};

const MAX_REDIRECTS: usize = 10;

/// What the mock does for one URL.
#[derive(Debug, Clone)]
pub enum MockRoute {
    Respond {
        status: StatusCode,
        headers: HeaderMap,
        body: Bytes,
    },
    Redirect {
        status: StatusCode,
        to: Url,
    },
    Fail(MockFailure),
}

impl MockRoute {
    pub fn html(body: impl Into<Bytes>) -> Self {
        Self::respond(StatusCode::OK, "text/html; charset=utf-8", body)
    }

    pub fn respond(status: StatusCode, content_type: &str, body: impl Into<Bytes>) -> Self {
        Self::status(status, body).with_header(header::CONTENT_TYPE, content_type)
    }

    /// A response with no `Content-Type`.
    pub fn status(status: StatusCode, body: impl Into<Bytes>) -> Self {
        Self::Respond {
            status,
            headers: HeaderMap::new(),
            body: body.into(),
        }
    }

    pub fn redirect(to: Url) -> Self {
        Self::Redirect {
            status: StatusCode::FOUND,
            to,
        }
    }

    /// Adds a response header. Ignored on redirects and failures, and when
    /// the value isn't a valid header value.
    pub fn with_header(mut self, name: HeaderName, value: &str) -> Self {
        if let Self::Respond { headers, .. } = &mut self
            && let Ok(value) = HeaderValue::from_str(value)
        {
            headers.append(name, value);
        }
        self
    }
}

/// Transport failures the mock can simulate. `FetchError` itself is not
/// `Clone`, so routes store this and build a fresh error per call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MockFailure {
    Timeout,
    Connect,
}

impl MockFailure {
    fn to_error(self) -> FetchError {
        match self {
            Self::Timeout => FetchError::Timeout,
            Self::Connect => FetchError::Connect("mock connection refused".to_owned()),
        }
    }
}

/// Serves registered routes and answers 404 for anything else.
#[derive(Debug, Default)]
pub struct MockRequester {
    capabilities: Capabilities,
    /// The nth request for a URL gets the nth route; the last one repeats.
    routes: HashMap<Url, Vec<MockRoute>>,
    hits: Mutex<HashMap<Url, usize>>,
    requests: Mutex<Vec<FetchRequest>>,
}

impl MockRequester {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_capabilities(mut self, capabilities: Capabilities) -> Self {
        self.capabilities = capabilities;
        self
    }

    pub fn route(self, url: Url, route: MockRoute) -> Self {
        self.route_sequence(url, vec![route])
    }

    /// Serves `routes` to successive requests for `url`, repeating the last.
    pub fn route_sequence(mut self, url: Url, routes: Vec<MockRoute>) -> Self {
        self.routes.insert(url, routes);
        self
    }

    /// Every URL passed to `fetch`, in call order (redirect hops excluded).
    pub fn calls(&self) -> Vec<Url> {
        self.requests()
            .into_iter()
            .map(|request| request.url)
            .collect()
    }

    /// Every request passed to `fetch`, in call order.
    pub fn requests(&self) -> Vec<FetchRequest> {
        self.requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn next_route(&self, url: &Url) -> Option<MockRoute> {
        let routes = self.routes.get(url)?;
        let mut hits = self.hits.lock().unwrap_or_else(PoisonError::into_inner);
        let hit = hits.entry(url.clone()).or_default();
        let route = routes.get(*hit).or_else(|| routes.last()).cloned();
        *hit += 1;
        route
    }
}

#[async_trait::async_trait]
impl Requester for MockRequester {
    fn id(&self) -> &'static str {
        "mock"
    }

    fn capabilities(&self) -> Capabilities {
        self.capabilities
    }

    async fn fetch(
        &self,
        request: FetchRequest,
        _context: &FetchContext,
    ) -> Result<FetchResponse, FetchError> {
        self.requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(request.clone());

        let limit = request.max_body_bytes;
        let mut url = request.url;
        let mut redirects = Vec::new();
        loop {
            let (status, headers, body) = match self.next_route(&url) {
                None => (StatusCode::NOT_FOUND, HeaderMap::new(), Bytes::new()),
                Some(MockRoute::Fail(failure)) => return Err(failure.to_error()),
                Some(MockRoute::Respond {
                    status,
                    headers,
                    body,
                }) => (status, headers, body),
                Some(MockRoute::Redirect { status, to }) => {
                    if redirects.len() == MAX_REDIRECTS {
                        return Err(FetchError::Other(
                            format!("more than {MAX_REDIRECTS} redirects").into(),
                        ));
                    }
                    redirects.push(Redirect {
                        from: url,
                        to: to.clone(),
                        status,
                    });
                    url = to;
                    continue;
                }
            };
            if let Some(limit) = limit.filter(|limit| body.len() as u64 > *limit) {
                return Err(FetchError::TooLarge { limit });
            }
            let mut response = FetchResponse::new(url, status, headers, Body::Bytes(body));
            response.redirects = redirects;
            return Ok(response);
        }
    }
}
