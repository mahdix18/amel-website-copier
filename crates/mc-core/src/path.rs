use std::collections::HashMap;
use std::fmt;
use std::path::PathBuf;

use percent_encoding::{AsciiSet, CONTROLS, percent_decode_str, utf8_percent_encode};
use unicode_normalization::UnicodeNormalization;
use url::Url;

const MAX_SEGMENT_BYTES: usize = 200;
const MAX_PATH_BYTES: usize = 240;

/// Characters Windows forbids in file names. Replaced on every platform so a
/// copy made anywhere opens anywhere.
const FORBIDDEN: &[char] = &['<', '>', ':', '"', '/', '\\', '|', '?', '*'];

const RESERVED_NAMES: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM0", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7",
    "COM8", "COM9", "LPT0", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// What to percent-encode when a local path becomes an `href`. Non-ASCII is
/// always encoded. Parentheses and quotes are included so the result is also
/// safe inside an unquoted CSS `url()`.
const HREF_SEGMENT: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'%')
    .add(b'&')
    .add(b'\'')
    .add(b'(')
    .add(b')')
    .add(b'/')
    .add(b'<')
    .add(b'>')
    .add(b'?')
    .add(b'[')
    .add(b'\\')
    .add(b']')
    .add(b'^')
    .add(b'`')
    .add(b'{')
    .add(b'|')
    .add(b'}');

/// A file location under the output directory, as path segments. The first
/// segment is the host.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LocalPath(Vec<String>);

impl LocalPath {
    /// Parses the `/`-joined form produced by `Display`. Needs a host and a
    /// file name.
    pub fn parse(joined: &str) -> Option<Self> {
        let segments: Vec<String> = joined.split('/').map(str::to_owned).collect();
        (segments.len() >= 2 && segments.iter().all(|s| !s.is_empty())).then_some(Self(segments))
    }

    pub fn segments(&self) -> &[String] {
        &self.0
    }

    /// The path relative to the output directory, with platform separators.
    pub fn to_path_buf(&self) -> PathBuf {
        self.0.iter().collect()
    }

    /// A relative, percent-encoded reference from this file to `target`.
    pub fn relative_href(&self, target: &LocalPath) -> String {
        let from_dir = &self.0[..self.0.len() - 1];
        let target_dir_len = target.0.len() - 1;
        let common = from_dir
            .iter()
            .zip(&target.0[..target_dir_len])
            .take_while(|(a, b)| a == b)
            .count();

        let mut parts: Vec<String> = vec!["..".to_owned(); from_dir.len() - common];
        parts.extend(
            target.0[common..]
                .iter()
                .map(|segment| utf8_percent_encode(segment, HREF_SEGMENT).to_string()),
        );
        parts.join("/")
    }

    fn byte_len(&self) -> usize {
        self.0.iter().map(|s| s.len() + 1).sum()
    }

    /// Inserts `~suffix` before the file extension.
    fn with_suffix(&self, suffix: &str) -> LocalPath {
        let mut segments = self.0.clone();
        if let Some(last) = segments.last_mut() {
            let (stem, ext) = split_ext(last);
            *last = join_ext(&format!("{stem}~{suffix}"), ext);
        }
        LocalPath(segments)
    }

    /// Case-insensitive key, since Windows and macOS file systems usually are.
    fn collision_key(&self) -> String {
        self.0.join("/").to_lowercase()
    }
}

impl fmt::Display for LocalPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0.join("/"))
    }
}

/// Where a URL is saved, given the media type it was served with. A pure
/// function: the same input gives the same path on every run and OS.
///
/// `https://site.test/docs/` → `site.test/docs/index.html`,
/// `/about` (HTML) → `about.html`, `/app.js?v=3` → `app.<hash>.js`.
pub fn map_url(url: &Url, mime: Option<&str>) -> LocalPath {
    let mut segments = vec![host_segment(url)];
    let mut path: Vec<String> = url
        .path_segments()
        .map(|parts| parts.map(decode).collect())
        .unwrap_or_default();
    let last = path.pop().unwrap_or_default();
    segments.extend(path.iter().filter(|s| !s.is_empty()).map(|s| sanitize(s)));
    segments.push(file_name(&last, mime, url.query()));

    let local = LocalPath(segments);
    if local.byte_len() <= MAX_PATH_BYTES {
        return local;
    }
    // Too long for Windows: keep the host and extension, hash the rest.
    let ext = local
        .0
        .last()
        .and_then(|name| split_ext(name).1)
        .map(str::to_owned);
    LocalPath(vec![
        local.0[0].clone(),
        "_long".to_owned(),
        join_ext(&hash_hex(url.as_str(), 16), ext.as_deref()),
    ])
}

