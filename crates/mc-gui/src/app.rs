//! The main window: the list of copies on the left; copying, rules and the
//! link map on the right.

use std::path::PathBuf;
use std::time::Duration;

use gpui_kit::component::ActiveTheme;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::prelude::*;
use gpui_kit::{
    AnyElement, Context, Entity, FontWeight, Hsla, Role, SharedString, TestSupportExt, Window, div,
    px,
};
use mc_core::{StopHandle, human_bytes};
use tokio::sync::broadcast::error::RecvError;

use crate::insight::{self, MapLine};
use crate::job::{self, Progress, Settings};
use crate::projects::{Project, Projects};

/// Lines of the link map shown at once.
const MAP_LIMIT: usize = 500;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Copy,
    Rules,
    LinkMap,
}

pub struct CopyWindow {
    url: Entity<InputState>,
    output: Entity<InputState>,
    exclude: Entity<InputState>,
    include: Entity<InputState>,
    test_url: Entity<InputState>,
    render_js: bool,
    /// Gap between requests to one host. Polite by default.
    pub per_host_delay: Duration,
    projects: Projects,
    selected: Option<usize>,
    tab: Tab,
    progress: Progress,
    stop: Option<StopHandle>,
    rule_verdict: String,
    link_map: Result<Vec<MapLine>, String>,
}

