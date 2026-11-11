mod copy;
mod options;

use std::process::ExitCode;

use clap::{Parser, Subcommand};
use mc_requester::{RequesterInfo, RequesterRegistry};
use mc_requester_wreq::WreqFactory;

#[derive(Parser)]
#[command(
    name = "mistercopy",
    version,
    about = "Copy websites for offline browsing"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
#[allow(
    clippy::large_enum_variant,
    reason = "parsed once per process; boxing buys nothing"
)]
enum Command {
    /// Copy a website into a local directory.
    Copy(copy::CopyArgs),
    /// Continue a copy that was stopped or interrupted.
    Resume(copy::DirArgs),
    /// Refresh a finished copy, downloading only what changed.
    Update(copy::DirArgs),
    /// Crawl and report broken links and assets without saving anything.
    Scan(copy::ScanArgs),
    /// Inspect the fetch backends included in this build.
    Requesters {
        #[command(subcommand)]
        command: RequestersCommand,
    },
    /// Manage the browser behind `--requester playwright` and `auto`.
    Browsers {
        #[command(subcommand)]
        command: BrowsersCommand,
    },
}

#[derive(Subcommand)]
enum BrowsersCommand {
    /// Show which Chromium builds were found, newest (the one used) first.
    Status,
    /// Download Chromium with Playwright's installer.
    Install,
}

#[derive(Subcommand)]
enum RequestersCommand {
    /// List registered requesters and their capabilities.
    List {
        /// Print JSON instead of a table.
        #[arg(long)]
        json: bool,
    },
}

/// Every requester this build includes. Playwright needs the `browser` feature.
fn registry() -> RequesterRegistry {
    let mut registry = RequesterRegistry::new();
    registry
        .register(WreqFactory)
        .expect("requester ids are unique");
    #[cfg(feature = "browser")]
    registry
        .register(mc_requester_playwright::PlaywrightFactory)
        .expect("requester ids are unique");
    registry
}

#[cfg(feature = "browser")]
async fn browsers(command: BrowsersCommand) -> ExitCode {
    match command {
        BrowsersCommand::Status => {
            let found = mc_requester_playwright::installed_chromium();
            if found.is_empty() {
                println!("No Chromium found. Run `mistercopy browsers install`.");
            }
            for path in found {
                println!("{}", path.display());
            }
            ExitCode::SUCCESS
        }
        BrowsersCommand::Install => match mc_requester_playwright::install_chromium().await {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("error: {error}");
                ExitCode::FAILURE
            }
        },
    }
}

#[cfg(not(feature = "browser"))]
async fn browsers(_: BrowsersCommand) -> ExitCode {
    eprintln!("error: this build has no browser support; rebuild with `--features browser`");
    ExitCode::from(2)
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Command::Copy(args) => copy::copy(args, &registry()).await,
        Command::Resume(args) => copy::resume(args, &registry()).await,
        Command::Update(args) => copy::update(args, &registry()).await,
        Command::Scan(args) => copy::scan(args, &registry()).await,
        Command::Requesters {
            command: RequestersCommand::List { json },
        } => list_requesters(&registry().list(), json),
        Command::Browsers { command } => browsers(command).await,
    }
}

fn list_requesters(requesters: &[RequesterInfo], json: bool) -> ExitCode {
    if json {
        return match serde_json::to_string_pretty(requesters) {
            Ok(text) => {
                println!("{text}");
                ExitCode::SUCCESS
            }
            Err(err) => {
                eprintln!("error: {err}");
                ExitCode::FAILURE
            }
        };
    }

    if requesters.is_empty() {
        println!("No requesters are registered in this build.");
        return ExitCode::SUCCESS;
    }

    let yes_no = |flag: bool| if flag { "yes" } else { "no" };
    println!(
        "{:<12} {:<4} {:<9} {:<12} {:<6} STREAMING",
        "ID", "JS", "CAPTURES", "IMPERSONATE", "HTTP2"
    );
    for info in requesters {
        let caps = info.capabilities;
        println!(
            "{:<12} {:<4} {:<9} {:<12} {:<6} {}",
            info.id,
            yes_no(caps.executes_js),
            yes_no(caps.captures_subresources),
            yes_no(caps.impersonation),
            yes_no(caps.http2),
            yes_no(caps.streaming_body),
        );
    }
    ExitCode::SUCCESS
}
