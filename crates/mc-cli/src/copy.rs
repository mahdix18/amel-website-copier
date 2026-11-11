//! `copy`, `resume`, `update` and `scan`: one crawl runner, four ways in.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use mc_core::{
    CrawlEvent, CrawlStatus, CrawlSummary, Crawler, REPORT_HTML, REPORT_JSON, StopHandle,
    human_bytes,
};
use mc_requester::{AUTO_ID, HybridRequester, Requester, RequesterRegistry, SharedResources, Url};
use mc_store::SqliteStore;
use std::sync::Arc;
use tokio::sync::broadcast::{self, error::RecvError};

use crate::options::{CrawlOptions, SessionArgs};

/// Invalid command line, configuration or state.
const EXIT_USAGE: u8 = 2;
/// Stopped before finishing; `resume` continues.
const EXIT_STOPPED: u8 = 3;

const META_SEED: &str = "seed";
const META_OPTIONS: &str = "options";

#[derive(clap::Args)]
pub struct CopyArgs {
    /// The page to start from.
    url: Url,

    /// Directory to write into. The copy lands in a subdirectory named
    /// after the host; progress is kept in `.mistercopy/` for resume and update.
    #[arg(short, long, default_value = ".")]
    output: PathBuf,

    /// Discard an earlier copy's progress in the output directory and start over.
    #[arg(long)]
    fresh: bool,

    #[command(flatten)]
    options: CrawlOptions,

    #[command(flatten)]
    session: SessionArgs,

    /// Print only the summary and errors.
    #[arg(short, long)]
    quiet: bool,
}

#[derive(clap::Args)]
pub struct DirArgs {
    /// The output directory of an earlier `mistercopy copy`.
    #[arg(default_value = ".")]
    dir: PathBuf,

    #[command(flatten)]
    session: SessionArgs,

    /// Print only the summary and errors.
    #[arg(short, long)]
    quiet: bool,
}

#[derive(clap::Args)]
pub struct ScanArgs {
    /// The page to start from.
    url: Url,

    /// Write the report here: JSON, or HTML if the name ends in `.html`.
    #[arg(long, value_name = "FILE")]
    report: Option<PathBuf>,

    #[command(flatten)]
    options: CrawlOptions,

    #[command(flatten)]
    session: SessionArgs,

    /// Print only the summary and errors.
    #[arg(short, long)]
    quiet: bool,
}

pub async fn copy(args: CopyArgs, registry: &RequesterRegistry) -> ExitCode {
    let dir = &args.output;
    if SqliteStore::exists_in(dir) {
        let mut store = match SqliteStore::open(dir) {
            Ok(store) => store,
            Err(error) => return usage_error(&error.to_string()),
        };
        if !args.fresh {
            let seed = store.meta(META_SEED).ok().flatten().unwrap_or_default();
            let shown = dir.display();
            match store.status() {
                Ok(Some(CrawlStatus::Complete)) => {
                    return usage_error(&format!(
                        "{shown} already holds a finished copy of {seed}. Run `mistercopy update {shown}` to refresh it, or pass --fresh to start over."
                    ));
                }
                Ok(Some(_)) => {
                    return usage_error(&format!(
                        "{shown} holds an unfinished copy of {seed}. Run `mistercopy resume {shown}` to continue it, or pass --fresh to start over."
                    ));
                }
                Ok(None) => {}
                Err(error) => return usage_error(&error.to_string()),
            }
        }
        if let Err(error) = store.reset() {
            return usage_error(&error.to_string());
        }
    }

    let store = match SqliteStore::open(dir) {
        Ok(store) => store,
        Err(error) => return usage_error(&error.to_string()),
    };
    let saved = serde_json::to_string(&args.options)
        .map_err(|e| e.to_string())
        .and_then(|json| {
            store
                .set_meta(META_SEED, args.url.as_str())
                .map_err(|e| e.to_string())?;
            store
                .set_meta(META_OPTIONS, &json)
                .map_err(|e| e.to_string())
        });
    if let Err(error) = saved {
        return usage_error(&error);
    }
    let run = Run {
        seed: args.url,
        output: args.output.clone(),
        options: args.options,
        session: args.session,
        quiet: args.quiet,
    };
    run.execute(registry, Some(store)).await
}

