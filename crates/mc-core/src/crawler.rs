use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use mc_extract::{Link, LinkKind, extract_css, extract_html, rewrite_css, rewrite_html};
use mc_requester::{
    Body, CapturedResponse, FetchContext, FetchError, FetchRequest, FetchResponse, HeaderMap,
    Method, ProgressSink, Requester, StatusCode, Validators, header,
};
use tokio::sync::{Notify, broadcast};
use tokio::task::{self, JoinSet};
use tokio::time::Instant as Deadline;
use url::Url;

use crate::filetype::FileType;
use crate::normalize::normalize;
use crate::path::{LocalPath, PathAllocator, hash_hex};
use crate::replay::{self, Manifest, REPLAY_SCRIPT};
use crate::report::{Report, human_bytes};
use crate::retry::{self, MAX_RETRY_AFTER};
use crate::robots::{self, RobotsRules};
use crate::rules::{Decision, Policy, Rule, Scope, SkipReason};
use crate::sitemap;
use crate::store::{
    CrawlStatus, Document, EntryState, SavedFile, StateStore, StoreError, UrlEntry,
};
use crate::warc::WarcWriter;

/// Redirect hops followed between entries before giving up on a cycle.
const MAX_ALIAS_HOPS: usize = 16;
/// Smart slow-down: the first gap after a 429/503 (when none was set), the
/// longest it grows, and how many good answers in a row earn a step back.
const ADAPTIVE_START: Duration = Duration::from_millis(500);
const ADAPTIVE_MAX: Duration = Duration::from_secs(60);
const ADAPTIVE_RECOVERY: u32 = 10;

/// Sitemap files read per crawl, indexes included.
const MAX_SITEMAPS: usize = 200;
/// Pages queued from sitemaps per crawl.
const MAX_SITEMAP_PAGES: usize = 200_000;

/// Inside the output directory: crawl state and pristine copies of the
/// documents whose links get rewritten.
pub const STATE_DIR: &str = ".amel";
const ORIGINALS_DIR: &str = "originals";
pub const REPORT_JSON: &str = "amel-report.json";
pub const REPORT_HTML: &str = "amel-report.html";

#[derive(Debug, Clone)]
pub struct CrawlConfig {
    pub seeds: Vec<Url>,
    pub output_dir: PathBuf,
    /// Extra hosts treated as part of the site, besides the seeds' hosts.
    pub domain_aliases: Vec<String>,
    pub rules: Vec<Rule>,
    pub include_external_assets: bool,
    pub max_depth: Option<u32>,
    /// Stop fetching after this many URLs; the rest are skipped.
    pub max_urls: Option<usize>,
    /// Stop early, resumably, once this much has been downloaded.
    pub max_total_bytes: Option<u64>,
    /// Stop early, resumably, after running this long.
    pub max_duration: Option<Duration>,
    pub concurrency: usize,
    /// Minimum gap between two requests to the same host.
    pub per_host_delay: Duration,
    /// Retries after the first attempt, for transient failures.
    pub max_retries: u32,
    pub retry_base_delay: Duration,
    pub respect_robots: bool,
    /// Sent with every request to a host inside the scope.
    pub headers: HeaderMap,
    pub credentials: Vec<Credential>,
    /// Audit only: fetch and report, but write nothing.
    pub dry_run: bool,
    pub write_report: bool,
    /// Also write every saved response to this WARC (`.warc.gz`) file.
    pub warc: Option<PathBuf>,
    /// Embed data that scripts fetched in a replay shim, so JavaScript
    /// apps can load it offline. Useful with a browser requester that keeps
    /// scripts.
    pub replay: bool,
    /// Also queue the pages the site's sitemaps list (named in robots.txt,
    /// else `/sitemap.xml`), so pages nothing links to are copied too.
    pub use_sitemaps: bool,
    /// Average download speed cap, in bytes per second, across all workers.
    pub max_rate: Option<u64>,
    /// When the crawl finishes, check that every saved file is on disk and
    /// fetch again what is missing or failed for a passing reason.
    pub verify: bool,
    /// When continuing a copy from its store, queue the URLs that failed
    /// before (whatever the reason) to be fetched again.
    pub retry_failed: bool,
    /// Smart slow-down: a 429 or 503 doubles the gap between requests to
    /// that host; a run of good answers shrinks it back toward the
    /// configured delay.
    pub adaptive: bool,
    /// File types not downloaded (pages linked from the seeds still are).
    pub skip_types: Vec<FileType>,
    /// Size limits per file type, in bytes.
    pub type_limits: Vec<(FileType, u64)>,
    /// When continuing a copy from its store, fetch these URLs again,
    /// whatever became of them.
    pub retry_urls: Vec<Url>,
}

/// What a script (or any other observer) can do around a crawl. Every
/// method has a default, so a hook implements only what it needs.
pub trait Hooks: Send + Sync {
    fn on_start(&self, _seeds: &[Url], _output: &Path) {}
    /// Called before each download; `false` skips the URL.
    fn should_fetch(&self, _url: &Url, _kind: LinkKind) -> bool {
        true
    }
    fn on_file(&self, _file: &SavedInfo) {}
    fn on_error(&self, _url: &Url, _reason: &str) {}
    fn on_finish(&self, _summary: &CrawlSummary) {}
    /// Messages the hook wants shown, taken after each call.
    fn take_logs(&self) -> Vec<String> {
        Vec::new()
    }
}

/// A file just saved, as hooks see it.
#[derive(Debug, Clone)]
pub struct SavedInfo {
    pub url: Url,
    pub path: PathBuf,
    pub bytes: u64,
    pub mime: Option<String>,
    pub file_type: FileType,
}

impl CrawlConfig {
    /// Polite defaults: 4 requests in flight, one per second per host,
    /// robots.txt respected, 3 retries.
    pub fn new(seed: Url, output_dir: impl Into<PathBuf>) -> Self {
        Self {
            seeds: vec![seed],
            output_dir: output_dir.into(),
            domain_aliases: Vec::new(),
            rules: Vec::new(),
            include_external_assets: false,
            max_depth: None,
            max_urls: None,
            max_total_bytes: None,
            max_duration: None,
            concurrency: 4,
            per_host_delay: Duration::from_secs(1),
            max_retries: 3,
            retry_base_delay: Duration::from_secs(1),
            respect_robots: true,
            headers: HeaderMap::new(),
            credentials: Vec::new(),
            dry_run: false,
            write_report: true,
            warc: None,
            replay: false,
            use_sitemaps: false,
            max_rate: None,
            verify: false,
            retry_failed: false,
            adaptive: false,
            skip_types: Vec::new(),
            type_limits: Vec::new(),
            retry_urls: Vec::new(),
        }
    }
}

/// HTTP Basic credentials for one host.
#[derive(Clone)]
pub struct Credential {
    pub host: String,
    pub username: String,
    pub password: String,
}

impl fmt::Debug for Credential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credential")
            .field("host", &self.host)
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    Requested,
    TimeLimit,
    ByteLimit,
}

impl fmt::Display for StopReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Requested => "stop requested",
            Self::TimeLimit => "time limit reached",
            Self::ByteLimit => "download size limit reached",
        })
    }
}

/// Controls a running crawl from outside: stop it after the requests in
/// flight, or pause and resume it.
#[derive(Debug, Clone, Default)]
pub struct StopHandle {
    inner: Arc<StopSignal>,
}

#[derive(Debug, Default)]
struct StopSignal {
    requested: AtomicBool,
    paused: AtomicBool,
    notify: Notify,
}

impl StopHandle {
    pub fn stop(&self) {
        self.inner.requested.store(true, Ordering::SeqCst);
        self.inner.notify.notify_one();
    }

    /// No new requests start; those in flight finish, and the crawl waits
    /// until [`Self::resume`] (or [`Self::stop`]).
    pub fn pause(&self) {
        self.inner.paused.store(true, Ordering::SeqCst);
        self.inner.notify.notify_one();
    }

    pub fn resume(&self) {
        self.inner.paused.store(false, Ordering::SeqCst);
        self.inner.notify.notify_one();
    }

    pub fn is_paused(&self) -> bool {
        self.inner.paused.load(Ordering::SeqCst)
    }