/// Hands out paths for a crawl, giving a stable `~hash` suffix to a URL whose
/// path is already taken by a different URL (for example `/A` and `/a` on a
/// case-insensitive file system).
#[derive(Debug, Default)]
pub struct PathAllocator {
    taken: HashMap<String, Url>,
}

impl PathAllocator {
    pub fn allocate(&mut self, url: &Url, mime: Option<&str>) -> LocalPath {
        let mut path = map_url(url, mime);
        if self
            .taken
            .get(&path.collision_key())
            .is_some_and(|owner| owner != url)
        {
            path = path.with_suffix(&hash_hex(url.as_str(), 8));
        }
        self.taken.insert(path.collision_key(), url.clone());
        path
    }

    /// Marks `path` as belonging to `url`, for paths restored from an
    /// earlier run.
    pub fn reserve(&mut self, path: &LocalPath, url: &Url) {
        self.taken.insert(path.collision_key(), url.clone());
    }
}

fn host_segment(url: &Url) -> String {
    let host = url.host_str().unwrap_or("_");
    match url.port() {
        Some(port) => sanitize(&format!("{host}_{port}")),
        None => sanitize(host),
    }
}

fn decode(segment: &str) -> String {
    percent_decode_str(segment).decode_utf8_lossy().into_owned()
}

/// Extensions a media type may already carry; the first is appended when the
/// name lacks one.
fn extensions_for(mime: &str) -> Option<&'static [&'static str]> {
    Some(match mime {
        "text/html" | "application/xhtml+xml" => &["html", "htm", "xhtml", "shtml"],
        "text/css" => &["css"],
        "text/javascript"
        | "application/javascript"
        | "application/x-javascript"
        | "application/ecmascript"
        | "text/ecmascript" => &["js", "mjs", "cjs"],
        "application/json" | "application/manifest+json" => &["json", "webmanifest"],
        "application/xml" | "text/xml" => &["xml"],
        "text/plain" => &["txt"],
        "image/svg+xml" => &["svg"],
        "image/png" => &["png"],
        "image/jpeg" => &["jpg", "jpeg", "jpe", "jfif"],
        "image/gif" => &["gif"],
        "image/webp" => &["webp"],
        "image/avif" => &["avif"],
        "image/x-icon" | "image/vnd.microsoft.icon" => &["ico"],
        "image/bmp" => &["bmp"],
        "font/woff" => &["woff"],
        "font/woff2" => &["woff2"],
        "font/ttf" => &["ttf"],
        "font/otf" => &["otf"],
        "application/pdf" => &["pdf"],
        "application/zip" => &["zip"],
        "video/mp4" => &["mp4", "m4v"],
        "video/webm" => &["webm"],
        "audio/mpeg" => &["mp3"],
        _ => return None,
    })
}

/// Types a browser treats by file extension when opened from disk, so a
/// mismatched extension breaks the copy: `style.php` serving CSS must become
/// `style.php.css`. Other types keep whatever extension they were served with.
fn extension_must_match(mime: &str) -> bool {
    matches!(
        extensions_for(mime),
        Some(exts) if ["html", "css", "js"].contains(&exts[0])
    )
}

fn file_name(last: &str, mime: Option<&str>, query: Option<&str>) -> String {
    let name = if last.is_empty() { "index" } else { last };
    let (stem, ext) = split_ext(name);

    let (stem, ext): (&str, Option<&str>) = match mime
        .and_then(|m| extensions_for(m).map(|e| (m, e)))
    {
        Some((mime, accepted)) => match ext {
            Some(ext) if accepted.contains(&ext.to_ascii_lowercase().as_str()) => (stem, Some(ext)),
            Some(ext) if !extension_must_match(mime) => (stem, Some(ext)),
            Some(_) => (name, Some(accepted[0])),
            None => (name, Some(accepted[0])),
        },
        None => (stem, ext),
    };

    let stem = match query {
        Some(query) if !query.is_empty() => format!("{stem}.{}", hash_hex(query, 8)),
        _ => stem.to_owned(),
    };
    sanitize(&join_ext(&stem, ext))
}

