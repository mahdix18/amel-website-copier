use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;

use crate::{CookieJar, FetchContext, FetchError, FetchRequest, FetchResponse, ProxyConfig};

/// What a requester can do, so the crawler and UI can pick one per URL.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Capabilities {
    pub executes_js: bool,
    /// Loading a page also yields its subresources (see `FetchResponse::subresources`).
    pub captures_subresources: bool,
    /// Presents a real browser's TLS and HTTP/2 fingerprint.
    pub impersonation: bool,
    pub http2: bool,
    /// Can spill large bodies to disk instead of buffering them.
    pub streaming_body: bool,
}

/// A fetch backend.
#[async_trait::async_trait]
pub trait Requester: Send + Sync + 'static {
    /// Stable id used in project files and rules, such as `"wreq"`.
    fn id(&self) -> &'static str;

    fn capabilities(&self) -> Capabilities;

    async fn fetch(
        &self,
        request: FetchRequest,
        context: &FetchContext,
    ) -> Result<FetchResponse, FetchError>;

    /// Runs once before the crawl's first request, for setup such as
    /// logging in. Failing here stops the crawl before it starts.
    async fn prepare(&self) -> Result<(), FetchError> {
        Ok(())
    }

    /// Releases pooled connections or browser processes.
    async fn shutdown(&self) {}
}

/// Settings every requester honours, owned by the crawl rather than a backend.
#[derive(Debug, Clone)]
pub struct SharedResources {
    pub default_timeout: Duration,
    pub proxy: ProxyConfig,
    /// Responses larger than this fail with `FetchError::TooLarge`.
    pub max_body_bytes: u64,
    /// One session for every requester, so a login done in one carries over.
    pub cookies: Arc<CookieJar>,
}

impl Default for SharedResources {
    fn default() -> Self {
        Self {
            default_timeout: Duration::from_secs(30),
            proxy: ProxyConfig::default(),
            max_body_bytes: 512 * 1024 * 1024,
            cookies: Arc::new(CookieJar::new()),
        }
    }
}

/// Builds a [`Requester`] from its block in the project file.
///
/// The block arrives untyped so a new requester can add settings without any
/// change to the crawler or the config crate; the factory validates it.
pub trait RequesterFactory: Send + Sync + 'static {
    fn id(&self) -> &'static str;

    /// Reported without building, for `mistercopy requesters list` and the GUI.
    fn capabilities(&self) -> Capabilities;

    fn build(
        &self,
        config: &serde_json::Value,
        shared: &SharedResources,
    ) -> Result<Arc<dyn Requester>, BuildError>;
}

#[derive(Debug, thiserror::Error)]
#[error("invalid configuration for requester `{requester}`: {message}")]
pub struct BuildError {
    pub requester: &'static str,
    pub message: String,
}