    /// Waits while paused (and not stopped). Workers poll: the crawl loop
    /// owns the notification.
    async fn hold_while_paused(&self) {
        while self.is_paused() && !self.requested() {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    fn requested(&self) -> bool {
        self.inner.requested.load(Ordering::SeqCst)
    }
}

#[derive(Debug, Clone)]
pub enum CrawlEvent {
    Saved {
        url: Url,
        path: PathBuf,
        bytes: u64,
    },
    /// Update found the server copy unchanged (304) and kept the local file.
    Unchanged {
        url: Url,
    },
    Failed {
        url: Url,
        reason: String,
    },
    Skipped {
        url: Url,
        reason: SkipReason,
    },
    Retrying {
        url: Url,
        attempt: u32,
        delay: Duration,
        reason: String,
    },
    /// No new requests start; those in flight finish.
    Stopping {
        reason: StopReason,
    },
    /// Paused: no new requests start until resumed.
    Paused,
    Resumed,
    /// Fetching is over; links in this many documents are being rewritten.
    Rewriting {
        documents: usize,
    },
    /// Worker `worker` (numbered from 0 up to the concurrency) took `url`
    /// but waits for the host's turn (the per-host delay) before sending.
    Waiting {
        worker: usize,
        url: Url,
    },
    /// Worker `worker` sent a request for `url`.
    Fetching {
        worker: usize,
        url: Url,
    },
    /// Worker `worker` finished its request and waits for the next one.
    WorkerIdle {
        worker: usize,
    },
    /// The crawl is over and the check for missed files starts: this many
    /// saved files are checked on disk, and this many URLs fetched again.
    Verifying {
        files: usize,
        retrying: usize,
    },
    /// The check for missed files is done.
    Verified {
        recovered: usize,
        still_missing: usize,
    },
    /// Worker `worker` has received this much of its file so far.
    Downloading {
        worker: usize,
        received: u64,
        total: Option<u64>,
    },
    /// A message from a script hook.
    ScriptLog {
        message: String,
    },
    /// Smart slow-down changed the gap between requests to `host`: longer
    /// after a 429/503 (`slower`), shorter after a run of good answers.
    Pace {
        host: String,
        gap: Duration,
        slower: bool,
    },
    /// A sitemap was read and listed this many pages.
    SitemapRead {
        url: Url,
        pages: usize,
    },
    /// Counts for a progress bar, sent whenever they change. `done` URLs
    /// are finished one way or another; `total` grows as links are found.
    Progress {
        done: usize,
        total: usize,
        active: usize,
    },
}

/// How much a log line matters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum LogLevel {
    /// Each request, wait and skip: useful when digging.
    Debug,
    Info,
    Warn,
    Error,
}

impl LogLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Debug => "DEBUG",
            Self::Info => "INFO",
            Self::Warn => "WARN",
            Self::Error => "ERROR",
        }
    }
}

impl CrawlEvent {
    /// The event as a log line: its level, its source and the message.
    /// `None` for counts and byte progress, which would drown the rest.
    pub fn describe(&self) -> Option<(LogLevel, &'static str, String)> {
        use CrawlEvent as E;
        use LogLevel as Level;
        Some(match self {
            E::Saved { url, path, bytes } => (
                Level::Info,
                "copy",
                format!(
                    "saved {url} ({}) -> {}",
                    human_bytes(*bytes),
                    path.display()
                ),
            ),
            E::Unchanged { url } => (Level::Info, "copy", format!("unchanged {url}")),
            E::Failed { url, reason } => (Level::Error, "copy", format!("failed {url}: {reason}")),
            E::Skipped { url, reason } => {
                (Level::Debug, "copy", format!("skipped {url}: {reason}"))
            }
            E::Retrying {
                url,
                attempt,
                delay,
                reason,
            } => (
                Level::Warn,
                "copy",
                format!(
                    "retry {attempt} for {url} in {:.1}s: {reason}",
                    delay.as_secs_f64()
                ),
            ),
            E::Stopping { reason } => (Level::Warn, "copy", format!("stopping: {reason}")),
            E::Paused => (Level::Info, "copy", "paused".to_owned()),
            E::Resumed => (Level::Info, "copy", "resumed".to_owned()),
            E::Rewriting { documents } => (
                Level::Info,
                "copy",
                format!("rewriting links in {documents} pages"),
            ),
            E::Waiting { worker, url } => (
                Level::Debug,
                "copy",
                format!("worker {} waits for its turn: {url}", worker + 1),
            ),
            E::Fetching { worker, url } => (
                Level::Debug,
                "copy",
                format!("worker {} GET {url}", worker + 1),
            ),
            E::Verifying { files, retrying } => (
                Level::Info,
                "copy",
                format!("checking {files} saved files; fetching {retrying} again"),
            ),
            E::Verified {
                recovered,
                still_missing,
            } => (
                if *still_missing > 0 {
                    Level::Warn
                } else {
                    Level::Info
                },
                "copy",
                format!("check done: {recovered} recovered, {still_missing} still missing"),
            ),
            E::Pace { host, gap, slower } => (
                if *slower { Level::Warn } else { Level::Info },
                "copy",
                format!(
                    "{host}: {} to {:.1}s between requests",
                    if *slower {
                        "slowing down"
                    } else {
                        "speeding up"
                    },
                    gap.as_secs_f64()
                ),
            ),
            E::SitemapRead { url, pages } => {
                (Level::Info, "copy", format!("sitemap {url}: {pages} pages"))
            }
            E::ScriptLog { message } => (Level::Info, "script", message.clone()),
            // Counts and byte-by-byte progress would drown everything else.
            E::WorkerIdle { .. } | E::Progress { .. } | E::Downloading { .. } => return None,
        })
    }
}

#[derive(Debug, Clone, Default)]
pub struct CrawlSummary {
    pub saved: usize,
    pub unchanged: usize,
    pub bytes: u64,
    pub failed: Vec<(Url, String)>,
    pub skipped: usize,
    pub elapsed: Duration,
    /// Set when the crawl ended early and can be resumed.
    pub stopped: Option<StopReason>,
    pub report: Report,
    /// What the check for missed files found, when it ran.
    pub verification: Option<Verification>,
}

/// The result of checking a finished copy for missed files.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Verification {
    /// Saved files checked on disk.
    pub files: usize,
    /// URLs fetched again: failed for a passing reason, or saved but gone.
    pub retried: usize,
    /// Of those, now saved.
    pub recovered: usize,
    /// Still not on disk, with the reason.
    pub still_missing: Vec<(Url, String)>,
}

#[derive(Debug, thiserror::Error)]
pub enum CrawlError {
    #[error("no start URL given")]
    NoSeeds,
    #[error("start URL `{0}` must be an http or https URL with a host")]
    InvalidSeed(Url),
    #[error("concurrency must be at least 1")]
    ZeroConcurrency,
    #[error("cannot use output directory {}: {source}", path.display())]
    Output {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("could not prepare the requester: {0}")]
    Requester(FetchError),
}

pub struct Crawler {
    config: CrawlConfig,
    requester: Arc<dyn Requester>,
    events: broadcast::Sender<CrawlEvent>,
    store: Option<Box<dyn StateStore>>,
    stop: StopHandle,
    limit: Option<Arc<RateLimit>>,
    hooks: Option<Arc<dyn Hooks>>,
}

impl Crawler {
    pub fn new(config: CrawlConfig, requester: Arc<dyn Requester>) -> Self {
        let (events, _) = broadcast::channel(1024);
        let limit = config
            .max_rate
            .filter(|rate| *rate > 0)
            .map(|rate| Arc::new(RateLimit::new(rate)));
        Self {
            limit,
            hooks: None,
            config,
            requester,
            events,
            store: None,
            stop: StopHandle::default(),
        }
    }

    /// Persists progress, and resumes or updates whatever the store holds.
    pub fn with_store(mut self, store: impl StateStore + 'static) -> Self {
        self.store = Some(Box::new(store));
        self
    }

    /// Calls `hooks` around the crawl: at the start, before each download,
    /// for each file and failure, and at the finish.
    pub fn with_hooks(mut self, hooks: Arc<dyn Hooks>) -> Self {
        self.hooks = Some(hooks);
        self
    }

    /// Runs a hook, then shows what it logged.
    fn hook<R>(&self, call: impl FnOnce(&dyn Hooks) -> R) -> Option<R> {
        let hooks = self.hooks.as_deref()?;
        let result = call(hooks);
        for message in hooks.take_logs() {
            self.emit(CrawlEvent::ScriptLog { message });
        }
        Some(result)
    }

    /// Progress events. A receiver that falls too far behind skips ahead.
    pub fn subscribe(&self) -> broadcast::Receiver<CrawlEvent> {
        self.events.subscribe()
    }

    pub fn stop_handle(&self) -> StopHandle {
        self.stop.clone()
    }

