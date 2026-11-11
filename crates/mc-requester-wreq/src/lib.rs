//! The default MisterCopy requester.
//!
//! [wreq] presents the TLS, HTTP/2 and header fingerprint of a real browser
//! profile, so sites that turn away non-browser clients still copy correctly.
//! It is there for compatibility; the crawler's politeness limits apply to it
//! like any other requester.

use std::pin::pin;
use std::sync::Arc;

use futures_util::StreamExt;
use mc_requester::{
    BuildError, Capabilities, FetchContext, FetchError, FetchRequest, FetchResponse, Redirect,
    Requester, RequesterFactory, SharedResources, Url, async_trait, header,
};
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
    /// Responses larger than this fail with `TooLarge` instead of filling memory.
    pub max_body_bytes: u64,
}

impl Default for WreqConfig {
    fn default() -> Self {
        Self {
            profile: DEFAULT_PROFILE,
            platform: None,
            max_redirects: 10,
            max_body_bytes: 512 * 1024 * 1024,
        }
    }
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
    config: WreqConfig,
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
        let mut builder = wreq::Client::builder()
            .emulation(emulation)
            // Redirects are followed in `fetch` so every hop is recorded.
            .redirect(wreq::redirect::Policy::none())
            .timeout(shared.default_timeout);
        if let Some(proxy) = &shared.proxy {
            let proxy = wreq::Proxy::all(proxy.as_str())
                .map_err(|e| build_error(format!("invalid proxy: {e}")))?;
            builder = builder.proxy(proxy);
        }
        let client = builder.build().map_err(|e| build_error(e.to_string()))?;
        Ok(Self { client, config })
    }
}

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

        loop {
            if !matches!(url.scheme(), "http" | "https") {
                return Err(FetchError::UnsupportedScheme(url.scheme().to_owned()));
            }
            let mut builder = self
                .client
                .request(request.method.clone(), url.as_str())
                .headers(request.headers.clone());
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

            let response = builder.send().await.map_err(fetch_error)?;
            let status = response.status();

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
            let limit = self.config.max_body_bytes;
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
