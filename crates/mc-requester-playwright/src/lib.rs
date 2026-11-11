//! The browser requester: real Chromium driven by Playwright, for pages that
//! are built by JavaScript.
//!
//! Each fetch opens a page, waits for the network to settle, scrolls so lazy
//! content loads, and returns the rendered DOM. Every response the page
//! loaded along the way (scripts, styles, fonts, images, API data) comes back
//! in `FetchResponse::subresources`, so the crawler saves them without
//! fetching them again.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use mc_requester::{
    Body, BuildError, Capabilities, CapturedResponse, CookieJar, FetchContext, FetchError,
    FetchRequest, FetchResponse, HeaderMap, Requester, RequesterFactory, SharedResources,
    StatusCode, Url, async_trait, header,
};
use playwright_rs::protocol::ResponseObject;
use playwright_rs::{
    Browser, BrowserContext, Cookie, GotoOptions, LaunchOptions, Page, Playwright, WaitUntil,
};
use serde::Deserialize;
use tokio::sync::{OnceCell, Semaphore};

pub const ID: &str = "playwright";

/// Scrolls to the bottom until the page stops growing, so lazy-loaded
/// content and infinite lists get a chance to load, then back to the top.
const SCROLL_JS: &str = "async () => {
    const root = document.scrollingElement || document.documentElement;
    let last = -1;
    for (let i = 0; i < 30 && root.scrollHeight !== last; i++) {
        last = root.scrollHeight;
        window.scrollTo(0, last);
        await new Promise(resolve => setTimeout(resolve, 150));
    }
    window.scrollTo(0, 0);
    return true;
}";

/// A rendered snapshot plus the scripts that built it tends to rebuild
/// itself badly offline (or blank itself when its API calls fail).
const REMOVE_SCRIPTS_JS: &str = "() => {
    document.querySelectorAll('script, link[rel=modulepreload], link[rel=preload][as=script]')
        .forEach(element => element.remove());
    return true;
}";

/// The `playwright` block of a project file.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PlaywrightConfig {
    /// Chromium to drive. Defaults to the newest one Playwright installed.
    pub executable_path: Option<PathBuf>,
    /// Scroll to the bottom of each page so lazy content loads.
    pub scroll: bool,
    /// Extra wait once the network is quiet, in milliseconds.
    pub settle_ms: u64,
    /// Keep `<script>` elements in saved pages.
    pub keep_scripts: bool,
    /// Pages rendering at once. Each one costs a renderer process.
    pub pages: usize,
    pub headless: bool,
}

impl Default for PlaywrightConfig {
    fn default() -> Self {
        Self {
            executable_path: None,
            scroll: true,
            settle_ms: 300,
            keep_scripts: false,
            pages: 2,
            headless: true,
        }
    }
}

const CAPABILITIES: Capabilities = Capabilities {
    executes_js: true,
    captures_subresources: true,
    impersonation: true,
    http2: true,
    streaming_body: false,
};

pub struct PlaywrightFactory;

impl RequesterFactory for PlaywrightFactory {
    fn id(&self) -> &'static str {
        ID
    }

    fn capabilities(&self) -> Capabilities {
        CAPABILITIES
    }

    fn build(
        &self,
        config: &serde_json::Value,
        shared: &SharedResources,
    ) -> Result<Arc<dyn Requester>, BuildError> {
        let config: PlaywrightConfig = if config.is_null() {
            PlaywrightConfig::default()
        } else {
            serde_json::from_value(config.clone()).map_err(|e| build_error(e.to_string()))?
        };
        if config.pages == 0 {
            return Err(build_error("pages must be at least 1".to_owned()));
        }
        if let Some(path) = &config.executable_path
            && !path.is_file()
        {
            return Err(build_error(format!("no browser at {}", path.display())));
        }
        Ok(Arc::new(PlaywrightRequester::new(config, shared)))
    }
}

fn build_error(message: String) -> BuildError {
    BuildError {
        requester: ID,
        message,
    }
}

