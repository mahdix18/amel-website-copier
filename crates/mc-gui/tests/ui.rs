//! The copy window, driven through gpui-kit's headless test harness: real
//! components, real hit testing, simulated clicks and typing.

#![allow(clippy::unwrap_used)]

use std::time::{Duration, Instant};

use gpui_kit::test::TestWindowExt;
use gpui_kit::{
    AppContext, Bounds, Entity, Point, TestAppContext, WindowBounds, WindowHandle, WindowOptions,
    base::Root, px, size,
};
use mc_fixtures::{FixtureServer, Site};
use mc_gui::CopyWindow;
use mc_gui::job::Phase;
use mc_gui::projects::Projects;

/// Opens the window with a throwaway project list.
fn open(
    cx: &mut TestAppContext,
    projects: &std::path::Path,
) -> (WindowHandle<Root>, Entity<CopyWindow>) {
    open_sized(cx, projects, 880., 680.)
}

fn open_sized(
    cx: &mut TestAppContext,
    projects: &std::path::Path,
    width: f32,
    height: f32,
) -> (WindowHandle<Root>, Entity<CopyWindow>) {
    let projects = Projects::load(Some(projects.join("projects.json")));
    cx.update(|cx| {
        gpui_kit::init(cx);
        mc_gui::look::Look::dark().apply(cx);
        mc_gui::bind_keys(cx);
        let bounds = Bounds {
            origin: Point::default(),
            size: size(px(width), px(height)),
        };
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            ..Default::default()
        };
        let (window, view) = gpui_kit::open_window(options, cx, |window, cx| {
            cx.new(|cx| CopyWindow::with_projects(projects, window, cx))
        })
        .unwrap();
        (window.downcast::<Root>().unwrap(), view)
    })
}

#[gpui_kit::test]
fn explains_what_is_wrong_with_the_form(cx: &mut TestAppContext) {
    let config = tempfile::tempdir().unwrap();
    let (handle, _) = open(cx, config.path());
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(window.find("status").label(), Some("Ready."));
        window.click("copy", cx);
        assert_eq!(
            window.find("status").label(),
            Some("Enter the address of the site to copy.")
        );
    })
    .unwrap();
}

