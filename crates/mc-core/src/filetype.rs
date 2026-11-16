//! File types, for rules such as "skip videos" or "images up to 5 MB": a
//! category from the response's MIME type, or guessed from the URL's
//! extension before anything is downloaded.

use std::fmt;
use std::str::FromStr;

use url::Url;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FileType {
    /// HTML pages.
    Page,
    Style,
    Script,
    Image,
    Font,
    Video,
    Audio,
    /// PDF, office documents, plain text.
    Document,
    /// Zip, tar, installers and other bundles.
    Archive,
    /// JSON, XML and other data.
    Data,
    Other,
}

impl FileType {
    pub const ALL: [Self; 11] = [
        Self::Page,
        Self::Style,
        Self::Script,
        Self::Image,
        Self::Font,
        Self::Video,
        Self::Audio,
        Self::Document,
        Self::Archive,
        Self::Data,
        Self::Other,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Page => "page",
            Self::Style => "style",
            Self::Script => "script",
            Self::Image => "image",
            Self::Font => "font",
            Self::Video => "video",
            Self::Audio => "audio",
            Self::Document => "document",
            Self::Archive => "archive",
            Self::Data => "data",
            Self::Other => "other",
        }
    }

    /// The category of a MIME type such as `image/png; charset=…`.
    pub fn from_mime(mime: &str) -> Self {
        let mime = mime
            .split(';')
            .next()
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();
        let (top, sub) = mime.split_once('/').unwrap_or((mime.as_str(), ""));
        match (top, sub) {
            ("text", "html") | ("application", "xhtml+xml") => Self::Page,
            ("text", "css") => Self::Style,
            ("text" | "application", "javascript" | "ecmascript" | "x-javascript")
            | ("application", "wasm") => Self::Script,
            ("image", _) => Self::Image,
            ("font", _)
            | ("application", "font-woff" | "font-woff2" | "x-font-ttf" | "vnd.ms-fontobject") => {
                Self::Font
            }
            ("video", _) | ("application", "vnd.apple.mpegurl" | "x-mpegurl" | "dash+xml") => {
                Self::Video
            }
            ("audio", _) => Self::Audio,
            (
                "application",
                "json" | "xml" | "ld+json" | "manifest+json" | "rss+xml" | "atom+xml",
            )
            | ("text", "xml" | "csv") => Self::Data,
            ("application", "pdf" | "msword" | "rtf" | "epub+zip")
            | ("text", "plain" | "markdown") => Self::Document,
            ("application", sub)
                if sub.starts_with("vnd.openxmlformats")
                    || sub.starts_with("vnd.ms-")
                    || sub.starts_with("vnd.oasis.opendocument") =>
            {
                Self::Document
            }
            (
                "application",
                "zip"
                | "gzip"
                | "x-gzip"
                | "x-tar"
                | "x-7z-compressed"
                | "x-rar-compressed"
                | "vnd.rar"
                | "x-bzip2"
                | "x-xz"
                | "zstd"
                | "x-msdownload"
                | "vnd.debian.binary-package"
                | "x-apple-diskimage"
                | "java-archive",
            ) => Self::Archive,
            _ => Self::Other,
        }
    }

    /// The category the URL's extension suggests, before any request.
    /// `None` when the path has no telling extension (most pages).
    pub fn guess(url: &Url) -> Option<Self> {
        let last = url.path_segments()?.next_back()?;
        let (_, ext) = last.rsplit_once('.')?;
        let ext = ext.to_ascii_lowercase();
        let kind = match ext.as_str() {
            // mime_guess has these wrong or missing.
            "m3u8" | "mpd" => Self::Video,
            "woff" | "woff2" | "ttf" | "otf" | "eot" => Self::Font,
            "dmg" | "deb" | "rpm" | "msi" | "exe" | "apk" | "jar" | "iso" | "7z" | "rar" => {
                Self::Archive
            }
            _ => Self::from_mime(mime_guess::from_ext(&ext).first_raw()?),
        };
        Some(kind)
    }
}

impl fmt::Display for FileType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Page => "pages",
            Self::Style => "styles",
            Self::Script => "scripts",
            Self::Image => "images",
            Self::Font => "fonts",
            Self::Video => "videos",
            Self::Audio => "audio",
            Self::Document => "documents",
            Self::Archive => "archives",
            Self::Data => "data files",
            Self::Other => "other files",
        })
    }
}

impl FromStr for FileType {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let lower = text.trim().to_ascii_lowercase();
        let name = lower.strip_suffix('s').unwrap_or(&lower);
        Self::ALL
            .into_iter()
            .find(|kind| kind.as_str() == name || kind.as_str() == lower)
            .ok_or_else(|| {
                format!(
                    "unknown file type `{lower}`; use {}",
                    Self::ALL.map(Self::as_str).join(", ")
                )
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[test]
    fn classifies_mime_types() {
        assert_eq!(
            FileType::from_mime("text/html; charset=utf-8"),
            FileType::Page
        );
        assert_eq!(FileType::from_mime("IMAGE/PNG"), FileType::Image);
        assert_eq!(FileType::from_mime("font/woff2"), FileType::Font);
        assert_eq!(FileType::from_mime("video/mp4"), FileType::Video);
        assert_eq!(
            FileType::from_mime("application/vnd.apple.mpegurl"),
            FileType::Video
        );
        assert_eq!(FileType::from_mime("audio/mpeg"), FileType::Audio);
        assert_eq!(FileType::from_mime("application/pdf"), FileType::Document);
        assert_eq!(
            FileType::from_mime(
                "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
            ),
            FileType::Document
        );
        assert_eq!(FileType::from_mime("application/zip"), FileType::Archive);
        assert_eq!(FileType::from_mime("application/json"), FileType::Data);
        assert_eq!(FileType::from_mime("text/javascript"), FileType::Script);
        assert_eq!(
            FileType::from_mime("application/x-unknown"),
            FileType::Other
        );
    }

    #[test]
    fn guesses_from_the_url() {
        let guess = |s: &str| FileType::guess(&url(s));
        assert_eq!(
            guess("https://s.test/a/movie.MP4?x=1"),
            Some(FileType::Video)
        );
        assert_eq!(guess("https://s.test/logo.svg"), Some(FileType::Image));
        assert_eq!(guess("https://s.test/f.woff2"), Some(FileType::Font));
        assert_eq!(guess("https://s.test/setup.exe"), Some(FileType::Archive));
        assert_eq!(guess("https://s.test/report.pdf"), Some(FileType::Document));
        assert_eq!(guess("https://s.test/live.m3u8"), Some(FileType::Video));
        assert_eq!(guess("https://s.test/about/"), None);
        assert_eq!(guess("https://s.test/page"), None);
    }

    #[test]
    fn parses_names() {
        assert_eq!("videos".parse::<FileType>(), Ok(FileType::Video));
        assert_eq!("Image".parse::<FileType>(), Ok(FileType::Image));
        assert_eq!("audio".parse::<FileType>(), Ok(FileType::Audio));
        assert!("movies".parse::<FileType>().is_err());
    }
}
