use url::Url;

/// Query parameters that only track clicks and never change the content.
const TRACKING_PARAMS: &[&str] = &["fbclid", "gclid", "dclid", "msclkid", "mc_cid", "mc_eid"];

/// The canonical form of a URL, used to tell whether two links point at the
/// same resource. `None` for anything that is not an http(s) URL with a host.
///
/// The `url` crate already lowercases the host, drops default ports and
/// resolves dot segments; on top of that the fragment, empty queries and
/// tracking parameters are removed.
pub fn normalize(url: &Url) -> Option<Url> {
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return None;
    }
    let mut url = url.clone();
    url.set_fragment(None);

    if let Some(query) = url.query().map(str::to_owned) {
        let kept: Vec<&str> = query
            .split('&')
            .filter(|pair| {
                let key = pair.split('=').next().unwrap_or_default();
                !pair.is_empty() && !key.starts_with("utm_") && !TRACKING_PARAMS.contains(&key)
            })
            .collect();
        if kept.is_empty() {
            url.set_query(None);
        } else {
            url.set_query(Some(&kept.join("&")));
        }
    }
    Some(url)
}

#[cfg(test)]
mod tests {
    use super::normalize;
    use url::Url;

    fn norm(s: &str) -> Option<String> {
        normalize(&Url::parse(s).unwrap()).map(String::from)
    }

    #[test]
    fn canonicalizes() {
        assert_eq!(
            norm("HTTP://Example.TEST:80/a/../b#frag").as_deref(),
            Some("http://example.test/b")
        );
        assert_eq!(
            norm("https://x.test/p?").as_deref(),
            Some("https://x.test/p")
        );
        assert_eq!(
            norm("https://x.test/p?utm_source=a&id=3&fbclid=z&&b=").as_deref(),
            Some("https://x.test/p?id=3&b=")
        );
        assert_eq!(
            norm("https://x.test/p?utm_medium=x").as_deref(),
            Some("https://x.test/p")
        );
    }

    #[test]
    fn rejects_non_http() {
        assert_eq!(norm("file:///tmp/a.html"), None);
        assert_eq!(norm("ftp://x.test/a"), None);
    }
}
