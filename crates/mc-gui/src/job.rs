//! A copy running on its own thread, and the progress the window shows.
//! Plain Rust, no UI: the window renders a [`Progress`] and feeds it events.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use mc_cli::archive::zip_copy;
use mc_cli::options::{CrawlOptions, SessionArgs, parse_rate};
use mc_cli::requesters::{build_requester, registry};
use mc_core::{
    CrawlEvent, CrawlStatus, CrawlSummary, Crawler, REPORT_HTML, StopHandle, human_bytes,
};
use mc_requester::{AUTO_ID, ProxyRotation, Url, parse_proxy_list, without_credentials};
use mc_store::SqliteStore;
use tokio::sync::{broadcast, oneshot};

/// Log lines kept for display; older ones scroll away for good.
const LOG_LINES: usize = 500;
/// Download speed is averaged over this window.
const RATE_WINDOW: Duration = Duration::from_secs(3);
/// Most parallel downloads the window offers.
pub const MAX_WORKERS: usize = 32;

/// Keys shared with the command-line tool, so `mistercopy resume` and
/// `update` work on copies started here.
const META_SEED: &str = "seed";
const META_OPTIONS: &str = "options";

/// Everything the window's form holds, as typed. [`Settings::options`]
/// turns it into the command line's options, so a copy started here saves
/// the same choices `mistercopy resume` and `update` read back.
#[derive(Debug, Clone)]
pub struct Settings {
    pub url: String,
    pub output: String,
    pub render_js: bool,
    /// Browser profile to impersonate (`chrome_149`…); `None` for the default.
    pub profile: Option<String>,
    /// Platform to present (`windows`…); `None` for the profile's own.
    pub platform: Option<String>,
    /// Parallel downloads.
    pub workers: usize,
    /// Minimum gap between two requests to the same host.
    pub per_host_delay: Duration,
    pub respect_robots: bool,
    /// Skip URLs matching this regex (empty: none).
    pub exclude: String,
    /// Fetch URLs matching this regex even outside the site (empty: none).
    pub include: String,
    /// Save URLs matching this regex without following their links.
    pub download_only: String,
    // Limits, each empty for none.
    pub max_depth: String,
    pub max_urls: String,
    pub max_size_mb: String,
    pub max_time_min: String,
    pub max_file_mb: String,
    /// e.g. `500K` or `2M`.
    pub limit_rate: String,
    /// Other hosts of the same site, separated by commas or spaces.
    pub aliases: String,
    pub external_assets: bool,
    pub sitemaps: bool,
    // Session: used for this run only, never saved.
    pub auth_user: String,
    pub auth_password: String,
    /// `Name: value`, one per line.
    pub headers: String,
    pub cookies: String,
    /// Proxy URLs, one per line.
    pub proxies: String,
    pub proxy_rotation: ProxyRotation,
    /// Hosts that skip the proxy, separated by commas or spaces.
    pub no_proxy: String,
    pub system_proxy: bool,
    // Extra output.
    pub warc: bool,
    pub zip: bool,
    pub replay: bool,
    /// After downloading, check for missed files and fetch them again.
    pub verify: bool,
    /// Re-check a finished copy instead of starting over.
    pub update: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            url: String::new(),
            output: default_output(),
            render_js: false,
            profile: None,
            platform: None,
            workers: 4,
            per_host_delay: Speed::default().delay(),
            respect_robots: true,
            exclude: String::new(),
            include: String::new(),
            download_only: String::new(),
            max_depth: String::new(),
            max_urls: String::new(),
            max_size_mb: String::new(),
            max_time_min: String::new(),
            max_file_mb: "512".to_owned(),
            limit_rate: String::new(),
            aliases: String::new(),
            external_assets: false,
            sitemaps: true,
            auth_user: String::new(),
            auth_password: String::new(),
            headers: String::new(),
            cookies: String::new(),
            proxies: String::new(),
            proxy_rotation: ProxyRotation::default(),
            no_proxy: String::new(),
            system_proxy: false,
            warc: false,
            zip: false,
            replay: false,
            verify: true,
            update: false,
        }
    }
}

/// An optional number field: empty is `None`.
fn number<T: std::str::FromStr>(text: &str, label: &str) -> Result<Option<T>, String> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }
    text.parse()
        .map(Some)
        .map_err(|_| format!("{label} must be a whole number, not “{text}”."))
}

fn pattern(text: &str) -> Vec<String> {
    let text = text.trim();
    if text.is_empty() {
        Vec::new()
    } else {
        vec![text.to_owned()]
    }
}