impl CopyWindow {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        Self::with_projects(Projects::load(Projects::default_path()), window, cx)
    }

    /// With a given project list, so tests don't touch the user's.
    pub fn with_projects(projects: Projects, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = |placeholder: &'static str, window: &mut Window, cx: &mut Context<Self>| {
            cx.new(|cx| InputState::new(window, cx).placeholder(placeholder))
        };
        Self {
            url: input("https://example.com", window, cx),
            output: cx.new(|cx| InputState::new(window, cx).default_value(job::default_output())),
            exclude: input("e.g. /admin/|\\.zip$", window, cx),
            include: input("e.g. ^https://cdn\\.example\\.com/", window, cx),
            test_url: input("https://example.com/some/page", window, cx),
            render_js: false,
            per_host_delay: Duration::from_secs(1),
            projects,
            selected: None,
            tab: Tab::Copy,
            progress: Progress::default(),
            stop: None,
            rule_verdict: String::new(),
            link_map: Ok(Vec::new()),
        }
    }

    pub fn progress(&self) -> &Progress {
        &self.progress
    }

    pub fn projects(&self) -> &Projects {
        &self.projects
    }

    pub fn url_input(&self) -> &Entity<InputState> {
        &self.url
    }

    pub fn output_input(&self) -> &Entity<InputState> {
        &self.output
    }

    fn text(input: &Entity<InputState>, cx: &Context<Self>) -> String {
        input.read(cx).value().to_string()
    }

    fn settings(&self, update: bool, cx: &Context<Self>) -> Settings {
        Settings {
            url: Self::text(&self.url, cx),
            output: Self::text(&self.output, cx),
            render_js: self.render_js,
            per_host_delay: self.per_host_delay,
            exclude: Self::text(&self.exclude, cx),
            include: Self::text(&self.include, cx),
            update,
        }
    }

    fn start(&mut self, update: bool, cx: &mut Context<Self>) {
        if self.progress.running() {
            return;
        }
        let settings = self.settings(update, cx);
        self.tab = Tab::Copy;
        let running = match job::start(&settings) {
            Ok(running) => running,
            Err(message) => {
                self.progress = Progress::failed(message);
                cx.notify();
                return;
            }
        };

        // A copy that started is worth remembering, even if it fails later.
        if let Ok((url, output)) = job::parse(&settings) {
            self.projects.remember(Project {
                url: url.to_string(),
                output,
            });
            self.selected = Some(0);
            let _ = self.projects.save();
        }

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

    fn select(&mut self, index: Option<usize>, window: &mut Window, cx: &mut Context<Self>) {
        if self.progress.running() {
            return;
        }
        self.selected = index;
        let (url, output) = match index.and_then(|i| self.projects.items.get(i)) {
            Some(project) => (
                project.url.clone(),
                project.output.to_string_lossy().into_owned(),
            ),
            None => (String::new(), job::default_output()),
        };
        self.url
            .update(cx, |input, cx| input.set_value(url, window, cx));
        self.output
            .update(cx, |input, cx| input.set_value(output, window, cx));
        self.progress = Progress::default();
        self.link_map = Ok(Vec::new());
        self.tab = Tab::Copy;
        cx.notify();
    }

    fn test_rules(&mut self, cx: &mut Context<Self>) {
        let settings = self.settings(false, cx);
        self.rule_verdict = match (
            job::parse(&settings),
            insight::rules(&settings.exclude, &settings.include),
        ) {
            (Err(message), _) | (_, Err(message)) => message,
            (Ok((seed, _)), Ok(rules)) => {
                insight::explain(&seed, rules, &Self::text(&self.test_url, cx))
            }
        };
        cx.notify();
    }

    fn refresh_map(&mut self, cx: &mut Context<Self>) {
        let output = PathBuf::from(Self::text(&self.output, cx).trim());
        self.link_map = insight::link_map(&output, MAP_LIMIT);
        cx.notify();
    }

    fn sidebar(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (muted, border, accent) = (theme.muted_foreground, theme.border, theme.accent);
        let busy = self.progress.running();
        let items =
            self.projects
                .items
                .iter()
                .enumerate()
                .map(|(index, project)| {
                    let selected = self.selected == Some(index);
                    div()
                        .id(SharedString::from(format!("project-{index}")))
                        .test_support()
                        .aria_label(SharedString::from(project.name()))
                        .px_2()
                        .py_1()
                        .rounded_md()
                        .when(selected, |el| el.bg(accent))
                        .when(!busy, |el| el.cursor_pointer())
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.select(Some(index), window, cx)
                        }))
                        .child(div().text_sm().child(project.name()))
                        .child(
                            div()
                                .text_xs()
                                .text_color(muted)
                                .child(project.status().label()),
                        )
                });
        div()
            .id("projects")
            .w(px(210.))
            .h_full()
            .flex()
            .flex_col()
            .gap_1()
            .p_3()
            .border_r_1()
            .border_color(border)
            .child(div().text_xs().text_color(muted).pb_1().child("COPIES"))
            .children(items)
            .child(
                div().pt_2().child(
                    Button::new("new-copy")
                        .ghost()
                        .label("+ New copy")
                        .on_click(cx.listener(|this, _, window, cx| this.select(None, window, cx))),
                ),
            )
            .into_any_element()
    }

    fn tabs(&self, cx: &mut Context<Self>) -> AnyElement {
        let tab = |id: &'static str, label: &'static str, which: Tab, cx: &mut Context<Self>| {
            let button =
                Button::new(id)
                    .label(label)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.tab = which;
                        if which == Tab::LinkMap {
                            this.refresh_map(cx);
                        }
                        cx.notify();
                    }));
            if self.tab == which {
                button.primary()
            } else {
                button.ghost()
            }
        };
        div()
            .flex()
            .gap_1()
            .child(tab("tab-copy", "Copy", Tab::Copy, cx))
            .child(tab("tab-rules", "Rules", Tab::Rules, cx))
            .child(tab("tab-map", "Link map", Tab::LinkMap, cx))
            .into_any_element()
    }

    fn copy_view(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (muted, border) = (theme.muted_foreground, theme.border);
        let running = self.progress.running();
        let status = SharedString::from(self.progress.status());
        let finished_copy = self
            .selected
            .and_then(|i| self.projects.items.get(i))
            .is_some_and(|p| p.status() == crate::projects::ProjectStatus::Complete);

        let actions = if running {
            div().child(
                Button::new("stop")
                    .danger()
                    .label("Stop")
                    .on_click(cx.listener(|this, _, _, cx| this.stop(cx))),
            )
        } else {
            div()
                .flex()
                .gap_2()
                .child(
                    Button::new("copy")
                        .primary()
                        .label("Copy")
                        .on_click(cx.listener(|this, _, _, cx| this.start(false, cx))),
                )
                .when(finished_copy, |el| {
                    el.child(
                        Button::new("update")
                            .label("Update")
                            .on_click(cx.listener(|this, _, _, cx| this.start(true, cx))),
                    )
                })
                .when_some(self.progress.report.clone(), |el, report| {
                    el.child(
                        Button::new("report")
                            .outline()
                            .label("Open report")
                            .on_click(move |_, _, cx| {
                                cx.open_url(&format!("file://{}", report.display()))
                            }),
                    )
                })
        };

        div()
            .flex()
            .flex_col()
            .flex_1()
            .gap_4()
            .child(field("Website", Input::new(&self.url).id("url").aria_label("Website address"), muted))
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
            .child(actions)
            .child(
                div()
                    .id("status")
                    .role(Role::Status)
                    .test_support()
                    .aria_label(status.clone())
                    .text_sm()
                    .child(status),
            )
            .child(
                div()
                    .flex()
                    .gap_3()
                    .child(stat("saved", "Saved", self.progress.saved.to_string(), border, muted))
                    .child(stat("failed", "Failed", self.progress.failed.to_string(), border, muted))
                    .child(stat("size", "Downloaded", human_bytes(self.progress.bytes), border, muted)),
            )
            .child(
                div()
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
                    .children(self.progress.log.iter().map(|line| div().child(line.clone()))),
            )
            .into_any_element()
    }

    fn rules_view(&self, cx: &mut Context<Self>) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let verdict = SharedString::from(self.rule_verdict.clone());
        div()
            .flex()
            .flex_col()
            .gap_4()
            .child(div().text_sm().text_color(muted).child(
                "Patterns are regular expressions matched against each full URL. They apply to the next copy.",
            ))
            .child(field("Skip URLs matching", Input::new(&self.exclude).id("exclude").aria_label("Skip pattern"), muted))
            .child(field(
                "Also fetch URLs matching (even on other sites)",
                Input::new(&self.include).id("include").aria_label("Include pattern"),
                muted,
            ))
            .child(field("Test a URL", Input::new(&self.test_url).id("test-url").aria_label("URL to test"), muted))
            .child(
                div().child(
                    Button::new("test")
                        .label("Test")
                        .on_click(cx.listener(|this, _, _, cx| this.test_rules(cx))),
                ),
            )
            .child(
                div()
                    .id("verdict")
                    .role(Role::Status)
                    .test_support()
                    .aria_label(verdict.clone())
                    .text_sm()
                    .child(verdict),
            )
            .into_any_element()
    }

    fn map_view(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (muted, border, danger) = (theme.muted_foreground, theme.border, theme.danger);
        let body = match &self.link_map {
            Err(message) => div().text_sm().text_color(muted).child(message.clone()),
            Ok(lines) if lines.is_empty() => {
                div().text_sm().text_color(muted).child("No pages yet.")
            }
            Ok(lines) => div().children(lines.iter().map(|line| {
                div()
                    .pl(px(line.depth as f32 * 16.))
                    .flex()
                    .gap_2()
                    .child(line.url.clone())
                    .when(!line.note.is_empty(), |el| {
                        el.child(div().text_color(danger).child(line.note.clone()))
                    })
            })),
        };
        let count = self.link_map.as_ref().map_or(0, Vec::len);
        div()
            .flex()
            .flex_col()
            .flex_1()
            .gap_3()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(
                        Button::new("refresh-map")
                            .label("Refresh")
                            .on_click(cx.listener(|this, _, _, cx| this.refresh_map(cx))),
                    )
                    .child(
                        div()
                            .id("map-count")
                            .test_support()
                            .aria_label(SharedString::from(format!("{count} pages")))
                            .text_sm()
                            .text_color(muted)
                            .child(format!(
                                "{count} pages, each under the page that first linked to it"
                            )),
                    ),
            )
            .child(
                div()
                    .id("map")
                    .flex_1()
                    .overflow_y_scroll()
                    .border_1()
                    .border_color(border)
                    .rounded_md()
                    .p_2()
                    .text_xs()
                    .font_family("monospace")
                    .child(body),
            )
            .into_any_element()
    }
}

impl Render for CopyWindow {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (background, foreground, muted) = {
            let theme = cx.theme();
            (theme.background, theme.foreground, theme.muted_foreground)
        };
        let sidebar = self.sidebar(cx);
        let tabs = self.tabs(cx);
        let view = match self.tab {
            Tab::Copy => self.copy_view(cx),
            Tab::Rules => self.rules_view(cx),
            Tab::LinkMap => self.map_view(cx),
        };
        div()
            .id("main-window")
            .size_full()
            .flex()
            .bg(background)
            .text_color(foreground)
            .child(sidebar)
            .child(
                div()
                    .flex_1()
                    .h_full()
                    .flex()
                    .flex_col()
                    .gap_4()
                    .p_6()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .child(
                                        div()
                                            .text_xl()
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .child("MisterCopy"),
                                    )
                                    .child(
                                        div()
                                            .text_sm()
                                            .text_color(muted)
                                            .child("Copy a website for offline browsing."),
                                    ),
                            )
                            .child(tabs),
                    )
                    .child(view),
            )
    }
}

fn field(label: &'static str, input: Input, muted: Hsla) -> impl IntoElement {
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
    border: Hsla,
    muted: Hsla,
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
