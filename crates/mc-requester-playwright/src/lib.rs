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

/// For callers that drive a browser themselves, such as tests.
pub use playwright_rs;

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
    /// Steps run once before the crawl, to log in. See [`LoginStep`].
    pub login: Vec<LoginStep>,
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
            login: Vec::new(),
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
        for step in &config.login {
            step.action().map_err(build_error)?;
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
    builds.sort_by_key(|(revision, _)| std::cmp::Reverse(*revision));
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

    /// Copies the browser's cookies (those for `url`, or all of them) into
    /// the shared session, so other requesters stay logged in too.
    async fn cookies_out(&self, session: &Session, url: Option<&Url>) {
        let filter = url.map(|url| [url.as_str()]);
        let Ok(cookies) = session
            .context
            .cookies(filter.as_ref().map(|f| &f[..]))
            .await
        else {
            return;
        };
        for cookie in cookies {
            if let Some((origin, line)) = set_cookie_line(&cookie) {
                self.cookies.store_line(&origin, &line);
            }
        }
    }

    /// Runs the configured login steps in a page of the shared context.
    async fn log_in(&self, session: &Session) -> Result<(), FetchError> {
        let page = session.context.new_page().await.map_err(other)?;
        let mut outcome = Ok(());
        for (index, step) in self.config.login.iter().enumerate() {
            let result = match step.action() {
                Ok(LoginAction::Goto(url)) => {
                    let options = GotoOptions::new()
                        .timeout(self.timeout)
                        .wait_until(WaitUntil::Load);
                    page.goto(url, options).await.map(drop)
                }
                Ok(LoginAction::Fill(selector, value)) => {
                    page.locator(selector).fill(&value, None).await
                }
                Ok(LoginAction::Click(selector)) => {
                    let clicked = page.locator(selector).click(None).await;
                    // A submit usually navigates; let it land.
                    let _ = page.wait_for_load_state(Some(WaitUntil::Load)).await;
                    clicked
                }
                Ok(LoginAction::WaitFor(selector)) => page.locator(selector).wait_for(None).await,
                Err(message) => {
                    outcome = Err(FetchError::Other(message.into()));
                    break;
                }
            };
            if let Err(error) = result {
                outcome = Err(FetchError::Other(
                    format!("login step {}: {error}", index + 1).into(),
                ));
                break;
            }
        }
        let _ = page.close().await;
        outcome?;
        self.cookies_out(session, None).await;
        Ok(())
    }
}

/// A browser cookie as a `Set-Cookie` line plus the origin that would have
/// sent it, so the shared jar applies the same domain rules.
fn set_cookie_line(cookie: &Cookie) -> Option<(Url, String)> {
    let host = cookie.domain.trim_start_matches('.');
    if host.is_empty() {
        return None;
    }
    let scheme = if cookie.secure { "https" } else { "http" };
    let path = if cookie.path.is_empty() {
        "/"
    } else {
        &cookie.path
    };
    let origin = Url::parse(&format!("{scheme}://{host}{path}")).ok()?;
    let mut line = format!("{}={}; Path={path}", cookie.name, cookie.value);
    if cookie.domain.starts_with('.') {
        line.push_str(&format!("; Domain={host}"));
    }
    if cookie.secure {
        line.push_str("; Secure");
    }
    Some((origin, line))
}

/// One step of a scripted login, as written in a login script:
/// `{"goto": URL}`, `{"fill": SELECTOR, "value": TEXT}` (or `"value_env":
/// VAR` to read the text from the environment), `{"click": SELECTOR}`,
/// `{"wait_for": SELECTOR}`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoginStep {
    #[serde(default)]
    pub goto: Option<String>,
    #[serde(default)]
    pub fill: Option<String>,
    #[serde(default)]
    pub value: Option<String>,
    #[serde(default)]
    pub value_env: Option<String>,
    #[serde(default)]
    pub click: Option<String>,
    #[serde(default)]
    pub wait_for: Option<String>,
}