impl Settings {
    /// The crawl options (saved with the copy) and session (not saved) this
    /// form describes, for the site `seed` copied into `output`.
    pub fn options(
        &self,
        seed: &Url,
        output: &Path,
    ) -> Result<(CrawlOptions, SessionArgs), String> {
        let mut options = CrawlOptions {
            requester: if self.render_js { AUTO_ID } else { "wreq" }.to_owned(),
            profile: self.profile.clone(),
            platform: self.platform.clone(),
            concurrency: self.workers.clamp(1, MAX_WORKERS),
            delay_ms: self.per_host_delay.as_millis() as u64,
            ignore_robots: !self.respect_robots,
            exclude: pattern(&self.exclude),
            include: pattern(&self.include),
            download_only: pattern(&self.download_only),
            max_depth: number(&self.max_depth, "Max depth")?,
            max_urls: number(&self.max_urls, "Max pages")?,
            max_size_mb: number(&self.max_size_mb, "Stop after (MB)")?,
            max_time_secs: number::<u64>(&self.max_time_min, "Stop after (minutes)")?
                .map(|minutes| minutes.saturating_mul(60)),
            aliases: self
                .aliases
                .split(|c: char| c == ',' || c.is_whitespace())
                .filter(|host| !host.is_empty())
                .map(str::to_owned)
                .collect(),
            external_assets: self.external_assets,
            sitemaps: self.sitemaps,
            zip: self.zip,
            replay: self.replay && self.render_js,
            verify: self.verify,
            ..CrawlOptions::default()
        };
        if let Some(mb) = number(&self.max_file_mb, "Skip files over (MB)")? {
            options.max_file_mb = mb;
        }
        let rate = self.limit_rate.trim();
        if !rate.is_empty() {
            options.limit_rate =
                Some(parse_rate(rate).map_err(|e| format!("Bandwidth limit: {e}."))?);
        }
        options.proxy = parse_proxy_list(&self.proxies).map_err(|e| format!("Proxy: {e}."))?;
        options.proxy_rotation = self.proxy_rotation;
        options.no_proxy = self
            .no_proxy
            .split(|c: char| c == ',' || c.is_whitespace())
            .filter(|host| !host.is_empty())
            .map(str::to_owned)
            .collect();
        options.system_proxy = self.system_proxy;
        options.proxy_config().map_err(|e| format!("Proxy: {e}."))?;
        if self.warc {
            let host = seed.host_str().unwrap_or("copy");
            options.warc = Some(output.join(format!("{host}.warc.gz")));
        }
        // Checked now, so a bad pattern is reported before anything starts.
        options.crawl_config(seed.clone(), output.to_path_buf())?;
        options.check_impersonation()?;

        let mut session = SessionArgs::default();
        let user = self.auth_user.trim();
        if !user.is_empty() {
            let host = seed.host_str().unwrap_or_default();
            session.auth = vec![format!("{host}={user}:{}", self.auth_password)];
        }
        session.headers = self
            .headers
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_owned)
            .collect();
        let cookies = self.cookies.trim();
        if !cookies.is_empty() {
            session.cookies = Some(expand_home(cookies));
        }
        Ok((options, session))
    }
}

/// The browser profiles to impersonate, grouped by browser in wreq-util's
/// order: `[("chrome", ["chrome_100", …]), ("firefox", […]), …]`.
pub fn profile_families() -> Vec<(String, Vec<String>)> {
    let mut families: Vec<(String, Vec<String>)> = Vec::new();
    for name in mc_requester_wreq::profiles() {
        let family = name.split('_').next().unwrap_or_default().to_owned();
        match families.iter_mut().find(|(f, _)| *f == family) {
            Some((_, names)) => names.push(name),
            None => families.push((family, vec![name])),
        }
    }
    families
}

/// The profile copies use when none is chosen.
pub fn default_profile() -> String {
    mc_requester_wreq::default_profile()
}

