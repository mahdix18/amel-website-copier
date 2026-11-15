//! Proxies: which one, if any, carries a request. Requesters ask a
//! [`ProxyPicker`] per request, so a list of proxies can rotate and some
//! hosts can go direct.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use url::Url;

/// How long a proxy that couldn't be reached sits out.
pub const PROXY_COOLDOWN: Duration = Duration::from_secs(30);

/// Schemes a proxy URL may use.
pub const PROXY_SCHEMES: [&str; 5] = ["http", "https", "socks4", "socks5", "socks5h"];

/// How requests spread over several proxies.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProxyRotation {
    /// Each request takes the next proxy in turn.
    #[default]
    RoundRobin,
    /// Every request to one host uses the same proxy, so a site sees one
    /// address and sessions hold.
    Sticky,
}

impl std::str::FromStr for ProxyRotation {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text {
            "round-robin" => Ok(Self::RoundRobin),
            "sticky" => Ok(Self::Sticky),
            _ => Err(format!(
                "unknown rotation `{text}`; use round-robin or sticky"
            )),
        }
    }
}

/// The proxy settings, as configured.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProxyConfig {
    /// Proxy URLs, credentials in the user info (`http://user:pass@host:3128`).
    pub servers: Vec<Url>,
    pub rotation: ProxyRotation,
    /// Hosts that go direct: `example.com` (and its subdomains),
    /// `.example.com`, an IP address, a CIDR range such as `10.0.0.0/8`,
    /// or `*` for everything.
    pub bypass: Vec<String>,
    /// With no servers listed, use `HTTPS_PROXY`/`HTTP_PROXY`/`ALL_PROXY`
    /// and `NO_PROXY` from the environment.
    pub system: bool,
}

/// Picks the proxy for each request from a [`ProxyConfig`].
#[derive(Debug, Default)]
pub struct ProxyPicker {
    servers: Vec<Url>,
    /// Requests to `http://` URLs, when the environment names a separate
    /// `HTTP_PROXY`.
    http_servers: Vec<Url>,
    rotation: ProxyRotation,
    bypass: Vec<String>,
    next: AtomicUsize,
    /// Proxies that failed to connect, and when.
    benched: Mutex<HashMap<Url, Instant>>,
}

impl ProxyPicker {
    pub fn new(config: &ProxyConfig) -> Result<Self, String> {
        Self::with_env(config, |name| std::env::var(name).ok())
    }

    /// [`Self::new`] with the environment supplied, for tests.
    pub fn with_env(
        config: &ProxyConfig,
        env: impl Fn(&str) -> Option<String>,
    ) -> Result<Self, String> {
        let mut servers = config.servers.clone();
        let mut http_servers = Vec::new();
        let mut bypass = config.bypass.clone();
        if servers.is_empty() && config.system {
            let read = |names: &[&str]| {
                names
                    .iter()
                    .find_map(|name| env(name).filter(|v| !v.trim().is_empty()))
            };
            let parse = |value: String| {
                let value = value.trim().to_owned();
                // `HTTP_PROXY=host:3128` is common and means http://.
                let value = if value.contains("://") {
                    value
                } else {
                    format!("http://{value}")
                };
                Url::parse(&value).map_err(|_| format!("the system proxy `{value}` is not a URL"))
            };
            if let Some(value) = read(&["HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy"]) {
                servers.push(parse(value)?);
            }
            if let Some(value) = read(&["HTTP_PROXY", "http_proxy"]) {
                let proxy = parse(value)?;
                if servers.is_empty() {
                    servers.push(proxy);
                } else {
                    http_servers.push(proxy);
                }
            }
            if let Some(value) = read(&["NO_PROXY", "no_proxy"]) {
                bypass.extend(value.split(',').map(str::to_owned));
            }
        }
        for proxy in servers.iter().chain(&http_servers) {
            check(proxy)?;
        }
        let bypass = bypass
            .iter()
            .map(|pattern| pattern.trim().to_ascii_lowercase())
            .filter(|pattern| !pattern.is_empty())
            .collect();
        Ok(Self {
            servers,
            http_servers,
            rotation: config.rotation,
            bypass,
            next: AtomicUsize::new(0),
            benched: Mutex::new(HashMap::new()),
        })
    }

    pub fn is_empty(&self) -> bool {
        self.servers.is_empty()
    }

    /// Every proxy this picker may return.
    pub fn servers(&self) -> &[Url] {
        &self.servers
    }

    /// The bypass patterns, normalized.
    pub fn bypass(&self) -> &[String] {
        &self.bypass
    }

