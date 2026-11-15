//! The default MisterCopy requester.
//!
//! [wreq] presents the TLS, HTTP/2 and header fingerprint of a real browser
//! profile, so sites that turn away non-browser clients still copy correctly.
//! It is there for compatibility; the crawler's politeness limits apply to it
//! like any other requester.

use std::collections::HashMap;
use std::pin::pin;
use std::sync::Arc;

use futures_util::StreamExt;
use mc_requester::{
    BuildError, Capabilities, CookieJar, FetchContext, FetchError, FetchRequest, FetchResponse,
    Redirect, Requester, RequesterFactory, SharedResources, Url, async_trait, header,
};
use mc_requester::{ProxyPicker, without_credentials};
use serde::Deserialize;
use wreq_util::{Emulation, Platform, Profile};

pub const ID: &str = "wreq";

/// The profile used when the project doesn't name one: the newest Chrome in
/// the pinned wreq-util release.
const DEFAULT_PROFILE: Profile = Profile::Chrome149;

/// The `wreq` block of a project file.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WreqConfig {
    /// A wreq-util profile name, such as `chrome_149`, `firefox_151` or `safari_26.4`.
    pub profile: Profile,
    /// `windows`, `macos`, `linux`, `android` or `ios`. Defaults to the profile's own.
    pub platform: Option<Platform>,
    pub max_redirects: usize,
    /// Overrides the crawl-wide response size limit for this requester.
    pub max_body_bytes: Option<u64>,
}

impl Default for WreqConfig {
    fn default() -> Self {
        Self {
            profile: DEFAULT_PROFILE,
            platform: None,
            max_redirects: 10,
            max_body_bytes: None,
        }
    }
}

/// Every browser profile this build can impersonate, by name (`chrome_149`,
/// `firefox_151`, `safari_26.4`, …), in wreq-util's order: oldest first
/// within each browser.
pub fn profiles() -> Vec<String> {
    Profile::VARIANTS
        .iter()
        .filter_map(|profile| match serde_json::to_value(profile) {
            Ok(serde_json::Value::String(name)) => Some(name),
            _ => None,
        })
        .collect()
}

/// The profile used when none is named.
pub fn default_profile() -> String {
    match serde_json::to_value(DEFAULT_PROFILE) {
        Ok(serde_json::Value::String(name)) => name,
        _ => "chrome_149".to_owned(),
    }
}

/// Platforms a profile can present.
pub const PLATFORMS: [&str; 5] = ["windows", "macos", "linux", "android", "ios"];

/// Checks a profile and platform name before a copy starts.
pub fn check_impersonation(profile: Option<&str>, platform: Option<&str>) -> Result<(), String> {
    if let Some(name) = profile
        && !profiles().iter().any(|known| known == name)
    {
        return Err(format!(
            "unknown browser profile `{name}`; `mistercopy requesters profiles` lists them"
        ));
    }
    if let Some(name) = platform
        && !PLATFORMS.contains(&name)
    {
        return Err(format!(
            "unknown platform `{name}`; use one of {}",
            PLATFORMS.join(", ")
        ));
    }
    Ok(())
}

const CAPABILITIES: Capabilities = Capabilities {
    executes_js: false,
    captures_subresources: false,
    impersonation: true,
    http2: true,
    streaming_body: false,
};

pub struct WreqFactory;

impl RequesterFactory for WreqFactory {
    fn id(&self) -> &'static str {
        ID
    }

    fn capabilities(&self) -> Capabilities {
        CAPABILITIES
    }

    fn build(
        &self,
        config: &serde_json::Value,
        shared: &SharedResources,
    ) -> Result<Arc<dyn Requester>, BuildError> {
        let config: WreqConfig = if config.is_null() {
            WreqConfig::default()
        } else {
            serde_json::from_value(config.clone())
                .map_err(|error| build_error(error.to_string()))?
        };
        Ok(Arc::new(WreqRequester::new(config, shared)?))
    }
}

