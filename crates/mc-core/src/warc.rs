//! WARC/1.1 output: each response the crawl saves, as served, in the
//! standard web-archive format. Every record is its own gzip member, so the
//! file is a valid `.warc.gz` that archive tools can index and seek into.

use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use flate2::Compression;
use flate2::write::GzEncoder;
use mc_requester::{HeaderMap, StatusCode, header};
use url::Url;

use crate::path::fnv1a;

pub(crate) struct WarcWriter {
    out: BufWriter<File>,
    /// Makes record ids unique within the file.
    salt: String,
    count: u64,
}

impl WarcWriter {
    /// Opens `path` for appending, so a resumed crawl adds to the same
    /// archive. A new file starts with a `warcinfo` record.
    pub(crate) fn open(path: &Path) -> io::Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        let is_new = file.metadata()?.len() == 0;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        let mut writer = Self {
            out: BufWriter::new(file),
            salt: format!("{}{}", path.display(), now.as_nanos()),
            count: 0,
        };
        if is_new {
            let info = format!(
                "software: MisterCopy {}\r\nformat: WARC File Format 1.1\r\n",
                env!("CARGO_PKG_VERSION")
            );
            writer.record("warcinfo", None, "application/warc-fields", info.as_bytes())?;
        }
        Ok(writer)
    }

    /// One `response` record: the HTTP status line, headers and body. Bodies
    /// are stored decoded, so encoding and length headers are rewritten.
    pub(crate) fn response(
        &mut self,
        url: &Url,
        status: StatusCode,
        headers: &HeaderMap,
        body: &[u8],
    ) -> io::Result<()> {
        let mut http = format!(
            "HTTP/1.1 {} {}\r\n",
            status.as_u16(),
            status.canonical_reason().unwrap_or_default()
        );
        for (name, value) in headers {
            if [
                header::CONTENT_ENCODING,
                header::TRANSFER_ENCODING,
                header::CONTENT_LENGTH,
            ]
            .contains(name)
            {
                continue;
            }
            http.push_str(&format!(
                "{name}: {}\r\n",
                String::from_utf8_lossy(value.as_bytes())
            ));
        }
        http.push_str(&format!("content-length: {}\r\n\r\n", body.len()));
        let mut block = http.into_bytes();
        block.extend_from_slice(body);
        self.record(
            "response",
            Some(url),
            "application/http;msgtype=response",
            &block,
        )
    }

    fn record(
        &mut self,
        kind: &str,
        target: Option<&Url>,
        content_type: &str,
        block: &[u8],
    ) -> io::Result<()> {
        self.count += 1;
        let mut head = format!(
            "WARC/1.1\r\nWARC-Type: {kind}\r\nWARC-Record-ID: <urn:uuid:{}>\r\nWARC-Date: {}\r\n",
            self.record_id(),
            warc_date(SystemTime::now())
        );
        if let Some(url) = target {
            head.push_str(&format!("WARC-Target-URI: {url}\r\n"));
        }
        head.push_str(&format!(
            "Content-Type: {content_type}\r\nContent-Length: {}\r\n\r\n",
            block.len()
        ));

        let mut gz = GzEncoder::new(Vec::new(), Compression::default());
        gz.write_all(head.as_bytes())?;
        gz.write_all(block)?;
        gz.write_all(b"\r\n\r\n")?;
        self.out.write_all(&gz.finish()?)
    }

    pub(crate) fn flush(&mut self) -> io::Result<()> {
        self.out.flush()
    }

    /// A version-4-shaped UUID from hashes; unique, not cryptographic.
    fn record_id(&self) -> String {
        let a = fnv1a(format!("{}#{}a", self.salt, self.count).as_bytes());
        let b = fnv1a(format!("{}#{}b", self.salt, self.count).as_bytes());
        let hex = format!("{a:016x}{b:016x}");
        format!(
            "{}-{}-4{}-{}{}-{}",
            &hex[0..8],
            &hex[8..12],
            &hex[13..16],
            ["8", "9", "a", "b"][(b % 4) as usize],
            &hex[17..20],
            &hex[20..32]
        )
    }
}

/// `YYYY-MM-DDThh:mm:ssZ` in UTC, as WARC requires.
fn warc_date(time: SystemTime) -> String {
    let secs = time
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let (days, rest) = (secs / 86_400, secs % 86_400);
    // Days since 1970-01-01 to a civil date (Howard Hinnant's algorithm).
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::time::Duration;

    #[test]
    fn formats_dates() {
        assert_eq!(warc_date(UNIX_EPOCH), "1970-01-01T00:00:00Z");
        let leap = UNIX_EPOCH + Duration::from_secs(1_709_210_096); // 2024-02-29T12:34:56Z
        assert_eq!(warc_date(leap), "2024-02-29T12:34:56Z");
    }

    #[test]
    fn writes_gzip_members_with_correct_lengths() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("crawl.warc.gz");
        let mut warc = WarcWriter::open(&path).unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(header::CONTENT_TYPE, "text/html".parse().unwrap());
        headers.insert(header::CONTENT_ENCODING, "gzip".parse().unwrap());
        warc.response(
            &Url::parse("https://s.test/").unwrap(),
            StatusCode::OK,
            &headers,
            b"<p>hi</p>",
        )
        .unwrap();
        warc.flush().unwrap();
        drop(warc);
        // Reopening appends without a second warcinfo.
        let mut again = WarcWriter::open(&path).unwrap();
        again
            .response(
                &Url::parse("https://s.test/b").unwrap(),
                StatusCode::NOT_FOUND,
                &HeaderMap::new(),
                b"",
            )
            .unwrap();
        again.flush().unwrap();

        let mut text = String::new();
        flate2::read::MultiGzDecoder::new(File::open(&path).unwrap())
            .read_to_string(&mut text)
            .unwrap();
        assert_eq!(text.matches("WARC/1.1\r\n").count(), 3);
        assert_eq!(text.matches("WARC-Type: warcinfo").count(), 1);
        assert!(text.contains("WARC-Target-URI: https://s.test/\r\n"));
        assert!(text.contains(
            "HTTP/1.1 200 OK\r\ncontent-type: text/html\r\ncontent-length: 9\r\n\r\n<p>hi</p>"
        ));
        assert!(
            !text.contains("gzip"),
            "decoded bodies drop Content-Encoding"
        );
        assert!(text.contains("HTTP/1.1 404 Not Found"));

        // Each declared Content-Length covers exactly the block before "\r\n\r\n".
        for record in text.split("WARC/1.1\r\n").skip(1) {
            let (head, rest) = record.split_once("\r\n\r\n").unwrap();
            let length: usize = head
                .lines()
                .find_map(|l| l.strip_prefix("Content-Length: "))
                .unwrap()
                .parse()
                .unwrap();
            assert_eq!(&rest[length..length + 4], "\r\n\r\n");
            let id = head
                .lines()
                .find_map(|l| l.strip_prefix("WARC-Record-ID: <urn:uuid:"))
                .unwrap();
            assert_eq!(id.len(), 37, "{id}");
        }
    }
}