    pub async fn run(mut self) -> Result<CrawlSummary, CrawlError> {
        let started = Instant::now();
        let seeds = self.validate().await?;
        let policy = Policy {
            scope: Scope::new(
                seeds
                    .iter()
                    .filter_map(|seed| seed.host_str().map(str::to_owned))
                    .chain(self.config.domain_aliases.iter().cloned()),
            ),
            rules: self.config.rules.clone(),
            include_external_assets: self.config.include_external_assets,
            max_depth: self.config.max_depth,
        };

        let mut state = State {
            store: self.store.take(),
            ..State::default()
        };
        self.restore(&mut state, &seeds)?;
        if let Some(path) = self.config.warc.as_ref().filter(|_| !self.config.dry_run) {
            let warc = WarcWriter::open(path).map_err(|source| CrawlError::Output {
                path: path.clone(),
                source,
            })?;
            state.warc = Some(warc);
        }
        state.set_status(CrawlStatus::Running)?;
        self.requester
            .prepare()
            .await
            .map_err(CrawlError::Requester)?;

        self.hook(|hooks| hooks.on_start(&seeds, &self.config.output_dir));
        if self.config.use_sitemaps {
            self.discover_sitemaps(&mut state, &policy, &seeds).await;
        }
        self.crawl(&mut state, &policy, started).await;
        let verification = if self.config.verify
            && !self.config.dry_run
            && state.stopped.is_none()
            && state.store_error.is_none()
        {
            Some(self.verify(&mut state, &policy, started).await)
        } else {
            None
        };
        if let Some(error) = state.store_error.take() {
            return Err(error.into());
        }
        if let Some(warc) = state.warc.as_mut()
            && let Err(error) = warc.flush()
        {
            state.warc_error.get_or_insert(error);
        }
        if let Some(source) = state.warc_error.take() {
            let path = self.config.warc.clone().unwrap_or_default();
            return Err(CrawlError::Output { path, source });
        }
        if !self.config.dry_run {
            self.rewrite_documents(&mut state).await;
        }
        let status = if state.stopped.is_some() {
            CrawlStatus::Stopped
        } else {
            CrawlStatus::Complete
        };
        state.set_status(status)?;

        let mut summary = self.summarize(&state, &seeds, started.elapsed());
        summary.verification = verification;
        self.hook(|hooks| hooks.on_finish(&summary));
        if !self.config.dry_run && self.config.write_report {
            self.write_report(&summary.report).await?;
        }
        Ok(summary)
    }

    /// Checks every saved file is on disk, then fetches again the URLs
    /// whose files are gone and those that failed for a reason that may
    /// pass (a timeout, a dropped connection, a 5xx, a write error).
    async fn verify(&self, state: &mut State, policy: &Policy, started: Instant) -> Verification {
        let mut files = 0;
        let mut missed = Vec::new();
        for entry in state.entries.values() {
            match &entry.state {
                EntryState::Saved(saved) => {
                    files += 1;
                    if !self
                        .config
                        .output_dir
                        .join(saved.path.to_path_buf())
                        .is_file()
                    {
                        missed.push(entry.clone());
                    }
                }
                EntryState::Failed(reason) if worth_retrying(reason) => missed.push(entry.clone()),
                _ => {}
            }
        }
        self.emit(CrawlEvent::Verifying {
            files,
            retrying: missed.len(),
        });
        if !missed.is_empty() {
            for entry in &missed {
                // A gone file must be fetched whole, not revalidated.
                state.previous.remove(&entry.url);
                let job = self.job_from(entry);
                state.record(job.entry(EntryState::Queued));
                state.queue.push_back(job);
            }
            self.crawl(state, policy, started).await;
        }

        let mut still_missing = Vec::new();
        let mut recovered = 0;
        for entry in &missed {
            match state.entries.get(&entry.url).map(|e| &e.state) {
                Some(EntryState::Saved(saved))
                    if self
                        .config
                        .output_dir
                        .join(saved.path.to_path_buf())
                        .is_file() =>
                {
                    recovered += 1;
                }
                Some(EntryState::Alias(_) | EntryState::Remote(_)) => recovered += 1,
                Some(EntryState::Failed(reason)) => {
                    still_missing.push((entry.url.clone(), reason.clone()));
                }
                Some(EntryState::Queued) => {
                    still_missing.push((
                        entry.url.clone(),
                        "not fetched: the copy stopped".to_owned(),
                    ));
                }
                _ => still_missing.push((entry.url.clone(), "file not on disk".to_owned())),
            }
        }
        still_missing.sort();
        self.emit(CrawlEvent::Verified {
            recovered,
            still_missing: still_missing.len(),
        });
        Verification {
            files,
            retried: missed.len(),
            recovered,
            still_missing,
        }
    }

    async fn validate(&self) -> Result<Vec<Url>, CrawlError> {
        if self.config.concurrency == 0 {
            return Err(CrawlError::ZeroConcurrency);
        }
        if self.config.seeds.is_empty() {
            return Err(CrawlError::NoSeeds);
        }
        let seeds = self
            .config
            .seeds
            .iter()
            .map(|seed| normalize(seed).ok_or_else(|| CrawlError::InvalidSeed(seed.clone())))
            .collect::<Result<Vec<_>, _>>()?;
        if !self.config.dry_run {
            tokio::fs::create_dir_all(&self.config.output_dir)
                .await
                .map_err(|source| CrawlError::Output {
                    path: self.config.output_dir.clone(),
                    source,
                })?;
        }
        Ok(seeds)
    }

    /// Loads stored progress: queued URLs go back on the queue, saved files
    /// keep their paths. Seeds not seen yet are queued.
    fn restore(&self, state: &mut State, seeds: &[Url]) -> Result<(), CrawlError> {
        if let Some(store) = state.store.as_mut() {
            let stored = store.load()?;
            for entry in stored.previous {
                if let EntryState::Saved(saved) = entry.state {
                    state.previous.insert(entry.url, saved);
                }
            }
            for entry in stored.entries {
                match &entry.state {
                    EntryState::Queued => state.queue.push_back(self.job_from(&entry)),
                    EntryState::Saved(saved) => {
                        state.allocator.reserve(&saved.path, &entry.url);
                        state.bytes += saved.bytes;
                    }
                    _ => {}
                }
                state.count(None, &entry.state);
                state.entries.insert(entry.url.clone(), entry);
            }
            if self.config.retry_failed || !self.config.retry_urls.is_empty() {
                let failed: Vec<UrlEntry> = state
                    .entries
                    .values()
                    .filter(|entry| {
                        (self.config.retry_failed && matches!(entry.state, EntryState::Failed(_)))
                            || self.config.retry_urls.contains(&entry.url)
                    })
                    .cloned()
                    .collect();
                for entry in failed {
                    let job = self.job_from(&entry);
                    state.record(job.entry(EntryState::Queued));
                    state.queue.push_back(job);
                }
            }
        }
        for seed in seeds {
            if !state.entries.contains_key(seed) {
                let job = Job {
                    url: seed.clone(),
                    depth: 0,
                    kind: LinkKind::Page,
                    referrer: None,
                    follow_links: true,
                    attempt: 0,
                    head: false,
                };
                state.record(job.entry(EntryState::Queued));
                state.queue.push_back(job);
            }
        }
        Ok(())
    }

    /// Reads the seeds' sitemaps (from robots.txt, else `/sitemap.xml`) and
    /// queues the pages they list one level below the seed. The robots.txt
    /// fetched here also serves as the crawl's own.
    async fn discover_sitemaps(&self, state: &mut State, policy: &Policy, seeds: &[Url]) {
        let mut todo = VecDeque::new();
        let mut seen = HashSet::new();
        for seed in seeds {
            let Some(robots_url) = robots::robots_url(seed) else {
                continue;
            };
            let result = self.fetch_now(state, policy, &robots_url).await;
            let mut found = match &result {
                Ok(response) if response.status.is_success() => read_body(&response.body)
                    .await
                    .map(|body| sitemap::from_robots(&body, &robots_url))
                    .unwrap_or_default(),
                _ => Vec::new(),
            };
            let key = robots::origin_key(seed);
            if !state.robots.contains_key(&key) {
                self.robots_fetched(state, key, &result);
            }
            if found.is_empty()
                && let Ok(fallback) = seed.join("/sitemap.xml")
            {
                found.push(fallback);
            }
            todo.extend(found.into_iter().filter(|url| seen.insert(url.clone())));
        }

        let Some(seed) = seeds.first() else {
            return;
        };
        let parent = Job {
            url: seed.clone(),
            depth: 0,
            kind: LinkKind::Page,
            referrer: None,
            follow_links: true,
            attempt: 0,
            head: false,
        };
        let (mut read, mut queued) = (0, 0);
        while let Some(url) = todo.pop_front() {
            if read >= MAX_SITEMAPS || queued >= MAX_SITEMAP_PAGES || self.stop.requested() {
                break;
            }
            read += 1;
            let Ok(response) = self.fetch_now(state, policy, &url).await else {
                continue;
            };
            if !response.status.is_success() {
                continue;
            }
            let Ok(body) = read_body(&response.body).await else {
                continue;
            };
            let listed = sitemap::parse(&body, &url);
            todo.extend(
                listed
                    .sitemaps
                    .into_iter()
                    .filter(|url| seen.insert(url.clone())),
            );
            let pages = listed.pages.len();
            for page in listed.pages.into_iter().take(MAX_SITEMAP_PAGES - queued) {
                let link = Link {
                    url: page,
                    kind: LinkKind::Page,
                };
                self.enqueue(state, policy, link, &parent);
                queued += 1;
            }
            self.emit(CrawlEvent::SitemapRead { url, pages });
        }
    }