pub async fn resume(args: DirArgs, registry: &RequesterRegistry) -> ExitCode {
    let (store, seed, options) = match open_existing(&args.dir) {
        Ok(found) => found,
        Err(message) => return usage_error(&message),
    };
    if let Ok(Some(CrawlStatus::Complete)) = store.status() {
        println!(
            "{} is already complete. Run `mistercopy update {}` to refresh it.",
            args.dir.display(),
            args.dir.display()
        );
        return ExitCode::SUCCESS;
    }
    let run = Run {
        seed,
        output: args.dir,
        options,
        session: args.session,
        quiet: args.quiet,
    };
    run.execute(registry, Some(store)).await
}

pub async fn update(args: DirArgs, registry: &RequesterRegistry) -> ExitCode {
    let (mut store, seed, options) = match open_existing(&args.dir) {
        Ok(found) => found,
        Err(message) => return usage_error(&message),
    };
    match store.status() {
        Ok(Some(CrawlStatus::Running | CrawlStatus::Stopped)) => {
            return usage_error(&format!(
                "{} holds an unfinished copy. Run `mistercopy resume {}` first.",
                args.dir.display(),
                args.dir.display()
            ));
        }
        Err(error) => return usage_error(&error.to_string()),
        _ => {}
    }
    if let Err(error) = store.begin_update() {
        return usage_error(&error.to_string());
    }
    let run = Run {
        seed,
        output: args.dir,
        options,
        session: args.session,
        quiet: args.quiet,
    };
    run.execute(registry, Some(store)).await
}

pub async fn scan(args: ScanArgs, registry: &RequesterRegistry) -> ExitCode {
    let report_path = args.report.clone();
    let run = Run {
        seed: args.url,
        output: PathBuf::new(),
        options: args.options,
        session: args.session,
        quiet: args.quiet,
    };
    let (code, summary) = run.crawl(registry, None, true).await;
    if let (Some(summary), Some(path)) = (summary, report_path) {
        let body = if path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("html"))
        {
            Ok(summary.report.to_html().into_bytes())
        } else {
            serde_json::to_vec_pretty(&summary.report)
        };
        let written = body
            .map_err(|e| e.to_string())
            .and_then(|bytes| std::fs::write(&path, bytes).map_err(|e| e.to_string()));
        match written {
            Ok(()) => println!("Report written to {}", path.display()),
            Err(error) => return usage_error(&format!("cannot write {}: {error}", path.display())),
        }
    }
    code
}

fn open_existing(dir: &Path) -> Result<(SqliteStore, Url, CrawlOptions), String> {
    if !SqliteStore::exists_in(dir) {
        return Err(format!(
            "no MisterCopy copy in {} (expected {})",
            dir.display(),
            SqliteStore::path_in(dir).display()
        ));
    }
    let store = SqliteStore::open(dir).map_err(|e| e.to_string())?;
    let seed = store
        .meta(META_SEED)
        .map_err(|e| e.to_string())?
        .and_then(|s| Url::parse(&s).ok())
        .ok_or("the saved copy has no start URL")?;
    let options = match store.meta(META_OPTIONS).map_err(|e| e.to_string())? {
        Some(json) => {
            serde_json::from_str(&json).map_err(|e| format!("saved options are unreadable: {e}"))?
        }
        None => CrawlOptions::default(),
    };
    Ok((store, seed, options))
}

/// The requester named by `--requester`. `auto` combines wreq with the
/// browser, using the browser only for pages that need it.
fn build_requester(
    registry: &RequesterRegistry,
    options: &CrawlOptions,
    shared: &SharedResources,
) -> Result<Arc<dyn Requester>, String> {
    let build = |id: &str| {
        registry
            .build(id, &options.requester_config(id), shared)
            .map_err(|error| error.to_string())
    };
    if options.requester != AUTO_ID {
        return build(&options.requester);
    }
    let fast = build("wreq")?;
    let browser = build("playwright").map_err(|error| {
        format!("--requester auto needs a build with browser support ({error})")
    })?;
    Ok(Arc::new(HybridRequester::new(fast, browser)))
}

fn usage_error(message: &str) -> ExitCode {
    eprintln!("error: {message}");
    ExitCode::from(EXIT_USAGE)
}

struct Run {
    seed: Url,
    output: PathBuf,
    options: CrawlOptions,
    session: SessionArgs,
    quiet: bool,
}

impl Run {
    async fn execute(self, registry: &RequesterRegistry, store: Option<SqliteStore>) -> ExitCode {
        let output = self.output.clone();
        let (code, summary) = self.crawl(registry, store, false).await;
        if let Some(summary) = summary {
            if summary.stopped.is_some() {
                eprintln!("Run `mistercopy resume {}` to continue.", output.display());
            }
            println!(
                "Report: {} and {}",
                output.join(REPORT_HTML).display(),
                output.join(REPORT_JSON).display()
            );
        }
        code
    }

