mod copy;

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
    /// Inspect the fetch backends included in this build.
    Requesters {
        #[command(subcommand)]
        command: RequestersCommand,
    },
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

/// Requester crates register here as they land: Playwright follows in M3.
fn registry() -> RequesterRegistry {
    let mut registry = RequesterRegistry::new();
    registry
        .register(WreqFactory)
        .expect("requester ids are unique");
    registry
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Command::Copy(args) => copy::run(args, &registry()).await,
        Command::Requesters {
            command: RequestersCommand::List { json },
        } => list_requesters(&registry().list(), json),
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
