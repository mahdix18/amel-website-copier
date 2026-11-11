/// A fetch that produced no usable response.
///
/// HTTP error statuses are not errors at this level: they come back as a
/// [`FetchResponse`](crate::FetchResponse) so the crawler can record them,
/// honour `Retry-After` and decide on retries itself.
#[derive(Debug, thiserror::Error)]
pub enum FetchError {
    #[error("request timed out")]
    Timeout,
    #[error("connection failed: {0}")]
    Connect(String),
    #[error("TLS handshake failed: {0}")]
    Tls(String),
    #[error("could not decode the response: {0}")]
    Decode(String),
    #[error("response is larger than the {limit}-byte limit")]
    TooLarge { limit: u64 },
    /// The server answered with a challenge or block page instead of content.
    #[error("blocked by the server: {0}")]
    Blocked(String),
    #[error("this requester cannot fetch `{0}` URLs")]
    UnsupportedScheme(String),
    #[error(transparent)]
    Other(Box<dyn std::error::Error + Send + Sync>),
}