    /// One request outside the worker pool, still waiting for the host's
    /// turn and carrying the site's headers and credentials.
    async fn fetch_now(
        &self,
        state: &mut State,
        policy: &Policy,
        url: &Url,
    ) -> Result<FetchResponse, FetchError> {
        self.wait_while_paused().await;
        let slot = state.reserve_slot(url, self.config.per_host_delay);
        tokio::time::sleep_until(slot).await;
        let job = Job {
            url: url.clone(),
            depth: 0,
            kind: LinkKind::Asset,
            referrer: None,
            follow_links: false,
            attempt: 0,
            head: false,
        };
        let request = self.request_for(state, policy, &job);
        self.requester
            .fetch(request, &FetchContext::default())
            .await
    }

    /// Outside the crawl loop (reading sitemaps), a pause holds here until
    /// resumed or stopped.
    async fn wait_while_paused(&self) {
        if !self.stop.is_paused() || self.stop.requested() {
            return;
        }
        self.emit(CrawlEvent::Paused);
        while self.stop.is_paused() && !self.stop.requested() {
            self.stop.inner.notify.notified().await;
        }
        self.emit(CrawlEvent::Resumed);
    }

    fn job_from(&self, entry: &UrlEntry) -> Job {
        Job {
            url: entry.url.clone(),
            depth: entry.depth,
            kind: entry.kind,
            referrer: entry.referrer.clone(),
            follow_links: entry.follow_links,
            attempt: 0,
            head: self.config.dry_run && entry.kind == LinkKind::Asset,
        }
    }

    async fn crawl(&self, state: &mut State, policy: &Policy, started: Instant) {
        let mut tasks: JoinSet<Task> = JoinSet::new();
        let mut in_flight: HashMap<task::Id, InFlight> = HashMap::new();
        // Free worker numbers, lowest first.
        let mut workers: Vec<usize> = (0..self.config.concurrency).rev().collect();
        let mut last_progress = None;
        let mut was_paused = false;
        loop {
            state.release_due_retries();
            if state.stopped.is_none()
                && let Some(reason) = self.stop_reason(state, started)
            {
                state.stopped = Some(reason);
                self.emit(CrawlEvent::Stopping { reason });
            }
            let halted = state.stopped.is_some() || state.store_error.is_some();
            let paused = self.stop.is_paused() && !halted;
            if paused != was_paused {
                was_paused = paused;
                self.emit(if paused {
                    CrawlEvent::Paused
                } else {
                    CrawlEvent::Resumed
                });
            }
            // While paused nothing new starts, and with the queue still full
            // the loop below waits for resume or stop instead of ending.
            if !halted && !paused {
                self.dispatch(state, policy, &mut tasks, &mut in_flight, &mut workers);
            }
            let progress = (state.done, state.done + state.queued, tasks.len());
            if last_progress != Some(progress) {
                last_progress = Some(progress);
                self.emit(CrawlEvent::Progress {
                    done: progress.0,
                    total: progress.1,
                    active: progress.2,
                });
            }
            let idle = tasks.is_empty();
            if idle && (halted || (state.queue.is_empty() && state.delayed.is_empty())) {
                break;
            }

            let next_retry = state.delayed.iter().map(|(at, _)| *at).min();
            tokio::select! {
                joined = tasks.join_next_with_id(), if !idle => match joined {
                    Some(Ok((id, task))) => {
                        if let Some(flight) = in_flight.remove(&id) {
                            self.release(&mut workers, flight.worker());
                        }
                        self.complete(state, policy, task).await;
                    }
                    Some(Err(error)) => match in_flight.remove(&error.id()) {
                        // A requester panicked; the crawl carries on without it.
                        Some(InFlight::Page(job, worker)) => {
                            self.release(&mut workers, worker);
                            self.fail(state, &job, format!("requester crashed: {error}"));
                        }
                        Some(InFlight::Robots(key, worker)) => {
                            self.release(&mut workers, worker);
                            let result = Err(FetchError::Other(error.to_string().into()));
                            self.robots_fetched(state, key, &result);
                        }
                        None => {}
                    },
                    None => {}
                },
                () = sleep_until(next_retry) => {}
                () = self.stop.inner.notify.notified() => {}
            }
        }
    }

    /// Tells listeners a worker is waiting for its slot, if it is.
    fn announce(&self, worker: usize, url: &Url, slot: Deadline) {
        if slot > Deadline::now() {
            self.emit(CrawlEvent::Waiting {
                worker,
                url: url.clone(),
            });
        }
    }

    fn release(&self, workers: &mut Vec<usize>, worker: usize) {
        workers.push(worker);
        workers.sort_unstable_by(|a, b| b.cmp(a));
        self.emit(CrawlEvent::WorkerIdle { worker });
    }

    fn stop_reason(&self, state: &State, started: Instant) -> Option<StopReason> {
        if self.stop.requested() {
            Some(StopReason::Requested)
        } else if self
            .config
            .max_duration
            .is_some_and(|max| started.elapsed() >= max)
        {
            Some(StopReason::TimeLimit)
        } else if self
            .config
            .max_total_bytes
            .is_some_and(|max| state.bytes >= max)
        {
            Some(StopReason::ByteLimit)
        } else {
            None
        }
    }

    fn dispatch(
        &self,
        state: &mut State,
        policy: &Policy,
        tasks: &mut JoinSet<Task>,
        in_flight: &mut HashMap<task::Id, InFlight>,
        workers: &mut Vec<usize>,
    ) {
        while tasks.len() < self.config.concurrency {
            let Some(job) = state.queue.pop_front() else {
                break;
            };
            // Saved meanwhile from a capture made while rendering another page.
            if state
                .entries
                .get(&job.url)
                .is_some_and(|entry| entry.state != EntryState::Queued)
            {
                continue;
            }
            if self.config.max_urls.is_some_and(|max| state.fetched >= max) {
                self.skip(state, &job, SkipReason::UrlLimit);
                continue;
            }

            if self.config.respect_robots {
                let key = robots::origin_key(&job.url);
                match state.robots.get(&key) {
                    Some(RobotsSlot::Ready(rules)) => {
                        if !rules.allows(&job.url) {
                            self.skip(state, &job, SkipReason::Robots);
                            continue;
                        }
                    }
                    Some(RobotsSlot::Fetching(_)) => {
                        if let Some(RobotsSlot::Fetching(parked)) = state.robots.get_mut(&key) {
                            parked.push(job);
                        }
                        continue;
                    }
                    None => {
                        let Some(robots_url) = robots::robots_url(&job.url) else {
                            state
                                .robots
                                .insert(key, RobotsSlot::Ready(RobotsRules::AllowAll));
                            state.queue.push_front(job);
                            continue;
                        };
                        state
                            .robots
                            .insert(key.clone(), RobotsSlot::Fetching(vec![job]));
                        let slot = state.reserve_slot(&robots_url, self.config.per_host_delay);
                        let requester = Arc::clone(&self.requester);
                        let task_key = key.clone();
                        let worker = workers.pop().unwrap_or_default();
                        self.announce(worker, &robots_url, slot);
                        let events = self.events.clone();
                        let handle = tasks.spawn(async move {
                            tokio::time::sleep_until(slot).await;
                            let url = robots_url.clone();
                            let _ = events.send(CrawlEvent::Fetching { worker, url });
                            let result = requester
                                .fetch(FetchRequest::get(robots_url), &FetchContext::default())
                                .await;
                            Task::Robots(task_key, result)
                        });
                        in_flight.insert(handle.id(), InFlight::Robots(key, worker));
                        continue;
                    }
                }
            }

            // A file type the rules leave out, judged by its extension before
            // anything is downloaded (the start pages are always fetched).
            let guessed = FileType::guess(&job.url);
            if job.referrer.is_some()
                && guessed.is_some_and(|kind| self.config.skip_types.contains(&kind))
            {
                self.skip(state, &job, SkipReason::FileType);
                continue;
            }
            if self.hook(|hooks| hooks.should_fetch(&job.url, job.kind)) == Some(false) {
                self.skip(state, &job, SkipReason::Script);
                continue;
            }

            state.fetched += 1;
            let mut request = self.request_for(state, policy, &job);
            request.max_body_bytes = guessed.and_then(|kind| self.type_limit(kind));
            let slot = state.reserve_slot(&job.url, self.config.per_host_delay);
            let requester = Arc::clone(&self.requester);
            let tracked = job.clone();
            let worker = workers.pop().unwrap_or_default();
            let context = FetchContext {
                referrer: job.referrer.clone(),
                depth: job.depth,
                attempt: job.attempt,
                progress: Some(self.progress_sink(worker)),
                partial_dir: (!self.config.dry_run)
                    .then(|| self.config.output_dir.join(STATE_DIR).join(PARTIAL_DIR)),
            };
            self.announce(worker, &job.url, slot);
            let events = self.events.clone();
            let limit = self.limit.clone();
            let control = self.stop.clone();
            let handle = tasks.spawn(async move {
                tokio::time::sleep_until(slot).await;
                // A worker waiting its turn holds while paused too.
                control.hold_while_paused().await;
                let url = job.url.clone();
                let _ = events.send(CrawlEvent::Fetching { worker, url });
                let result = requester.fetch(request, &context).await;
                if let (Some(limit), Ok(response)) = (&limit, &result) {
                    limit.take(body_len(&response.body).await).await;
                }
                Task::Fetch(job, result)
            });
            in_flight.insert(handle.id(), InFlight::Page(tracked, worker));
        }
    }