#[gpui_kit::test]
fn copies_a_site_from_the_window(cx: &mut TestAppContext) {
    // The crawl runs on its own thread; let the test wait for its events.
    cx.executor().allow_parking();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let server = runtime
        .block_on(FixtureServer::start(Site::StaticBlog))
        .unwrap();
    let out = tempfile::tempdir().unwrap();
    let config = tempfile::tempdir().unwrap();
    let (handle, view) = open(cx, config.path());

    cx.update_window(handle.into(), |_, window, cx| {
        let output = out.path().to_string_lossy().into_owned();
        view.update(cx, |view, cx| {
            view.speed = mc_gui::job::Speed::Fast;
            view.output_input()
                .clone()
                .update(cx, |input, cx| input.set_value(output, window, cx));
        });
        window.render_frame(cx);
        window.click("url", cx);
        window.input(server.base_url().as_str(), cx);
        window.click("copy", cx);
        assert_eq!(window.find("status").label(), Some("Copying…"));
        // While copying, the button offers Stop instead.
        assert!(window.try_find("stop").is_some());
    })
    .unwrap();

    let deadline = Instant::now() + Duration::from_secs(60);
    while cx.update(|cx| view.read(cx).progress().running()) {
        assert!(Instant::now() < deadline, "the copy didn't finish");
        cx.run_until_parked();
        std::thread::sleep(Duration::from_millis(20));
    }

    let progress = cx.update(|cx| view.read(cx).progress().clone());
    assert_eq!(progress.phase, Phase::Finished, "{}", progress.status());
    assert_eq!(progress.failed, 0, "{:?}", progress.log);
    assert!(progress.saved >= 11, "{}", progress.saved);

    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        let status = window.find("status").label().unwrap_or_default().to_owned();
        assert!(status.starts_with("Copied "), "{status}");
        assert_eq!(
            window.find("saved").label(),
            Some(format!("Saved: {}", progress.saved).as_str())
        );
        assert!(
            window.try_find("report").is_some(),
            "the report can be opened"
        );
        assert!(window.try_find("copy").is_some());
    })
    .unwrap();

    // The copy is remembered in the sidebar, and its page tree can be shown.
    cx.update(|cx| assert_eq!(view.read(cx).projects().items.len(), 1));
    assert!(config.path().join("projects.json").is_file());
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(window.find("project-0").label(), Some("127.0.0.1"));
        // Finished copies can be updated.
        assert!(window.try_find("update").is_some());
        window.click("tab-map", cx);
        let count = window
            .find("map-count")
            .label()
            .unwrap_or_default()
            .to_owned();
        let pages: usize = count.split(' ').next().unwrap().parse().unwrap();
        assert!(pages >= 5, "{count}");
        // The tree exports as an SVG next to the report.
        window.click("export-map", cx);
        let notice = window
            .find("map-notice")
            .label()
            .unwrap_or_default()
            .to_owned();
        assert!(notice.starts_with("Saved "), "{notice}");
    })
    .unwrap();

    let host = format!("127.0.0.1_{}", server.addr().port());
    assert!(out.path().join(&host).join("posts/café.html").is_file());
    assert!(out.path().join("amel-report.html").is_file());
    let svg = std::fs::read_to_string(out.path().join(mc_gui::app::LINK_MAP_SVG)).unwrap();
    assert!(
        svg.starts_with("<svg") && svg.contains("posts/caf%C3%A9.html"),
        "{svg}"
    );
    drop(runtime);
}

#[gpui_kit::test]
fn tests_rules_before_copying(cx: &mut TestAppContext) {
    let config = tempfile::tempdir().unwrap();
    let (handle, _) = open(cx, config.path());
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        window.click("url", cx);
        window.input("https://site.test/", cx);
        window.click("tab-rules", cx);
        window.click("exclude", cx);
        window.input("/private/", cx);
        window.click("test-url", cx);
        window.input("https://site.test/private/report.html", cx);
        window.click("test", cx);
        assert_eq!(
            window.find("verdict").label(),
            Some("Skipped: excluded by a rule.")
        );

        window.click("test-url", cx);
        window.press("ctrl-a", cx);
        window.input("https://site.test/blog/", cx);
        window.click("test", cx);
        assert_eq!(
            window.find("verdict").label(),
            Some("Copied, and its links are followed.")
        );
    })
    .unwrap();
}

#[gpui_kit::test]
fn command_palette_runs_commands(cx: &mut TestAppContext) {
    let config = tempfile::tempdir().unwrap();
    let (handle, _) = open(cx, config.path());
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find("palette").is_none());

        window.press("ctrl-k", cx);
        assert!(
            window.try_find("palette").is_some(),
            "Ctrl+K opens the palette"
        );
        window.input("link map", cx);
        assert_eq!(window.find("cmd-0").label(), Some("Show link map"));
        window.press("enter", cx);
    })
    .unwrap();
    // The input reports Enter as an event, delivered once the update ends.
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(
            window.try_find("palette").is_none(),
            "running a command closes it"
        );
        assert!(
            window.try_find("map-count").is_some(),
            "the link map is showing"
        );

        window.click("commands", cx);
        assert!(
            window.try_find("palette").is_some(),
            "the button opens it too"
        );
        window.press("escape", cx);
        assert!(window.try_find("palette").is_none(), "Esc closes it");
    })
    .unwrap();
}

