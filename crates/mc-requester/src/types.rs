use std::path::PathBuf;
use std::time::Duration;

use bytes::Bytes;
use http::{HeaderMap, Method, StatusCode, header};
use url::Url;

/// One request handed to a [`Requester`](crate::Requester).
#[derive(Debug, Clone)]
pub struct FetchRequest {
    pub url: Url,
    pub method: Method,
    /// Extra headers. Requesters that emulate a browser may reorder or
    /// override these to stay consistent with their fingerprint.
    pub headers: HeaderMap,
    /// Validators from a previous copy, sent as conditional request headers.
    pub validators: Option<Validators>,
    /// Overrides the requester's default timeout for this request.
    pub timeout: Option<Duration>,
}

impl FetchRequest {
    pub fn get(url: Url) -> Self {
        Self {
            url,
            method: Method::GET,
            headers: HeaderMap::new(),
            validators: None,
            timeout: None,
        }
    }
}

/// Cache validators remembered from an earlier fetch of the same URL.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Validators {
    pub etag: Option<String>,
    pub last_modified: Option<String>,
}

/// Where a request sits in the crawl. Requesters may use it for headers such
/// as `Referer`; it never changes what is fetched.
#[derive(Debug, Clone, Default)]
pub struct FetchContext {
    pub referrer: Option<Url>,
    pub depth: u32,
    /// Zero for the first try, incremented on each retry.
    pub attempt: u32,
}

/// A response body, kept in memory or spilled to a temporary file when large.
#[derive(Debug, Clone)]
pub enum Body {
    Bytes(Bytes),
    File(PathBuf),
}

impl Body {
    pub fn empty() -> Self {
        Self::Bytes(Bytes::new())
    }

    /// The in-memory bytes, or `None` when the body was spilled to disk.
    pub fn as_bytes(&self) -> Option<&Bytes> {
        match self {
            Self::Bytes(bytes) => Some(bytes),
            Self::File(_) => None,
        }
    }
}

impl From<Bytes> for Body {
    fn from(bytes: Bytes) -> Self {
        Self::Bytes(bytes)
    }
}

/// One hop of a redirect chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Redirect {
    pub from: Url,
    pub to: Url,
    pub status: StatusCode,
}

/// A response a browser requester observed while loading a page (scripts,
/// fonts, XHR data, lazy images…). Saved directly instead of fetched again.
#[derive(Debug, Clone)]
pub struct CapturedResponse {
    pub url: Url,
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Body,
}

/// The outcome of a fetch that reached a server, whatever its status code.
#[derive(Debug, Clone)]
pub struct FetchResponse {
    /// The URL after following redirects.
    pub final_url: Url,
    pub redirects: Vec<Redirect>,
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Body,
    /// The serialized DOM after scripts ran, from requesters that execute JS.
    pub rendered_dom: Option<String>,
    pub subresources: Vec<CapturedResponse>,
    /// Links found only at runtime, such as anchors built by scripts.
    pub discovered_links: Vec<Url>,
}

impl FetchResponse {
    pub fn new(final_url: Url, status: StatusCode, headers: HeaderMap, body: Body) -> Self {
        Self {
            final_url,
            redirects: Vec::new(),
            status,
            headers,
            body,
            rendered_dom: None,
            subresources: Vec::new(),
            discovered_links: Vec::new(),
        }
    }

    /// The media type from `Content-Type`, without parameters, lowercased.
    pub fn mime_type(&self) -> Option<String> {
        let value = self.headers.get(header::CONTENT_TYPE)?.to_str().ok()?;
        let mime = value.split(';').next()?.trim();
        (!mime.is_empty()).then(|| mime.to_ascii_lowercase())
    }
}