    fn request_for(&self, state: &State, policy: &Policy, job: &Job) -> FetchRequest {
        let mut request = FetchRequest::get(job.url.clone());
        if job.head {
            request.method = Method::HEAD;
        }
        if policy.scope.contains(&job.url) {
            request.headers = self.config.headers.clone();
        }
        let host = job.url.host_str().unwrap_or_default();
        if let Some(credential) = self
            .config
            .credentials
            .iter()
            .find(|c| c.host.eq_ignore_ascii_case(host))
        {
            let token = BASE64.encode(format!("{}:{}", credential.username, credential.password));
            if let Ok(value) = header::HeaderValue::from_str(&format!("Basic {token}")) {
                request.headers.insert(header::AUTHORIZATION, value);
            }
        }
        if !self.config.dry_run
            && let Some(previous) = state.previous.get(&job.url)
            && self.can_reuse(&job.url, previous)
        {
            request.validators = Some(previous.validators.clone());
        }
        request
    }

    /// A 304 is only useful if the local file (and, for documents, the
    /// pristine original) is still on disk.
    fn can_reuse(&self, url: &Url, previous: &SavedFile) -> bool {
        let validators = &previous.validators;
        (validators.etag.is_some() || validators.last_modified.is_some())
            && self
                .config
                .output_dir
                .join(previous.path.to_path_buf())
                .is_file()
            && (previous.document.is_none() || self.original_path(url).is_file())
    }

    async fn complete(&self, state: &mut State, policy: &Policy, task: Task) {
        match task {
            Task::Robots(key, result) => self.robots_fetched(state, key, &result),
            Task::Fetch(job, result) => self.fetched(state, policy, job, result).await,
        }
    }

    fn robots_fetched(
        &self,
        state: &mut State,
        key: String,
        result: &Result<FetchResponse, FetchError>,
    ) {
        let (rules, delay) = RobotsRules::from_fetch(result);
        if let Some(delay) = delay {
            state.host_gap.insert(key.clone(), delay);
        }
        if let Some(RobotsSlot::Fetching(parked)) =
            state.robots.insert(key, RobotsSlot::Ready(rules))
        {
            for job in parked.into_iter().rev() {
                state.queue.push_front(job);
            }
        }
    }

    async fn fetched(
        &self,
        state: &mut State,
        policy: &Policy,
        job: Job,
        result: Result<FetchResponse, FetchError>,
    ) {
        let response = match result {
            Ok(response) => response,
            Err(error) => {
                // Over the size limit set for its file type: a rule, not a failure.
                if matches!(error, FetchError::TooLarge { .. })
                    && FileType::guess(&job.url)
                        .and_then(|kind| self.type_limit(kind))
                        .is_some()
                {
                    return self.skip(state, &job, SkipReason::TooBig);
                }
                if retry::retryable_error(&error) && job.attempt < self.config.max_retries {
                    return self.retry(state, job, None, error.to_string());
                }
                return self.fail(state, &job, error.to_string());
            }
        };
        let status = response.status;
        if self.config.adaptive {
            self.adapt(state, &job.url, status);
        }

        if retry::retryable_status(status) && job.attempt < self.config.max_retries {
            let after = retry::retry_after(&response.headers, SystemTime::now());
            if after.is_none_or(|delay| delay <= MAX_RETRY_AFTER) {
                return self.retry(state, job, after, format!("HTTP {status}"));
            }
        }
        if job.head {
            let needs_body = matches!(status.as_u16(), 405 | 501)
                || document_kind(response.mime_type().as_deref(), &response.headers).is_some();
            if needs_body {
                let mut job = job;
                job.head = false;
                state.queue.push_front(job);
                return;
            }
        }
        if status == StatusCode::NOT_MODIFIED
            && let Some(previous) = state.previous.get(&job.url).cloned()
        {
            return self.unchanged(state, policy, job, previous).await;
        }
        if !status.is_success() {
            return self.fail(state, &job, format!("HTTP {status}"));
        }

        let Some(final_url) = normalize(&response.final_url) else {
            let reason = format!("redirected to unsupported URL {}", response.final_url);
            return self.fail(state, &job, reason);
        };
        if final_url != job.url {
            if state
                .entries
                .get(&final_url)
                .is_some_and(|e| !matches!(e.state, EntryState::Skipped(SkipReason::TooDeep)))
            {
                state.record(job.entry(EntryState::Alias(final_url)));
                return;
            }
            if let Decision::Skip(_) = policy.decide(&final_url, job.kind, job.depth) {
                state.record(job.entry(EntryState::Remote(final_url)));
                return;
            }
            state.record(job.entry(EntryState::Alias(final_url.clone())));
        }
        let page = Job {
            url: final_url.clone(),
            ..job
        };

        let mime = response.mime_type().or_else(|| {
            mime_guess::from_path(final_url.path())
                .first_raw()
                .map(str::to_owned)
        });
        let document = document_kind(mime.as_deref(), &response.headers);
        let file_type = mime.as_deref().map_or(FileType::Other, FileType::from_mime);
        // A large body the requester left in a file moves into place as is,
        // unless something needs its bytes (links, the WARC, replay).
        let spilled = match &response.body {
            Body::File(temp)
                if !page.head
                    && document.is_none()
                    && self.config.warc.is_none()
                    && !self.config.replay =>
            {
                Some(temp.clone())
            }
            _ => None,
        };
        let body = if page.head || spilled.is_some() {
            Vec::new()
        } else {
            match read_body(&response.body).await {
                Ok(body) => body,
                Err(error) => {
                    return self.fail(state, &page, format!("could not read body: {error}"));
                }
            }
        };
        let size = match &spilled {
            Some(temp) => tokio::fs::metadata(temp).await.map_or(0, |m| m.len()),
            None => body.len() as u64,
        };
        if page.referrer.is_some() && self.config.skip_types.contains(&file_type) {
            discard(spilled.as_deref()).await;
            return self.skip(state, &page, SkipReason::FileType);
        }
        if self.type_limit(file_type).is_some_and(|limit| size > limit) {
            discard(spilled.as_deref()).await;
            return self.skip(state, &page, SkipReason::TooBig);
        }
        // A "soft 404": an image, script or font link answered with the
        // site's HTML page. Saving it is wrong, and following its relative
        // links nests paths ever deeper.
        if page.kind == LinkKind::Asset
            && matches!(document, Some(Document::Html { .. }))
            && let Some(expected) = mime_guess::from_path(final_url.path())
                .first_raw()
                .filter(|m| !matches!(*m, "text/html" | "application/xhtml+xml"))
        {
            let reason =
                format!("got a web page where {expected} was expected (probably a missing file)");
            return self.fail(state, &page, reason);
        }
        let path = state.allocator.allocate(&final_url, mime.as_deref());
        let file = self.config.output_dir.join(path.to_path_buf());

        if !self.config.dry_run {
            let written = match &spilled {
                Some(temp) => move_into_place(temp, &file).await,
                None => write_atomic(&file, &body).await,
            };
            if let Err(error) = written {
                let reason = format!("could not write {}: {error}", file.display());
                return self.fail(state, &page, reason);
            }
            if document.is_some()
                && let Err(error) = write_atomic(&self.original_path(&final_url), &body).await
            {
                return self.fail(state, &page, format!("could not keep original: {error}"));
            }
        }

        // What the browser already loaded goes first, so the page's own
        // links to those URLs find them saved rather than queue a refetch.
        for captured in &response.subresources {
            self.save_captured(state, policy, &page, captured).await;
        }
        if let Some(document) = &document {
            for link in extract(document, &body, &final_url) {
                self.enqueue(state, policy, link, &page);
            }
        }
        for url in &response.discovered_links {
            let link = Link {
                url: url.clone(),
                kind: LinkKind::Page,
            };
            self.enqueue(state, policy, link, &page);
        }

        let bytes = if page.head {
            response
                .headers
                .get(header::CONTENT_LENGTH)
                .and_then(|v| v.to_str().ok()?.parse().ok())
                .unwrap_or(0)
        } else {
            size
        };
        state.bytes += size;
        if !page.head && spilled.is_none() {
            self.archive(
                state,
                &final_url,
                status,
                &response.headers,
                &body,
                mime.as_deref(),
                &path,
            );
        }
        let info = SavedInfo {
            url: final_url.clone(),
            path: file.clone(),
            bytes,
            mime: mime.clone(),
            file_type,
        };
        state.record(page.entry(EntryState::Saved(SavedFile {
            path,
            mime,
            document,
            bytes,
            validators: validators(&response.headers),
        })));
        self.emit(CrawlEvent::Saved {
            url: final_url,
            path: file,
            bytes,
        });
        self.hook(|hooks| hooks.on_file(&info));
    }