enum LoginAction<'a> {
    Goto(&'a str),
    Fill(&'a str, String),
    Click(&'a str),
    WaitFor(&'a str),
}

impl LoginStep {
    fn action(&self) -> Result<LoginAction<'_>, String> {
        let actions = [&self.goto, &self.fill, &self.click, &self.wait_for]
            .iter()
            .filter(|action| action.is_some())
            .count();
        if actions != 1 {
            return Err(
                "each login step needs exactly one of goto, fill, click, wait_for".to_owned(),
            );
        }
        if let Some(url) = &self.goto {
            return Ok(LoginAction::Goto(url));
        }
        if let Some(selector) = &self.click {
            return Ok(LoginAction::Click(selector));
        }
        if let Some(selector) = &self.wait_for {
            return Ok(LoginAction::WaitFor(selector));
        }
        let selector = self.fill.as_deref().unwrap_or_default();
        let value = match (&self.value, &self.value_env) {
            (Some(value), None) => value.clone(),
            (None, Some(var)) => std::env::var(var)
                .map_err(|_| format!("login step reads ${var}, which is not set"))?,
            _ => {
                return Err(format!(
                    "fill `{selector}` needs exactly one of value, value_env"
                ));
            }
        };
        Ok(LoginAction::Fill(selector, value))
    }
}

/// Opens a visible browser at `url` for the user to log in by hand, then
/// saves the session's cookies to `save_to` in Netscape format, readable by
/// `--cookies`. The file is created readable by its owner only.
pub async fn interactive_login(
    url: &Url,
    executable: Option<PathBuf>,
    save_to: &Path,
) -> Result<usize, String> {
    let playwright = Playwright::launch().await.map_err(|e| e.to_string())?;
    let mut options = LaunchOptions::new().headless(false);
    if let Some(path) = executable.or_else(|| installed_chromium().into_iter().next()) {
        options = options.executable_path(path.to_string_lossy().into_owned());
    }
    let browser = playwright
        .chromium()
        .launch_with_options(options)
        .await
        .map_err(|e| format!("could not open Chromium: {e}"))?;
    let context = browser.new_context().await.map_err(|e| e.to_string())?;
    let page = context.new_page().await.map_err(|e| e.to_string())?;
    page.goto(url.as_str(), None)
        .await
        .map_err(|e| e.to_string())?;

    println!("Log in in the browser window, then press Enter here.");
    let mut line = String::new();
    let mut stdin = tokio::io::BufReader::new(tokio::io::stdin());
    tokio::io::AsyncBufReadExt::read_line(&mut stdin, &mut line)
        .await
        .map_err(|e| e.to_string())?;

    let cookies = context.cookies(None).await.map_err(|e| e.to_string())?;
    let _ = browser.close().await;
    write_cookie_file(save_to, &cookies)
        .map_err(|e| format!("cannot write {}: {e}", save_to.display()))?;
    Ok(cookies.len())
}

/// Cookies in the Netscape `cookies.txt` format.
pub fn netscape_cookies(cookies: &[Cookie]) -> String {
    let mut text = String::from(
        "# Netscape HTTP Cookie File\n# Saved by MisterCopy. Contains a login session: keep it private.\n",
    );
    for cookie in cookies {
        let bool_field = |flag: bool| if flag { "TRUE" } else { "FALSE" };
        let domain = if cookie.http_only {
            format!("#HttpOnly_{}", cookie.domain)
        } else {
            cookie.domain.clone()
        };
        let expires = if cookie.expires > 0.0 {
            cookie.expires as i64
        } else {
            0
        };
        text.push_str(&format!(
            "{domain}\t{}\t{}\t{}\t{expires}\t{}\t{}\n",
            bool_field(cookie.domain.starts_with('.')),
            if cookie.path.is_empty() {
                "/"
            } else {
                &cookie.path
            },
            bool_field(cookie.secure),
            cookie.name,
            cookie.value,
        ));
    }
    text
}

fn write_cookie_file(path: &Path, cookies: &[Cookie]) -> std::io::Result<()> {
    use std::io::Write as _;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options
        .open(path)?
        .write_all(netscape_cookies(cookies).as_bytes())
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
            self.cookies_out(session, Some(&response.final_url)).await;
        }
        result
    }

    /// Starts the browser and runs the login script, if there is one, so
    /// the whole crawl (other requesters included) starts logged in.
    async fn prepare(&self) -> Result<(), FetchError> {
        if self.config.login.is_empty() {
            return Ok(());
        }
        let session = self.session().await?;
        self.log_in(session).await
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

#[cfg(test)]
mod tests {
    use super::*;

    fn step(json: serde_json::Value) -> LoginStep {
        serde_json::from_value(json).unwrap()
    }

    #[test]
    fn validates_login_steps() {
        assert!(matches!(
            step(serde_json::json!({"goto": "https://s.test/login"})).action(),
            Ok(LoginAction::Goto(_))
        ));
        assert!(matches!(
            step(serde_json::json!({"fill": "#user", "value": "ann"})).action(),
            Ok(LoginAction::Fill("#user", value)) if value == "ann"
        ));
        assert!(step(serde_json::json!({"fill": "#user"})).action().is_err());
        assert!(
            step(serde_json::json!({"click": "a", "goto": "b"}))
                .action()
                .is_err()
        );
        assert!(step(serde_json::json!({})).action().is_err());
        assert!(serde_json::from_value::<LoginStep>(serde_json::json!({"tap": "x"})).is_err());
        let missing =
            step(serde_json::json!({"fill": "#p", "value_env": "MISTERCOPY_SURELY_UNSET_VAR"}));
        assert!(
            missing
                .action()
                .err()
                .unwrap()
                .contains("MISTERCOPY_SURELY_UNSET_VAR")
        );
    }

    #[test]
    fn writes_netscape_cookies() {
        let mut session = Cookie::new("session", "abc");
        session.domain = "site.test".to_owned();
        session.path = "/".to_owned();
        session.http_only = true;
        session.expires = -1.0;
        let mut wide = Cookie::new("theme", "dark");
        wide.domain = ".site.test".to_owned();
        wide.path = "/docs".to_owned();
        wide.secure = true;
        wide.expires = 4_102_444_800.0;

        let text = netscape_cookies(&[session, wide]);
        assert!(
            text.contains("#HttpOnly_site.test\tFALSE\t/\tFALSE\t0\tsession\tabc\n"),
            "{text}"
        );
        assert!(
            text.contains(".site.test\tTRUE\t/docs\tTRUE\t4102444800\ttheme\tdark\n"),
            "{text}"
        );

        // What we write, the jar reads back.
        let jar = CookieJar::new();
        assert_eq!(jar.import_netscape(&text).unwrap(), 2);
    }

    #[test]
    fn converts_browser_cookies_for_the_jar() {
        let mut cookie = Cookie::new("sid", "1");
        cookie.domain = ".site.test".to_owned();
        cookie.path = "/".to_owned();
        let (origin, line) = set_cookie_line(&cookie).unwrap();
        assert_eq!(origin.as_str(), "http://site.test/");
        assert_eq!(line, "sid=1; Path=/; Domain=site.test");
        let jar = CookieJar::new();
        jar.store_line(&origin, &line);
        assert_eq!(
            jar.header_for(&Url::parse("http://www.site.test/").unwrap())
                .as_deref(),
            Some("sid=1")
        );
    }
}
