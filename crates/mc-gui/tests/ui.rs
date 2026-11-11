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

fn open(cx: &mut TestAppContext) -> (WindowHandle<Root>, Entity<CopyWindow>) {
    cx.update(|cx| {
        gpui_kit::init(cx);
        let bounds = Bounds {
            origin: Point::default(),
            size: size(px(880.), px(680.)),
        };
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            ..Default::default()
        };
        let (window, view) = gpui_kit::open_window(options, cx, |window, cx| {
            cx.new(|cx| CopyWindow::new(window, cx))
        })
        .unwrap();
        (window.downcast::<Root>().unwrap(), view)
    })
}

#[gpui_kit::test]
fn explains_what_is_wrong_with_the_form(cx: &mut TestAppContext) {
    let (handle, _) = open(cx);
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
    let (handle, view) = open(cx);

    cx.update_window(handle.into(), |_, window, cx| {
        let output = out.path().to_string_lossy().into_owned();
        view.update(cx, |view, cx| {
            view.per_host_delay = Duration::ZERO;
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

    let host = format!("127.0.0.1_{}", server.addr().port());
    assert!(out.path().join(&host).join("posts/café.html").is_file());
    assert!(out.path().join("mistercopy-report.html").is_file());
    drop(runtime);
}
