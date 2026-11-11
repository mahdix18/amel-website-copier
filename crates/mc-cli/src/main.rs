use std::process::ExitCode;

use clap::{Parser, Subcommand};
use mc_requester::{RequesterInfo, RequesterRegistry};

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
enum Command {
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

/// Requester crates register here as they land: wreq in M1, Playwright in M3.
fn registry() -> RequesterRegistry {
    RequesterRegistry::new()
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
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
