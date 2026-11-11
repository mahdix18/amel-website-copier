//! Authenticated copies of the `auth` fixture: HTTP Basic, a cookie file,
//! and (with the `browser` feature) a scripted form login.

#![allow(clippy::unwrap_used)]

mod common;

use std::process::Output;

use common::host_dir;
use mc_fixtures::auth::{PASSWORD, SESSION_COOKIE, USERNAME};
use mc_fixtures::{FixtureServer, Site};

async fn mistercopy(args: &[&str]) -> Output {
    tokio::process::Command::new(env!("CARGO_BIN_EXE_mistercopy"))
        .args(args)
        .output()
        .await
        .unwrap()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[tokio::test]
async fn basic_auth_unlocks_the_basic_area() {
    let server = FixtureServer::start(Site::Auth).await.unwrap();
    let seed = server.url("/basic/");
    let host = server.addr().ip().to_string();

    let without = tempfile::tempdir().unwrap();
    let denied = mistercopy(&[
        "copy",
        seed.as_str(),
        "--delay-ms",
        "0",
        "-q",
        "-o",
        without.path().to_str().unwrap(),
    ])
    .await;
    assert_eq!(denied.status.code(), Some(1), "{}", stderr(&denied));
    assert!(stderr(&denied).contains("401"));

    let with = tempfile::tempdir().unwrap();
    let auth = format!("{host}={USERNAME}");
    let allowed = tokio::process::Command::new(env!("CARGO_BIN_EXE_mistercopy"))
        .args([
            "copy",
            seed.as_str(),
            "--delay-ms",
            "0",
            "-q",
            "--auth",
            &auth,
            "-o",
            with.path().to_str().unwrap(),
        ])
        // The password comes from the environment, not the command line.
        .env("MISTERCOPY_PASSWORD", PASSWORD)
        .output()
        .await
        .unwrap();
    assert!(allowed.status.success(), "{}", stderr(&allowed));
    let root = host_dir(with.path(), server.addr().port());
    assert!(
        std::fs::read_to_string(root.join("basic/doc.html"))
            .unwrap()
            .contains("Basic document")
    );
    server.stop().await.unwrap();
}

#[tokio::test]
async fn cookie_file_opens_the_members_area_without_logging_out() {
    let server = FixtureServer::start(Site::Auth).await.unwrap();
    let out = tempfile::tempdir().unwrap();
    let cookies = out.path().join("cookies.txt");
    let (name, value) = SESSION_COOKIE.split_once('=').unwrap();
    std::fs::write(
        &cookies,
        format!("127.0.0.1\tFALSE\t/\tFALSE\t0\t{name}\t{value}\n"),
    )
    .unwrap();
    let dir = out.path().join("copy");

    let copied = mistercopy(&[
        "copy",
        server.url("/members/").as_str(),
        "--delay-ms",
        "0",
        "-q",
        "--cookies",
        cookies.to_str().unwrap(),
        "-o",
        dir.to_str().unwrap(),
    ])
    .await;
    assert!(copied.status.success(), "{}", stderr(&copied));
    let root = host_dir(&dir, server.addr().port());
    assert!(
        std::fs::read_to_string(root.join("members/index.html"))
            .unwrap()
            .contains("Welcome back")
    );
    assert!(
        std::fs::read_to_string(root.join("members/page2.html"))
            .unwrap()
            .contains("Members page 2")
    );
    // The logout link was seen but never followed.
    assert!(
        !server.requests().iter().any(|r| r == "/logout"),
        "{:#?}",
        server.requests()
    );
    server.stop().await.unwrap();
}

#[tokio::test]
async fn login_script_needs_a_browser_requester() {
    let out = tempfile::tempdir().unwrap();
    let script = out.path().join("login.json");
    std::fs::write(&script, "[]").unwrap();
    let refused = mistercopy(&[
        "copy",
        "http://127.0.0.1:9/",
        "--login-script",
        script.to_str().unwrap(),
        "-o",
        out.path().to_str().unwrap(),
    ])
    .await;
    assert_eq!(refused.status.code(), Some(2));
    assert!(stderr(&refused).contains("--requester playwright or auto"));
}

#[cfg(feature = "browser")]
#[tokio::test]
async fn scripted_login_shares_the_session_with_plain_http() {
    if mc_requester_playwright::installed_chromium().is_empty() {
        eprintln!("skipping: no Chromium installed (run `mistercopy browsers install`)");
        return;
    }
    let server = FixtureServer::start(Site::Auth).await.unwrap();
    let out = tempfile::tempdir().unwrap();
    let script = out.path().join("login.json");
    let steps = serde_json::json!([
        { "goto": server.url("/login").as_str() },
        { "fill": "#username", "value": USERNAME },
        { "fill": "#password", "value_env": "MISTERCOPY_TEST_PASSWORD" },
        { "click": "button[type=submit]" },
        { "wait_for": ".welcome" },
    ]);
    std::fs::write(&script, steps.to_string()).unwrap();
    let dir = out.path().join("copy");

    // `auto` logs in with Chromium, then copies the plain pages over HTTP
    // using the session the browser obtained.
    let copied = tokio::process::Command::new(env!("CARGO_BIN_EXE_mistercopy"))
        .args([
            "copy",
            server.url("/members/").as_str(),
            "--requester",
            "auto",
            "--delay-ms",
            "0",
            "-q",
            "--login-script",
            script.to_str().unwrap(),
            "-o",
            dir.to_str().unwrap(),
        ])
        .env("MISTERCOPY_TEST_PASSWORD", PASSWORD)
        .output()
        .await
        .unwrap();
    assert!(copied.status.success(), "{}", stderr(&copied));
    let root = host_dir(&dir, server.addr().port());
    let members = std::fs::read_to_string(root.join("members/index.html")).unwrap();
    assert!(members.contains("Welcome back"), "{members}");
    // A plain page: fetched over wreq, logged in with the browser's session.
    assert!(
        std::fs::read_to_string(root.join("members/page2.html"))
            .unwrap()
            .contains("Members page 2")
    );
    let requests = server.requests();
    assert!(
        requests.iter().any(|r| r == "/login"),
        "the browser logged in: {requests:#?}"
    );
    assert!(!requests.iter().any(|r| r == "/logout"), "{requests:#?}");
    server.stop().await.unwrap();
}