/// Splits `name.ext` when the extension is 1–8 ASCII alphanumerics.
fn split_ext(name: &str) -> (&str, Option<&str>) {
    match name.rfind('.') {
        Some(dot) if dot > 0 => {
            let ext = &name[dot + 1..];
            if (1..=8).contains(&ext.len()) && ext.bytes().all(|b| b.is_ascii_alphanumeric()) {
                (&name[..dot], Some(ext))
            } else {
                (name, None)
            }
        }
        _ => (name, None),
    }
}

fn join_ext(stem: &str, ext: Option<&str>) -> String {
    match ext {
        Some(ext) => format!("{stem}.{ext}"),
        None => stem.to_owned(),
    }
}

/// Makes one path segment valid on Windows, macOS and Linux.
fn sanitize(segment: &str) -> String {
    let replaced: String = segment
        .nfc()
        .map(|c| {
            if c.is_control() || FORBIDDEN.contains(&c) {
                '_'
            } else {
                c
            }
        })
        .collect();

    // Windows silently drops trailing dots and spaces.
    let kept = replaced.trim_end_matches(['.', ' ']);
    let mut name = format!("{kept}{}", "_".repeat(replaced.len() - kept.len()));
    if name.is_empty() {
        name.push('_');
    }

    let device = name.split('.').next().unwrap_or_default();
    if RESERVED_NAMES
        .iter()
        .any(|r| r.eq_ignore_ascii_case(device))
    {
        name.insert(0, '_');
    }

    if name.len() > MAX_SEGMENT_BYTES {
        let (stem, ext) = split_ext(&name);
        let suffix = format!("~{}", hash_hex(segment, 8));
        let room = MAX_SEGMENT_BYTES - suffix.len() - ext.map_or(0, |e| e.len() + 1);
        let mut cut = room.min(stem.len());
        while !stem.is_char_boundary(cut) {
            cut -= 1;
        }
        name = join_ext(&format!("{}{suffix}", &stem[..cut]), ext);
    }
    name
}

