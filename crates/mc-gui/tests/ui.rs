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
    let projects = Projects::load(Some(projects.join("projects.json")));
    cx.update(|cx| {
        gpui_kit::init(cx);
        mc_gui::look::Look::dark().apply(cx);
        mc_gui::bind_keys(cx);
        let bounds = Bounds {
            origin: Point::default(),
            size: size(px(880.), px(680.)),
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
    assert!(out.path().join("mistercopy-report.html").is_file());
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
