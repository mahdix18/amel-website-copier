use gpui_kit::component::TitleBar;
use gpui_kit::{
    AppContext, Bounds, TitlebarOptions, WindowBounds, WindowDecorations, WindowOptions, px, size,
};
use mc_gui::CopyWindow;

const USAGE: &str = "usage: mistercopy-gui [URL] [--output DIR] [--start] [--mini]";

/// What the command line asks the window to do.
#[derive(Default)]
struct Launch {
    url: Option<String>,
    output: Option<String>,
    start: bool,
    /// Open straight into the mini window.
    mini: bool,
}

fn launch() -> Launch {
    let mut launch = Launch::default();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--output" | "-o" => launch.output = args.next(),
            "--start" => launch.start = true,
            "--mini" => launch.mini = true,
            "--help" | "-h" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            "--version" | "-V" => {
                println!("mistercopy-gui {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            _ if arg.starts_with('-') => {
                eprintln!("unknown option {arg}\n{USAGE}");
                std::process::exit(2);
            }
            _ => launch.url = Some(arg),
        }
    }
    launch
}

fn main() {
    mc_gui::mark_start();
    let launch = launch();
    gpui_kit::application()
        .with_assets(gpui_kit::assets::Assets)
        .run(move |cx| {
            gpui_kit::init(cx);
            // The window switches to the saved theme if it differs.
            mc_gui::look::Look::dark().apply(cx);
            mc_gui::bind_keys(cx);
            let bounds = Bounds::centered(None, size(px(1180.), px(780.)), cx);
            // The app draws its own title bar, with the window buttons where
            // the desktop leaves that to apps (GNOME on Wayland).
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some("MisterCopy".into()),
                    ..TitleBar::title_bar_options()
                }),
                window_decorations: Some(WindowDecorations::Client),
                window_min_size: Some(size(px(860.), px(600.))),
                app_id: Some("mistercopy".into()),
                ..TitleBar::window_options()
            };
            let mini = launch.mini;
            let (handle, view) = gpui_kit::open_window(options, cx, move |window, cx| {
                cx.new(|cx| {
                    let mut view = CopyWindow::new(window, cx);
                    view.prefill(launch.url, launch.output, launch.start, window, cx);
                    view
                })
            })
            .expect("failed to open the MisterCopy window");
            if mini {
                let _ = handle.update(cx, |_, window, cx| {
                    view.update(cx, |view, cx| view.show_mini(window, cx));
                });
            }
            // Closing the last window quits the app, as on Linux and Windows.
            cx.on_window_closed(|cx, _| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();
            cx.activate(true);
        });
}