    /// The proxy for a request to `url`, or `None` to go direct.
    pub fn pick(&self, url: &Url) -> Option<&Url> {
        let host = url.host_str()?;
        if self.bypasses(host) {
            return None;
        }
        let list = if url.scheme() == "http" && !self.http_servers.is_empty() {
            &self.http_servers
        } else {
            &self.servers
        };
        if list.is_empty() {
            return None;
        }
        // Benched proxies sit out, unless every one of them is benched.
        let benched = self
            .benched
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let healthy: Vec<&Url> = list
            .iter()
            .filter(|proxy| {
                benched
                    .get(*proxy)
                    .is_none_or(|since| since.elapsed() >= PROXY_COOLDOWN)
            })
            .collect();
        let pool: Vec<&Url> = if healthy.is_empty() {
            list.iter().collect()
        } else {
            healthy
        };
        let index = match self.rotation {
            ProxyRotation::RoundRobin => self.next.fetch_add(1, Ordering::Relaxed),
            ProxyRotation::Sticky => fnv(host.as_bytes()) as usize,
        };
        Some(pool[index % pool.len()])
    }

    /// Takes `proxy` out of rotation for [`PROXY_COOLDOWN`] after it
    /// couldn't be reached.
    pub fn bench(&self, proxy: &Url) {
        self.benched
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(proxy.clone(), Instant::now());
    }

    pub fn bypasses(&self, host: &str) -> bool {
        let host = host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .to_ascii_lowercase();
        let ip: Option<IpAddr> = host.parse().ok();
        self.bypass.iter().any(|pattern| {
            if pattern == "*" {
                return true;
            }
            if let (Some(ip), Some((network, bits))) = (ip, pattern.split_once('/')) {
                return in_cidr(ip, network, bits);
            }
            let name = pattern.trim_start_matches("*.").trim_start_matches('.');
            host == name || host.ends_with(&format!(".{name}"))
        })
    }
}

/// Rejects a proxy URL the requesters can't use.
pub fn check(proxy: &Url) -> Result<(), String> {
    if !PROXY_SCHEMES.contains(&proxy.scheme()) {
        return Err(format!(
            "proxy `{proxy}` must start with one of {}",
            PROXY_SCHEMES.map(|s| format!("{s}://")).join(", ")
        ));
    }
    if proxy.host_str().is_none() {
        return Err(format!("proxy `{proxy}` has no host"));
    }
    Ok(())
}

/// Reads a proxy list: one URL per line; blank lines and `#` comments are
/// skipped, and `host:port` means `http://host:port`.
pub fn parse_proxy_list(text: &str) -> Result<Vec<Url>, String> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(|line| {
            let full = if line.contains("://") {
                line.to_owned()
            } else {
                format!("http://{line}")
            };
            let url = Url::parse(&full).map_err(|_| format!("`{line}` is not a proxy URL"))?;
            check(&url)?;
            Ok(url)
        })
        .collect()
}

/// `proxy` with its user info removed, for logs and for requesters that
/// take credentials separately.
pub fn without_credentials(proxy: &Url) -> Url {
    let mut bare = proxy.clone();
    let _ = bare.set_username("");
    let _ = bare.set_password(None);
    bare
}

fn in_cidr(ip: IpAddr, network: &str, bits: &str) -> bool {
    let (Ok(network), Ok(bits)) = (network.parse::<IpAddr>(), bits.parse::<u32>()) else {
        return false;
    };
    match (ip, network) {
        (IpAddr::V4(ip), IpAddr::V4(net)) if bits <= 32 => {
            let mask = u32::MAX.checked_shl(32 - bits).unwrap_or(0);
            u32::from(ip) & mask == u32::from(net) & mask
        }
        (IpAddr::V6(ip), IpAddr::V6(net)) if bits <= 128 => {
            let mask = u128::MAX.checked_shl(128 - bits).unwrap_or(0);
            u128::from(ip) & mask == u128::from(net) & mask
        }
        _ => false,
    }
}

