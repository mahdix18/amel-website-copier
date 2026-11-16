//! Large files resume after a dropped connection; scripts and file-type
//! rules work from the command line.

#![allow(clippy::unwrap_used)]

use mc_fixtures::{FixtureServer, Site};

async fn amel(args: &[&str]) -> std::process::Output {
    tokio::process::Command::new(env!("CARGO_BIN_EXE_amel"))
        .args(args)
        .output()
        .await
        .unwrap()
}

fn find(dir: &std::path::Path, name: &str) -> Option<std::path::PathBuf> {
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if let Some(found) = find(&path, name) {
                return Some(found);
            }
        } else if path.file_name().is_some_and(|n| n == name) {
            return Some(path);
        }
    }
    None
}

#[tokio::test]
async fn a_large_file_resumes_after_the_connection_drops() {
    let site = FixtureServer::start(Site::Hostile).await.unwrap();
    let out = tempfile::tempdir().unwrap();
    let output = amel(&[
        "copy",
        site.url("/big.bin").as_str(),
        "-o",
        out.path().to_str().unwrap(),
        "--delay-ms",
        "0",
        "--retry-delay-ms",
        "10",
        "--ignore-robots",
    ])
    .await;
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert!(
        stderr.contains("retry"),
        "the first try broke off: {stderr}"
    );
    let file = find(out.path(), "big.bin").expect("the file was saved");
    let body = std::fs::read(file).unwrap();
    assert_eq!(body.len(), mc_fixtures::BIG_BYTES);
    assert!(
        body.iter()
            .enumerate()
            .all(|(i, b)| *b == mc_fixtures::big_byte(i))
    );
    // Nothing is left half-downloaded.
    let partial = out.path().join(".amel/partial");
    let leftovers = std::fs::read_dir(&partial).map_or(0, |d| d.count());
    assert_eq!(leftovers, 0);
}

#[tokio::test]
async fn scripts_and_type_rules_run_from_the_command_line() {
    let site = FixtureServer::start(Site::StaticBlog).await.unwrap();
    let out = tempfile::tempdir().unwrap();
    let script = out.path().join("hooks.js");
    std::fs::write(
        &script,
        r#"
        function onStart(info) { log("start " + info.url) }
        function shouldFetch(url) { return !url.endsWith("about/") }
        function onFinish(s) { log("finish " + s.saved) }
        "#,
    )
    .unwrap();
    let copies = out.path().join("copy");
    let output = amel(&[
        "copy",
        site.base_url().as_str(),
        "-o",
        copies.to_str().unwrap(),
        "--delay-ms",
        "0",
        "--skip-types",
        "image",
        "--script",
        script.to_str().unwrap(),
    ])
    .await;
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "{stdout}{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains("script: start "), "{stdout}");
    assert!(stdout.contains("script: finish "), "{stdout}");
    assert!(find(&copies, "logo.svg").is_none(), "images were left out");
    assert!(find(&copies, "style.css").is_some());
    assert!(!copies.join("127.0.0.1").exists() || find(&copies, "about").is_none());

    let bad = amel(&[
        "copy",
        site.base_url().as_str(),
        "-o",
        out.path().join("bad").to_str().unwrap(),
        "--script",
        out.path().join("missing.js").to_str().unwrap(),
    ])
    .await;
    assert_eq!(bad.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&bad.stderr).contains("cannot read script"));
}