#[gpui_kit::test]
fn limits_and_output_tabs_reach_the_crawl(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let server = runtime
        .block_on(FixtureServer::start(Site::StaticBlog))
        .unwrap();
    let out = tempfile::tempdir().unwrap();
    let config = tempfile::tempdir().unwrap();
    let (handle, view) = open(cx, config.path());

    cx.update_window(handle.into(), |_, window, cx| {
        let output = out.path().to_string_lossy().into_owned();
        view.update(cx, |view, cx| {
            view.speed = mc_gui::job::Speed::Fast;
            view.output_input()
                .clone()
                .update(cx, |input, cx| input.set_value(output, window, cx));
        });
        window.render_frame(cx);
        window.click("url", cx);
        window.input(server.base_url().as_str(), cx);
        // A bad limit is explained, not run.
        window.click("tab-limits", cx);
        window.click("max-urls", cx);
        window.input("lots", cx);
        window.click("copy", cx);
        assert_eq!(
            window.find("status").label(),
            Some("Max pages must be a whole number, not “lots”.")
        );
        window.click("max-urls", cx);
        for _ in 0..4 {
            window.press("backspace", cx);
        }
        window.input("3", cx);
        window.click("tab-output", cx);
        window.click("zip", cx);
        window.click("copy", cx);
    })
    .unwrap();

    let deadline = Instant::now() + Duration::from_secs(60);
    while cx.update(|cx| view.read(cx).progress().running()) {
        assert!(Instant::now() < deadline, "the copy didn't finish");
        cx.run_until_parked();
        std::thread::sleep(Duration::from_millis(20));
    }
    // Let the zip, written after the crawl, reach the window.
    cx.run_until_parked();

    let progress = cx.update(|cx| view.read(cx).progress().clone());
    assert_eq!(progress.phase, Phase::Finished, "{}", progress.status());
    assert!(
        progress.saved <= 3,
        "max pages caps fetches: {}",
        progress.saved
    );
    assert!(
        progress.log.iter().any(|line| line.contains("Zip saved")),
        "{:?}",
        progress.log
    );
    let zips = std::fs::read_dir(out.path())
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| entry.path().extension().is_some_and(|e| e == "zip"))
        .count();
    assert_eq!(zips, 1, "the finished copy was zipped");
}

#[gpui_kit::test]
fn picks_a_browser_to_impersonate(cx: &mut TestAppContext) {
    let config = tempfile::tempdir().unwrap();
    let (handle, _) = open(cx, config.path());
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(
            window.try_find("verify").is_some(),
            "the missed-files checkbox"
        );
        assert!(window.try_find("adaptive").is_some(), "the smart checkbox");
        assert!(window.try_find("retries").is_some(), "the retries input");
        // The requester chip opens the Browser tab.
        window.click("requester", cx);
        assert_eq!(
            window.find("profile-version").label(),
            Some("Profile: chrome_149")
        );
        window.click("browser-firefox", cx);
        let label = window
            .find("profile-version")
            .label()
            .unwrap_or_default()
            .to_owned();
        assert!(label.starts_with("Profile: firefox_"), "{label}");
        assert!(
            !label.contains("android") && !label.contains("private"),
            "{label}"
        );
        window.click("profile-older", cx);
        let older = window
            .find("profile-version")
            .label()
            .unwrap_or_default()
            .to_owned();
        assert_ne!(older, label, "the stepper moves to an older version");
        window.click("platform-windows", cx);
        assert!(
            window
                .find("requester")
                .label()
                .unwrap_or_default()
                .contains("FIREFOX")
        );
    })
    .unwrap();
    // The choice is remembered for the next launch.
    let prefs = std::fs::read_to_string(config.path().join("prefs.json")).unwrap();
    assert!(prefs.contains("\"platform\": \"windows\""), "{prefs}");
    assert!(prefs.contains("firefox_"), "{prefs}");
}