    /// The size limit the rules set for `kind`, if any.
    fn type_limit(&self, kind: FileType) -> Option<u64> {
        self.config
            .type_limits
            .iter()
            .find(|(limited, _)| *limited == kind)
            .map(|(_, limit)| *limit)
    }

    /// Reports a worker's download progress, at most a few times a second.
    fn progress_sink(&self, worker: usize) -> ProgressSink {
        let events = self.events.clone();
        let last = std::sync::Mutex::new(Instant::now() - Duration::from_secs(1));
        ProgressSink(Arc::new(move |received, total| {
            let mut last = last
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let done = total.is_some_and(|total| received >= total);
            if done || last.elapsed() >= Duration::from_millis(150) {
                *last = Instant::now();
                let _ = events.send(CrawlEvent::Downloading {
                    worker,
                    received,
                    total,
                });
            }
        }))
    }

    /// Saves a response a browser requester captured while loading `parent`
    /// (a script, image, font or API answer), as if it had been fetched on
    /// its own. URLs already known keep their own fate.
    async fn save_captured(
        &self,
        state: &mut State,
        policy: &Policy,
        parent: &Job,
        captured: &CapturedResponse,
    ) {
        let Some(url) = normalize(&captured.url) else {
            return;
        };
        // A queued URL is satisfied by the capture; the dispatcher then
        // skips its job. Anything already decided keeps its fate.
        if state
            .entries
            .get(&url)
            .is_some_and(|entry| entry.state != EntryState::Queued)
        {
            return;
        }
        if let Decision::Skip(_) = policy.decide(&url, LinkKind::Asset, parent.depth) {
            return;
        }
        let body = match read_body(&captured.body).await {
            Ok(body) => body,
            Err(_) => return,
        };
        let job = Job {
            url: url.clone(),
            depth: parent.depth,
            kind: LinkKind::Asset,
            referrer: Some(parent.url.clone()),
            follow_links: false,
            attempt: 0,
            head: false,
        };
        let mime = mime_type(&captured.headers).or_else(|| {
            mime_guess::from_path(url.path())
                .first_raw()
                .map(str::to_owned)
        });
        let document = document_kind(mime.as_deref(), &captured.headers);
        let path = state.allocator.allocate(&url, mime.as_deref());
        let file = self.config.output_dir.join(path.to_path_buf());
        if !self.config.dry_run {
            if let Err(error) = write_atomic(&file, &body).await {
                return self.fail(
                    state,
                    &job,
                    format!("could not write {}: {error}", file.display()),
                );
            }
            if document.is_some()
                && let Err(error) = write_atomic(&self.original_path(&url), &body).await
            {
                return self.fail(state, &job, format!("could not keep original: {error}"));
            }
        }
        // A stylesheet's own imports, fonts and images still need fetching.
        if document == Some(Document::Css) {
            for link in extract(&Document::Css, &body, &url) {
                self.enqueue(state, policy, link, &job);
            }
        }
        let bytes = body.len() as u64;
        state.bytes += bytes;
        self.archive(
            state,
            &url,
            captured.status,
            &captured.headers,
            &body,
            mime.as_deref(),
            &path,
        );
        state.record(job.entry(EntryState::Saved(SavedFile {
            path,
            mime,
            document,
            bytes,
            validators: validators(&captured.headers),
        })));
        self.emit(CrawlEvent::Saved {
            url,
            path: file,
            bytes,
        });
    }

    /// The server answered 304: keep the earlier file and crawl on from its
    /// pristine original.
    async fn unchanged(&self, state: &mut State, policy: &Policy, job: Job, previous: SavedFile) {
        state.allocator.reserve(&previous.path, &job.url);
        if let Some(document) = &previous.document
            && let Ok(body) = tokio::fs::read(self.original_path(&job.url)).await
        {
            for link in extract(document, &body, &job.url) {
                self.enqueue(state, policy, link, &job);
            }
        }
        state.unchanged += 1;
        state.record(job.entry(EntryState::Saved(previous)));
        self.emit(CrawlEvent::Unchanged { url: job.url });
    }

    /// Smart slow-down for the host of `url` after an answer with `status`.
    fn adapt(&self, state: &mut State, url: &Url, status: StatusCode) {
        let key = robots::origin_key(url);
        let host = url.host_str().unwrap_or_default().to_owned();
        let current = state.adaptive_gap.get(&key).copied();
        if matches!(status.as_u16(), 429 | 503) {
            state.good_streak.remove(&key);
            // Each URL slows its host once: retrying one page that is always
            // busy must not keep doubling the gap for the whole site.
            if !state.slowed_by.insert(url.clone()) {
                return;
            }
            let base = current.unwrap_or(self.config.per_host_delay.max(ADAPTIVE_START / 2));
            let gap = (base * 2).clamp(ADAPTIVE_START, ADAPTIVE_MAX);
            if current != Some(gap) {
                state.adaptive_gap.insert(key, gap);
                // Requests not yet started wait at least one new gap.
                state.delay_host(url, gap);
                self.emit(CrawlEvent::Pace {
                    host,
                    gap,
                    slower: true,
                });
            }
        } else if status.is_success() || status.is_redirection() {
            let Some(gap) = current else {
                return;
            };
            let streak = state.good_streak.entry(key.clone()).or_default();
            *streak += 1;
            if *streak < ADAPTIVE_RECOVERY {
                return;
            }
            *streak = 0;
            let shorter = gap * 3 / 4;
            if shorter <= self.config.per_host_delay || shorter < Duration::from_millis(50) {
                state.adaptive_gap.remove(&key);
                state.good_streak.remove(&key);
                self.emit(CrawlEvent::Pace {
                    host,
                    gap: self.config.per_host_delay,
                    slower: false,
                });
            } else {
                state.adaptive_gap.insert(key, shorter);
                self.emit(CrawlEvent::Pace {
                    host,
                    gap: shorter,
                    slower: false,
                });
            }
        }
    }

    fn retry(&self, state: &mut State, mut job: Job, after: Option<Duration>, reason: String) {
        let delay = after.unwrap_or_else(|| {
            retry::backoff(self.config.retry_base_delay, job.attempt, job.url.as_str())
        });
        if after.is_some() {
            // The server asked everyone to back off, not just this URL.
            state.delay_host(&job.url, delay);
        }
        job.attempt += 1;
        self.emit(CrawlEvent::Retrying {
            url: job.url.clone(),
            attempt: job.attempt,
            delay,
            reason,
        });
        state.delayed.push((Deadline::now() + delay, job));
    }

    fn enqueue(&self, state: &mut State, policy: &Policy, link: Link, parent: &Job) {
        let Some(url) = normalize(&link.url) else {
            return;
        };
        // A link first seen too deep may be reached again by a shorter path.
        let was_too_deep = match state.entries.get(&url) {
            None => false,
            Some(entry) if entry.state == EntryState::Skipped(SkipReason::TooDeep) => true,
            Some(_) => return,
        };
        if link.kind == LinkKind::Page && !parent.follow_links {
            return;
        }
        let depth = match link.kind {
            LinkKind::Page => parent.depth + 1,
            LinkKind::Asset => parent.depth,
        };
        let mut job = Job {
            url,
            depth,
            kind: link.kind,
            referrer: Some(parent.url.clone()),
            follow_links: false,
            attempt: 0,
            head: self.config.dry_run && link.kind == LinkKind::Asset,
        };
        match policy.decide(&job.url, link.kind, depth) {
            Decision::Fetch { follow_links } => {
                job.follow_links = follow_links;
                state.record(job.entry(EntryState::Queued));
                state.queue.push_back(job);
            }
            Decision::Skip(SkipReason::TooDeep) if was_too_deep => {}
            Decision::Skip(reason) => self.skip(state, &job, reason),
        }
    }

