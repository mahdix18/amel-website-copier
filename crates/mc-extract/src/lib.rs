//! Finds and rewrites the URLs in HTML and CSS documents.
//!
//! One walk serves both jobs. The crawler extracts links from each document
//! as it arrives, and once the crawl ends it walks the same documents again to
//! point every reference at its local copy. Sharing the walk means every link
//! that was found is also a link that gets rewritten.

mod css;
mod html;
mod srcset;

pub use css::{extract_css, rewrite_css};
pub use html::{HtmlError, extract_html, rewrite_html};
pub use url::Url;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LinkKind {
    /// Another document to crawl: anchors, frames, meta refresh.
    Page,
    /// Something a document needs to display: stylesheets, scripts, images,
    /// media, fonts.
    Asset,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    /// Absolute, fragment included.
    pub url: Url,
    pub kind: LinkKind,
}

/// Resolves a raw reference against `base`, skipping ones that can never
/// point at a fetchable resource.
fn resolve(base: &Url, raw: &str) -> Option<Url> {
    // HTML strips leading and trailing ASCII whitespace from URL attributes.
    let raw = raw.trim_matches(|c: char| c.is_ascii_whitespace());
    if raw.is_empty() || raw.starts_with('#') {
        return None;
    }
    let url = base.join(raw).ok()?;
    match url.scheme() {
        "javascript" | "mailto" | "data" | "tel" | "sms" | "about" | "blob" => None,
        _ => Some(url),
    }
}