#[gpui_kit::test]
fn pauses_resumes_and_shrinks_to_a_mini_window(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let server = runtime
        .block_on(FixtureServer::start(Site::StaticBlog))
        .unwrap();
    let out = tempfile::tempdir().unwrap();
    let config = tempfile::tempdir().unwrap();
    let (handle, view) = open(cx, config.path());

    cx.update_window(handle.into(), |_, window, cx| {
        let output = out.path().to_string_lossy().into_owned();
        view.update(cx, |view, cx| {
            // Polite: one request a second, so the copy is still running.
            view.speed = mc_gui::job::Speed::Polite;
            view.minimize_for_mini = false;
            view.output_input()
                .clone()
                .update(cx, |input, cx| input.set_value(output, window, cx));
        });
        window.render_frame(cx);
        window.click("url", cx);
        window.input(server.base_url().as_str(), cx);
        window.click("copy", cx);
        window.render_frame(cx);
        window.click("pause", cx);
        assert!(
            window
                .find("status")
                .label()
                .unwrap_or_default()
                .starts_with("Paused"),
        );
    })
    .unwrap();
    assert_eq!(
        cx.update(|cx| view.read(cx).progress().phase.clone()),
        Phase::Paused
    );

    // The mini window opens and shows the same copy.
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        window.click("mini", cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert!(cx.update(|cx| view.read(cx).mini_open()));
    assert_eq!(cx.update(|cx| cx.windows().len()), 2);

    // The crawl confirms the pause.
    let deadline = Instant::now() + Duration::from_secs(30);
    while !cx.update(|cx| {
        view.read(cx)
            .progress()
            .log
            .iter()
            .any(|line| line.contains("Paused"))
    }) {
        assert!(Instant::now() < deadline, "the crawl never paused");
        cx.run_until_parked();
        std::thread::sleep(Duration::from_millis(20));
    }

    // Resume, from the main window's view, and let the copy finish.
    cx.update(|cx| view.update(cx, |view, cx| view.toggle_pause(cx)));
    let deadline = Instant::now() + Duration::from_secs(60);
    while cx.update(|cx| view.read(cx).progress().running()) {
        assert!(Instant::now() < deadline, "the copy didn't finish");
        cx.run_until_parked();
        std::thread::sleep(Duration::from_millis(20));
    }
    let progress = cx.update(|cx| view.read(cx).progress().clone());
    assert_eq!(progress.phase, Phase::Finished, "{}", progress.status());
    assert!(
        progress.log.iter().any(|line| line.contains("Paused")),
        "{:#?}",
        progress.log.iter().map(|l| &l.text).collect::<Vec<_>>()
    );
    assert!(progress.log.iter().any(|line| line.contains("Resumed")));
}

#[gpui_kit::test]
fn retries_only_the_failed_files(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let server = runtime
        .block_on(FixtureServer::start(Site::StaticBlog))
        .unwrap();
    let out = tempfile::tempdir().unwrap();
    let config = tempfile::tempdir().unwrap();
    let (handle, view) = open(cx, config.path());
    let wait = |cx: &mut TestAppContext| {
        let deadline = Instant::now() + Duration::from_secs(60);
        while cx.update(|cx| view.read(cx).progress().running()) {
            assert!(Instant::now() < deadline, "the copy didn't finish");
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(20));
        }
        cx.run_until_parked();
    };

    cx.update_window(handle.into(), |_, window, cx| {
        let output = out.path().to_string_lossy().into_owned();
        view.update(cx, |view, cx| {
            view.speed = mc_gui::job::Speed::Fast;
            view.output_input()
                .clone()
                .update(cx, |input, cx| input.set_value(output, window, cx));
        });
        window.render_frame(cx);
        window.click("url", cx);
        window.input(server.base_url().as_str(), cx);
        // Every response is "too large": the start page fails.
        window.click("robots", cx);
        window.click("tab-limits", cx);
        window.click("max-file", cx);
        for _ in 0..3 {
            window.press("backspace", cx);
        }
        window.input("0", cx);
        window.click("copy", cx);
    })
    .unwrap();
    wait(cx);
    let failed = cx.update(|cx| view.read(cx).progress().failed);
    assert!(failed > 0, "the start page should have failed");

    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        let label = window
            .find("retry-failed")
            .label()
            .unwrap_or_default()
            .to_owned();
        assert!(label.contains(&format!("Retry {failed} failed")), "{label}");
        // Lift the limit and retry only what failed.
        window.click("max-file", cx);
        window.press("backspace", cx);
        window.input("512", cx);
        window.click("retry-failed", cx);
    })
    .unwrap();
    wait(cx);
    let progress = cx.update(|cx| view.read(cx).progress().clone());
    assert_eq!(progress.phase, Phase::Finished, "{}", progress.status());
    assert_eq!(progress.failed, 0, "{:?}", progress.log);
    assert!(
        progress.saved >= 11,
        "the retried start page led to the rest"
    );
    assert!(progress.log.iter().any(|line| line.contains("Retrying")));
}