    /// Points every reference in saved documents at its local copy, and
    /// every reference to something not copied at its absolute URL. Always
    /// starts from the pristine original, so running it again is harmless.
    async fn rewrite_documents(&self, state: &mut State) {
        let documents: Vec<(Url, LocalPath, Document)> = state
            .entries
            .values()
            .filter_map(|entry| match &entry.state {
                EntryState::Saved(SavedFile {
                    path,
                    document: Some(document),
                    ..
                }) => Some((entry.url.clone(), path.clone(), document.clone())),
                _ => None,
            })
            .collect();
        self.emit(CrawlEvent::Rewriting {
            documents: documents.len(),
        });

        // One replay script per host, shared by its pages.
        let mut replay_scripts: HashMap<String, LocalPath> = HashMap::new();
        for (host, manifest) in &state.replay {
            let Some(script) = LocalPath::parse(&format!("{host}/{REPLAY_SCRIPT}")) else {
                continue;
            };
            let file = self.config.output_dir.join(script.to_path_buf());
            if !manifest.is_empty()
                && write_atomic(&file, manifest.script().as_bytes())
                    .await
                    .is_ok()
            {
                replay_scripts.insert(host.clone(), script);
            }
        }

        let mut failures = Vec::new();
        for (url, path, document) in documents {
            let file = self.config.output_dir.join(path.to_path_buf());
            let original = match tokio::fs::read(self.original_path(&url)).await {
                Ok(bytes) => bytes,
                // Copies made before originals were kept: use the file itself.
                Err(_) => match tokio::fs::read(&file).await {
                    Ok(bytes) => bytes,
                    Err(error) => {
                        failures.push((
                            url,
                            format!("could not re-read {}: {error}", file.display()),
                        ));
                        continue;
                    }
                },
            };
            let map = |target: &Url, _: LinkKind| Some(state.href_for(&path, target));
            let rewritten = match &document {
                Document::Html { charset } => {
                    rewrite_html(&original, charset.as_deref(), &url, map)
                        .map_err(|error| error.to_string())
                }
                Document::Css => {
                    Ok(rewrite_css(&String::from_utf8_lossy(&original), &url, map).into_bytes())
                }
            };
            let result = match rewritten {
                Ok(bytes) => {
                    let host = &path.segments()[0];
                    let bytes = match (&document, replay_scripts.get(host)) {
                        (Document::Html { .. }, Some(script)) => {
                            replay::inject(&bytes, &path.relative_href(script))
                        }
                        _ => bytes,
                    };
                    write_atomic(&file, &bytes)
                        .await
                        .map_err(|error| format!("could not write {}: {error}", file.display()))
                }
                Err(error) => Err(error),
            };
            if let Err(reason) = result {
                failures.push((url, format!("links not rewritten: {reason}")));
            }
        }
        // The files stay saved, so these are reported, not recorded.
        for (url, reason) in failures {
            self.emit(CrawlEvent::Failed { url, reason });
        }
    }

    fn summarize(&self, state: &State, seeds: &[Url], elapsed: Duration) -> CrawlSummary {
        let mut summary = CrawlSummary {
            unchanged: state.unchanged,
            elapsed,
            stopped: state.stopped,
            ..CrawlSummary::default()
        };
        for entry in state.entries.values() {
            match &entry.state {
                EntryState::Saved(saved) => {
                    summary.saved += 1;
                    summary.bytes += saved.bytes;
                }
                EntryState::Failed(reason) => {
                    summary.failed.push((entry.url.clone(), reason.clone()))
                }
                EntryState::Skipped(_) => summary.skipped += 1,
                _ => {}
            }
        }
        summary.failed.sort();
        let status = match state.stopped {
            Some(reason) => format!("stopped: {reason}"),
            None => "complete".to_owned(),
        };
        summary.report = Report::build(
            state.entries.values(),
            seeds.iter().map(ToString::to_string).collect(),
            status,
            elapsed.as_secs_f64(),
            state.unchanged,
        );
        summary
    }

    async fn write_report(&self, report: &Report) -> Result<(), CrawlError> {
        let output = |path: PathBuf, source: std::io::Error| CrawlError::Output { path, source };
        let json_path = self.config.output_dir.join(REPORT_JSON);
        let json =
            serde_json::to_vec_pretty(report).map_err(|e| output(json_path.clone(), e.into()))?;
        write_atomic(&json_path, &json)
            .await
            .map_err(|e| output(json_path, e))?;
        let html_path = self.config.output_dir.join(REPORT_HTML);
        write_atomic(&html_path, report.to_html().as_bytes())
            .await
            .map_err(|e| output(html_path, e))
    }

    fn original_path(&self, url: &Url) -> PathBuf {
        self.config
            .output_dir
            .join(STATE_DIR)
            .join(ORIGINALS_DIR)
            .join(hash_hex(url.as_str(), 16))
    }

    /// Records a saved response in the WARC and, when it is data a script
    /// fetched, in the replay manifest of its host.
    #[allow(
        clippy::too_many_arguments,
        reason = "one call per saved response, all parts needed"
    )]
    fn archive(
        &self,
        state: &mut State,
        url: &Url,
        status: StatusCode,
        headers: &HeaderMap,
        body: &[u8],
        mime: Option<&str>,
        path: &LocalPath,
    ) {
        if self.config.dry_run {
            return;
        }
        if state.warc_error.is_none()
            && let Some(warc) = state.warc.as_mut()
            && let Err(error) = warc.response(url, status, headers, body)
        {
            state.warc_error = Some(error);
        }
        if self.config.replay
            && let Some(mime) = mime.filter(|m| replay::is_data(m))
        {
            let key = match url.query() {
                Some(query) => format!("{}?{query}", url.path()),
                None => url.path().to_owned(),
            };
            let host = path.segments()[0].clone();
            state.replay.entry(host).or_default().add(key, mime, body);
        }
    }

    fn fail(&self, state: &mut State, job: &Job, reason: String) {
        self.hook(|hooks| hooks.on_error(&job.url, &reason));
        state.record(job.entry(EntryState::Failed(reason.clone())));
        self.emit(CrawlEvent::Failed {
            url: job.url.clone(),
            reason,
        });
    }

    fn skip(&self, state: &mut State, job: &Job, reason: SkipReason) {
        state.record(job.entry(EntryState::Skipped(reason)));
        self.emit(CrawlEvent::Skipped {
            url: job.url.clone(),
            reason,
        });
    }

    fn emit(&self, event: CrawlEvent) {
        // No subscribers is fine.
        let _ = self.events.send(event);
    }
}

