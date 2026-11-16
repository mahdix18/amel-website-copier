//! `mistercopy retry`: fetch again what failed, end to end over real HTTP.

#![allow(clippy::unwrap_used)]

use mc_fixtures::{FixtureServer, Site};

async fn mistercopy(args: &[&str]) -> std::process::Output {
    tokio::process::Command::new(env!("CARGO_BIN_EXE_mistercopy"))
        .args(args)
        .output()
        .await
        .unwrap()
}

#[tokio::test]
async fn retry_recovers_a_failure_without_copying_again() {
    let site = FixtureServer::start(Site::Hostile).await.unwrap();
    let out = tempfile::tempdir().unwrap();
    let dir = out.path().to_str().unwrap();
    // No retries: /rate-limited (429 twice, then OK) fails the first time.
    let first = mistercopy(&[
        "copy",
        site.base_url().as_str(),
        "-o",
        dir,
        "--delay-ms",
        "0",
        "--retries",
        "0",
        // The hostile site is full of crawler traps; stay near the start.
        "--max-depth",
        "1",
        "--timeout-secs",
        "1",
        "--max-file-mb",
        "1",
        "-j",
        "8",
        "-q",
    ])
    .await;
    let stderr = String::from_utf8_lossy(&first.stderr);
    assert!(stderr.contains("rate-limited"), "{stderr}");

    // Its second refusal, then the answer.
    let second = mistercopy(&["retry", dir, "-q"]).await;
    assert!(String::from_utf8_lossy(&second.stderr).contains("rate-limited"));
    let third = mistercopy(&["retry", dir]).await;
    let stdout = String::from_utf8_lossy(&third.stdout);
    assert!(stdout.contains("Retrying"), "{stdout}");
    let stderr = String::from_utf8_lossy(&third.stderr);
    assert!(!stderr.contains("rate-limited"), "{stderr}");
    let saved = walk(out.path());
    assert!(
        saved.iter().any(|p| p.contains("rate-limited")),
        "{saved:?}"
    );
}

#[tokio::test]
async fn retry_on_a_clean_copy_does_nothing() {
    let site = FixtureServer::start(Site::StaticBlog).await.unwrap();
    let out = tempfile::tempdir().unwrap();
    let dir = out.path().to_str().unwrap();
    let copy = mistercopy(&[
        "copy",
        site.base_url().as_str(),
        "-o",
        dir,
        "--delay-ms",
        "0",
        "-q",
    ])
    .await;
    assert!(copy.status.success());
    let retry = mistercopy(&["retry", dir]).await;
    assert!(retry.status.success());
    assert!(String::from_utf8_lossy(&retry.stdout).contains("nothing to retry"));
}

fn walk(dir: &std::path::Path) -> Vec<String> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap().flatten() {
        let path = entry.path();
        if path.is_dir() {
            found.extend(walk(&path));
        } else {
            found.push(path.to_string_lossy().into_owned());
        }
    }
    found
}