/// Downloads Chromium into Playwright's browser cache.
pub async fn install_chromium() -> Result<(), String> {
    playwright_rs::install_browsers(Some(&["chromium"]))
        .await
        .map_err(|error| error.to_string())
}

/// Chromium builds Playwright installed, newest first.
pub fn installed_chromium() -> Vec<PathBuf> {
    let Some(root) = browsers_dir() else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(&root) else {
        return Vec::new();
    };
    let mut builds: Vec<(u64, PathBuf)> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            let revision = name.strip_prefix("chromium-")?.parse().ok()?;
            Some((revision, entry.path()))
        })
        .collect();
    builds.sort_by(|a, b| b.0.cmp(&a.0));
    builds
        .into_iter()
        .filter_map(|(_, dir)| chromium_executable(&dir))
        .collect()
}

fn browsers_dir() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("PLAYWRIGHT_BROWSERS_PATH") {
        return Some(PathBuf::from(path));
    }
    let home = std::env::var_os("HOME").map(PathBuf::from);
    if cfg!(target_os = "macos") {
        home.map(|h| h.join("Library/Caches/ms-playwright"))
    } else if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join("ms-playwright"))
    } else {
        home.map(|h| h.join(".cache/ms-playwright"))
    }
}

fn chromium_executable(build: &Path) -> Option<PathBuf> {
    [
        "chrome-linux64/chrome",
        "chrome-linux/chrome",
        "chrome-mac-arm64/Chromium.app/Contents/MacOS/Chromium",
        "chrome-mac/Chromium.app/Contents/MacOS/Chromium",
        "chrome-win64/chrome.exe",
        "chrome-win/chrome.exe",
    ]
    .iter()
    .map(|candidate| build.join(candidate))
    .find(|path| path.is_file())
}

struct Session {
    // Dropping the Playwright handle stops the driver; keep it alive.
    _playwright: Playwright,
    _browser: Browser,
    context: BrowserContext,
}

pub struct PlaywrightRequester {
    config: PlaywrightConfig,
    timeout: Duration,
    max_body_bytes: u64,
    cookies: Arc<CookieJar>,
    session: OnceCell<Session>,
    pages: Semaphore,
}

impl PlaywrightRequester {
    pub fn new(config: PlaywrightConfig, shared: &SharedResources) -> Self {
        Self {
            pages: Semaphore::new(config.pages),
            config,
            timeout: shared.default_timeout,
            max_body_bytes: shared.max_body_bytes,
            cookies: Arc::clone(&shared.cookies),
            session: OnceCell::new(),
        }
    }

    /// Starts Playwright and Chromium on first use, so a crawl that never
    /// needs the browser never pays for it.
    async fn session(&self) -> Result<&Session, FetchError> {
        self.session
            .get_or_try_init(|| async {
                let playwright = Playwright::launch().await.map_err(other)?;
                let mut options = LaunchOptions::new().headless(self.config.headless);
                let executable = self
                    .config
                    .executable_path
                    .clone()
                    .or_else(|| installed_chromium().into_iter().next());
                if let Some(path) = executable {
                    options = options.executable_path(path.to_string_lossy().into_owned());
                }
                let browser = playwright
                    .chromium()
                    .launch_with_options(options)
                    .await
                    .map_err(|e| {
                        FetchError::Other(format!("could not start Chromium: {e}").into())
                    })?;
                let context = browser.new_context().await.map_err(other)?;
                Ok(Session {
                    _playwright: playwright,
                    _browser: browser,
                    context,
                })
            })
            .await
    }