#[gpui_kit::test]
fn types_scripts_and_row_actions(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let server = runtime
        .block_on(FixtureServer::start(Site::StaticBlog))
        .unwrap();
    let out = tempfile::tempdir().unwrap();
    let config = tempfile::tempdir().unwrap();
    // Tall enough to show the activity rows below the settings.
    let (handle, view) = open_sized(cx, config.path(), 1180., 1100.);

    cx.update_window(handle.into(), |_, window, cx| {
        let output = out.path().to_string_lossy().into_owned();
        view.update(cx, |view, cx| {
            view.speed = mc_gui::job::Speed::Fast;
            view.output_input()
                .clone()
                .update(cx, |input, cx| input.set_value(output, window, cx));
        });
        window.render_frame(cx);
        window.click("url", cx);
        window.input(server.base_url().as_str(), cx);
        window.click("tab-types", cx);
        window.click("type-image", cx);
        window.click("tab-script", cx);
        window.click("script-on", cx);
        window.click("copy", cx);
    })
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    while cx.update(|cx| view.read(cx).progress().running()) {
        assert!(Instant::now() < deadline, "the copy didn't finish");
        cx.run_until_parked();
        std::thread::sleep(Duration::from_millis(20));
    }
    cx.run_until_parked();

    let progress = cx.update(|cx| view.read(cx).progress().clone());
    assert_eq!(progress.phase, Phase::Finished, "{}", progress.status());
    let tags: Vec<&str> = progress.log.iter().map(|l| l.kind.tag()).collect();
    assert!(tags.contains(&"SKIP"), "images were left out: {tags:?}");
    let script: Vec<&str> = progress
        .log
        .iter()
        .filter(|l| l.kind.tag() == "JS")
        .map(|l| l.text.as_str())
        .collect();
    assert!(
        script.iter().any(|t| t.starts_with("Copying ")),
        "{script:?}"
    );
    assert!(script.iter().any(|t| t.starts_with("Done: ")), "{script:?}");

    // The newest row is about a URL; its copy button puts it on the clipboard.
    let newest_url = progress
        .log
        .iter()
        .rev()
        .find_map(|l| l.url.clone())
        .unwrap();
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        let first_with_url = progress
            .log
            .iter()
            .rev()
            .position(|l| l.url.is_some())
            .unwrap();
        window.click(
            gpui_kit::ElementId::Name(format!("act-copy-{first_with_url}").into()),
            cx,
        );
    })
    .unwrap();
    let clipboard = cx.update(|cx| cx.read_from_clipboard().and_then(|c| c.text()));
    assert_eq!(clipboard.as_deref(), Some(newest_url.as_str()));
}