fn build_error(message: String) -> BuildError {
    BuildError {
        requester: ID,
        message,
    }
}

pub struct WreqRequester {
    client: wreq::Client,
    proxies: ProxyPicker,
    wreq_proxies: HashMap<Url, wreq::Proxy>,
    config: WreqConfig,
    max_body_bytes: u64,
    cookies: Arc<CookieJar>,
}

impl WreqRequester {
    pub fn new(config: WreqConfig, shared: &SharedResources) -> Result<Self, BuildError> {
        let emulation = match config.platform {
            Some(platform) => Emulation::builder()
                .profile(config.profile)
                .platform(platform)
                .build(),
            None => Emulation::builder().profile(config.profile).build(),
        };
        let builder = wreq::Client::builder()
            .emulation(emulation)
            // Redirects are followed in `fetch` so every hop is recorded.
            .redirect(wreq::redirect::Policy::none())
            .timeout(shared.default_timeout);
        let proxies = ProxyPicker::new(&shared.proxy).map_err(build_error)?;
        // Each server is parsed once; requests pick one of them.
        let mut wreq_proxies = HashMap::new();
        for server in proxies.servers() {
            let proxy = wreq::Proxy::all(server.as_str()).map_err(|e| {
                build_error(format!(
                    "invalid proxy {}: {e}",
                    without_credentials(server)
                ))
            })?;
            wreq_proxies.insert(server.clone(), proxy);
        }
        let client = builder.build().map_err(|e| build_error(e.to_string()))?;
        Ok(Self {
            client,
            proxies,
            wreq_proxies,
            max_body_bytes: config.max_body_bytes.unwrap_or(shared.max_body_bytes),
            config,
            cookies: Arc::clone(&shared.cookies),
        })
    }
}

/// Headers that carry credentials and must not follow a redirect to
/// another origin.
const CREDENTIAL_HEADERS: [header::HeaderName; 3] = [
    header::AUTHORIZATION,
    header::PROXY_AUTHORIZATION,
    header::COOKIE,
];

#[async_trait]
impl Requester for WreqRequester {
    fn id(&self) -> &'static str {
        ID
    }

    fn capabilities(&self) -> Capabilities {
        CAPABILITIES
    }

    async fn fetch(
        &self,
        request: FetchRequest,
        context: &FetchContext,
    ) -> Result<FetchResponse, FetchError> {
        let mut url = request.url.clone();
        let mut redirects = Vec::new();
        let mut headers = request.headers.clone();

        loop {
            if !matches!(url.scheme(), "http" | "https") {
                return Err(FetchError::UnsupportedScheme(url.scheme().to_owned()));
            }
            let mut builder = self
                .client
                .request(request.method.clone(), url.as_str())
                .headers(headers.clone());
            if let Some(cookies) = self.cookies.header_for(&url)
                && !headers.contains_key(header::COOKIE)
            {
                builder = builder.header(header::COOKIE, cookies);
            }
            if let Some(timeout) = request.timeout {
                builder = builder.timeout(timeout);
            }

            if let Some(referrer) = &context.referrer
                && !request.headers.contains_key(header::REFERER)
            {
                builder = builder.header(header::REFERER, referrer.as_str());
            }
            // Validators only make sense for the URL they were stored for.
            if redirects.is_empty()
                && let Some(validators) = &request.validators
            {
                if let Some(etag) = &validators.etag {
                    builder = builder.header(header::IF_NONE_MATCH, etag.as_str());
                }
                if let Some(modified) = &validators.last_modified {
                    builder = builder.header(header::IF_MODIFIED_SINCE, modified.as_str());
                }
            }

            // The proxy is picked per hop: a redirect to a bypassed host goes direct.
            let response = self.send(builder, &url).await?;
            let status = response.status();
            self.cookies.store_from(&url, response.headers());

            if status.is_redirection() && status != mc_requester::StatusCode::NOT_MODIFIED {
                let location = response
                    .headers()
                    .get(header::LOCATION)
                    .and_then(|value| value.to_str().ok())
                    .and_then(|value| url.join(value).ok());
                if let Some(next) = location {
                    if redirects.len() >= self.config.max_redirects {
                        return Err(FetchError::Other(
                            format!("more than {} redirects", self.config.max_redirects).into(),
                        ));
                    }
                    if next.origin() != url.origin() {
                        for name in &CREDENTIAL_HEADERS {
                            headers.remove(name);
                        }
                    }
                    redirects.push(Redirect {
                        from: url.clone(),
                        to: next.clone(),
                        status,
                    });
                    url = next;
                    continue;
                }
            }

            let headers = response.headers().clone();
            let limit = self.max_body_bytes;
            if response.content_length().is_some_and(|len| len > limit) {
                return Err(FetchError::TooLarge { limit });
            }
            let mut body = Vec::new();
            let mut stream = pin!(response.bytes_stream());
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.map_err(fetch_error)?;
                if body.len() as u64 + chunk.len() as u64 > limit {
                    return Err(FetchError::TooLarge { limit });
                }
                body.extend_from_slice(&chunk);
            }

            let mut fetched =
                FetchResponse::new(Url::clone(&url), status, headers, body_from(body));
            fetched.redirects = redirects;
            return Ok(fetched);
        }
    }
}