    async fn render(
        &self,
        page: &Page,
        request: &FetchRequest,
    ) -> Result<FetchResponse, FetchError> {
        let captured: Arc<Mutex<Vec<ResponseObject>>> = Arc::default();
        let sink = Arc::clone(&captured);
        page.on_response(move |response| {
            let sink = Arc::clone(&sink);
            async move {
                sink.lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push(response);
                Ok(())
            }
        })
        .await
        .map_err(other)?;

        let timeout = request.timeout.unwrap_or(self.timeout);
        let options = GotoOptions::new()
            .timeout(timeout)
            .wait_until(WaitUntil::Load);
        let main = page
            .goto(request.url.as_str(), options)
            .await
            .map_err(navigation_error)?;

        // Not a page (a stylesheet, image or JSON reached directly): return
        // the bytes as served, not Chromium's HTML wrapper around them.
        let main_type = main
            .as_ref()
            .and_then(|response| response.headers().get("content-type").cloned())
            .unwrap_or_default()
            .to_ascii_lowercase();
        if !main_type.is_empty() && !main_type.contains("html") {
            let responses =
                std::mem::take(&mut *captured.lock().unwrap_or_else(PoisonError::into_inner));
            return self
                .raw_document(responses, &request.url, &page.url())
                .await;
        }

        // Quiet network is best-effort: pages that poll never get there.
        let _ = page.wait_for_load_state(Some(WaitUntil::NetworkIdle)).await;
        if self.config.scroll {
            let _ = page
                .evaluate::<(), serde_json::Value>(SCROLL_JS, None)
                .await;
            let _ = page.wait_for_load_state(Some(WaitUntil::NetworkIdle)).await;
        }
        tokio::time::sleep(Duration::from_millis(self.config.settle_ms)).await;
        if !self.config.keep_scripts {
            page.evaluate::<(), serde_json::Value>(REMOVE_SCRIPTS_JS, None)
                .await
                .map_err(other)?;
        }

        let html = page.content().await.map_err(other)?;
        let final_url = Url::parse(&page.url()).unwrap_or_else(|_| request.url.clone());
        let status = main
            .as_ref()
            .and_then(|response| StatusCode::from_u16(response.status()).ok())
            .unwrap_or(StatusCode::OK);

        // The DOM is serialized as UTF-8 whatever the original encoding.
        let mut headers = HeaderMap::new();
        headers.insert(
            header::CONTENT_TYPE,
            header::HeaderValue::from_static("text/html; charset=utf-8"),
        );
        if let Some(response) = &main {
            for name in [header::ETAG, header::LAST_MODIFIED] {
                if let Some(value) = response
                    .headers()
                    .get(name.as_str())
                    .and_then(|v| header::HeaderValue::from_str(v).ok())
                {
                    headers.insert(name, value);
                }
            }
        }

        let responses =
            std::mem::take(&mut *captured.lock().unwrap_or_else(PoisonError::into_inner));
        let mut fetched = FetchResponse::new(
            final_url.clone(),
            status,
            headers,
            Body::Bytes(html.clone().into_bytes().into()),
        );
        fetched.rendered_dom = Some(html);
        fetched.subresources = self.collect(responses, &[&request.url, &final_url]).await;
        Ok(fetched)
    }

    /// The navigation's own response, body and headers as served.
    async fn raw_document(
        &self,
        responses: Vec<ResponseObject>,
        requested: &Url,
        landed: &str,
    ) -> Result<FetchResponse, FetchError> {
        let final_url = Url::parse(landed).unwrap_or_else(|_| requested.clone());
        let document = responses
            .into_iter()
            .rev()
            .find(|response| {
                response.url() == final_url.as_str() || response.url() == requested.as_str()
            })
            .ok_or_else(|| FetchError::Other("the browser kept no response for this URL".into()))?;
        let body = document.body().await.map_err(other)?;
        if body.len() as u64 > self.max_body_bytes {
            return Err(FetchError::TooLarge {
                limit: self.max_body_bytes,
            });
        }
        let status = StatusCode::from_u16(document.status()).unwrap_or(StatusCode::OK);
        let headers = header_map(&document).await;
        Ok(FetchResponse::new(
            final_url,
            status,
            headers,
            Body::Bytes(body.into()),
        ))
    }

