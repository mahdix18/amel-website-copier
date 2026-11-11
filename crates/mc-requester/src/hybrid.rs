//! A requester that tries a fast one first and falls back to a browser.

use std::sync::Arc;

use crate::{
    Body, Capabilities, FetchContext, FetchError, FetchRequest, FetchResponse, Requester,
    StatusCode,
};

pub const AUTO_ID: &str = "auto";

/// Fetches with `fast` (usually plain HTTP) and repeats the request with
/// `browser` when the answer looks like it needs JavaScript: an HTML shell
/// with almost no text, or a refusal that a real browser might get past.
pub struct HybridRequester {
    fast: Arc<dyn Requester>,
    browser: Arc<dyn Requester>,
}

impl HybridRequester {
    pub fn new(fast: Arc<dyn Requester>, browser: Arc<dyn Requester>) -> Self {
        Self { fast, browser }
    }
}

#[async_trait::async_trait]
impl Requester for HybridRequester {
    fn id(&self) -> &'static str {
        AUTO_ID
    }

    fn capabilities(&self) -> Capabilities {
        let fast = self.fast.capabilities();
        let browser = self.browser.capabilities();
        Capabilities {
            executes_js: browser.executes_js,
            captures_subresources: browser.captures_subresources,
            impersonation: fast.impersonation || browser.impersonation,
            http2: fast.http2 || browser.http2,
            streaming_body: fast.streaming_body && browser.streaming_body,
        }
    }

    async fn fetch(
        &self,
        request: FetchRequest,
        context: &FetchContext,
    ) -> Result<FetchResponse, FetchError> {
        match self.fast.fetch(request.clone(), context).await {
            Ok(response) if !needs_browser(&response) => Ok(response),
            Err(FetchError::Blocked(_)) | Ok(_) => self.browser.fetch(request, context).await,
            Err(error) => Err(error),
        }
    }

    async fn shutdown(&self) {
        self.fast.shutdown().await;
        self.browser.shutdown().await;
    }
}

/// Next to a script, less visible text than this is a JavaScript shell.
const EMPTY_TEXT_CHARS: usize = 40;
/// Next to a script and an SPA mount point, less text than this is a shell.
const SHELL_TEXT_CHARS: usize = 200;

/// Elements single-page-app frameworks render into.
const MOUNT_POINTS: &[&str] = &[
    r#"id="app""#,
    r#"id="root""#,
    r#"id="__next""#,
    r#"id="__nuxt""#,
    r#"id="svelte""#,
    "data-reactroot",
    "ng-version",
];

const NEEDS_JS_PHRASES: &[&str] = &[
    "enable javascript",
    "requires javascript",
    "needs javascript",
    "javascript is required",
    "javascript to run",
];

/// Whether a response should be fetched again with a browser.
pub fn needs_browser(response: &FetchResponse) -> bool {
    if response.status == StatusCode::FORBIDDEN {
        return true;
    }
    if !response.status.is_success() || response.mime_type().as_deref() != Some("text/html") {
        return false;
    }
    let Body::Bytes(body) = &response.body else {
        return false;
    };
    looks_like_shell(&String::from_utf8_lossy(body))
}

fn looks_like_shell(html: &str) -> bool {
    let lower = html.to_ascii_lowercase();
    if !lower.contains("<script") {
        return false;
    }
    let asks_for_js =
        lower.contains("<noscript") && NEEDS_JS_PHRASES.iter().any(|phrase| lower.contains(phrase));
    if asks_for_js {
        return true;
    }
    let text = visible_text_len(&lower);
    let has_mount_point = MOUNT_POINTS.iter().any(|mount| lower.contains(mount));
    text < EMPTY_TEXT_CHARS || (has_mount_point && text < SHELL_TEXT_CHARS)
}

/// Counts characters outside tags, scripts and styles. Crude, but a shell
/// page is unambiguous: it has next to nothing.
fn visible_text_len(html: &str) -> usize {
    let mut count = 0;
    let mut rest = html;
    while let Some(open) = rest.find('<') {
        count += rest[..open].chars().filter(|c| !c.is_whitespace()).count();
        rest = &rest[open..];
        let skip_to = ["<script", "<style", "<noscript"]
            .iter()
            .find(|tag| rest.starts_with(**tag))
            .map(|tag| format!("</{}", &tag[1..]));
        // Past the whole element for scripts and styles, else past the tag.
        if let Some(close) = skip_to {
            rest = rest.find(&close).map_or("", |end| &rest[end..]);
        }
        rest = rest.find('>').map_or("", |end| &rest[end + 1..]);
    }
    count + rest.chars().filter(|c| !c.is_whitespace()).count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_javascript_shells() {
        assert!(looks_like_shell(
            r#"<html><head><script src="app.js"></script></head><body><div id="root"></div></body></html>"#
        ));
        assert!(looks_like_shell(&format!(
            r#"<body><noscript>You need to enable JavaScript to run this app.</noscript><script>x()</script><p>{}</p></body>"#,
            "word ".repeat(100)
        )));
        let article = format!(
            "<script>track()</script><article>{}</article>",
            "Real content. ".repeat(40)
        );
        assert!(!looks_like_shell(&article));
        assert!(!looks_like_shell("<p>tiny page, no scripts</p>"));
        // A small but real page with a script is not a shell…
        assert!(!looks_like_shell(
            "<h1>Notes on gardening</h1><ul><li><a href=a>Planting tomatoes in spring</a></li><li><a href=b>Why compost matters</a></li></ul><script src=x.js></script>"
        ));
        // …unless it is only a mount point waiting to be filled.
        assert!(looks_like_shell(
            r#"<div id="app"><h1>Loading the application…</h1></div><script src="x.js"></script>"#
        ));
    }

    #[test]
    fn counts_only_visible_text() {
        assert_eq!(
            visible_text_len("<p>ab c</p><script>var long = 1;</script><style>p{}</style>d"),
            4
        );
        assert_eq!(visible_text_len("<unterminated"), 0);
    }
}
