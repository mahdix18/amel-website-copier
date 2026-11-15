//! Sitemaps: the `Sitemap:` lines of robots.txt and the files they name
//! (XML url sets, sitemap indexes, gzipped XML, plain-text lists). They seed
//! the crawl with pages nothing links to.

use std::io::Read;

use url::Url;

/// Largest decompressed sitemap read; the format caps files at 50 MB.
const MAX_SITEMAP_BYTES: u64 = 64 * 1024 * 1024;

/// What one sitemap file lists.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Sitemap {
    pub pages: Vec<Url>,
    /// Further sitemaps, from a sitemap index.
    pub sitemaps: Vec<Url>,
}

/// The sitemap URLs a robots.txt names.
pub(crate) fn from_robots(body: &[u8], base: &Url) -> Vec<Url> {
    String::from_utf8_lossy(body)
        .lines()
        .filter_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.trim()
                .eq_ignore_ascii_case("sitemap")
                .then(|| base.join(value.trim()).ok())
                .flatten()
        })
        .collect()
}

/// Reads one sitemap file, gzipped or not.
pub(crate) fn parse(body: &[u8], base: &Url) -> Sitemap {
    let text = if body.starts_with(&[0x1f, 0x8b]) {
        let mut out = Vec::new();
        let decoder = flate2::read::MultiGzDecoder::new(body);
        if decoder
            .take(MAX_SITEMAP_BYTES)
            .read_to_end(&mut out)
            .is_err()
        {
            return Sitemap::default();
        }
        String::from_utf8_lossy(&out).into_owned()
    } else {
        String::from_utf8_lossy(body).into_owned()
    };

    if !text.trim_start().starts_with('<') {
        // A plain-text sitemap: one absolute URL per line.
        let pages = text
            .lines()
            .filter_map(|line| Url::parse(line.trim()).ok())
            .filter(|url| matches!(url.scheme(), "http" | "https"))
            .collect();
        return Sitemap {
            pages,
            sitemaps: Vec::new(),
        };
    }

    let is_index = text.contains("<sitemapindex");
    let locs = locs(&text)
        .filter_map(|loc| base.join(&loc).ok())
        .filter(|url| matches!(url.scheme(), "http" | "https"));
    if is_index {
        Sitemap {
            pages: Vec::new(),
            sitemaps: locs.collect(),
        }
    } else {
        Sitemap {
            pages: locs.collect(),
            sitemaps: Vec::new(),
        }
    }
}

/// The text of every `<loc>` element, entities and CDATA undone. Namespaced
/// `<image:loc>` and friends are left out: they name assets, not pages.
fn locs(xml: &str) -> impl Iterator<Item = String> + '_ {
    xml.split("<loc>").skip(1).filter_map(|rest| {
        let (inner, _) = rest.split_once("</loc>")?;
        let inner = inner.trim();
        let inner = inner
            .strip_prefix("<![CDATA[")
            .and_then(|s| s.strip_suffix("]]>"))
            .unwrap_or(inner)
            .trim();
        Some(
            inner
                .replace("&lt;", "<")
                .replace("&gt;", ">")
                .replace("&quot;", "\"")
                .replace("&apos;", "'")
                .replace("&amp;", "&"),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn base() -> Url {
        Url::parse("https://s.test/").unwrap()
    }

    #[test]
    fn reads_robots_lines() {
        let robots =
            b"User-agent: *\nDisallow: /x\nSitemap: https://s.test/a.xml\nsitemap:/b.xml.gz\n";
        let urls = from_robots(robots, &base());
        assert_eq!(
            urls.iter().map(Url::as_str).collect::<Vec<_>>(),
            ["https://s.test/a.xml", "https://s.test/b.xml.gz"]
        );
    }

    #[test]
    fn reads_url_sets_indexes_text_and_gzip() {
        let set = r#"<?xml version="1.0"?>
            <urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9"
                    xmlns:image="http://www.google.com/schemas/sitemap-image/1.1">
              <url><loc>https://s.test/a?x=1&amp;y=2</loc>
                <image:image><image:loc>https://s.test/i.png</image:loc></image:image></url>
              <url><loc> <![CDATA[https://s.test/b]]> </loc></url>
              <url><loc>ftp://s.test/no</loc></url>
            </urlset>"#;
        let parsed = parse(set.as_bytes(), &base());
        assert_eq!(
            parsed.pages.iter().map(Url::as_str).collect::<Vec<_>>(),
            ["https://s.test/a?x=1&y=2", "https://s.test/b"]
        );
        assert!(parsed.sitemaps.is_empty());

        let index =
            "<sitemapindex><sitemap><loc>https://s.test/p1.xml</loc></sitemap></sitemapindex>";
        let parsed = parse(index.as_bytes(), &base());
        assert!(parsed.pages.is_empty());
        assert_eq!(parsed.sitemaps[0].as_str(), "https://s.test/p1.xml");

        let text = "https://s.test/one\n\nnot a url\nhttps://s.test/two\n";
        assert_eq!(parse(text.as_bytes(), &base()).pages.len(), 2);

        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(set.as_bytes()).unwrap();
        assert_eq!(parse(&gz.finish().unwrap(), &base()).pages.len(), 2);
    }
}