    /// The successful responses the page loaded, bodies included, except the
    /// document itself.
    async fn collect(
        &self,
        responses: Vec<ResponseObject>,
        documents: &[&Url],
    ) -> Vec<CapturedResponse> {
        let mut captured = Vec::new();
        for response in responses {
            let Ok(url) = Url::parse(response.url()) else {
                continue;
            };
            let same_document = documents.iter().any(|doc| {
                let mut doc = (*doc).clone();
                doc.set_fragment(None);
                doc == url
            });
            if same_document
                || !matches!(url.scheme(), "http" | "https")
                || !(200..300).contains(&response.status())
            {
                continue;
            }
            // Redirects and opaque responses have no body to keep.
            let Ok(body) = response.body().await else {
                continue;
            };
            if body.len() as u64 > self.max_body_bytes {
                continue;
            }
            let headers = header_map(&response).await;
            let Ok(status) = StatusCode::from_u16(response.status()) else {
                continue;
            };
            captured.push(CapturedResponse {
                url,
                status,
                headers,
                body: Body::Bytes(body.into()),
            });
        }
        captured
    }

    /// Hands the shared session to the browser before a page loads.
    async fn cookies_in(&self, session: &Session, url: &Url) {
        let cookies: Vec<Cookie> = self
            .cookies
            .pairs_for(url)
            .into_iter()
            .map(|(name, value)| Cookie::new(name, value).url(url.as_str()))
            .collect();
        if !cookies.is_empty() {
            let _ = session.context.add_cookies(&cookies).await;
        }
    }

    /// Copies cookies the page set back into the shared session, so other
    /// requesters stay logged in too.
    async fn cookies_out(&self, session: &Session, url: &Url) {
        let Ok(cookies) = session.context.cookies(Some(&[url.as_str()])).await else {
            return;
        };
        for cookie in cookies {
            let mut line = format!("{}={}; Path={}", cookie.name, cookie.value, cookie.path);
            if !cookie.domain.is_empty() && cookie.domain.starts_with('.') {
                line.push_str(&format!("; Domain={}", cookie.domain));
            }
            if cookie.secure {
                line.push_str("; Secure");
            }
            self.cookies.store_line(url, &line);
        }
    }
}

#[async_trait]
impl Requester for PlaywrightRequester {
    fn id(&self) -> &'static str {
        ID
    }

    fn capabilities(&self) -> Capabilities {
        CAPABILITIES
    }

    async fn fetch(
        &self,
        request: FetchRequest,
        _context: &FetchContext,
    ) -> Result<FetchResponse, FetchError> {
        if !matches!(request.url.scheme(), "http" | "https") {
            return Err(FetchError::UnsupportedScheme(
                request.url.scheme().to_owned(),
            ));
        }
        let _permit = self.pages.acquire().await.map_err(other)?;
        let session = self.session().await?;
        self.cookies_in(session, &request.url).await;
        let page = session.context.new_page().await.map_err(other)?;
        let result = self.render(&page, &request).await;
        let _ = page.close().await;
        if let Ok(response) = &result {
            self.cookies_out(session, &response.final_url).await;
        }
        result
    }
}

/// A response's headers as served, skipping any that aren't valid HTTP.
async fn header_map(response: &ResponseObject) -> HeaderMap {
    let mut headers = HeaderMap::new();
    if let Ok(entries) = response.raw_headers().await {
        for entry in entries {
            if let (Ok(name), Ok(value)) = (
                header::HeaderName::from_bytes(entry.name.as_bytes()),
                header::HeaderValue::from_str(&entry.value),
            ) {
                headers.append(name, value);
            }
        }
    }
    headers
}

fn other(error: impl std::fmt::Display) -> FetchError {
    FetchError::Other(error.to_string().into())
}

/// Chromium reports network failures as `net::ERR_*` messages.
fn navigation_error(error: playwright_rs::Error) -> FetchError {
    let message = error.to_string();
    if message.contains("Timeout") || message.contains("timeout") {
        FetchError::Timeout
    } else if message.contains("net::ERR_CERT") || message.contains("SSL") {
        FetchError::Tls(message)
    } else if message.contains("net::ERR_") {
        FetchError::Connect(message)
    } else {
        FetchError::Other(message.into())
    }
}