fn body_from(bytes: Vec<u8>) -> mc_requester::Body {
    mc_requester::Body::Bytes(bytes.into())
}

impl WreqRequester {
    /// Sends through the picked proxy. A proxy that can't be reached is
    /// benched and the request moves to the next, so one dead proxy in a
    /// list doesn't fail its share of the requests.
    async fn send(
        &self,
        builder: wreq::RequestBuilder,
        url: &Url,
    ) -> Result<wreq::Response, FetchError> {
        let mut tries = self.proxies.servers().len();
        let mut next = Some(builder);
        while let Some(current) = next.take() {
            let Some(server) = self.proxies.pick(url).cloned() else {
                return current.send().await.map_err(fetch_error);
            };
            tries = tries.saturating_sub(1);
            if tries > 0 {
                next = current.try_clone();
            }
            let attempt = match self.wreq_proxies.get(&server) {
                Some(proxy) => current.proxy(proxy.clone()),
                None => current,
            };
            match attempt.send().await {
                Ok(response) => return Ok(response),
                Err(error) if proxy_unreachable(&error) => {
                    self.proxies.bench(&server);
                    if next.is_none() {
                        return Err(FetchError::Connect(format!(
                            "proxy {} unreachable: {error}",
                            without_credentials(&server)
                        )));
                    }
                }
                Err(error) => return Err(fetch_error(error)),
            }
        }
        Err(FetchError::Other("no proxy could carry the request".into()))
    }
}

/// The proxy itself, not the site, couldn't be reached.
fn proxy_unreachable(error: &wreq::Error) -> bool {
    error.is_connect() || format!("{error:?}").contains("ProxyConnect")
}

fn fetch_error(error: wreq::Error) -> FetchError {
    if error.is_timeout() {
        FetchError::Timeout
    } else if error.is_tls() {
        FetchError::Tls(error.to_string())
    } else if error.is_connect() || error.is_dns() || error.is_connection_reset() {
        FetchError::Connect(error.to_string())
    } else if error.is_decode() || error.is_body() {
        FetchError::Decode(error.to_string())
    } else {
        FetchError::Other(Box::new(error))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_and_checks_profiles() {
        let all = profiles();
        assert!(all.len() > 20, "{all:?}");
        for family in ["chrome_", "firefox_", "safari_", "edge_"] {
            assert!(all.iter().any(|p| p.starts_with(family)), "{family}");
        }
        assert!(all.contains(&default_profile()));
        assert!(check_impersonation(Some(&default_profile()), Some("linux")).is_ok());
        assert!(
            check_impersonation(Some("chrome_1"), None)
                .unwrap_err()
                .contains("unknown browser profile")
        );
        assert!(check_impersonation(None, Some("beos")).is_err());
    }
}