fn fnv(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    fn config(servers: &[&str]) -> ProxyConfig {
        ProxyConfig {
            servers: servers.iter().map(|s| url(s)).collect(),
            ..ProxyConfig::default()
        }
    }

    #[test]
    fn rotates_round_robin_and_sticky() {
        let picker = ProxyPicker::new(&config(&["http://a:1", "http://b:1"])).unwrap();
        let page = url("https://s.test/");
        let picks: Vec<_> = (0..4)
            .map(|_| picker.pick(&page).unwrap().host_str().unwrap().to_owned())
            .collect();
        assert_eq!(picks, ["a", "b", "a", "b"]);

        let mut sticky = config(&["http://a:1", "http://b:1", "http://c:1"]);
        sticky.rotation = ProxyRotation::Sticky;
        let picker = ProxyPicker::new(&sticky).unwrap();
        let first = picker.pick(&url("https://s.test/x")).cloned();
        for path in ["/y", "/z", "/w"] {
            let again = picker.pick(&url(&format!("https://s.test{path}"))).cloned();
            assert_eq!(again, first, "one host keeps its proxy");
        }
    }

    #[test]
    fn benched_proxies_sit_out_unless_all_are_benched() {
        let picker = ProxyPicker::new(&config(&["http://a:1", "http://b:1"])).unwrap();
        let page = url("https://s.test/");
        picker.bench(&url("http://a:1"));
        for _ in 0..4 {
            assert_eq!(picker.pick(&page).unwrap().host_str(), Some("b"));
        }
        picker.bench(&url("http://b:1"));
        let hosts: std::collections::HashSet<_> = (0..4)
            .map(|_| picker.pick(&page).unwrap().host_str().unwrap().to_owned())
            .collect();
        assert_eq!(hosts.len(), 2, "with all benched, all are tried again");
    }

    #[test]
    fn bypasses_hosts_suffixes_and_ranges() {
        let mut cfg = config(&["socks5h://p:1080"]);
        cfg.bypass = vec![
            "Example.com".to_owned(),
            ".corp.test".to_owned(),
            "10.0.0.0/8".to_owned(),
            "::1/128".to_owned(),
        ];
        let picker = ProxyPicker::new(&cfg).unwrap();
        for direct in [
            "https://example.com/",
            "https://www.example.com/",
            "https://a.corp.test/",
            "http://10.1.2.3/",
            "http://[::1]:8080/",
        ] {
            assert!(picker.pick(&url(direct)).is_none(), "{direct}");
        }
        for proxied in [
            "https://notexample.com/",
            "https://corp.test.evil/",
            "http://11.0.0.1/",
        ] {
            assert!(picker.pick(&url(proxied)).is_some(), "{proxied}");
        }
        cfg.bypass = vec!["*".to_owned()];
        assert!(
            ProxyPicker::new(&cfg)
                .unwrap()
                .pick(&url("https://x.test/"))
                .is_none()
        );
    }

    #[test]
    fn reads_the_environment_only_when_asked() {
        let env = |name: &str| match name {
            "HTTPS_PROXY" => Some("http://secure:3128".to_owned()),
            "http_proxy" => Some("plain:8080".to_owned()),
            "NO_PROXY" => Some("localhost, .internal".to_owned()),
            _ => None,
        };
        let off = ProxyPicker::with_env(&ProxyConfig::default(), env).unwrap();
        assert!(off.is_empty());

        let system = ProxyConfig {
            system: true,
            ..ProxyConfig::default()
        };
        let picker = ProxyPicker::with_env(&system, env).unwrap();
        assert_eq!(
            picker.pick(&url("https://s.test/")).unwrap().host_str(),
            Some("secure")
        );
        assert_eq!(
            picker.pick(&url("http://s.test/")).unwrap().as_str(),
            "http://plain:8080/"
        );
        assert!(picker.pick(&url("http://localhost:3000/")).is_none());
        assert!(picker.pick(&url("https://a.internal/")).is_none());

        // Servers given explicitly win over the environment.
        let explicit = ProxyConfig {
            system: true,
            ..config(&["http://mine:1"])
        };
        let picker = ProxyPicker::with_env(&explicit, env).unwrap();
        assert_eq!(picker.servers().len(), 1);
        assert!(picker.bypass().is_empty());
    }

    #[test]
    fn parses_lists_and_rejects_bad_proxies() {
        let list = parse_proxy_list("# office\nhttp://u:p@a:3128\n\n  b:8080 \nsocks5://c:1080\n")
            .unwrap();
        assert_eq!(
            list.iter().map(Url::as_str).collect::<Vec<_>>(),
            ["http://u:p@a:3128/", "http://b:8080/", "socks5://c:1080"]
        );
        assert!(
            parse_proxy_list("ftp://x:21")
                .unwrap_err()
                .contains("must start with")
        );
        assert!(ProxyPicker::new(&config(&["ftp://x:21"])).is_err());
        assert_eq!(
            without_credentials(&url("http://u:p@a:3128/")).as_str(),
            "http://a:3128/"
        );
    }
}
