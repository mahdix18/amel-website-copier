use std::net::SocketAddr;

use clap::Parser;
use mc_fixtures::{FixtureServer, Site};

/// Serve a MisterCopy test site on localhost.
#[derive(Parser)]
#[command(name = "mc-fixtures")]
struct Args {
    /// Site to serve, e.g. `static-blog`.
    site: Site,
    #[arg(long, default_value_t = 8000)]
    port: u16,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let server =
        FixtureServer::bind(args.site, SocketAddr::from(([127, 0, 0, 1], args.port))).await?;
    println!("Serving {} at {}", args.site, server.base_url());
    println!("Press Ctrl-C to stop.");
    tokio::signal::ctrl_c().await?;
    server.stop().await?;
    Ok(())
}
