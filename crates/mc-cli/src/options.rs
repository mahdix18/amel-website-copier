//! Command-line options. Crawl options are saved with a copy so `resume`
//! and `update` repeat them; session options carry secrets and are not.

use std::path::PathBuf;
use std::time::Duration;

use clap::Parser;
use mc_core::{CrawlConfig, Credential, Rule, RuleAction};
use mc_requester::{CookieJar, HeaderMap, SharedResources, Url};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

const MIB: u64 = 1024 * 1024;

/// Environment variable read for `--auth HOST=USER` without a password.
pub const PASSWORD_ENV: &str = "MISTERCOPY_PASSWORD";

#[derive(clap::Args, Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CrawlOptions {
    /// Requester to fetch with (see `mistercopy requesters list`).
    #[arg(long, default_value = "wreq")]
    pub requester: String,

    /// Browser profile to present, e.g. `chrome_149`, `firefox_151`, `safari_26.4`.
    #[arg(long)]
    pub profile: Option<String>,

    /// Platform to present: windows, macos, linux, android or ios.
    #[arg(long)]
    pub platform: Option<String>,

    /// Requests in flight at once.
    #[arg(long, default_value_t = 4)]
    pub concurrency: usize,

    /// Minimum milliseconds between two requests to the same host.
    #[arg(long, default_value_t = 1000)]
    pub delay_ms: u64,

    /// Retries for timeouts, dropped connections and 429/5xx answers.
    #[arg(long, default_value_t = 3)]
    pub retries: u32,

    /// Seconds before a request is abandoned.
    #[arg(long, default_value_t = 30)]
    pub timeout_secs: u64,

    /// Don't follow page links deeper than this from the start page.
    #[arg(long)]
    pub max_depth: Option<u32>,

    /// Stop after fetching this many URLs.
    #[arg(long)]
    pub max_urls: Option<usize>,

    /// Stop, resumably, after downloading this many MiB in total.
    #[arg(long, value_name = "MIB")]
    pub max_size_mb: Option<u64>,

    /// Skip any single response larger than this many MiB.
    #[arg(long, value_name = "MIB", default_value_t = 512)]
    pub max_file_mb: u64,

    /// Stop, resumably, after this many seconds.
    #[arg(long, value_name = "SECS")]
    pub max_time_secs: Option<u64>,

    /// Skip URLs matching this regex. Checked before --download-only and --include.
    #[arg(long, value_name = "REGEX")]
    pub exclude: Vec<String>,

    /// Save URLs matching this regex without following their page links.
    #[arg(long, value_name = "REGEX")]
    pub download_only: Vec<String>,

    /// Fetch URLs matching this regex even outside the site.
    #[arg(long, value_name = "REGEX")]
    pub include: Vec<String>,

    /// Another host that belongs to the same site, e.g. `www.example.com`.
    #[arg(long = "alias", value_name = "HOST")]
    pub aliases: Vec<String>,

    /// Also fetch images, stylesheets and scripts hosted elsewhere.
    #[arg(long)]
    pub external_assets: bool,

    /// Proxy for all requests, e.g. `socks5://127.0.0.1:1080`.
    #[arg(long)]
    pub proxy: Option<Url>,

    /// Fetch what robots.txt disallows. Only for sites you are allowed to copy.
    #[arg(long)]
    pub ignore_robots: bool,

    /// Chromium for `--requester playwright` or `auto`. Defaults to the
    /// newest one Playwright installed (see `mistercopy browsers status`).
    #[arg(long, value_name = "PATH")]
    pub browser_path: Option<PathBuf>,

    /// Keep `<script>` elements in pages rendered by the browser.
    #[arg(long)]
    pub keep_scripts: bool,

    /// Don't scroll rendered pages to trigger lazy loading.
    #[arg(long)]
    pub no_scroll: bool,
}

impl Default for CrawlOptions {
    /// The command-line defaults, so saved options from older versions fill
    /// in missing fields the same way.
    fn default() -> Self {
        #[derive(Parser)]
        struct Defaults {
            #[command(flatten)]
            options: CrawlOptions,
        }
        Defaults::parse_from(["mistercopy"]).options
    }
}

impl CrawlOptions {
    pub fn crawl_config(&self, seed: Url, output: PathBuf) -> Result<CrawlConfig, String> {
        let mut config = CrawlConfig::new(seed, output);
        config.domain_aliases = self.aliases.clone();
        config.rules = self.rules()?;
        config.include_external_assets = self.external_assets;
        config.max_depth = self.max_depth;
        config.max_urls = self.max_urls;
        config.max_total_bytes = self.max_size_mb.map(|mb| mb.saturating_mul(MIB));
        config.max_duration = self.max_time_secs.map(Duration::from_secs);
        config.concurrency = self.concurrency;
        config.per_host_delay = Duration::from_millis(self.delay_ms);
        config.max_retries = self.retries;
        config.respect_robots = !self.ignore_robots;
        Ok(config)
    }

    pub fn shared_resources(&self) -> SharedResources {
        SharedResources {
            default_timeout: Duration::from_secs(self.timeout_secs),
            proxy: self.proxy.clone(),
            max_body_bytes: self.max_file_mb.saturating_mul(MIB),
            ..SharedResources::default()
        }
    }