/// `chrome_149` as `Chrome 149`, `safari_ios_18.1` as `Safari iOS 18.1`.
pub fn pretty_profile(name: &str) -> String {
    name.split('_')
        .map(|part| match part {
            "ios" => "iOS".to_owned(),
            "okhttp" => "OkHttp".to_owned(),
            _ => {
                let mut chars = part.chars();
                match chars.next() {
                    Some(first) => first.to_uppercase().chain(chars).collect(),
                    None => String::new(),
                }
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// `~/x` as a path under the home directory.
pub fn expand_home(text: &str) -> PathBuf {
    match text.strip_prefix("~/") {
        Some(rest) => std::env::var_os("HOME")
            .map(|home| PathBuf::from(home).join(rest))
            .unwrap_or_else(|| PathBuf::from(text)),
        None => PathBuf::from(text),
    }
}

/// Where a browser login made from the window keeps its cookies: inside the
/// copy's state folder, which zips and reports leave out.
pub fn login_cookies(output: &Path) -> PathBuf {
    output.join(mc_core::STATE_DIR).join("login-cookies.txt")
}

/// How hard to press the site: the gap between two requests to one host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Speed {
    /// One request a second per host: kind to small servers.
    #[default]
    Polite,
    Balanced,
    /// No gap: all workers download at once.
    Fast,
}

impl Speed {
    pub const ALL: [Self; 3] = [Self::Polite, Self::Balanced, Self::Fast];

    pub fn delay(self) -> Duration {
        match self {
            Self::Polite => Duration::from_secs(1),
            Self::Balanced => Duration::from_millis(250),
            Self::Fast => Duration::ZERO,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Polite => "Polite",
            Self::Balanced => "Balanced",
            Self::Fast => "Fast",
        }
    }

    pub fn hint(self) -> &'static str {
        match self {
            Self::Polite => "1 request/s per site",
            Self::Balanced => "4 requests/s per site",
            Self::Fast => "no delay, all workers at once",
        }
    }
}

/// Whether this build can render JavaScript with Chromium.
pub fn browser_support() -> bool {
    cfg!(feature = "browser")
}

/// Where copies go unless the user picks another folder.
pub fn default_output() -> String {
    std::env::var_os("HOME")
        .map(|home| {
            PathBuf::from(home)
                .join("MisterCopy")
                .to_string_lossy()
                .into_owned()
        })
        .unwrap_or_else(|| "MisterCopy".to_owned())
}

/// Checks the form. Accepts a bare host (`example.com`) as `https://`.
pub fn parse(settings: &Settings) -> Result<(Url, PathBuf), String> {
    let text = settings.url.trim();
    if text.is_empty() {
        return Err("Enter the address of the site to copy.".to_owned());
    }
    let url = Url::parse(text)
        .ok()
        .filter(|url| url.has_host())
        .or_else(|| Url::parse(&format!("https://{text}")).ok())
        .filter(|url| matches!(url.scheme(), "http" | "https") && url.has_host())
        .ok_or_else(|| format!("“{text}” doesn't look like a web address."))?;

    let folder = settings.output.trim();
    if folder.is_empty() {
        return Err("Choose a folder to save the copy in.".to_owned());
    }
    Ok((url, expand_home(folder)))
}

/// A copy in progress.
pub struct Running {
    pub stop: StopHandle,
    pub events: broadcast::Receiver<CrawlEvent>,
    pub done: oneshot::Receiver<Outcome>,
    pub output: PathBuf,
    /// An unfinished copy of the same site was found and is being continued.
    pub resumed: bool,
    pub workers: usize,
}

/// How one proxy answered a test request for the site.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxyCheck {
    /// The proxy without its credentials.
    pub proxy: String,
    /// The HTTP status and how long it took, or why it failed.
    pub result: Result<(u16, Duration), String>,
}

/// Fetches the site's address through each configured proxy at once (the
/// system's, when that is on and none are listed), on a thread of its own.
pub fn check_proxies(settings: &Settings) -> Result<oneshot::Receiver<Vec<ProxyCheck>>, String> {
    let (seed, output) = parse(settings)?;
    let (options, _) = settings.options(&seed, &output)?;
    let config = options.proxy_config()?;
    let servers = mc_requester::ProxyPicker::new(&config)?.servers().to_vec();
    if servers.is_empty() {
        return Err("Add a proxy, or turn on the system proxy, to test it.".to_owned());
    }
    let (send, receive) = oneshot::channel();
    std::thread::Builder::new()
        .name("mistercopy-proxy-check".to_owned())
        .spawn(move || {
            let checks = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .map(|runtime| runtime.block_on(check_all(seed, servers)))
                .unwrap_or_default();
            let _ = send.send(checks);
        })
        .map_err(|e| format!("could not test the proxies: {e}"))?;
    Ok(receive)
}

async fn check_all(seed: Url, servers: Vec<Url>) -> Vec<ProxyCheck> {
    let mut tasks = tokio::task::JoinSet::new();
    for (index, server) in servers.into_iter().enumerate() {
        let seed = seed.clone();
        tasks.spawn(async move {
            let shared = mc_requester::SharedResources {
                default_timeout: Duration::from_secs(15),
                proxy: mc_requester::ProxyConfig {
                    servers: vec![server.clone()],
                    ..mc_requester::ProxyConfig::default()
                },
                ..mc_requester::SharedResources::default()
            };
            let started = std::time::Instant::now();
            let result = match registry().build("wreq", &serde_json::json!({}), &shared) {
                Ok(requester) => requester
                    .fetch(
                        mc_requester::FetchRequest::get(seed),
                        &mc_requester::FetchContext::default(),
                    )
                    .await
                    .map(|response| (response.status.as_u16(), started.elapsed()))
                    .map_err(|e| explain_proxy_error(&e.to_string())),
                Err(e) => Err(e.to_string()),
            };
            let proxy = without_credentials(&server).to_string();
            (index, ProxyCheck { proxy, result })
        });
    }
    let mut checks: Vec<_> = tasks.join_all().await;
    checks.sort_by_key(|(index, _)| *index);
    checks.into_iter().map(|(_, check)| check).collect()
}

/// A proxy failure in plain words, keeping the detail for odd cases.
pub fn explain_proxy_error(error: &str) -> String {
    let lower = error.to_ascii_lowercase();
    if lower.contains("proxyconnect") || lower.contains("connection refused") {
        "Couldn't connect through this proxy: is it running, on this port, of this type?".to_owned()
    } else if lower.contains("timed out") || lower.contains("timeout") {
        "No answer within 15 seconds.".to_owned()
    } else if lower.contains("dns") || lower.contains("resolve") {
        "The proxy's host name doesn't resolve.".to_owned()
    } else {
        error.to_owned()
    }
}

/// How a copy ended: the crawl's own result, and the zip if one was asked
/// for and the copy finished.
pub struct Outcome {
    pub result: Result<CrawlSummary, String>,
    pub zip: Option<Result<PathBuf, String>>,
}

pub fn start(settings: &Settings) -> Result<Running, String> {
    let (seed, output) = parse(settings)?;
    let (options, session) = settings.options(&seed, &output)?;
    let mut config = options.crawl_config(seed.clone(), output.clone())?;
    let shared = options.shared_resources()?;
    session.apply(&mut config, &shared)?;
    let requester = build_requester(&registry(), &options, session.login_steps()?, &shared)
        .map_err(|e| {
            if settings.render_js && !browser_support() {
                "This build can't render JavaScript.".to_owned()
            } else {
                e
            }
        })?;

    let mut store = SqliteStore::open(&output).map_err(|e| e.to_string())?;
    // Continue an unfinished copy of the same site; otherwise start fresh.
    let same_site = store.meta(META_SEED).ok().flatten().as_deref() == Some(seed.as_str());
    let unfinished = matches!(
        store.status(),
        Ok(Some(CrawlStatus::Running | CrawlStatus::Stopped))
    );
    let complete = matches!(store.status(), Ok(Some(CrawlStatus::Complete)));
    let resumed = same_site && unfinished;
    if settings.update {
        if !(same_site && complete) {
            return Err(
                "Only a finished copy of this site can be updated. Copy it first.".to_owned(),
            );
        }
        store.begin_update().map_err(|e| e.to_string())?;
    } else if !resumed {
        store.reset().map_err(|e| e.to_string())?;
        store
            .set_meta(META_SEED, seed.as_str())
            .map_err(|e| e.to_string())?;
    }
    // Saved so the command line's `resume` and `update` repeat these choices.
    let saved = serde_json::to_string(&options).map_err(|e| e.to_string())?;
    store
        .set_meta(META_OPTIONS, &saved)
        .map_err(|e| e.to_string())?;

    let workers = config.concurrency;
    let zip = options.zip;
    let crawler = Crawler::new(config, requester).with_store(store);
    let stop = crawler.stop_handle();
    let events = crawler.subscribe();
    let (finished, done) = oneshot::channel();
    let zip_dir = output.clone();
    std::thread::Builder::new()
        .name("mistercopy-crawl".to_owned())
        .spawn(move || {
            let result = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .map_err(|e| e.to_string())
                .and_then(|runtime| runtime.block_on(crawler.run()).map_err(|e| e.to_string()));
            // Only a finished copy is worth packing.
            let zip = (zip && matches!(&result, Ok(summary) if summary.stopped.is_none()))
                .then(|| zip_copy(&zip_dir).map_err(|e| e.to_string()));
            let _ = finished.send(Outcome { result, zip });
        })
        .map_err(|e| format!("could not start the copy: {e}"))?;

    Ok(Running {
        stop,
        events,
        done,
        output,
        resumed,
        workers,
    })
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Phase {
    #[default]
    Idle,
    Running,
    Stopping,
    Finished,
    /// Stopped early; copying the same site again continues it.
    Stopped,
    Failed(String),
}

/// What one line of the activity log is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    Saved,
    Unchanged,
    Failed,
    Retry,
    Info,
}

impl LineKind {
    /// The short tag shown in the log's first column.
    pub fn tag(self) -> &'static str {
        match self {
            Self::Saved => "200",
            Self::Unchanged => "304",
            Self::Failed => "ERR",
            Self::Retry => "RETRY",
            Self::Info => "INFO",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogLine {
    pub kind: LineKind,
    pub text: String,
    /// Size, delay or reason, shown right-aligned.
    pub detail: String,
}

impl LogLine {
    pub fn contains(&self, needle: &str) -> bool {
        self.text.contains(needle) || self.detail.contains(needle)
    }
}

/// What one download worker is doing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Worker {
    /// The URL taken; `None` while idle.
    pub url: Option<String>,
    /// Holding the URL until the host's per-request delay has passed.
    pub waiting: bool,
    pub since: Option<Instant>,
    /// Requests this worker has made.
    pub requests: usize,
}

/// What the window shows about the current (or last) copy.
#[derive(Debug, Clone, Default)]
pub struct Progress {
    pub phase: Phase,
    pub saved: usize,
    pub unchanged: usize,
    pub failed: usize,
    pub retries: usize,
    pub bytes: u64,
    /// URLs finished, and found so far (finished or not).
    pub done: usize,
    pub total: usize,
    pub workers: Vec<Worker>,
    /// The check for missed files is running.
    pub checking: bool,
    pub started: Option<Instant>,
    pub ended: Option<Instant>,
    /// (when, bytes so far), for the download speed.
    samples: VecDeque<(Instant, u64)>,
    /// Newest last.
    pub log: VecDeque<LogLine>,
    pub summary: Option<String>,
    pub report: Option<PathBuf>,
}

impl Progress {
    pub fn started(resumed: bool, workers: usize) -> Self {
        let mut progress = Self {
            phase: Phase::Running,
            workers: vec![Worker::default(); workers],
            started: Some(Instant::now()),
            ..Self::default()
        };
        if resumed {
            progress.info("Continuing the unfinished copy of this site.");
        }
        progress
    }

    pub fn failed(message: String) -> Self {
        Self {
            phase: Phase::Failed(message),
            ..Self::default()
        }
    }

    pub fn running(&self) -> bool {
        matches!(self.phase, Phase::Running | Phase::Stopping)
    }

    /// 0.0 to 1.0, or `None` before anything is known.
    pub fn fraction(&self) -> Option<f32> {
        match self.phase {
            Phase::Finished => Some(1.0),
            _ if self.total == 0 => None,
            _ => Some((self.done as f32 / self.total as f32).clamp(0.0, 1.0)),
        }
    }

    pub fn elapsed(&self) -> Duration {
        match (self.started, self.ended) {
            (Some(start), Some(end)) => end - start,
            (Some(start), None) => start.elapsed(),
            _ => Duration::ZERO,
        }
    }

    /// Bytes per second over the last few seconds while running; the
    /// average over the whole copy once it ended.
    pub fn rate(&self) -> u64 {
        if !self.running() {
            let secs = self.elapsed().as_secs_f64();
            return if secs > 0.0 {
                (self.bytes as f64 / secs) as u64
            } else {
                0
            };
        }
        let now = Instant::now();
        let Some(&(at, bytes)) = self
            .samples
            .iter()
            .find(|(at, _)| now.duration_since(*at) <= RATE_WINDOW)
        else {
            return 0;
        };
        let secs = now.duration_since(at).as_secs_f64().max(0.5);
        (self.bytes.saturating_sub(bytes) as f64 / secs) as u64
    }

    /// Workers with a request on the wire.
    pub fn active(&self) -> usize {
        self.workers
            .iter()
            .filter(|w| w.url.is_some() && !w.waiting)
            .count()
    }

    pub fn apply(&mut self, event: &CrawlEvent) {
        match event {
            CrawlEvent::Saved { url, bytes, .. } => {
                self.saved += 1;
                self.bytes += bytes;
                self.sample();
                self.push(LineKind::Saved, url.as_str(), human_bytes(*bytes));
            }
            CrawlEvent::Unchanged { url } => {
                self.unchanged += 1;
                self.push(LineKind::Unchanged, url.as_str(), "unchanged".to_owned());
            }
            CrawlEvent::Failed { url, reason } => {
                self.failed += 1;
                self.push(LineKind::Failed, url.as_str(), reason.clone());
            }
            CrawlEvent::Retrying {
                url, delay, reason, ..
            } => {
                self.retries += 1;
                let detail = format!("in {:.1}s · {reason}", delay.as_secs_f64());
                self.push(LineKind::Retry, url.as_str(), detail);
            }
            CrawlEvent::Stopping { reason } => {
                self.phase = Phase::Stopping;
                self.info(&format!("Stopping: {reason}"));
            }
            CrawlEvent::Rewriting { documents } => {
                self.info(&format!("Rewriting links in {documents} pages"));
            }
            CrawlEvent::Waiting { worker, url } => {
                if let Some(slot) = self.worker(*worker) {
                    slot.url = Some(url.to_string());
                    slot.waiting = true;
                    slot.since = Some(Instant::now());
                }
            }
            CrawlEvent::Fetching { worker, url } => {
                if let Some(slot) = self.worker(*worker) {
                    slot.url = Some(url.to_string());
                    slot.waiting = false;
                    slot.since = Some(Instant::now());
                    slot.requests += 1;
                }
            }
            CrawlEvent::WorkerIdle { worker } => {
                if let Some(slot) = self.worker(*worker) {
                    *slot = Worker {
                        requests: slot.requests,
                        ..Worker::default()
                    };
                }
            }
            CrawlEvent::Progress { done, total, .. } => {
                self.done = *done;
                self.total = *total;
            }
            CrawlEvent::Verifying { files, retrying } => {
                self.checking = true;
                self.info(&format!(
                    "Checking {files} saved files for missed downloads; fetching {retrying} again"
                ));
            }
            CrawlEvent::Verified {
                recovered,
                still_missing,
            } => {
                self.checking = false;
                let text =
                    format!("Check done: {recovered} recovered, {still_missing} still missing");
                if *still_missing > 0 {
                    self.push(LineKind::Failed, &text, String::new());
                } else {
                    self.info(&text);
                }
            }
            CrawlEvent::SitemapRead { url, pages } => {
                self.info(&format!("Sitemap {url}: {pages} pages listed"));
            }
            CrawlEvent::Skipped { .. } => {}
        }
    }

    fn worker(&mut self, index: usize) -> Option<&mut Worker> {
        if index >= self.workers.len() && index < MAX_WORKERS {
            self.workers.resize(index + 1, Worker::default());
        }
        self.workers.get_mut(index)
    }

    fn sample(&mut self) {
        let now = Instant::now();
        while self
            .samples
            .front()
            .is_some_and(|(at, _)| now.duration_since(*at) > RATE_WINDOW)
        {
            self.samples.pop_front();
        }
        self.samples.push_back((now, self.bytes));
    }

    pub fn finish(&mut self, result: Result<CrawlSummary, String>, output: &Path) {
        self.ended = Some(Instant::now());
        for worker in &mut self.workers {
            *worker = Worker {
                requests: worker.requests,
                ..Worker::default()
            };
        }
        match result {
            Ok(summary) => {
                self.phase = if summary.stopped.is_some() {
                    Phase::Stopped
                } else {
                    Phase::Finished
                };
                // The summary covers the whole copy, resumed runs included.
                self.saved = summary.saved;
                self.failed = summary.failed.len();
                self.bytes = summary.bytes;
                let mut text = format!(
                    "Copied {} files ({}) in {:.1}s; {} failed, {} skipped.",
                    summary.saved,
                    human_bytes(summary.bytes),
                    summary.elapsed.as_secs_f64(),
                    summary.failed.len(),
                    summary.skipped,
                );
                if let Some(check) = &summary.verification {
                    text.push_str(&match (check.retried, check.still_missing.len()) {
                        (0, _) => " No missed files.".to_owned(),
                        (_, 0) => format!(" Recovered all {} missed files.", check.recovered),
                        (_, left) => format!(
                            " Recovered {} missed files; {left} still missing.",
                            check.recovered
                        ),
                    });
                }
                self.summary = Some(text);
                self.report = Some(output.join(REPORT_HTML));
            }
            Err(message) => self.phase = Phase::Failed(message),
        }
    }

    /// Notes where the zip went, or why it couldn't be written.
    pub fn zipped(&mut self, zip: Result<PathBuf, String>) {
        match zip {
            Ok(path) => self.info(&format!("Zip saved: {}", path.display())),
            Err(error) => {
                self.push(LineKind::Failed, "Could not write the zip", error);
            }
        }
    }

    pub fn status(&self) -> String {
        match &self.phase {
            Phase::Idle => "Ready.".to_owned(),
            Phase::Running if self.checking => "Checking for missed files…".to_owned(),
            Phase::Running => "Copying…".to_owned(),
            Phase::Stopping => "Stopping after the requests in flight…".to_owned(),
            Phase::Finished => self.summary.clone().unwrap_or_else(|| "Done.".to_owned()),
            Phase::Stopped => {
                "Stopped. Copy the same site again to continue where it left off.".to_owned()
            }
            Phase::Failed(message) => message.clone(),
        }
    }

    fn info(&mut self, text: &str) {
        self.push(LineKind::Info, text, String::new());
    }

    fn push(&mut self, kind: LineKind, text: &str, detail: String) {
        if self.log.len() == LOG_LINES {
            self.log.pop_front();
        }
        self.log.push_back(LogLine {
            kind,
            text: text.to_owned(),
            detail,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(url: &str, output: &str) -> Settings {
        Settings {
            url: url.to_owned(),
            output: output.to_owned(),
            per_host_delay: Duration::ZERO,
            ..Settings::default()
        }
    }

    #[test]
    fn tests_each_proxy_against_the_site() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let (site, good, locked) = runtime.block_on(async {
            (
                mc_fixtures::FixtureServer::start(mc_fixtures::Site::StaticBlog)
                    .await
                    .unwrap(),
                mc_fixtures::ProxyServer::start(false).await.unwrap(),
                mc_fixtures::ProxyServer::start(true).await.unwrap(),
            )
        });
        let form = Settings {
            proxies: format!(
                "{}\n# spare\n{}\nhttp://127.0.0.1:1\n",
                good.url(false),
                locked.url(false)
            ),
            ..settings(site.base_url().as_str(), "/tmp/unused")
        };
        let checks = check_proxies(&form).unwrap().blocking_recv().unwrap();
        assert_eq!(checks.len(), 3);
        assert!(matches!(checks[0].result, Ok((200, _))), "{:?}", checks[0]);
        assert!(matches!(checks[1].result, Ok((407, _))), "{:?}", checks[1]);
        assert!(checks[2].result.is_err(), "nothing listens on port 1");
        assert!(!checks[0].proxy.contains('@'));

        let empty = settings(site.base_url().as_str(), "/tmp/unused");
        assert!(check_proxies(&empty).unwrap_err().contains("Add a proxy"));
        let bad = Settings {
            proxies: "ftp://x:21".to_owned(),
            ..empty
        };
        assert!(check_proxies(&bad).unwrap_err().starts_with("Proxy:"));
        drop(runtime);
    }

    #[test]
    fn groups_and_names_profiles() {
        let families = profile_families();
        let chrome = &families.iter().find(|(f, _)| f == "chrome").unwrap().1;
        assert!(chrome.contains(&default_profile()));
        assert_eq!(pretty_profile("chrome_149"), "Chrome 149");
        assert_eq!(pretty_profile("safari_ios_18.1"), "Safari iOS 18.1");
        let form = Settings {
            profile: Some("netscape_4".to_owned()),
            ..settings("https://s.test/", "/out")
        };
        let seed = Url::parse("https://s.test/").unwrap();
        assert!(
            form.options(&seed, Path::new("/out"))
                .unwrap_err()
                .contains("unknown browser profile")
        );
    }

    #[test]
    fn turns_the_form_into_options() {
        let seed = Url::parse("https://s.test/").unwrap();
        let out = Path::new("/out");
        let mut form = Settings {
            workers: 99,
            max_depth: " 3 ".to_owned(),
            max_time_min: "2".to_owned(),
            limit_rate: "500K".to_owned(),
            aliases: "www.s.test, cdn.s.test".to_owned(),
            auth_user: "ann".to_owned(),
            auth_password: "p:w".to_owned(),
            headers: "X-One: 1\n\n  X-Two: 2 ".to_owned(),
            warc: true,
            replay: true,
            ..settings("https://s.test/", "/out")
        };
        let (options, session) = form.options(&seed, out).unwrap();
        assert_eq!(options.concurrency, MAX_WORKERS);
        assert_eq!(options.max_depth, Some(3));
        assert_eq!(options.max_time_secs, Some(120));
        assert_eq!(options.limit_rate, Some(512_000));
        assert_eq!(options.aliases, ["www.s.test", "cdn.s.test"]);
        assert!(options.sitemaps);
        assert!(!options.replay, "replay needs the browser");
        assert_eq!(options.warc, Some(PathBuf::from("/out/s.test.warc.gz")));
        assert_eq!(session.auth, ["s.test=ann:p:w"]);
        assert_eq!(session.headers, ["X-One: 1", "X-Two: 2"]);

        form.max_urls = "lots".to_owned();
        assert!(form.options(&seed, out).unwrap_err().contains("Max pages"));
        form.max_urls.clear();
        form.limit_rate = "fast".to_owned();
        assert!(form.options(&seed, out).unwrap_err().contains("Bandwidth"));
        form.limit_rate.clear();
        form.exclude = "(".to_owned();
        assert!(form.options(&seed, out).unwrap_err().contains("regex"));
    }

    #[test]
    fn parses_the_form() {
        let (url, output) = parse(&settings(" example.com ", "/tmp/out")).unwrap();
        assert_eq!(url.as_str(), "https://example.com/");
        assert_eq!(output, PathBuf::from("/tmp/out"));
        let (url, _) = parse(&settings("http://localhost:8000/docs", "x")).unwrap();
        assert_eq!(url.as_str(), "http://localhost:8000/docs");

        assert!(
            parse(&settings("", "x"))
                .unwrap_err()
                .contains("Enter the address")
        );
        assert!(
            parse(&settings("ftp://x.test", "x"))
                .unwrap_err()
                .contains("web address")
        );
        assert!(
            parse(&settings("example.com", "  "))
                .unwrap_err()
                .contains("folder")
        );
    }

    #[test]
    fn tracks_events() {
        let url = Url::parse("https://s.test/").unwrap();
        let mut progress = Progress::started(true, 2);
        assert!(progress.running());
        assert_eq!(progress.fraction(), None);
        progress.apply(&CrawlEvent::Fetching {
            worker: 1,
            url: url.clone(),
        });
        assert_eq!(progress.active(), 1);
        assert_eq!(progress.workers[1].url.as_deref(), Some("https://s.test/"));
        progress.apply(&CrawlEvent::WorkerIdle { worker: 1 });
        assert_eq!(progress.active(), 0);
        progress.apply(&CrawlEvent::Progress {
            done: 1,
            total: 4,
            active: 0,
        });
        assert_eq!(progress.fraction(), Some(0.25));
        progress.apply(&CrawlEvent::Saved {
            url: url.clone(),
            path: PathBuf::from("x"),
            bytes: 2048,
        });
        progress.apply(&CrawlEvent::Failed {
            url: url.clone(),
            reason: "HTTP 404 Not Found".to_owned(),
        });
        progress.apply(&CrawlEvent::Stopping {
            reason: mc_core::StopReason::Requested,
        });
        assert_eq!(
            (progress.saved, progress.failed, progress.bytes),
            (1, 1, 2048)
        );
        assert_eq!(progress.phase, Phase::Stopping);
        assert_eq!(progress.log.len(), 4);
        assert!(progress.log[0].contains("Continuing"));

        for _ in 0..LOG_LINES {
            progress.apply(&CrawlEvent::Rewriting { documents: 1 });
        }
        assert_eq!(progress.log.len(), LOG_LINES);
    }

    #[test]
    fn reports_how_it_ended() {
        let mut progress = Progress::started(false, 4);
        progress.finish(Err("disk full".to_owned()), Path::new("/out"));
        assert_eq!(progress.status(), "disk full");
        assert!(!progress.running());

        let mut progress = Progress::started(false, 4);
        let summary = CrawlSummary {
            saved: 3,
            bytes: 1024,
            ..CrawlSummary::default()
        };
        progress.finish(Ok(summary), Path::new("/out"));
        assert_eq!(progress.phase, Phase::Finished);
        assert!(
            progress.status().starts_with("Copied 3 files (1.0 KiB)"),
            "{}",
            progress.status()
        );
        assert_eq!(
            progress.report,
            Some(PathBuf::from("/out").join(REPORT_HTML))
        );
    }
}
