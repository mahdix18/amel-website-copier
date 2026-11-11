//! The offline replay shim: data a JavaScript app fetched while it was
//! rendered (JSON and other text) is embedded in a script that answers the
//! app's own `fetch` and `XMLHttpRequest` calls when the copy is opened
//! offline, even from `file://`, where real requests fail.

use std::collections::BTreeMap;

use serde::Serialize;

/// Saved next to each host's pages.
pub const REPLAY_SCRIPT: &str = "mistercopy-replay.js";

/// A single response larger than this isn't embedded.
const MAX_ITEM_BYTES: usize = 1024 * 1024;
/// Stop embedding once a host's manifest reaches this size.
const MAX_TOTAL_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, Serialize)]
pub(crate) struct Item {
    #[serde(rename = "type")]
    pub mime: String,
    pub body: String,
}

/// Recorded responses for one host, keyed by path and query.
#[derive(Debug, Default)]
pub(crate) struct Manifest {
    items: BTreeMap<String, Item>,
    bytes: usize,
}

/// Media types worth replaying: data a script reads, not things the page
/// links to (those are saved as files and rewritten).
pub(crate) fn is_data(mime: &str) -> bool {
    matches!(
        mime,
        "application/json" | "text/plain" | "text/csv" | "application/xml" | "text/xml"
    ) || mime.ends_with("+json")
}

impl Manifest {
    pub(crate) fn add(&mut self, key: String, mime: &str, body: &[u8]) {
        if body.len() > MAX_ITEM_BYTES || self.bytes + body.len() > MAX_TOTAL_BYTES {
            return;
        }
        let Ok(text) = std::str::from_utf8(body) else {
            return;
        };
        self.bytes += body.len();
        self.items.insert(
            key,
            Item {
                mime: mime.to_owned(),
                body: text.to_owned(),
            },
        );
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub(crate) fn script(&self) -> String {
        // `</script>` can't occur: the script is a separate file.
        let data = serde_json::to_string(&self.items).unwrap_or_else(|_| "{}".to_owned());
        SHIM.replace("__MISTERCOPY_DATA__", &data)
    }
}

const SHIM: &str = r#"// Written by MisterCopy. Answers this copy's own data requests offline.
(() => {
  const data = __MISTERCOPY_DATA__;
  const keys = Object.keys(data);
  const find = (input) => {
    try {
      const url = new URL(typeof input === "string" ? input : input.url, location.href);
      const exact = data[url.pathname + url.search] || data[url.pathname];
      if (exact) return exact;
      // From file:// a relative URL resolves under the copy's folder.
      const key = keys.find((k) => (url.pathname + url.search).endsWith(k));
      return key && data[key];
    } catch (_) {
      return undefined;
    }
  };
  const realFetch = window.fetch ? window.fetch.bind(window) : null;
  window.fetch = (input, init) => {
    const hit = find(input);
    if (hit) {
      return Promise.resolve(new Response(hit.body, { status: 200, headers: { "Content-Type": hit.type } }));
    }
    return realFetch ? realFetch(input, init) : Promise.reject(new TypeError("offline"));
  };
  const open = XMLHttpRequest.prototype.open;
  const send = XMLHttpRequest.prototype.send;
  XMLHttpRequest.prototype.open = function (method, url, ...rest) {
    this.__misterCopyHit = find(url);
    return open.call(this, method, url, ...rest);
  };
  XMLHttpRequest.prototype.send = function (body) {
    const hit = this.__misterCopyHit;
    if (!hit) return send.call(this, body);
    for (const [name, value] of [["readyState", 4], ["status", 200], ["responseText", hit.body], ["response", hit.body]]) {
      Object.defineProperty(this, name, { value, configurable: true });
    }
    setTimeout(() => {
      if (this.onreadystatechange) this.onreadystatechange();
      this.dispatchEvent(new Event("load"));
      this.dispatchEvent(new Event("loadend"));
    });
  };
})();
"#;

/// Puts a `<script src>` for the shim first in `<head>`, so it is in place
/// before the page's own scripts run.
pub(crate) fn inject(html: &[u8], src: &str) -> Vec<u8> {
    let tag = format!(r#"<script src="{src}"></script>"#);
    let lower = String::from_utf8_lossy(html).to_ascii_lowercase();
    let at = lower
        .find("<head")
        .and_then(|start| lower[start..].find('>').map(|end| start + end + 1))
        .unwrap_or(0);
    let mut out = Vec::with_capacity(html.len() + tag.len());
    out.extend_from_slice(&html[..at]);
    out.extend_from_slice(tag.as_bytes());
    out.extend_from_slice(&html[at..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embeds_text_data_within_limits() {
        let mut manifest = Manifest::default();
        manifest.add(
            "/api/a.json".into(),
            "application/json",
            br#"{"title":"</script>"}"#,
        );
        manifest.add("/bin".into(), "application/json", &[0xff, 0xfe]);
        manifest.add(
            "/huge".into(),
            "text/plain",
            &vec![b'x'; MAX_ITEM_BYTES + 1],
        );
        let script = manifest.script();
        assert!(
            script.contains(
                r#""/api/a.json":{"type":"application/json","body":"{\"title\":\"</script>\"}"}"#
            ),
            "{script}"
        );
        assert!(!script.contains("/bin") && !script.contains("/huge"));
        assert!(is_data("application/ld+json") && !is_data("text/html") && !is_data("image/png"));
    }

    #[test]
    fn injects_first_in_head() {
        let out = inject(
            b"<!doctype html><html><HEAD lang=x><script>a()</script></HEAD>",
            "r.js",
        );
        assert_eq!(
            String::from_utf8(out).unwrap(),
            r#"<!doctype html><html><HEAD lang=x><script src="r.js"></script><script>a()</script></HEAD>"#
        );
        assert!(
            String::from_utf8(inject(b"<p>no head</p>", "r.js"))
                .unwrap()
                .starts_with("<script src")
        );
    }
}
