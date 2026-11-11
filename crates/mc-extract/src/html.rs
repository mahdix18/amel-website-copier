use std::cell::RefCell;

use lol_html::html_content::{ContentType, Element};
use lol_html::{AsciiCompatibleEncoding, HandlerResult, HtmlRewriter, Settings, element, text};
use url::Url;

use crate::{Link, LinkKind, css, resolve, srcset};

#[derive(Debug, thiserror::Error)]
#[error("could not parse HTML: {0}")]
pub struct HtmlError(#[from] lol_html::errors::RewritingError);

/// `<link rel>` values whose target a page needs in order to display.
const ASSET_RELS: &[&str] = &[
    "stylesheet",
    "icon",
    "apple-touch-icon",
    "apple-touch-icon-precomposed",
    "mask-icon",
    "preload",
    "prefetch",
    "modulepreload",
    "manifest",
];

/// Every URL a document references. `charset` is the label from the
/// response's `Content-Type`, if any; unknown labels fall back to UTF-8.
pub fn extract_html(
    html: &[u8],
    charset: Option<&str>,
    page_url: &Url,
) -> Result<Vec<Link>, HtmlError> {
    let mut links = Vec::new();
    walk(html, charset, page_url, false, &mut |url, kind| {
        links.push(Link {
            url: url.clone(),
            kind,
        });
        None
    })?;
    Ok(links)
}

/// Replaces each URL for which `map` returns a value. Also drops what would
/// break a local copy: `<base>`, `integrity` hashes, and CSP meta tags.
pub fn rewrite_html(
    html: &[u8],
    charset: Option<&str>,
    page_url: &Url,
    mut map: impl FnMut(&Url, LinkKind) -> Option<String>,
) -> Result<Vec<u8>, HtmlError> {
    walk(html, charset, page_url, true, &mut map)
}

type MapFn<'a> = dyn FnMut(&Url, LinkKind) -> Option<String> + 'a;

struct State<'m, 'a> {
    base: Url,
    map: &'m mut MapFn<'a>,
}

impl State<'_, '_> {
    fn map(&mut self, raw: &str, kind: LinkKind) -> Option<String> {
        let url = resolve(&self.base, raw)?;
        (self.map)(&url, kind)
    }
}

fn walk(
    html: &[u8],
    charset: Option<&str>,
    page_url: &Url,
    rewriting: bool,
    map: &mut MapFn<'_>,
) -> Result<Vec<u8>, HtmlError> {
    let encoding = charset
        .and_then(|label| encoding_rs::Encoding::for_label(label.trim().as_bytes()))
        .and_then(AsciiCompatibleEncoding::new)
        .unwrap_or_else(AsciiCompatibleEncoding::utf_8);

    let state = RefCell::new(State {
        base: page_url.clone(),
        map,
    });
    let style_text = RefCell::new(String::new());
    let mut output = Vec::with_capacity(html.len());

    let attr = |el: &mut Element<'_, '_>, name: &str, kind: LinkKind| -> HandlerResult {
        if let Some(value) = el.get_attribute(name)
            && let Some(new) = state.borrow_mut().map(&value, kind)
        {
            el.set_attribute(name, &new)?;
        }
        Ok(())
    };
    let srcset_attr = |el: &mut Element<'_, '_>, name: &str| -> HandlerResult {
        if let Some(value) = el.get_attribute(name)
            && let Some(new) =
                srcset::rewrite(&value, |raw| state.borrow_mut().map(raw, LinkKind::Asset))
        {
            el.set_attribute(name, &new)?;
        }
        Ok(())
    };

    let settings = Settings::new()
        .with_encoding(encoding)
        .append_element_content_handler(element!("base[href]", |el| {
            if let Some(href) = el.get_attribute("href") {
                let mut state = state.borrow_mut();
                if let Ok(base) = state.base.join(href.trim()) {
                    state.base = base;
                }
            }
            if rewriting {
                el.remove();
            }
            Ok(())
        }))
        .append_element_content_handler(element!("a[href], area[href]", |el| {
            attr(el, "href", LinkKind::Page)
        }))
        .append_element_content_handler(element!("iframe, frame", |el| {
            attr(el, "src", LinkKind::Page)?;
            attr(el, "data-src", LinkKind::Page)
        }))
        .append_element_content_handler(element!("link[href]", |el| {
            let rel = el
                .get_attribute("rel")
                .unwrap_or_default()
                .to_ascii_lowercase();
            if rel
                .split_ascii_whitespace()
                .any(|r| ASSET_RELS.contains(&r))
            {
                attr(el, "href", LinkKind::Asset)?;
                if rewriting {
                    el.remove_attribute("integrity");
                }
            }
            Ok(())
        }))
        .append_element_content_handler(element!("script[src]", |el| {
            attr(el, "src", LinkKind::Asset)?;
            if rewriting {
                el.remove_attribute("integrity");
            }
            Ok(())
        }))
        .append_element_content_handler(element!(
            "img, source, video, audio, track, embed, input[type=image]",
            |el| {
                attr(el, "src", LinkKind::Asset)?;
                attr(el, "data-src", LinkKind::Asset)?;
                attr(el, "poster", LinkKind::Asset)?;
                srcset_attr(el, "srcset")?;
                srcset_attr(el, "data-srcset")
            }
        ))
        .append_element_content_handler(element!("object[data]", |el| {
            attr(el, "data", LinkKind::Asset)
        }))
        .append_element_content_handler(element!("meta[http-equiv]", |el| {
            let equiv = el
                .get_attribute("http-equiv")
                .unwrap_or_default()
                .to_ascii_lowercase();
            match equiv.trim() {
                "refresh" => {
                    if let Some(content) = el.get_attribute("content")
                        && let Some((prefix, raw)) = split_refresh(&content)
                        && let Some(new) = state.borrow_mut().map(raw, LinkKind::Page)
                    {
                        el.set_attribute("content", &format!("{prefix}{new}"))?;
                    }
                }
                "content-security-policy" if rewriting => el.remove(),
                _ => {}
            }
            Ok(())
        }))
        .append_element_content_handler(element!("[style]", |el| {
            if let Some(style) = el.get_attribute("style") {
                let new = css::walk(&style, |raw| state.borrow_mut().map(raw, LinkKind::Asset));
                if new != style {
                    el.set_attribute("style", &new)?;
                }
            }
            Ok(())
        }))
        .append_element_content_handler(text!("style", |chunk| {
            // Text arrives in chunks; buffer the whole stylesheet first.
            let mut buffer = style_text.borrow_mut();
            buffer.push_str(chunk.as_str());
            if chunk.last_in_text_node() {
                let css = std::mem::take(&mut *buffer);
                let new = css::walk(&css, |raw| state.borrow_mut().map(raw, LinkKind::Asset));
                chunk.replace(&new, ContentType::Html);
            } else {
                chunk.remove();
            }
            Ok(())
        }));

    let mut rewriter = HtmlRewriter::new(settings, |chunk: &[u8]| output.extend_from_slice(chunk));
    rewriter.write(html)?;
    rewriter.end()?;
    Ok(output)
}

/// Splits `5; url=next.html` into (`5; url=`, `next.html`), so the URL can be
/// replaced while the delay and spelling are kept.
fn split_refresh(content: &str) -> Option<(&str, &str)> {
    let separator = content.find([';', ','])?;
    let rest = &content[separator + 1..];
    let trimmed = rest.trim_start();
    let mut url_start = content.len() - trimmed.len();
    if trimmed.len() >= 3 && trimmed[..3].eq_ignore_ascii_case("url") {
        let after = trimmed[3..].trim_start();
        if let Some(value) = after.strip_prefix('=') {
            url_start = content.len() - value.trim_start().len();
        }
    }
    let mut value = &content[url_start..];
    let mut prefix_end = url_start;
    if let Some(quote @ ('"' | '\'')) = value.chars().next() {
        prefix_end += 1;
        value = value[1..].trim_end().trim_end_matches(quote);
    } else {
        value = value.trim_end();
    }
    (!value.is_empty()).then(|| (&content[..prefix_end], value))
}

#[cfg(test)]
mod tests {
    use super::split_refresh;

    #[test]
    fn splits_meta_refresh_content() {
        assert_eq!(
            split_refresh("5; url=next.html"),
            Some(("5; url=", "next.html"))
        );
        assert_eq!(
            split_refresh("0;URL='a b.html'"),
            Some(("0;URL='", "a b.html"))
        );
        assert_eq!(split_refresh("3, /home"), Some(("3, ", "/home")));
        assert_eq!(split_refresh("10"), None);
        assert_eq!(split_refresh("1; url="), None);
    }
}
