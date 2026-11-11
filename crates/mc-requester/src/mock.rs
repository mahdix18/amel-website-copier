//! An in-memory [`Requester`] that serves canned responses.
//!
//! Lets the crawler be tested without a network, and doubles as a check that
//! the trait is enough to write a requester outside this workspace.

use std::collections::HashMap;
use std::sync::{Mutex, PoisonError};

use bytes::Bytes;
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
        content_type: Option<String>,
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
        Self::Respond {
            status,
            content_type: Some(content_type.to_owned()),
            body: body.into(),
        }
    }

    pub fn redirect(to: Url) -> Self {
        Self::Redirect {
            status: StatusCode::FOUND,
            to,
        }
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
    routes: HashMap<Url, MockRoute>,
    calls: Mutex<Vec<Url>>,
}

impl MockRequester {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_capabilities(mut self, capabilities: Capabilities) -> Self {
        self.capabilities = capabilities;
        self
    }

    pub fn route(mut self, url: Url, route: MockRoute) -> Self {
        self.routes.insert(url, route);
        self
    }

    /// Every URL passed to `fetch`, in call order (redirect hops excluded).
    pub fn calls(&self) -> Vec<Url> {
        self.calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
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
        self.calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(request.url.clone());

        let mut url = request.url;
        let mut redirects = Vec::new();
        loop {
            match self.routes.get(&url) {
                None => {
                    let mut response = FetchResponse::new(
                        url,
                        StatusCode::NOT_FOUND,
                        HeaderMap::new(),
                        Body::empty(),
                    );
                    response.redirects = redirects;
                    return Ok(response);
                }
                Some(MockRoute::Fail(failure)) => return Err(failure.to_error()),
                Some(MockRoute::Respond {
                    status,
                    content_type,
                    body,
                }) => {
                    let mut headers = HeaderMap::new();
                    if let Some(value) = content_type
                        .as_deref()
                        .and_then(|ct| HeaderValue::from_str(ct).ok())
                    {
                        headers.insert(header::CONTENT_TYPE, value);
                    }
                    let mut response =
                        FetchResponse::new(url, *status, headers, Body::Bytes(body.clone()));
                    response.redirects = redirects;
                    return Ok(response);
                }
                Some(MockRoute::Redirect { status, to }) => {
                    if redirects.len() == MAX_REDIRECTS {
                        return Err(FetchError::Other(
                            format!("more than {MAX_REDIRECTS} redirects").into(),
                        ));
                    }
                    redirects.push(Redirect {
                        from: url,
                        to: to.clone(),
                        status: *status,
                    });
                    url = to.clone();
                }
            }
        }
    }
}
