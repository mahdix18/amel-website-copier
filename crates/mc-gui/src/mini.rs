//! The mini window: a small strip that stays above other windows and shows
//! the copy's progress while the main window is minimized. Drag it anywhere;
//! its buttons pause or resume the copy and bring the main window back.

use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::{IconName, Sizable, window_paddings};
use gpui_kit::prelude::*;
use gpui_kit::{
    AnyWindowHandle, App, Bounds, Context, Entity, FontWeight, MouseButton, Point, SharedString,
    Subscription, TestSupportExt, WeakEntity, Window, WindowBounds, WindowDecorations, WindowKind,
    WindowOptions, div, px, relative, size,
};
use mc_core::human_bytes;

use crate::app::CopyWindow;
use crate::job::Phase;
use crate::look::Look;

/// The mini window's size.
pub const MINI_SIZE: (f32, f32) = (360., 74.);

pub struct MiniWindow {
    main: WeakEntity<CopyWindow>,
    main_window: AnyWindowHandle,
    /// Set once the window has been grown to fit its frame.
    fitted: bool,
    _redraw: Subscription,
}

impl MiniWindow {
    /// Opens the mini window near the top-left corner of the screen (where
    /// the desktop lets apps choose; Wayland places it itself).
    pub fn open(
        main: &Entity<CopyWindow>,
        main_window: AnyWindowHandle,
        cx: &mut App,
    ) -> Result<AnyWindowHandle, String> {
        let origin = cx
            .primary_display()
            .map(|display| display.bounds().origin)
            .unwrap_or_default();
        let bounds = Bounds {
            origin: Point {
                x: origin.x + px(16.),
                y: origin.y + px(48.),
            },
            size: size(px(MINI_SIZE.0), px(MINI_SIZE.1)),
        };
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: None,
            kind: WindowKind::PopUp,
            is_resizable: false,
            window_decorations: Some(WindowDecorations::Client),
            app_id: Some("amel".into()),
            ..WindowOptions::default()
        };
        let main = main.clone();
        gpui_kit::open_window(options, cx, move |_, cx| {
            cx.new(|cx| MiniWindow {
                // Redraws whenever the main window's progress changes.
                _redraw: cx.observe(&main, |_, _, cx| cx.notify()),
                main: main.downgrade(),
                main_window,
                fitted: false,
            })
        })
        .map(|(handle, _)| handle)
        .map_err(|e| format!("could not open the mini window: {e}"))
    }

    /// Where the app draws its own decorations (GNOME on Wayland), the
    /// window frame keeps room for a shadow and a border around the content;
    /// grows the window by that much so the content keeps [`MINI_SIZE`].
    fn fit(&mut self, window: &mut Window) {
        if self.fitted {
            return;
        }
        self.fitted = true;
        let frame = window_paddings(window);
        let border = if frame.top > px(0.) { px(2.) } else { px(0.) };
        let wanted = size(
            px(MINI_SIZE.0) + frame.left + frame.right + border,
            px(MINI_SIZE.1) + frame.top + frame.bottom + border,
        );
        if window.viewport_size() != wanted {
            window.resize(wanted);
        }
    }

    /// Back to the main window; the mini window closes.
    fn expand(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(main) = self.main.upgrade() {
            main.update(cx, |main, _| main.mini_closed());
        }
        let _ = self
            .main_window
            .update(cx, |_, window, _| window.activate_window());
        window.remove_window();
    }
}

impl Render for MiniWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.fit(window);
        let look = Look::global(cx);
        let Some(main) = self.main.upgrade() else {
            // The main window is gone; so is the copy's window.
            window.on_next_frame(|window, _| window.remove_window());
            return div().into_any_element();
        };
        let main = main.read(cx);
        let progress = main.progress();
        let site = main.site_name(cx);
        let paused = progress.phase == Phase::Paused;
        let (color, phase) = match &progress.phase {
            Phase::Idle => (look.muted, "Ready"),
            Phase::Running if progress.checking => (look.accent, "Checking"),
            Phase::Running => (look.accent, "Copying"),
            Phase::Paused => (look.warning, "Paused"),
            Phase::Stopping => (look.warning, "Stopping"),
            Phase::Finished if progress.failed > 0 => (look.warning, "Done, with errors"),
            Phase::Finished => (look.success, "Done"),
            Phase::Stopped => (look.warning, "Stopped"),
            Phase::Failed(_) => (look.danger, "Failed"),
        };
        let fraction = progress.fraction();
        let percent = fraction
            .map(|f| format!("{:.0}%", f * 100.))
            .unwrap_or_default();
        let detail = format!(
            "{} / {} · {} · {}/s",
            progress.done,
            progress.total.max(progress.done),
            human_bytes(progress.live_bytes()),
            human_bytes(progress.rate())
        );
        let running = progress.running();
        let can_pause = matches!(progress.phase, Phase::Running | Phase::Paused);

        // Everything but the buttons drags the window.
        let info = div()
            .id("mini-drag")
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            .gap_1()
            .on_mouse_down(MouseButton::Left, |_, window, _| window.start_window_move())
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .text_xs()
                    .child(div().size(px(8.)).flex_none().rounded_full().bg(color))
                    .child(
                        div()
                            .id("mini-status")
                            .test_support()
                            .aria_label(SharedString::from(phase))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(color)
                            .child(phase),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .whitespace_nowrap()
                            .overflow_hidden()
                            .text_ellipsis()
                            .child(site),
                    )
                    .child(div().font_weight(FontWeight::SEMIBOLD).child(percent)),
            )
            .child(
                div()
                    .id("mini-progress")
                    .test_support()
                    .aria_label(SharedString::from(detail.clone()))
                    .h(px(5.))
                    .w_full()
                    .rounded_full()
                    .bg(look.raised)
                    .overflow_hidden()
                    .child(
                        div()
                            .h_full()
                            .rounded_full()
                            .bg(match progress.phase {
                                Phase::Finished => look.success,
                                Phase::Paused => look.warning,
                                Phase::Failed(_) => look.danger,
                                _ => look.accent,
                            })
                            .when_some(fraction, |el, f| el.w(relative(f)))
                            .when(fraction.is_none() && running, |el| {
                                el.w_full().opacity(0.35)
                            })
                            .when(fraction.is_none() && !running, |el| el.w(px(0.))),
                    ),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(look.muted)
                    .whitespace_nowrap()
                    .overflow_hidden()
                    .text_ellipsis()
                    .child(detail),
            );

        let main_entity = self.main.clone();
        div()
            .id("mini-window")
            .size_full()
            .p_2()
            .flex()
            .items_center()
            .gap_2()
            .bg(look.chrome)
            .border_1()
            .border_color(look.border)
            .rounded_lg()
            .text_color(look.text)
            .child(info)
            .when(can_pause, |el| {
                el.child(
                    Button::new("mini-pause")
                        .ghost()
                        .small()
                        .icon(if paused {
                            IconName::Play
                        } else {
                            IconName::Pause
                        })
                        .tooltip(if paused { "Resume" } else { "Pause" })
                        .on_click(move |_, _, cx| {
                            if let Some(main) = main_entity.upgrade() {
                                main.update(cx, |main, cx| main.toggle_pause(cx));
                            }
                        }),
                )
            })
            .child(
                Button::new("mini-expand")
                    .ghost()
                    .small()
                    .icon(IconName::Maximize)
                    .tooltip("Back to the full window")
                    .on_click(cx.listener(|this, _, window, cx| this.expand(window, cx))),
            )
            .into_any_element()
    }
}
