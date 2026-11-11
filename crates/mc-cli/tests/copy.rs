//! End-to-end copies of the fixture sites over real HTTP, checked offline.

#![allow(clippy::unwrap_used)]

use std::path::{Path, PathBuf};
use std::time::Duration;

use mc_core::{CrawlConfig, CrawlSummary, Crawler};
use mc_extract::{Url, extract_css, extract_html};
use mc_fixtures::{FixtureServer, Site};
use mc_requester::{RequesterFactory, SharedResources};
use mc_requester_wreq::WreqFactory;

async fn copy(site: Site) -> (tempfile::TempDir, PathBuf, CrawlSummary) {
    let server = FixtureServer::start(site).await.unwrap();
    let out = tempfile::tempdir().unwrap();
    let requester = WreqFactory
        .build(&serde_json::json!({}), &SharedResources::default())
        .unwrap();
    let mut config = CrawlConfig::new(server.base_url(), out.path());
    config.per_host_delay = Duration::ZERO;
    let summary = Crawler::new(config, requester).run().await.unwrap();
    let root = out
        .path()
        .join(format!("127.0.0.1_{}", server.addr().port()));
    server.stop().await.unwrap();
    (out, root, summary)
}

fn files(dir: &Path) -> Vec<PathBuf> {
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

/// Every reference in every saved document either resolves to a file on
/// disk or points at a site other than the one copied.
fn assert_links_resolve_offline(root: &Path) {
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
                _ => link.url.host_str() != Some("127.0.0.1"),
            };
            if !ok {
                broken.push(format!("{} -> {}", file.display(), link.url));
            }
        }
    }
    assert!(broken.is_empty(), "broken links:\n{}", broken.join("\n"));
}

#[tokio::test]
async fn copies_the_static_blog() {
    let (_out, root, summary) = copy(Site::StaticBlog).await;
    assert!(summary.failed.is_empty(), "{:?}", summary.failed);

    for file in [
        "index.html",
        "about/index.html",
        "posts/hello world.html",
        "posts/café.html",
        "assets/style.css",
        "assets/fonts.css",
        "images/logo.svg",
        "images/logo-2x.svg",
        "images/bg.svg",
        "images/pattern.svg",
    ] {
        assert!(root.join(file).is_file(), "missing {file}");
    }
    let scripts: Vec<_> = files(&root.join("assets"))
        .into_iter()
        .filter(|p| p.extension().is_some_and(|e| e == "js"))
        .collect();
    assert_eq!(scripts.len(), 1, "{scripts:?}");
    assert!(
        scripts[0]
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("app.")
    );

    let index = std::fs::read_to_string(root.join("index.html")).unwrap();
    assert!(
        index.contains(r#"href="posts/hello%20world.html""#),
        "{index}"
    );
    assert!(index.contains(r#"href="posts/caf%C3%A9.html""#), "{index}");
    assert!(
        index.contains(r#"href="posts/hello%20world.html#comments""#),
        "{index}"
    );
    assert!(
        index.contains(r#"<a href="about/index.html">A link that redirects</a>"#),
        "{index}"
    );
    assert!(index.contains(r#"href="https://example.com/""#), "{index}");

    assert_links_resolve_offline(&root);
}

#[tokio::test]
async fn copies_the_ua_gated_site() {
    let (_out, root, summary) = copy(Site::UaGate).await;
    assert!(summary.failed.is_empty(), "{:?}", summary.failed);
    for file in ["index.html", "page2.html", "style.css"] {
        assert!(root.join(file).is_file(), "missing {file}");
    }
    assert_links_resolve_offline(&root);
}

#[tokio::test]
async fn cli_copies_and_reports_exit_codes() {
    let server = FixtureServer::start(Site::StaticBlog).await.unwrap();
    let out = tempfile::tempdir().unwrap();
    let bin = env!("CARGO_BIN_EXE_mistercopy");

    let status = tokio::process::Command::new(bin)
        .args([
            "copy",
            server.base_url().as_str(),
            "--delay-ms",
            "0",
            "--quiet",
            "-o",
        ])
        .arg(out.path())
        .status()
        .await
        .unwrap();
    assert!(status.success());
    let root = out
        .path()
        .join(format!("127.0.0.1_{}", server.addr().port()));
    assert!(root.join("index.html").is_file());

    let bad_regex = tokio::process::Command::new(bin)
        .args(["copy", server.base_url().as_str(), "--exclude", "(", "-o"])
        .arg(out.path())
        .output()
        .await
        .unwrap();
    assert_eq!(bad_regex.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&bad_regex.stderr).contains("invalid regex"));

    let bad_profile = tokio::process::Command::new(bin)
        .args([
            "copy",
            server.base_url().as_str(),
            "--profile",
            "netscape_4",
            "-o",
        ])
        .arg(out.path())
        .output()
        .await
        .unwrap();
    assert_eq!(bad_profile.status.code(), Some(2));

    let listed = tokio::process::Command::new(bin)
        .args(["requesters", "list"])
        .output()
        .await
        .unwrap();
    assert!(String::from_utf8_lossy(&listed.stdout).contains("wreq"));
    server.stop().await.unwrap();
}