async fn sleep_until(deadline: Option<Deadline>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

#[derive(Debug, Clone)]
struct Job {
    url: Url,
    depth: u32,
    kind: LinkKind,
    referrer: Option<Url>,
    follow_links: bool,
    attempt: u32,
    /// Scan mode checks assets with HEAD instead of downloading them.
    head: bool,
}

impl Job {
    fn entry(&self, state: EntryState) -> UrlEntry {
        UrlEntry {
            url: self.url.clone(),
            depth: self.depth,
            kind: self.kind,
            referrer: self.referrer.clone(),
            follow_links: self.follow_links,
            state,
        }
    }
}

enum Task {
    Fetch(Job, Result<FetchResponse, FetchError>),
    Robots(String, Result<FetchResponse, FetchError>),
}

enum InFlight {
    Page(Job, usize),
    Robots(String, usize),
}

impl InFlight {
    fn worker(&self) -> usize {
        match self {
            Self::Page(_, worker) | Self::Robots(_, worker) => *worker,
        }
    }
}

enum RobotsSlot {
    /// robots.txt is being fetched; these jobs wait for it.
    Fetching(Vec<Job>),
    Ready(RobotsRules),
}

enum Target<'a> {
    Local(&'a LocalPath),
    Remote(&'a Url),
}

#[derive(Default)]
struct State {
    /// Every URL seen, by normalized form.
    entries: HashMap<Url, UrlEntry>,
    queue: VecDeque<Job>,
    /// Retries waiting for their backoff to pass.
    delayed: Vec<(Deadline, Job)>,
    allocator: PathAllocator,
    next_slot: HashMap<String, Deadline>,
    /// Per-origin gaps longer than the default, from `Crawl-delay`.
    host_gap: HashMap<String, Duration>,
    /// Gaps set by the smart slow-down, by origin.
    adaptive_gap: HashMap<String, Duration>,
    /// Good answers in a row since the last change, by origin.
    good_streak: HashMap<String, u32>,
    /// URLs whose 429/503 already slowed their host down.
    slowed_by: HashSet<Url>,
    robots: HashMap<String, RobotsSlot>,
    /// Saved files from the copy being updated.
    previous: HashMap<Url, SavedFile>,
    store: Option<Box<dyn StateStore>>,
    store_error: Option<StoreError>,
    fetched: usize,
    /// Entries waiting for (or in) a request.
    queued: usize,
    /// Entries that left the queue, or were finished without it.
    done: usize,
    bytes: u64,
    unchanged: usize,
    stopped: Option<StopReason>,
    warc: Option<WarcWriter>,
    warc_error: Option<std::io::Error>,
    /// Replay manifests, by host directory.
    replay: HashMap<String, Manifest>,
}

impl State {
    /// Remembers an entry and writes it through to the store. A store
    /// failure halts the crawl: progress that can't be saved can't be resumed.
    fn record(&mut self, entry: UrlEntry) {
        if self.store_error.is_none()
            && let Some(store) = self.store.as_mut()
            && let Err(error) = store.put(&entry)
        {
            self.store_error = Some(error);
        }
        let was_queued = self
            .entries
            .get(&entry.url)
            .map(|old| old.state == EntryState::Queued);
        self.count(was_queued, &entry.state);
        self.entries.insert(entry.url.clone(), entry);
    }

    /// Keeps `queued` and `done` in step with an entry moving from its old
    /// state (`None`: new) to `state`.
    fn count(&mut self, was_queued: Option<bool>, state: &EntryState) {
        let queued = *state == EntryState::Queued;
        match (was_queued, queued) {
            (Some(true), true) | (Some(false), false) => {}
            (Some(true), false) => {
                self.queued -= 1;
                self.done += 1;
            }
            (_, true) => self.queued += 1,
            (None, false) => {
                if !matches!(state, EntryState::Skipped(_)) {
                    self.done += 1;
                }
            }
        }
    }

    fn set_status(&mut self, status: CrawlStatus) -> Result<(), StoreError> {
        match self.store.as_mut() {
            Some(store) => store.set_status(status),
            None => Ok(()),
        }
    }

    fn release_due_retries(&mut self) {
        let now = Deadline::now();
        let (due, waiting): (Vec<_>, Vec<_>) = std::mem::take(&mut self.delayed)
            .into_iter()
            .partition(|(at, _)| *at <= now);
        self.delayed = waiting;
        self.queue.extend(due.into_iter().map(|(_, job)| job));
    }

    /// The earliest moment a request to `url`'s origin may start, keeping
    /// the per-host gap.
    fn reserve_slot(&mut self, url: &Url, default_gap: Duration) -> Deadline {
        let key = robots::origin_key(url);
        let gap = self
            .host_gap
            .get(&key)
            .copied()
            .unwrap_or_default()
            .max(self.adaptive_gap.get(&key).copied().unwrap_or_default())
            .max(default_gap);
        let now = Deadline::now();
        let slot = self
            .next_slot
            .get(&key)
            .copied()
            .filter(|next| *next > now)
            .unwrap_or(now);
        self.next_slot.insert(key, slot + gap);
        slot
    }

    fn delay_host(&mut self, url: &Url, delay: Duration) {
        let until = Deadline::now() + delay;
        let slot = self
            .next_slot
            .entry(robots::origin_key(url))
            .or_insert(until);
        *slot = (*slot).max(until);
    }

    fn target(&self, url: &Url) -> Option<Target<'_>> {
        let mut key = url;
        for _ in 0..MAX_ALIAS_HOPS {
            match &self.entries.get(key)?.state {
                EntryState::Saved(saved) => return Some(Target::Local(&saved.path)),
                EntryState::Remote(remote) => return Some(Target::Remote(remote)),
                EntryState::Alias(next) => key = next,
                _ => return None,
            }
        }
        None
    }

    fn href_for(&self, from: &LocalPath, target: &Url) -> String {
        let fragment = target
            .fragment()
            .map(|f| format!("#{f}"))
            .unwrap_or_default();
        let Some(key) = normalize(target) else {
            return target.to_string();
        };
        match self.target(&key) {
            Some(Target::Local(path)) => format!("{}{fragment}", from.relative_href(path)),
            Some(Target::Remote(remote)) => format!("{remote}{fragment}"),
            None => target.to_string(),
        }
    }
}

/// The media type from `Content-Type`, without parameters, lowercased.
fn mime_type(headers: &HeaderMap) -> Option<String> {
    let value = headers.get(header::CONTENT_TYPE)?.to_str().ok()?;
    let mime = value.split(';').next()?.trim();
    (!mime.is_empty()).then(|| mime.to_ascii_lowercase())
}

fn document_kind(mime: Option<&str>, headers: &HeaderMap) -> Option<Document> {
    match mime {
        Some("text/html" | "application/xhtml+xml") => Some(Document::Html {
            charset: charset(headers),
        }),
        Some("text/css") => Some(Document::Css),
        _ => None,
    }
}

/// Whether a failure may pass on a second try: not a 4xx answer (other than
/// 408 and 429), a soft 404, a size limit or an unsupported redirect.
fn worth_retrying(reason: &str) -> bool {
    if let Some(code) = reason
        .strip_prefix("HTTP ")
        .and_then(|rest| rest.get(..3))
        .and_then(|code| code.parse::<u16>().ok())
    {
        return !(400..500).contains(&code) || code == 408 || code == 429;
    }
    !(reason.starts_with("got a web page where")
        || reason.starts_with("redirected to unsupported")
        || reason.contains("larger than"))
}

fn extract(document: &Document, body: &[u8], url: &Url) -> Vec<Link> {
    match document {
        Document::Html { charset } => {
            extract_html(body, charset.as_deref(), url).unwrap_or_default()
        }
        Document::Css => extract_css(&String::from_utf8_lossy(body), url),
    }
}

fn validators(headers: &HeaderMap) -> Validators {
    let get = |name| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
    };
    Validators {
        etag: get(header::ETAG),
        last_modified: get(header::LAST_MODIFIED),
    }
}

fn charset(headers: &HeaderMap) -> Option<String> {
    let value = headers.get(header::CONTENT_TYPE)?.to_str().ok()?;
    value.split(';').skip(1).find_map(|param| {
        let (name, value) = param.split_once('=')?;
        name.trim()
            .eq_ignore_ascii_case("charset")
            .then(|| value.trim().trim_matches('"').to_owned())
    })
}

/// Spreads downloads out so their average speed stays under a cap: each
/// finished body books its share of time, and its worker waits it out
/// before taking the next URL.
struct RateLimit {
    bytes_per_sec: u64,
    next: std::sync::Mutex<Deadline>,
}

impl RateLimit {
    fn new(bytes_per_sec: u64) -> Self {
        Self {
            bytes_per_sec,
            next: std::sync::Mutex::new(Deadline::now()),
        }
    }

    async fn take(&self, bytes: u64) {
        let share = Duration::from_secs_f64(bytes as f64 / self.bytes_per_sec as f64);
        let until = {
            let mut next = self
                .next
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *next = (*next).max(Deadline::now()) + share;
            *next
        };
        tokio::time::sleep_until(until).await;
    }
}

/// Inside the state folder: large downloads in progress, kept to resume.
const PARTIAL_DIR: &str = "partial";

/// Moves a downloaded file to its final name (copying across file systems).
async fn move_into_place(temp: &Path, path: &Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    if tokio::fs::rename(temp, path).await.is_err() {
        tokio::fs::copy(temp, path).await?;
        tokio::fs::remove_file(temp).await?;
    }
    Ok(())
}

/// Removes a downloaded file that won't be kept.
async fn discard(temp: Option<&Path>) {
    if let Some(temp) = temp {
        let _ = tokio::fs::remove_file(temp).await;
    }
}

async fn body_len(body: &Body) -> u64 {
    match body {
        Body::Bytes(bytes) => bytes.len() as u64,
        Body::File(path) => tokio::fs::metadata(path).await.map_or(0, |m| m.len()),
    }
}

async fn read_body(body: &Body) -> std::io::Result<Vec<u8>> {
    match body {
        Body::Bytes(bytes) => Ok(bytes.to_vec()),
        Body::File(path) => tokio::fs::read(path).await,
    }
}

/// Writes via a temporary sibling and a rename, so an interrupted crawl never
/// leaves a half-written file under the final name.
async fn write_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy())
        .unwrap_or_default();
    let temp = path.with_file_name(format!(".{name}.mc-tmp"));
    tokio::fs::write(&temp, data).await?;
    tokio::fs::rename(&temp, path).await
}