    async fn crawl(
        self,
        registry: &RequesterRegistry,
        store: Option<SqliteStore>,
        dry_run: bool,
    ) -> (ExitCode, Option<CrawlSummary>) {
        let mut config = match self
            .options
            .crawl_config(self.seed.clone(), self.output.clone())
        {
            Ok(config) => config,
            Err(message) => return (usage_error(&message), None),
        };
        config.dry_run = dry_run;
        let shared = self.options.shared_resources();
        if let Err(message) = self.session.apply(&mut config, &shared) {
            return (usage_error(&message), None);
        }
        let requester = match build_requester(registry, &self.options, &shared) {
            Ok(requester) => requester,
            Err(message) => return (usage_error(&message), None),
        };
        if self.options.ignore_robots {
            eprintln!(
                "warning: ignoring robots.txt; only do this for sites you are allowed to copy"
            );
        }

        let mut crawler = Crawler::new(config, requester);
        if let Some(store) = store {
            crawler = crawler.with_store(store);
        }
        let interrupts = tokio::spawn(handle_interrupts(crawler.stop_handle()));
        let printer = (!self.quiet).then(|| tokio::spawn(print_events(crawler.subscribe())));
        let result = crawler.run().await;
        interrupts.abort();
        if let Some(printer) = printer {
            // Ends once the crawler, and with it the event sender, is gone.
            let _ = printer.await;
        }

        match result {
            Ok(summary) => {
                print_summary(&summary, dry_run);
                let code = if summary.stopped.is_some() {
                    ExitCode::from(EXIT_STOPPED)
                } else if summary.failed.is_empty() {
                    ExitCode::SUCCESS
                } else {
                    ExitCode::FAILURE
                };
                (code, Some(summary))
            }
            Err(error) => (usage_error(&error.to_string()), None),
        }
    }
}

/// First Ctrl-C: finish the requests in flight, save progress, stop.
/// Second: quit at once.
async fn handle_interrupts(stop: StopHandle) {
    if tokio::signal::ctrl_c().await.is_ok() {
        eprintln!("\nStopping after the requests in flight; press Ctrl-C again to quit now.");
        stop.stop();
    }
    if tokio::signal::ctrl_c().await.is_ok() {
        std::process::exit(i32::from(EXIT_STOPPED));
    }
}

async fn print_events(mut events: broadcast::Receiver<CrawlEvent>) {
    loop {
        match events.recv().await {
            Ok(CrawlEvent::Saved { url, bytes, .. }) => {
                println!("saved   {url} ({})", human_bytes(bytes))
            }
            Ok(CrawlEvent::Unchanged { url }) => println!("same    {url}"),
            Ok(CrawlEvent::Failed { url, reason }) => eprintln!("failed  {url}: {reason}"),
            Ok(CrawlEvent::Retrying {
                url,
                attempt,
                delay,
                reason,
            }) => eprintln!(
                "retry   {url} in {:.1}s ({reason}, attempt {attempt})",
                delay.as_secs_f64()
            ),
            Ok(CrawlEvent::Stopping { reason }) => eprintln!("stopping: {reason}"),
            Ok(CrawlEvent::Rewriting { documents }) => {
                println!("rewriting links in {documents} documents")
            }
            Ok(CrawlEvent::Skipped { .. }) => {}
            Err(RecvError::Lagged(missed)) => eprintln!("({missed} progress messages dropped)"),
            Err(RecvError::Closed) => break,
        }
    }
}

fn print_summary(summary: &CrawlSummary, dry_run: bool) {
    let verb = if dry_run { "Checked" } else { "Copied" };
    let unchanged = if summary.unchanged > 0 {
        format!(" ({} unchanged)", summary.unchanged)
    } else {
        String::new()
    };
    println!(
        "{verb} {} files{unchanged}, {} in {:.1}s; {} failed, {} skipped.",
        summary.saved,
        human_bytes(summary.bytes),
        summary.elapsed.as_secs_f64(),
        summary.failed.len(),
        summary.skipped,
    );
    if let Some(reason) = summary.stopped {
        eprintln!("Stopped early: {reason}.");
    }
    if !summary.failed.is_empty() {
        eprintln!("Failures:");
        for (url, reason) in &summary.failed {
            eprintln!("  {url}: {reason}");
        }
    }
}
