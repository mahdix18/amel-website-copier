//! A copy running on its own thread, and the progress the window shows.
//! Plain Rust, no UI: the window renders a [`Progress`] and feeds it events.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use mc_core::{
    CrawlConfig, CrawlEvent, CrawlStatus, CrawlSummary, Crawler, REPORT_HTML, StopHandle,
    human_bytes,
};
use mc_requester::{Requester, RequesterFactory, SharedResources, Url};
use mc_requester_wreq::WreqFactory;
use mc_store::SqliteStore;
use tokio::sync::{broadcast, oneshot};

/// Log lines kept for display; older ones scroll away for good.
const LOG_LINES: usize = 200;

/// Keys shared with the command-line tool, so `mistercopy resume` and
/// `update` work on copies started here.
const META_SEED: &str = "seed";
const META_OPTIONS: &str = "options";

#[derive(Debug, Clone)]
pub struct Settings {
    pub url: String,
    pub output: String,
    pub render_js: bool,
    /// Minimum gap between two requests to the same host.
    pub per_host_delay: Duration,
    /// Skip URLs matching this regex (empty: none).
    pub exclude: String,
    /// Fetch URLs matching this regex even outside the site (empty: none).
    pub include: String,
    /// Re-check a finished copy instead of starting over.
    pub update: bool,
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
    });
    store
        .set_meta(META_OPTIONS, &options.to_string())
        .map_err(|e| e.to_string())?;

    let mut config = CrawlConfig::new(seed, &output);
    config.per_host_delay = settings.per_host_delay;
    config.rules = rules;
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

/// What the window shows about the current (or last) copy.
#[derive(Debug, Clone, Default)]
pub struct Progress {
    pub phase: Phase,
    pub saved: usize,
    pub unchanged: usize,
    pub failed: usize,
    pub retries: usize,
    pub bytes: u64,
    /// Newest last.
    pub log: VecDeque<String>,
    pub summary: Option<String>,
    pub report: Option<PathBuf>,
}

impl Progress {
    pub fn started(resumed: bool) -> Self {
        let mut progress = Self {
            phase: Phase::Running,
            ..Self::default()
        };
        if resumed {
            progress.push("Continuing the unfinished copy of this site.".to_owned());
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

    pub fn apply(&mut self, event: &CrawlEvent) {
        match event {
            CrawlEvent::Saved { url, bytes, .. } => {
                self.saved += 1;
                self.bytes += bytes;
                self.push(format!("saved   {url}  ({})", human_bytes(*bytes)));
            }
            CrawlEvent::Unchanged { url } => {
                self.unchanged += 1;
                self.push(format!("same    {url}"));
            }
            CrawlEvent::Failed { url, reason } => {
                self.failed += 1;
                self.push(format!("failed  {url}: {reason}"));
            }
            CrawlEvent::Retrying {
                url, delay, reason, ..
            } => {
                self.retries += 1;
                self.push(format!(
                    "retry   {url} in {:.1}s ({reason})",
                    delay.as_secs_f64()
                ));
            }
            CrawlEvent::Stopping { reason } => {
                self.phase = Phase::Stopping;
                self.push(format!("stopping: {reason}"));
            }
            CrawlEvent::Rewriting { documents } => {
                self.push(format!("rewriting links in {documents} pages"));
            }
            CrawlEvent::Skipped { .. } => {}
        }
    }

    pub fn finish(&mut self, result: Result<CrawlSummary, String>, output: &Path) {
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

    fn push(&mut self, line: String) {
        if self.log.len() == LOG_LINES {
            self.log.pop_front();
        }
        self.log.push_back(line);
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
            per_host_delay: Duration::ZERO,
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
        let mut progress = Progress::started(true);
        assert!(progress.running());
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
        let mut progress = Progress::started(false);
        progress.finish(Err("disk full".to_owned()), Path::new("/out"));
        assert_eq!(progress.status(), "disk full");
        assert!(!progress.running());

        let mut progress = Progress::started(false);
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
