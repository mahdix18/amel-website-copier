//! The end-of-crawl report, as JSON and as a standalone HTML page.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use serde::Serialize;

use crate::rules::SkipReason;
use crate::store::{EntryState, UrlEntry};

/// External links listed in full; beyond this only the count is kept.
const MAX_EXTERNAL_LINKS: usize = 1000;
const LARGEST_FILES: usize = 10;

#[derive(Debug, Clone, Default, Serialize)]
pub struct Report {
    pub generator: String,
    pub seeds: Vec<String>,
    /// `complete`, or why the crawl stopped early.
    pub status: String,
    pub elapsed_secs: f64,
    pub totals: Totals,
    pub failures: Vec<Failure>,
    pub redirects: Vec<RedirectEntry>,
    /// Skipped URL counts, by reason.
    pub skipped: BTreeMap<String, usize>,
    pub external_links: Vec<String>,
    /// Saved files and bytes, by media type.
    pub types: BTreeMap<String, TypeStats>,
    pub largest: Vec<FileEntry>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Totals {
    pub saved: usize,
    pub unchanged: usize,
    pub bytes: u64,
    pub failed: usize,
    pub skipped: usize,
    pub redirects: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct Failure {
    pub url: String,
    pub reason: String,
    /// The page that linked to it.
    pub referrer: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RedirectEntry {
    pub from: String,
    pub to: String,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct TypeStats {
    pub files: usize,
    pub bytes: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct FileEntry {
    pub url: String,
    pub path: String,
    pub bytes: u64,
}

impl Report {
    pub(crate) fn build<'a>(
        entries: impl Iterator<Item = &'a UrlEntry>,
        seeds: Vec<String>,
        status: String,
        elapsed_secs: f64,
        unchanged: usize,
    ) -> Self {
        let mut entries: Vec<&UrlEntry> = entries.collect();
        entries.sort_by(|a, b| a.url.cmp(&b.url));

        let mut report = Report {
            generator: format!("MisterCopy {}", env!("CARGO_PKG_VERSION")),
            seeds,
            status,
            elapsed_secs,
            ..Report::default()
        };
        report.totals.unchanged = unchanged;
        let mut files = Vec::new();

        for entry in entries {
            match &entry.state {
                EntryState::Saved(saved) => {
                    report.totals.saved += 1;
                    report.totals.bytes += saved.bytes;
                    let mime = saved.mime.clone().unwrap_or_else(|| "unknown".to_owned());
                    let stats = report.types.entry(mime).or_default();
                    stats.files += 1;
                    stats.bytes += saved.bytes;
                    files.push(FileEntry {
                        url: entry.url.to_string(),
                        path: saved.path.to_string(),
                        bytes: saved.bytes,
                    });
                }
                EntryState::Failed(reason) => report.failures.push(Failure {
                    url: entry.url.to_string(),
                    reason: reason.clone(),
                    referrer: entry.referrer.as_ref().map(ToString::to_string),
                }),
                EntryState::Alias(to) | EntryState::Remote(to) => {
                    report.redirects.push(RedirectEntry {
                        from: entry.url.to_string(),
                        to: to.to_string(),
                    })
                }
                EntryState::Skipped(reason) => {
                    *report
                        .skipped
                        .entry(reason.as_str().to_owned())
                        .or_default() += 1;
                    if *reason == SkipReason::OutOfScope
                        && report.external_links.len() < MAX_EXTERNAL_LINKS
                    {
                        report.external_links.push(entry.url.to_string());
                    }
                }
                EntryState::Queued => {}
            }
        }

        report.totals.failed = report.failures.len();
        report.totals.redirects = report.redirects.len();
        report.totals.skipped = report.skipped.values().sum();
        files.sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.url.cmp(&b.url)));
        files.truncate(LARGEST_FILES);
        report.largest = files;
        report
    }

    /// A self-contained page: no scripts, no external resources.
    pub fn to_html(&self) -> String {
        let mut html = String::new();
        let t = &self.totals;
        let _ = write!(
            html,
            r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>MisterCopy report</title>
<style>
:root {{ color-scheme: light dark; --fg:#1f2328; --muted:#59636e; --bg:#fff; --line:#d1d9e0; --bad:#cf222e; }}
@media (prefers-color-scheme: dark) {{ :root {{ --fg:#e6edf3; --muted:#9198a1; --bg:#0d1117; --line:#3d444d; --bad:#ff7b72; }} }}
body {{ font: 15px/1.5 system-ui, sans-serif; color: var(--fg); background: var(--bg); max-width: 72rem; margin: 2rem auto; padding: 0 1rem; }}
h1 {{ margin-bottom: .2rem; }} .muted {{ color: var(--muted); }}
.stats {{ display: flex; flex-wrap: wrap; gap: 1rem; margin: 1.5rem 0; }}
.stat {{ border: 1px solid var(--line); border-radius: 8px; padding: .6rem 1rem; min-width: 7rem; }}
.stat b {{ display: block; font-size: 1.4rem; }} .bad b {{ color: var(--bad); }}
table {{ border-collapse: collapse; width: 100%; margin-bottom: 2rem; }}
th, td {{ text-align: left; padding: .35rem .6rem; border-bottom: 1px solid var(--line); vertical-align: top; overflow-wrap: anywhere; }}
th {{ color: var(--muted); font-weight: 600; }} td.num {{ text-align: right; white-space: nowrap; }}
</style></head><body>
<h1>MisterCopy report</h1>
<p class="muted">{generator} · {seeds} · {status} · {elapsed:.1}s</p>
<div class="stats">
<div class="stat"><b>{saved}</b>saved</div><div class="stat"><b>{size}</b>total size</div>
<div class="stat"><b>{unchanged}</b>unchanged</div><div class="stat{bad}"><b>{failed}</b>failed</div>
<div class="stat"><b>{skipped}</b>skipped</div><div class="stat"><b>{redirects}</b>redirects</div>
</div>
"#,
            generator = escape(&self.generator),
            seeds = escape(&self.seeds.join(", ")),
            status = escape(&self.status),
            elapsed = self.elapsed_secs,
            saved = t.saved,
            size = human_bytes(t.bytes),
            unchanged = t.unchanged,
            bad = if t.failed > 0 { " bad" } else { "" },
            failed = t.failed,
            skipped = t.skipped,
            redirects = t.redirects,
        );

        if !self.failures.is_empty() {
            html.push_str("<h2>Failures</h2><table><tr><th>URL</th><th>Reason</th><th>Linked from</th></tr>\n");
            for failure in &self.failures {
                let _ = writeln!(
                    html,
                    "<tr><td>{}</td><td>{}</td><td>{}</td></tr>",
                    escape(&failure.url),
                    escape(&failure.reason),
                    escape(failure.referrer.as_deref().unwrap_or("")),
                );
            }
            html.push_str("</table>\n");
        }

        html.push_str(
            "<h2>Files by type</h2><table><tr><th>Type</th><th>Files</th><th>Size</th></tr>\n",
        );
        for (mime, stats) in &self.types {
            let _ = writeln!(
                html,
                r#"<tr><td>{}</td><td class="num">{}</td><td class="num">{}</td></tr>"#,
                escape(mime),
                stats.files,
                human_bytes(stats.bytes),
            );
        }
        html.push_str("</table>\n<h2>Largest files</h2><table><tr><th>URL</th><th>Saved as</th><th>Size</th></tr>\n");
        for file in &self.largest {
            let _ = writeln!(
                html,
                r#"<tr><td>{}</td><td>{}</td><td class="num">{}</td></tr>"#,
                escape(&file.url),
                escape(&file.path),
                human_bytes(file.bytes),
            );
        }
        html.push_str("</table>\n");

        if !self.skipped.is_empty() {
            html.push_str("<h2>Skipped</h2><table><tr><th>Reason</th><th>URLs</th></tr>\n");
            for (reason, count) in &self.skipped {
                let _ = writeln!(
                    html,
                    r#"<tr><td>{}</td><td class="num">{count}</td></tr>"#,
                    escape(reason)
                );
            }
            html.push_str("</table>\n");
        }
        if !self.redirects.is_empty() {
            html.push_str("<h2>Redirects</h2><table><tr><th>From</th><th>To</th></tr>\n");
            for redirect in &self.redirects {
                let _ = writeln!(
                    html,
                    "<tr><td>{}</td><td>{}</td></tr>",
                    escape(&redirect.from),
                    escape(&redirect.to)
                );
            }
            html.push_str("</table>\n");
        }
        if !self.external_links.is_empty() {
            html.push_str("<h2>External links</h2><table><tr><th>URL</th></tr>\n");
            for link in &self.external_links {
                let _ = writeln!(html, "<tr><td>{}</td></tr>", escape(link));
            }
            html.push_str("</table>\n");
        }
        html.push_str("</body></html>\n");
        html
    }
}

fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
    out
}

pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::path::LocalPath;
    use crate::store::SavedFile;
    use mc_extract::LinkKind;
    use mc_requester::Validators;
    use url::Url;

    fn entry(url: &str, state: EntryState) -> UrlEntry {
        UrlEntry {
            url: Url::parse(url).unwrap(),
            depth: 0,
            kind: LinkKind::Page,
            referrer: Some(Url::parse("https://s.test/").unwrap()),
            follow_links: true,
            state,
        }
    }

    #[test]
    fn summarizes_entries_and_escapes_html() {
        let saved = |bytes| {
            EntryState::Saved(SavedFile {
                path: LocalPath::parse("s.test/index.html").unwrap(),
                mime: Some("text/html".to_owned()),
                document: None,
                bytes,
                validators: Validators::default(),
            })
        };
        let entries = [
            entry("https://s.test/", saved(100)),
            entry("https://s.test/b", saved(300)),
            entry(
                "https://s.test/<x>",
                EntryState::Failed("HTTP 404 Not Found".to_owned()),
            ),
            entry(
                "https://other.test/",
                EntryState::Skipped(SkipReason::OutOfScope),
            ),
            entry(
                "https://s.test/old",
                EntryState::Alias(Url::parse("https://s.test/b").unwrap()),
            ),
        ];
        let report = Report::build(
            entries.iter(),
            vec!["https://s.test/".into()],
            "complete".into(),
            1.5,
            0,
        );

        assert_eq!(report.totals.saved, 2);
        assert_eq!(report.totals.bytes, 400);
        assert_eq!(report.totals.failed, 1);
        assert_eq!(
            report.failures[0].referrer.as_deref(),
            Some("https://s.test/")
        );
        assert_eq!(report.skipped.get("out-of-scope"), Some(&1));
        assert_eq!(report.external_links, ["https://other.test/"]);
        assert_eq!(report.largest[0].bytes, 300);
        assert_eq!(report.types["text/html"].files, 2);

        let html = report.to_html();
        assert!(
            html.contains("https://s.test/%3Cx%3E"),
            "URLs are already percent-encoded"
        );
        assert!(!html.contains("<x>"));
        assert!(html.contains("HTTP 404 Not Found"));
    }

    #[test]
    fn formats_sizes() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(2048), "2.0 KiB");
        assert_eq!(human_bytes(5 * 1024 * 1024), "5.0 MiB");
    }
}