/// FNV-1a: tiny, dependency-free and stable across versions and platforms,
/// which is all a file-name disambiguator needs.
pub(crate) fn fnv1a(input: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in input {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

pub(crate) fn hash_hex(input: &str, len: usize) -> String {
    let hex = format!("{:016x}", fnv1a(input.as_bytes()));
    hex[..len.min(16)].to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(url: &str, mime: Option<&str>) -> String {
        map_url(&Url::parse(url).unwrap(), mime).to_string()
    }

    #[test]
    fn maps_common_urls() {
        let html = Some("text/html");
        assert_eq!(path("https://s.test/", html), "s.test/index.html");
        assert_eq!(path("https://s.test/docs/", html), "s.test/docs/index.html");
        assert_eq!(path("https://s.test/about", html), "s.test/about.html");
        assert_eq!(path("https://s.test/a/b.html", html), "s.test/a/b.html");
        assert_eq!(
            path("https://s.test/page.php", html),
            "s.test/page.php.html"
        );
        assert_eq!(
            path("https://s.test/s.css", Some("text/css")),
            "s.test/s.css"
        );
        assert_eq!(
            path("https://s.test/style.php", Some("text/css")),
            "s.test/style.php.css"
        );
        assert_eq!(
            path("https://s.test/logo", Some("image/png")),
            "s.test/logo.png"
        );
        assert_eq!(
            path("https://s.test/photo.JPEG", Some("image/jpeg")),
            "s.test/photo.JPEG"
        );
        // Non-strict types keep their served extension.
        assert_eq!(
            path("https://s.test/data.bin", Some("image/png")),
            "s.test/data.bin"
        );
        assert_eq!(path("https://s.test/blob", None), "s.test/blob");
        assert_eq!(path("http://s.test:8080/", html), "s.test_8080/index.html");
    }

    #[test]
    fn query_strings_get_a_stable_hash() {
        let a = path("https://s.test/app.js?v=3", Some("text/javascript"));
        assert!(a.starts_with("s.test/app.") && a.ends_with(".js"), "{a}");
        assert_eq!(
            a,
            path("https://s.test/app.js?v=3", Some("text/javascript"))
        );
        assert_ne!(
            a,
            path("https://s.test/app.js?v=4", Some("text/javascript"))
        );
        assert!(path("https://s.test/search?q=x", Some("text/html")).ends_with(".html"));
    }

    #[test]
    fn decodes_and_normalizes_unicode() {
        assert_eq!(
            path("https://s.test/posts/hello%20world.html", Some("text/html")),
            "s.test/posts/hello world.html"
        );
        // NFD "e + combining acute" becomes NFC "é".
        assert_eq!(
            path("https://s.test/cafe%CC%81.html", Some("text/html")),
            "s.test/café.html"
        );
    }

    #[test]
    fn produces_windows_safe_names() {
        assert_eq!(
            path("https://s.test/a%3Ab%3F.png", Some("image/png")),
            "s.test/a_b_.png"
        );
        assert_eq!(
            path("https://s.test/CON.html", Some("text/html")),
            "s.test/_CON.html"
        );
        assert_eq!(
            path("https://s.test/con", Some("text/html")),
            "s.test/_con.html"
        );
        assert_eq!(
            path("https://s.test/trail.%20./x.png", Some("image/png")),
            "s.test/trail___/x.png"
        );
        // `%2e%2e` is resolved away by URL parsing; three dots survive it.
        assert_eq!(
            path("https://s.test/%2e%2e%2e/x.png", Some("image/png")),
            "s.test/___/x.png"
        );
    }

    #[test]
    fn caps_segment_and_path_length() {
        let long = "a".repeat(300);
        let p = path(&format!("https://s.test/{long}.png"), Some("image/png"));
        let name = p.rsplit('/').next().unwrap();
        assert!(
            name.len() <= MAX_SEGMENT_BYTES && name.ends_with(".png"),
            "{name}"
        );

        let deep = vec!["dir"; 80].join("/");
        let p = path(&format!("https://s.test/{deep}/x.css"), Some("text/css"));
        assert!(
            p.starts_with("s.test/_long/") && p.ends_with(".css") && p.len() <= MAX_PATH_BYTES,
            "{p}"
        );
    }

    #[test]
    fn relative_hrefs() {
        let from = |s: &str| LocalPath(s.split('/').map(str::to_owned).collect());
        let index = from("h/index.html");
        assert_eq!(
            index.relative_href(&from("h/posts/hello world.html")),
            "posts/hello%20world.html"
        );
        assert_eq!(
            index.relative_href(&from("h/posts/café.html")),
            "posts/caf%C3%A9.html"
        );
        assert_eq!(
            from("h/posts/a.html").relative_href(&from("h/index.html")),
            "../index.html"
        );
        assert_eq!(
            from("h/a/b/c.html").relative_href(&from("h/a/x/y.css")),
            "../x/y.css"
        );
        assert_eq!(index.relative_href(&index), "index.html");
        assert_eq!(
            index.relative_href(&from("other/index.html")),
            "../other/index.html"
        );
        assert_eq!(
            index.relative_href(&from("h/a (1)&b#c.png")),
            "a%20%281%29%26b%23c.png"
        );
    }

    #[test]
    fn allocator_separates_case_collisions() {
        let mut alloc = PathAllocator::default();
        let upper = Url::parse("https://s.test/Page.html").unwrap();
        let lower = Url::parse("https://s.test/page.html").unwrap();
        let first = alloc.allocate(&upper, Some("text/html"));
        let second = alloc.allocate(&lower, Some("text/html"));
        assert_eq!(first.to_string(), "s.test/Page.html");
        assert!(
            second.to_string().starts_with("s.test/page~") && second.to_string().ends_with(".html")
        );
        // The same URL asked again keeps its path.
        assert_eq!(alloc.allocate(&upper, Some("text/html")), first);
    }
}
