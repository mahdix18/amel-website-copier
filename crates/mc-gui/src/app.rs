//! The copy window.

use std::time::Duration;

use gpui_kit::component::ActiveTheme;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::prelude::*;
use gpui_kit::{
    AnyElement, Context, Entity, FontWeight, Role, SharedString, TestSupportExt, Window, div, px,
};
use mc_core::{StopHandle, human_bytes};
use tokio::sync::broadcast::error::RecvError;

use crate::job::{self, Progress, Settings};

pub struct CopyWindow {
    url: Entity<InputState>,
    output: Entity<InputState>,
    render_js: bool,
    /// Gap between requests to one host. Polite by default.
    pub per_host_delay: Duration,
    progress: Progress,
    stop: Option<StopHandle>,
}

impl CopyWindow {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        Self {
            url: cx.new(|cx| InputState::new(window, cx).placeholder("https://example.com")),
            output: cx.new(|cx| InputState::new(window, cx).default_value(job::default_output())),
            render_js: false,
            per_host_delay: Duration::from_secs(1),
            progress: Progress::default(),
            stop: None,
        }
    }

    pub fn progress(&self) -> &Progress {
        &self.progress
    }

    pub fn url_input(&self) -> &Entity<InputState> {
        &self.url
    }

    pub fn output_input(&self) -> &Entity<InputState> {
        &self.output
    }

    fn start(&mut self, cx: &mut Context<Self>) {
        if self.progress.running() {
            return;
        }
        let settings = Settings {
            url: self.url.read(cx).value().to_string(),
            output: self.output.read(cx).value().to_string(),
            render_js: self.render_js,
            per_host_delay: self.per_host_delay,
        };
        let running = match job::start(&settings) {
            Ok(running) => running,
            Err(message) => {
                self.progress = Progress::failed(message);
                cx.notify();
                return;
            }
        };

        self.progress = Progress::started(running.resumed);
        self.stop = Some(running.stop.clone());
        let output = running.output;
        let mut events = running.events;
        let done = running.done;
        cx.spawn(async move |this, cx| {
            loop {
                match events.recv().await {
                    Ok(event) => {
                        let applied = this.update(cx, |this, cx| {
                            this.progress.apply(&event);
                            cx.notify();
                        });
                        if applied.is_err() {
                            return; // The window is gone.
                        }
                    }
                    Err(RecvError::Lagged(_)) => continue,
                    Err(RecvError::Closed) => break,
                }
            }
            let result = done
                .await
                .unwrap_or_else(|_| Err("The copy ended unexpectedly.".to_owned()));
            let _ = this.update(cx, |this, cx| {
                this.progress.finish(result, &output);
                this.stop = None;
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn stop(&mut self, cx: &mut Context<Self>) {
        if let Some(stop) = &self.stop {
            stop.stop();
            self.progress.phase = job::Phase::Stopping;
            cx.notify();
        }
    }
}

impl Render for CopyWindow {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let border = theme.border;
        let running = self.progress.running();
        let status = SharedString::from(self.progress.status());

        let action = if running {
            Button::new("stop")
                .danger()
                .label("Stop")
                .on_click(cx.listener(|this, _, _, cx| this.stop(cx)))
        } else {
            Button::new("copy")
                .primary()
                .label("Copy")
                .on_click(cx.listener(|this, _, _, cx| this.start(cx)))
        };

        let stats = div()
            .flex()
            .gap_3()
            .child(stat(
                "saved",
                "Saved",
                self.progress.saved.to_string(),
                border,
                muted,
            ))
            .child(stat(
                "failed",
                "Failed",
                self.progress.failed.to_string(),
                border,
                muted,
            ))
            .child(stat(
                "size",
                "Downloaded",
                human_bytes(self.progress.bytes),
                border,
                muted,
            ));

        let log = div()
            .id("log")
            .flex_1()
            .min_h(px(120.))
            .overflow_y_scroll()
            .border_1()
            .border_color(border)
            .rounded_md()
            .p_2()
            .text_xs()
            .font_family("monospace")
            .children(
                self.progress
                    .log
                    .iter()
                    .map(|line| div().child(line.clone())),
            );

        div()
            .id("copy-window")
            .size_full()
            .flex()
            .flex_col()
            .gap_4()
            .p_6()
            .bg(theme.background)
            .text_color(theme.foreground)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(div().text_xl().font_weight(FontWeight::SEMIBOLD).child("MisterCopy"))
                    .child(div().text_sm().text_color(muted).child("Copy a website for offline browsing.")),
            )
            .child(field(
                "Website",
                Input::new(&self.url).id("url").aria_label("Website address"),
                muted,
            ))
            .child(field(
                "Save in",
                Input::new(&self.output).id("output").aria_label("Folder to save in"),
                muted,
            ))
            .when(job::browser_support(), |el| {
                el.child(
                    Checkbox::new("render-js")
                        .label("Render JavaScript with Chromium, for sites that build pages in the browser")
                        .checked(self.render_js)
                        .on_click(cx.listener(|this, checked: &bool, _, cx| {
                            this.render_js = *checked;
                            cx.notify();
                        })),
                )
            })
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(action)
                    .when_some(self.progress.report.clone().filter(|_| !running), |el, report| {
                        el.child(Button::new("report").outline().label("Open report").on_click(
                            move |_, _, cx| cx.open_url(&format!("file://{}", report.display())),
                        ))
                    }),
            )
            .child(
                div()
                    .id("status")
                    .role(Role::Status)
                    .test_support()
                    .aria_label(status.clone())
                    .text_sm()
                    .child(status),
            )
            .child(stats)
            .child(log)
    }
}

fn field(label: &'static str, input: Input, muted: gpui_kit::Hsla) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(div().text_sm().text_color(muted).child(label))
        .child(input)
}

fn stat(
    id: &'static str,
    label: &'static str,
    value: String,
    border: gpui_kit::Hsla,
    muted: gpui_kit::Hsla,
) -> AnyElement {
    div()
        .id(id)
        .test_support()
        .aria_label(SharedString::from(format!("{label}: {value}")))
        .flex()
        .flex_col()
        .min_w(px(110.))
        .px_3()
        .py_2()
        .border_1()
        .border_color(border)
        .rounded_md()
        .child(
            div()
                .text_lg()
                .font_weight(FontWeight::SEMIBOLD)
                .child(value),
        )
        .child(div().text_xs().text_color(muted).child(label))
        .into_any_element()
}