#[gpui_kit::test]
fn the_log_panel_records_filters_and_clears(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let server = runtime
        .block_on(FixtureServer::start(Site::StaticBlog))
        .unwrap();
    let out = tempfile::tempdir().unwrap();
    let config = tempfile::tempdir().unwrap();
    let (handle, view) = open_sized(cx, config.path(), 1180., 1000.);
    cx.update_window(handle.into(), |_, window, cx| {
        let output = out.path().to_string_lossy().into_owned();
        view.update(cx, |view, cx| {
            view.speed = mc_gui::job::Speed::Fast;
            view.output_input()
                .clone()
                .update(cx, |input, cx| input.set_value(output, window, cx));
        });
        window.render_frame(cx);
        assert!(window.try_find("console").is_none(), "closed at first");
        window.click("console-toggle", cx);
        assert!(window.try_find("console").is_some());
        window.click("url", cx);
        window.input(server.base_url().as_str(), cx);
        window.click("copy", cx);
    })
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    while cx.update(|cx| view.read(cx).progress().running()) {
        assert!(Instant::now() < deadline, "the copy didn't finish");
        cx.run_until_parked();
        std::thread::sleep(Duration::from_millis(20));
    }
    cx.run_until_parked();
    let text = cx.update(|cx| {
        view.read(cx)
            .console()
            .text(mc_gui::console::Filter::All, "")
    });
    assert!(
        text.contains("INFO  copy    saved http://127.0.0.1"),
        "{text}"
    );
    assert!(text.contains("DEBUG copy    worker 1 GET"), "{text}");
    assert!(text.lines().last().unwrap().contains("Copied "), "{text}");

    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        window.click("log-filter-errors", cx);
        window.render_frame(cx);
        assert_eq!(window.find("log-count").label(), Some("0 lines"));
        window.click("log-filter-all", cx);
        window.click("log-search", cx);
        window.input("about", cx);
        window.render_frame(cx);
        let label = window
            .find("log-count")
            .label()
            .unwrap_or_default()
            .to_owned();
        assert!(label != "0 lines", "{label}");
        window.click("log-clear", cx);
        window.render_frame(cx);
        assert_eq!(window.find("log-count").label(), Some("0 lines"));
    })
    .unwrap();
    // The daily file keeps what the panel cleared.
    let logs = config.path().join("logs");
    let file = std::fs::read_dir(&logs)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let saved = std::fs::read_to_string(file).unwrap();
    assert!(saved.contains("saved http://127.0.0.1"));
    // The panel stays open next time.
    let prefs = std::fs::read_to_string(config.path().join("prefs.json")).unwrap();
    assert!(prefs.contains("\"console_open\": true"), "{prefs}");
}

#[gpui_kit::test]
fn ctrl_space_shows_script_suggestions(cx: &mut TestAppContext) {
    let config = tempfile::tempdir().unwrap();
    let (handle, view) = open_sized(cx, config.path(), 1180., 900.);
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        window.click("tab-script", cx);
        let editor = view.read(cx).script_editor().clone();
        editor.update(cx, |editor, cx| {
            editor.set_value("function onFile(file) {\n  \n}", window, cx);
            editor.focus(window, cx);
        });
        window.render_frame(cx);
    })
    .unwrap();
    // On the blank line: every hook and keyword.
    cx.update_window(handle.into(), |_, window, cx| {
        let editor = view.read(cx).script_editor().clone();
        editor.update(cx, |editor, cx| {
            editor.set_cursor_position(gpui_kit::component::input::Position::new(1, 2), window, cx)
        });
        window.press("ctrl-space", cx);
    })
    .unwrap();
    cx.run_until_parked();
    let labels = cx.update(|cx| {
        let editor = view.read(cx).script_editor().read(cx);
        let menu = editor.completion_menu_state();
        assert!(menu.open, "Ctrl+Space opens the suggestions");
        menu.items
            .iter()
            .map(|item| item.label.clone())
            .collect::<Vec<_>>()
    });
    assert!(labels.contains(&"onStart".to_owned()), "{labels:?}");
    assert!(labels.contains(&"log".to_owned()), "{labels:?}");
}
