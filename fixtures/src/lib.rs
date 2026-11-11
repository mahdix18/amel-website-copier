//! Local websites for MisterCopy's tests, served over real HTTP on loopback.
//!
//! Each [`Site`] is a directory under `fixtures/sites/` exercising one class of
//! problem. More sites (`spa-react`, `auth`, `hostile`) arrive with the
//! milestones that need them.

use std::fmt;
use std::io;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::str::FromStr;

use axum::Router;
use axum::extract::Request;
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::get;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tower_http::services::ServeDir;
use url::Url;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Site {
    /// Plain HTML and CSS: srcset, `@import`, query-string assets, a redirect,
    /// and filenames with spaces and non-ASCII characters.
    StaticBlog,
    /// Answers 403 to clients that don't send browser headers. Checks HTTP
    /// headers only; a TLS-fingerprint check would need a TLS server that
    /// inspects the ClientHello.
    UaGate,
}

impl Site {
    pub const ALL: &[Site] = &[Site::StaticBlog, Site::UaGate];

    pub fn name(self) -> &'static str {
        match self {
            Site::StaticBlog => "static-blog",
            Site::UaGate => "ua-gate",
        }
    }

    /// The directory holding the site's files.
    pub fn root(self) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("sites")
            .join(self.name())
    }
}

impl fmt::Display for Site {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

#[derive(Debug, thiserror::Error)]
#[error("unknown fixture site `{0}` (available: {available})", available = available_sites())]
pub struct UnknownSite(String);

fn available_sites() -> String {
    Site::ALL
        .iter()
        .map(|site| site.name())
        .collect::<Vec<_>>()
        .join(", ")
}

impl FromStr for Site {
    type Err = UnknownSite;

    fn from_str(name: &str) -> Result<Self, Self::Err> {
        Site::ALL
            .iter()
            .copied()
            .find(|site| site.name() == name)
            .ok_or_else(|| UnknownSite(name.to_owned()))
    }
}

pub fn router(site: Site) -> Router {
    let files = ServeDir::new(site.root());
    match site {
        Site::StaticBlog => Router::new()
            .route(
                "/old-about",
                get(|| async { Redirect::permanent("/about/") }),
            )
            .fallback_service(files),
        Site::UaGate => Router::new()
            .fallback_service(files)
            .layer(middleware::from_fn(require_browser)),
    }
}

async fn require_browser(request: Request, next: Next) -> Response {
    if looks_like_browser(request.headers()) {
        next.run(request).await
    } else {
        (StatusCode::FORBIDDEN, "browser required\n").into_response()
    }
}

/// Browsers send all of these; curl and plain HTTP libraries send none.
fn looks_like_browser(headers: &HeaderMap) -> bool {
    let user_agent = headers
        .get(header::USER_AGENT)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    user_agent.starts_with("Mozilla/5.0")
        && headers.contains_key("sec-fetch-mode")
        && headers.contains_key(header::ACCEPT_LANGUAGE)
}

/// A running fixture site. Shuts down when stopped or dropped.
#[derive(Debug)]
pub struct FixtureServer {
    addr: SocketAddr,
    shutdown: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<io::Result<()>>>,
}

impl FixtureServer {
    /// Serves `site` on an ephemeral loopback port.
    pub async fn start(site: Site) -> io::Result<Self> {
        Self::bind(site, SocketAddr::from(([127, 0, 0, 1], 0))).await
    }

    pub async fn bind(site: Site, addr: SocketAddr) -> io::Result<Self> {
        let listener = TcpListener::bind(addr).await?;
        let addr = listener.local_addr()?;
        let (shutdown, signal) = oneshot::channel::<()>();
        let task = tokio::spawn(async move {
            axum::serve(listener, router(site))
                .with_graceful_shutdown(async {
                    // Either a stop request or the sender being dropped.
                    let _ = signal.await;
                })
                .await
        });
        Ok(Self {
            addr,
            shutdown: Some(shutdown),
            task: Some(task),
        })
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn base_url(&self) -> Url {
        Url::parse(&format!("http://{}/", self.addr)).expect("a socket address forms a valid URL")
    }

    /// Resolves `path` against the server root. Panics on unparsable input,
    /// which in a test is a bug in the test.
    pub fn url(&self, path: &str) -> Url {
        self.base_url()
            .join(path)
            .unwrap_or_else(|err| panic!("invalid fixture path {path:?}: {err}"))
    }

    pub async fn stop(mut self) -> io::Result<()> {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        match self.task.take() {
            Some(task) => task.await.map_err(io::Error::other)?,
            None => Ok(()),
        }
    }
}

impl Drop for FixtureServer {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
    }
}
