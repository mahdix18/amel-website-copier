//! The fetch abstraction behind MisterCopy.
//!
//! The crawler never talks to an HTTP client or a browser directly. It hands a
//! [`FetchRequest`] to a [`Requester`] and gets back a [`FetchResponse`]. Each
//! backend (wreq, Playwright, …) lives in its own crate, implements
//! [`Requester`] plus [`RequesterFactory`], and is registered with a
//! [`RequesterRegistry`] by the binaries.
//!
//! A requester reports every HTTP response it receives as `Ok`, whatever the
//! status code. [`FetchError`] is reserved for failures where no usable
//! response arrived, so the crawler alone decides what a 404 or 503 means.

mod cookies;
mod error;
mod hybrid;
#[cfg(feature = "mock")]
pub mod mock;
mod proxy;
mod registry;
mod requester;
mod types;

pub use cookies::{CookieFileError, CookieJar};
pub use error::FetchError;
pub use hybrid::{AUTO_ID, HybridRequester, needs_browser};
pub use proxy::{
    PROXY_SCHEMES, ProxyConfig, ProxyPicker, ProxyRotation, check as check_proxy, parse_proxy_list,
    without_credentials,
};
pub use registry::{RegistryError, RequesterInfo, RequesterRegistry};
pub use requester::{BuildError, Capabilities, Requester, RequesterFactory, SharedResources};
pub use types::{
    Body, CapturedResponse, FetchContext, FetchRequest, FetchResponse, Redirect, Validators,
};

// Re-exported so requester crates and the crawler agree on one version.
pub use async_trait::async_trait;
pub use http::{HeaderMap, Method, StatusCode, header};
pub use url::Url;
