//! Helpers shared by the end-to-end tests.

#![allow(dead_code, clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use mc_extract::{Url, extract_css, extract_html};

pub fn files(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            found.extend(files(&path));
        } else {
            found.push(path);
        }
    }
    found
}

/// Every local reference in every saved document resolves to a file on disk.
/// With `strict`, references back to the copied server are errors too; turn
/// it off for sites where some URLs are meant to fail and stay remote.
pub fn assert_links_resolve_offline(root: &Path, strict: bool) {
    let mut broken = Vec::new();
    for file in files(root) {
        let name = file.file_name().unwrap().to_string_lossy().into_owned();
        assert!(
            !name.ends_with(".mc-tmp"),
            "leftover temp file {}",
            file.display()
        );
        let base = Url::from_file_path(&file).unwrap();
        let bytes = std::fs::read(&file).unwrap();
        let links = match file.extension().and_then(|e| e.to_str()) {
            Some("html") => extract_html(&bytes, None, &base).unwrap(),
            Some("css") => extract_css(&String::from_utf8_lossy(&bytes), &base),
            _ => continue,
        };
        for link in links {
            let ok = match link.url.scheme() {
                "file" => link.url.to_file_path().is_ok_and(|p| p.is_file()),
                _ => !strict || link.url.host_str() != Some("127.0.0.1"),
            };
            if !ok {
                broken.push(format!("{} -> {}", file.display(), link.url));
            }
        }
    }
    assert!(broken.is_empty(), "broken links:\n{}", broken.join("\n"));
}

/// Relative path → contents for every file under `dir`, leaving out crawl
/// state and reports.
pub fn tree(dir: &Path) -> BTreeMap<String, Vec<u8>> {
    files(dir)
        .into_iter()
        .filter_map(|path| {
            let rel = path
                .strip_prefix(dir)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            let skip = rel.starts_with(".amel/") || rel.starts_with("amel-report");
            (!skip).then(|| (rel, std::fs::read(&path).unwrap()))
        })
        .collect()
}

pub fn host_dir(out: &Path, port: u16) -> PathBuf {
    out.join(format!("127.0.0.1_{port}"))
}
