//! A copy running on its own thread, and the progress the window shows.
//! Plain Rust, no UI: the window renders a [`Progress`] and feeds it events.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use mc_core::{
    CrawlConfig, CrawlEvent, CrawlStatus, CrawlSummary, Crawler, REPORT_HTML, StopHandle,
    human_bytes,
};
use mc_requester::{Requester, RequesterFactory, SharedResources, Url};
use mc_requester_wreq::WreqFactory;
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

#[derive(Debug, Clone)]
pub struct Settings {
    pub url: String,
    pub output: String,
    pub render_js: bool,
    /// Parallel downloads.
    pub workers: usize,
    /// Minimum gap between two requests to the same host.
    pub per_host_delay: Duration,
    pub respect_robots: bool,
    /// Skip URLs matching this regex (empty: none).
    pub exclude: String,
    /// Fetch URLs matching this regex even outside the site (empty: none).
    pub include: String,
    /// Re-check a finished copy instead of starting over.
    pub update: bool,
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
    let output = match folder.strip_prefix("~/") {
        Some(rest) => std::env::var_os("HOME")
            .map(|home| PathBuf::from(home).join(rest))
            .unwrap_or_else(|| PathBuf::from(folder)),
        None => PathBuf::from(folder),
    };
    Ok((url, output))
}

/// A copy in progress.
pub struct Running {
    pub stop: StopHandle,
    pub events: broadcast::Receiver<CrawlEvent>,
    pub done: oneshot::Receiver<Result<CrawlSummary, String>>,
    pub output: PathBuf,
    /// An unfinished copy of the same site was found and is being continued.
    pub resumed: bool,
    pub workers: usize,
}

pub fn start(settings: &Settings) -> Result<Running, String> {
    let (seed, output) = parse(settings)?;
    let shared = SharedResources::default();
    let requester = requester(settings.render_js, &shared)?;

    let mut store = SqliteStore::open(&output).map_err(|e| e.to_string())?;
    // Continue an unfinished copy of the same site; otherwise start fresh.
    let same_site = store.meta(META_SEED).ok().flatten().as_deref() == Some(seed.as_str());
    let unfinished = matches!(
        store.status(),
        Ok(Some(CrawlStatus::Running | CrawlStatus::Stopped))
    );
    let complete = matches!(store.status(), Ok(Some(CrawlStatus::Complete)));
    let rules = crate::insight::rules(&settings.exclude, &settings.include)?;
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
    let list = |pattern: &str| {
        let pattern = pattern.trim();
        if pattern.is_empty() {
            vec![]
        } else {
            vec![pattern.to_owned()]
        }
    };
    let options = serde_json::json!({
        "requester": if settings.render_js { "auto" } else { "wreq" },
        "exclude": list(&settings.exclude),
        "include": list(&settings.include),
        "concurrency": settings.workers,
        "delay_ms": settings.per_host_delay.as_millis() as u64,
        "ignore_robots": !settings.respect_robots,
    });
    store
        .set_meta(META_OPTIONS, &options.to_string())
        .map_err(|e| e.to_string())?;

    let mut config = CrawlConfig::new(seed, &output);
    config.concurrency = settings.workers.clamp(1, MAX_WORKERS);
    config.per_host_delay = settings.per_host_delay;
    config.respect_robots = settings.respect_robots;
    config.rules = rules;
    let workers = config.concurrency;
    let crawler = Crawler::new(config, requester).with_store(store);
    let stop = crawler.stop_handle();
    let events = crawler.subscribe();
    let (finished, done) = oneshot::channel();
    std::thread::Builder::new()
        .name("mistercopy-crawl".to_owned())
        .spawn(move || {
            let result = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .map_err(|e| e.to_string())
                .and_then(|runtime| runtime.block_on(crawler.run()).map_err(|e| e.to_string()));
            let _ = finished.send(result);
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

fn requester(render_js: bool, shared: &SharedResources) -> Result<Arc<dyn Requester>, String> {
    let wreq = WreqFactory
        .build(&serde_json::json!({}), shared)
        .map_err(|e| e.to_string())?;
    if !render_js {
        return Ok(wreq);
    }
    #[cfg(feature = "browser")]
    {
        let browser = mc_requester_playwright::PlaywrightFactory
            .build(&serde_json::json!({}), shared)
            .map_err(|e| e.to_string())?;
        Ok(Arc::new(mc_requester::HybridRequester::new(wreq, browser)))
    }
    #[cfg(not(feature = "browser"))]
    Err("This build can't render JavaScript.".to_owned())
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
                self.summary = Some(format!(
                    "Copied {} files ({}) in {:.1}s; {} failed, {} skipped.",
                    summary.saved,
                    human_bytes(summary.bytes),
                    summary.elapsed.as_secs_f64(),
                    summary.failed.len(),
                    summary.skipped,
                ));
                self.report = Some(output.join(REPORT_HTML));
            }
            Err(message) => self.phase = Phase::Failed(message),
        }
    }

    pub fn status(&self) -> String {
        match &self.phase {
            Phase::Idle => "Ready.".to_owned(),
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
            render_js: false,
            workers: 4,
            per_host_delay: Duration::ZERO,
            respect_robots: true,
            exclude: String::new(),
            include: String::new(),
            update: false,
        }
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
