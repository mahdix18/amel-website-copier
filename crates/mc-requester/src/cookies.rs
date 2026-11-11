use std::fmt;
use std::sync::{Mutex, PoisonError};

use cookie::Cookie;
use cookie::time::OffsetDateTime;
use cookie_store::CookieStore;
use http::{HeaderMap, header};
use url::Url;

/// The cookie session shared by every requester in a crawl.
pub struct CookieJar {
    store: Mutex<CookieStore>,
}

impl fmt::Debug for CookieJar {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CookieJar").finish_non_exhaustive()
    }
}

impl Default for CookieJar {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, thiserror::Error)]
#[error("cookies file line {line}: {message}")]
pub struct CookieFileError {
    pub line: usize,
    pub message: String,
}

impl CookieJar {
    pub fn new() -> Self {
        Self {
            store: Mutex::new(CookieStore::new()),
        }
    }

    /// The `Cookie` header value to send to `url`, if any cookie matches.
    pub fn header_for(&self, url: &Url) -> Option<String> {
        let store = self.store.lock().unwrap_or_else(PoisonError::into_inner);
        let pairs: Vec<String> = store
            .get_request_values(url)
            .map(|(name, value)| format!("{name}={value}"))
            .collect();
        (!pairs.is_empty()).then(|| pairs.join("; "))
    }

    /// The name/value pairs that would be sent to `url`.
    pub fn pairs_for(&self, url: &Url) -> Vec<(String, String)> {
        let store = self.store.lock().unwrap_or_else(PoisonError::into_inner);
        store
            .get_request_values(url)
            .map(|(name, value)| (name.to_owned(), value.to_owned()))
            .collect()
    }

    /// Remembers one `Set-Cookie`-style line as if `url` had sent it.
    pub fn store_line(&self, url: &Url, line: &str) {
        let mut store = self.store.lock().unwrap_or_else(PoisonError::into_inner);
        let _ = store.parse(line, url);
    }

    /// Remembers the `Set-Cookie` headers of a response from `url`.
    pub fn store_from(&self, url: &Url, headers: &HeaderMap) {
        let mut store = self.store.lock().unwrap_or_else(PoisonError::into_inner);
        for value in headers.get_all(header::SET_COOKIE) {
            if let Ok(text) = value.to_str() {
                // Malformed or foreign-domain cookies are dropped, as browsers do.
                let _ = store.parse(text, url);
            }
        }
    }

    /// Imports a Netscape `cookies.txt` file, as exported by browser
    /// extensions and curl. Returns how many cookies were added.
    pub fn import_netscape(&self, text: &str) -> Result<usize, CookieFileError> {
        let mut store = self.store.lock().unwrap_or_else(PoisonError::into_inner);
        let mut added = 0;
        for (index, raw) in text.lines().enumerate() {
            let line = raw
                .strip_prefix("#HttpOnly_")
                .unwrap_or(raw)
                .trim_end_matches('\r');
            if line.trim().is_empty() || line.starts_with('#') {
                continue;
            }
            let error = |message: &str| CookieFileError {
                line: index + 1,
                message: message.to_owned(),
            };
            let fields: Vec<&str> = line.split('\t').collect();
            let [domain, subdomains, path, secure, expires, name, value] = fields[..] else {
                return Err(error("expected 7 tab-separated fields"));
            };
            let expires: i64 = expires
                .parse()
                .map_err(|_| error("expiry is not a number"))?;
            let secure = secure.eq_ignore_ascii_case("TRUE");
            let host = domain.trim_start_matches('.');
            let scheme = if secure { "https" } else { "http" };
            let request_url = Url::parse(&format!("{scheme}://{host}{path}"))
                .map_err(|_| error("invalid domain or path"))?;

            let mut cookie = Cookie::build((name.to_owned(), value.to_owned()))
                .path(path.to_owned())
                .secure(secure);
            if subdomains.eq_ignore_ascii_case("TRUE") {
                cookie = cookie.domain(host.to_owned());
            }
            if expires > 0 {
                let at = OffsetDateTime::from_unix_timestamp(expires)
                    .map_err(|_| error("expiry out of range"))?;
                if at < OffsetDateTime::now_utc() {
                    continue;
                }
                cookie = cookie.expires(at);
            }
            if store.insert_raw(&cookie.build(), &request_url).is_ok() {
                added += 1;
            }
        }
        Ok(added)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[test]
    fn round_trips_set_cookie() {
        let jar = CookieJar::new();
        let mut headers = HeaderMap::new();
        headers.append(header::SET_COOKIE, "session=abc; Path=/".parse().unwrap());
        headers.append(
            header::SET_COOKIE,
            "theme=dark; Path=/docs".parse().unwrap(),
        );
        jar.store_from(&url("https://site.test/login"), &headers);

        assert_eq!(
            jar.header_for(&url("https://site.test/")).as_deref(),
            Some("session=abc")
        );
        let docs = jar.header_for(&url("https://site.test/docs/a")).unwrap();
        assert!(
            docs.contains("session=abc") && docs.contains("theme=dark"),
            "{docs}"
        );
        assert_eq!(jar.header_for(&url("https://other.test/")), None);
    }

    #[test]
    fn imports_netscape_files() {
        let jar = CookieJar::new();
        let file = "# Netscape HTTP Cookie File\n\
            .site.test\tTRUE\t/\tFALSE\t0\tsid\t123\n\
            #HttpOnly_site.test\tFALSE\t/\tTRUE\t4102444800\ttoken\txyz\n\
            site.test\tFALSE\t/\tFALSE\t1\told\tgone\n\n";
        assert_eq!(jar.import_netscape(file).unwrap(), 2);
        assert_eq!(
            jar.header_for(&url("http://www.site.test/")).as_deref(),
            Some("sid=123")
        );
        let secure = jar.header_for(&url("https://site.test/")).unwrap();
        assert!(
            secure.contains("token=xyz") && secure.contains("sid=123"),
            "{secure}"
        );

        let err = jar.import_netscape("bad line").unwrap_err();
        assert_eq!(
            err.to_string(),
            "cookies file line 1: expected 7 tab-separated fields"
        );
    }
}