    /// The config block for requester `id`: only the options that requester
    /// understands, and only those the user set.
    pub fn requester_config(&self, id: &str) -> Value {
        let mut config = Map::new();
        let mut set = |key: &str, value: Value| {
            config.insert(key.to_owned(), value);
        };
        match id {
            "wreq" => {
                if let Some(profile) = &self.profile {
                    set("profile", Value::String(profile.clone()));
                }
                if let Some(platform) = &self.platform {
                    set("platform", Value::String(platform.clone()));
                }
            }
            "playwright" => {
                if let Some(path) = &self.browser_path {
                    set(
                        "executable_path",
                        Value::String(path.to_string_lossy().into_owned()),
                    );
                }
                if self.keep_scripts {
                    set("keep_scripts", Value::Bool(true));
                }
                if self.no_scroll {
                    set("scroll", Value::Bool(false));
                }
            }
            _ => {}
        }
        Value::Object(config)
    }

    fn rules(&self) -> Result<Vec<Rule>, String> {
        let groups = [
            (&self.exclude, RuleAction::Exclude),
            (&self.download_only, RuleAction::DownloadOnly),
            (&self.include, RuleAction::Include),
        ];
        let mut rules = Vec::new();
        for (patterns, action) in groups {
            for pattern in patterns {
                rules.push(
                    Rule::new(pattern, action)
                        .map_err(|e| format!("invalid regex `{pattern}`: {e}"))?,
                );
            }
        }
        Ok(rules)
    }
}

/// Options that carry credentials. Never written to disk; pass them again
/// to `resume` and `update`.
#[derive(clap::Args, Debug, Clone, Default)]
pub struct SessionArgs {
    /// HTTP Basic credentials: `HOST=USER:PASSWORD`, or `HOST=USER` to read
    /// the password from $MISTERCOPY_PASSWORD.
    #[arg(long, value_name = "HOST=USER[:PASSWORD]")]
    pub auth: Vec<String>,

    /// Extra request header for the site's own hosts, e.g. `"Authorization: Bearer …"`.
    #[arg(long = "header", value_name = "NAME: VALUE")]
    pub headers: Vec<String>,

    /// Netscape cookies.txt to start the session with.
    #[arg(long, value_name = "FILE")]
    pub cookies: Option<PathBuf>,
}

impl SessionArgs {
    pub fn apply(&self, config: &mut CrawlConfig, shared: &SharedResources) -> Result<(), String> {
        config.credentials = self
            .auth
            .iter()
            .map(|spec| parse_credential(spec, std::env::var(PASSWORD_ENV).ok()))
            .collect::<Result<_, _>>()?;
        config.headers = parse_headers(&self.headers)?;
        if let Some(path) = &self.cookies {
            let text = std::fs::read_to_string(path)
                .map_err(|e| format!("cannot read cookies file {}: {e}", path.display()))?;
            import_cookies(&shared.cookies, &text)?;
        }
        Ok(())
    }
}

fn parse_credential(spec: &str, env_password: Option<String>) -> Result<Credential, String> {
    let usage = || format!("--auth expects HOST=USER:PASSWORD or HOST=USER, got `{spec}`");
    let (host, user_pass) = spec.split_once('=').ok_or_else(usage)?;
    let (username, password) = match user_pass.split_once(':') {
        Some((user, pass)) => (user.to_owned(), pass.to_owned()),
        None => (
            user_pass.to_owned(),
            env_password.ok_or_else(|| {
                format!("--auth {spec} has no password and ${PASSWORD_ENV} is not set")
            })?,
        ),
    };
    if host.is_empty() || username.is_empty() {
        return Err(usage());
    }
    Ok(Credential {
        host: host.to_ascii_lowercase(),
        username,
        password,
    })
}

fn parse_headers(specs: &[String]) -> Result<HeaderMap, String> {
    let mut headers = HeaderMap::new();
    for spec in specs {
        let (name, value) = spec
            .split_once(':')
            .ok_or_else(|| format!("--header expects `Name: value`, got `{spec}`"))?;
        let name: mc_requester::header::HeaderName = name
            .trim()
            .parse()
            .map_err(|_| format!("invalid header name in `{spec}`"))?;
        let value = value
            .trim()
            .parse()
            .map_err(|_| format!("invalid header value in `{spec}`"))?;
        headers.append(name, value);
    }
    Ok(headers)
}

fn import_cookies(jar: &CookieJar, text: &str) -> Result<(), String> {
    jar.import_netscape(text)
        .map(drop)
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_command_line() {
        let options = CrawlOptions::default();
        assert_eq!(options.requester, "wreq");
        assert_eq!(options.concurrency, 4);
        assert_eq!(options.max_file_mb, 512);
        let restored: CrawlOptions = serde_json::from_str(r#"{"concurrency": 9}"#).unwrap();
        assert_eq!(restored.concurrency, 9);
        assert_eq!(restored.delay_ms, 1000);
    }

    #[test]
    fn parses_credentials() {
        let c = parse_credential("Site.test=ann:p:w", None).unwrap();
        assert_eq!(
            (c.host.as_str(), c.username.as_str(), c.password.as_str()),
            ("site.test", "ann", "p:w")
        );
        let c = parse_credential("site.test=ann", Some("secret".into())).unwrap();
        assert_eq!(c.password, "secret");
        assert!(
            parse_credential("site.test=ann", None)
                .unwrap_err()
                .contains(PASSWORD_ENV)
        );
        assert!(parse_credential("no-equals", None).is_err());
        assert!(parse_credential("=ann:pw", None).is_err());
    }

    #[test]
    fn parses_headers() {
        let headers =
            parse_headers(&["X-Token: abc".into(), "Accept-Language:  fr ".into()]).unwrap();
        assert_eq!(headers["x-token"], "abc");
        assert_eq!(headers["accept-language"], "fr");
        assert!(parse_headers(&["nonsense".into()]).is_err());
        assert!(parse_headers(&["bad name: x".into()]).is_err());
    }
}
