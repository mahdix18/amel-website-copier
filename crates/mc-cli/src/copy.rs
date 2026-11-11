use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use mc_core::{CrawlConfig, CrawlEvent, CrawlSummary, Crawler, Rule, RuleAction};
use mc_requester::{RequesterRegistry, SharedResources, Url};
use serde_json::{Map, Value};
use tokio::sync::broadcast::{self, error::RecvError};

/// Exit code for an invalid command line or configuration.
const EXIT_USAGE: u8 = 2;

#[derive(clap::Args)]
pub struct CopyArgs {
    /// The page to start from.
    url: Url,

    /// Directory to write into. The copy lands in a subdirectory named
    /// after the host.
    #[arg(short, long, default_value = ".")]
    output: PathBuf,

    /// Requester to fetch with (see `mistercopy requesters list`).
    #[arg(long, default_value = "wreq")]
    requester: String,

    /// Browser profile to present, e.g. `chrome_149`, `firefox_151`, `safari_26.4`.
    #[arg(long)]
    profile: Option<String>,

    /// Platform to present: windows, macos, linux, android or ios.
    #[arg(long)]
    platform: Option<String>,

    /// Requests in flight at once.
    #[arg(long, default_value_t = 4)]
    concurrency: usize,

    /// Minimum milliseconds between two requests to the same host.
    #[arg(long, default_value_t = 1000)]
    delay_ms: u64,

    /// Don't follow page links deeper than this from the start page.
    #[arg(long)]
    max_depth: Option<u32>,

    /// Stop after fetching this many URLs.
    #[arg(long)]
    max_urls: Option<usize>,

    /// Skip URLs matching this regex. Checked before --download-only and --include.
    #[arg(long, value_name = "REGEX")]
    exclude: Vec<String>,

    /// Save URLs matching this regex without following their page links.
    #[arg(long, value_name = "REGEX")]
    download_only: Vec<String>,

    /// Fetch URLs matching this regex even outside the site.
    #[arg(long, value_name = "REGEX")]
    include: Vec<String>,

    /// Another host that belongs to the same site, e.g. `www.example.com`.
    #[arg(long = "alias", value_name = "HOST")]
    aliases: Vec<String>,

    /// Also fetch images, stylesheets and scripts hosted elsewhere.
    #[arg(long)]
    external_assets: bool,

    /// Seconds before a request is abandoned.
    #[arg(long, default_value_t = 30)]
    timeout_secs: u64,

    /// Proxy for all requests, e.g. `socks5://127.0.0.1:1080`.
    #[arg(long)]
    proxy: Option<Url>,

    /// Print only the summary and errors.
    #[arg(short, long)]
    quiet: bool,
}

pub async fn run(args: CopyArgs, registry: &RequesterRegistry) -> ExitCode {
    let rules = match rules(&args) {
        Ok(rules) => rules,
        Err(message) => return usage_error(&message),
    };
    let shared = SharedResources {
        default_timeout: Duration::from_secs(args.timeout_secs),
        proxy: args.proxy.clone(),
    };
    let requester = match registry.build(&args.requester, &requester_config(&args), &shared) {
        Ok(requester) => requester,
        Err(error) => return usage_error(&error.to_string()),
    };

    let mut config = CrawlConfig::new(args.url.clone(), &args.output);
    config.domain_aliases = args.aliases.clone();
    config.rules = rules;
    config.include_external_assets = args.external_assets;
    config.max_depth = args.max_depth;
    config.max_urls = args.max_urls;
    config.concurrency = args.concurrency;
    config.per_host_delay = Duration::from_millis(args.delay_ms);

    let crawler = Crawler::new(config, requester);
    let printer = (!args.quiet).then(|| tokio::spawn(print_events(crawler.subscribe())));
    let result = crawler.run().await;
    if let Some(printer) = printer {
        // Ends once the crawler, and with it the event sender, is gone.
        let _ = printer.await;
    }

    match result {
        Ok(summary) => {
            print_summary(&summary, &args.output);
            if summary.failed.is_empty() {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
        Err(error) => usage_error(&error.to_string()),
    }
}

fn usage_error(message: &str) -> ExitCode {
    eprintln!("error: {message}");
    ExitCode::from(EXIT_USAGE)
}

fn rules(args: &CopyArgs) -> Result<Vec<Rule>, String> {
    let groups = [
        (&args.exclude, RuleAction::Exclude),
        (&args.download_only, RuleAction::DownloadOnly),
        (&args.include, RuleAction::Include),
    ];
    let mut rules = Vec::new();
    for (patterns, action) in groups {
        for pattern in patterns {
            let rule = Rule::new(pattern, action)
                .map_err(|e| format!("invalid regex `{pattern}`: {e}"))?;
            rules.push(rule);
        }
    }
    Ok(rules)
}

/// Only the options the user set, so requesters that don't know them only
/// complain when they're actually used.
fn requester_config(args: &CopyArgs) -> Value {
    let mut config = Map::new();
    if let Some(profile) = &args.profile {
        config.insert("profile".to_owned(), Value::String(profile.clone()));
    }
    if let Some(platform) = &args.platform {
        config.insert("platform".to_owned(), Value::String(platform.clone()));
    }
    Value::Object(config)
}

async fn print_events(mut events: broadcast::Receiver<CrawlEvent>) {
    loop {
        match events.recv().await {
            Ok(CrawlEvent::Saved { url, bytes, .. }) => {
                println!("saved   {url} ({})", human_bytes(bytes))
            }
            Ok(CrawlEvent::Failed { url, reason }) => eprintln!("failed  {url}: {reason}"),
            Ok(CrawlEvent::Rewriting { documents }) => {
                println!("rewriting links in {documents} documents")
            }
            Ok(CrawlEvent::Skipped { .. }) => {}
            Err(RecvError::Lagged(missed)) => eprintln!("({missed} progress messages dropped)"),
            Err(RecvError::Closed) => break,
        }
    }
}

fn print_summary(summary: &CrawlSummary, output: &std::path::Path) {
    println!(
        "Copied {} files ({}) into {} in {:.1}s; {} failed, {} skipped.",
        summary.saved,
        human_bytes(summary.bytes),
        output.display(),
        summary.elapsed.as_secs_f64(),
        summary.failed.len(),
        summary.skipped,
    );
    if !summary.failed.is_empty() {
        eprintln!("Failures:");
        for (url, reason) in &summary.failed {
            eprintln!("  {url}: {reason}");
        }
    }
}

fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}
