mod copy;

use std::process::ExitCode;

use clap::{Parser, Subcommand};
use mc_cli::archive;
use mc_cli::requesters::registry;
use mc_requester::RequesterInfo;

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
    /// Log in by hand in a browser window and save the session for `--cookies`.
    Login(LoginArgs),
    /// Serve a copy on localhost, for sites that need http rather than file://.
    Serve {
        /// The output directory of a copy.
        #[arg(default_value = ".")]
        dir: std::path::PathBuf,
        #[arg(long, default_value_t = 8000)]
        port: u16,
    },
}

#[derive(clap::Args)]
struct LoginArgs {
    /// The site's login page.
    url: mc_requester::Url,
    /// Where to save the session cookies (readable only by you).
    #[arg(long, default_value = "cookies.txt")]
    save: std::path::PathBuf,
    /// Chromium to open; defaults to the newest one Playwright installed.
    #[arg(long, value_name = "PATH")]
    browser_path: Option<std::path::PathBuf>,
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
    /// List the browser profiles `--profile` can impersonate, and the
    /// platforms `--platform` accepts.
    Profiles,
}

/// One line per browser: its profiles, oldest first.
fn list_profiles() -> ExitCode {
    let default = mc_requester_wreq::default_profile();
    let mut families: Vec<(String, Vec<String>)> = Vec::new();
    for name in mc_requester_wreq::profiles() {
        let family = name.split('_').next().unwrap_or_default().to_owned();
        match families.iter_mut().find(|(f, _)| *f == family) {
            Some((_, names)) => names.push(name),
            None => families.push((family, vec![name])),
        }
    }
    for (family, names) in families {
        println!("{family:<8} {}", names.join(" "));
    }
    println!(
        "\nDefault: {default}. Platforms: {}.",
        mc_requester_wreq::PLATFORMS.join(", ")
    );
    ExitCode::SUCCESS
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

#[cfg(feature = "browser")]
async fn login(args: LoginArgs) -> ExitCode {
    match mc_requester_playwright::interactive_login(&args.url, args.browser_path, &args.save).await
    {
        Ok(count) => {
            println!(
                "Saved {count} cookies to {}. Pass `--cookies {}` to copy as this user; the file holds a live session, so keep it private.",
                args.save.display(),
                args.save.display()
            );
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(not(feature = "browser"))]
async fn browsers(_: BrowsersCommand) -> ExitCode {
    no_browser_support()
}

#[cfg(not(feature = "browser"))]
async fn login(_: LoginArgs) -> ExitCode {
    no_browser_support()
}

#[cfg(not(feature = "browser"))]
fn no_browser_support() -> ExitCode {
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
        Command::Requesters {
            command: RequestersCommand::Profiles,
        } => list_profiles(),
        Command::Browsers { command } => browsers(command).await,
        Command::Login(args) => login(args).await,
        Command::Serve { dir, port } => serve(&dir, port).await,
    }
}

async fn serve(dir: &std::path::Path, port: u16) -> ExitCode {
    let root = match archive::serve_root(dir) {
        Ok(root) => root,
        Err(error) => {
            eprintln!("error: cannot read {}: {error}", dir.display());
            return ExitCode::from(2);
        }
    };
    let listener = match tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("error: cannot listen on port {port}: {error}");
            return ExitCode::from(2);
        }
    };
    println!(
        "Serving {} at http://127.0.0.1:{port}/ (Ctrl-C to stop)",
        root.display()
    );
    let app = axum::Router::new().fallback_service(tower_http::services::ServeDir::new(root));
    let served = axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await;
    match served {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
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
