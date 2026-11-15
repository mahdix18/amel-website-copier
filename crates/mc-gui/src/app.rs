//! The main window, laid out like an API client: a header with the command
//! search, the list of copies on the left, a request bar (requester, address,
//! Copy) with option tabs above, and the live result below: a progress bar,
//! what each parallel worker is fetching, and the activity log.

use std::path::PathBuf;
use std::time::Duration;

use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{Disableable, Icon, IconName, Selectable, Sizable};
use gpui_kit::prelude::*;
use gpui_kit::{
    AnyElement, App, Context, Entity, FocusHandle, Focusable, FontWeight, Hsla, KeyBinding,
    KeyDownEvent, Role, SharedString, TestSupportExt, Window, actions, div, px, relative,
};
use mc_core::{StopHandle, human_bytes};
use tokio::sync::broadcast::error::RecvError;

use crate::insight::{self, MapLine};
use crate::job::{self, LineKind, MAX_WORKERS, Progress, Settings, Speed};
use crate::look::Look;
use crate::palette::{self, Command};
use crate::prefs::Prefs;
use crate::projects::{Project, ProjectStatus, Projects};

/// Lines of the link map shown at once.
const MAP_LIMIT: usize = 500;
/// Matches listed in the command palette.
const PALETTE_ROWS: usize = 8;
/// Activity rows drawn; older ones stay in the log but off screen.
const ACTIVITY_ROWS: usize = 250;
/// How often the clock and speed refresh while copying.
const TICK: Duration = Duration::from_millis(500);
/// Written next to the report by "Export link map as SVG".
pub const LINK_MAP_SVG: &str = "mistercopy-linkmap.svg";

actions!(mistercopy, [TogglePalette]);

/// Keyboard shortcuts; call once when the app starts.
pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("ctrl-k", TogglePalette, None),
        KeyBinding::new("cmd-k", TogglePalette, None),
    ]);
}

/// The tabs above the request bar's options.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    /// Folder, workers, speed and switches.
    Copy,
    Rules,
    LinkMap,
}

/// The tabs of the result panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pane {
    Workers,
    Activity,
}

pub struct CopyWindow {
    url: Entity<InputState>,
    output: Entity<InputState>,
    exclude: Entity<InputState>,
    include: Entity<InputState>,
    test_url: Entity<InputState>,
    filter: Entity<InputState>,
    render_js: bool,
    /// Gap between requests to one host. Polite by default.
    pub speed: Speed,
    /// Parallel downloads.
    pub workers: usize,
    respect_robots: bool,
    projects: Projects,
    /// Each project's status, read from its store when the list changes
    /// rather than on every frame.
    statuses: Vec<ProjectStatus>,
    selected: Option<usize>,
    tab: Tab,
    pane: Pane,
    errors_only: bool,
    progress: Progress,
    stop: Option<StopHandle>,
    rule_verdict: String,
    link_map: Result<Vec<MapLine>, String>,
    map_notice: String,
    focus: FocusHandle,
    palette_open: bool,
    palette_query: Entity<InputState>,
    /// Set once the startup probe (MISTERCOPY_PROBE) has been armed.
    probed: bool,
    prefs_path: Option<PathBuf>,
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
        let palette_query = input("Type a command…", window, cx);
        // Enter runs the best match.
        cx.subscribe_in(
            &palette_query,
            window,
            |this, _, event: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { .. } = event
                    && let Some((_, command)) = this.palette_matches(cx).into_iter().next()
                {
                    this.run(command, window, cx);
                }
            },
        )
        .detach();
        let url = input("Enter a site address, e.g. https://example.com", window, cx);
        // Enter in the address bar starts the copy, like sending a request.
        cx.subscribe_in(&url, window, |this, _, event: &InputEvent, _, cx| {
            if let InputEvent::PressEnter { .. } = event {
                this.start(false, cx);
            }
        })
        .detach();
        let filter = input("Filter copies", window, cx);
        cx.subscribe_in(&filter, window, |_, _, event: &InputEvent, _, cx| {
            if let InputEvent::Change = event {
                cx.notify();
            }
        })
        .detach();
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        let statuses = projects.items.iter().map(Project::status).collect();
        let prefs_path = projects
            .path()
            .and_then(|p| p.parent())
            .map(|dir| dir.join("prefs.json"));
        let prefs = Prefs::load(prefs_path.as_ref());
        if prefs.dark != Look::global(cx).dark {
            let look = if prefs.dark {
                Look::dark()
            } else {
                Look::light()
            };
            look.apply(cx);
        }
        Self {
            url,
            output: cx.new(|cx| InputState::new(window, cx).default_value(job::default_output())),
            exclude: input("e.g. /admin/|\\.zip$", window, cx),
            include: input("e.g. ^https://cdn\\.example\\.com/", window, cx),
            test_url: input("https://example.com/some/page", window, cx),
            filter,
            render_js: prefs.render_js && job::browser_support(),
            speed: prefs.speed,
            workers: prefs.workers,
            respect_robots: prefs.respect_robots,
            projects,
            statuses,
            selected: None,
            tab: Tab::Copy,
            pane: Pane::Workers,
            errors_only: false,
            progress: Progress::default(),
            stop: None,
            rule_verdict: String::new(),
            link_map: Ok(Vec::new()),
            map_notice: String::new(),
            focus,
            palette_open: false,
            palette_query,
            probed: false,
            prefs_path,
        }
    }

    /// Saves the window's choices for the next launch, then redraws.
    fn changed(&mut self, cx: &mut Context<Self>) {
        let prefs = Prefs {
            workers: self.workers,
            speed: self.speed,
            respect_robots: self.respect_robots,
            render_js: self.render_js,
            dark: Look::global(cx).dark,
        };
        let _ = prefs.save(self.prefs_path.as_ref());
        cx.notify();
    }

    /// Fills the form from the command line (`mistercopy-gui URL
    /// [--output DIR] [--start]`), optionally starting the copy at once.
    pub fn prefill(
        &mut self,
        url: Option<String>,
        output: Option<String>,
        start: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(url) = url {
            self.url
                .update(cx, |input, cx| input.set_value(url, window, cx));
        }
        if let Some(output) = output {
            self.output
                .update(cx, |input, cx| input.set_value(output, window, cx));
        }
        if start {
            self.start(false, cx);
        }
    }

    fn refresh_statuses(&mut self) {
        self.statuses = self.projects.items.iter().map(Project::status).collect();
    }

    fn can_update(&self) -> bool {
        self.selected
            .and_then(|i| self.statuses.get(i))
            .is_some_and(|status| *status == ProjectStatus::Complete)
    }

    fn palette_matches(&self, cx: &Context<Self>) -> Vec<(String, Command)> {
        let names: Vec<String> = self.projects.items.iter().map(Project::name).collect();
        let commands = palette::commands(&palette::Context {
            running: self.progress.running(),
            can_update: self.can_update(),
            has_report: self.progress.report.is_some(),
            projects: &names,
        });
        palette::matching(commands, &Self::text(&self.palette_query, cx))
    }

    fn toggle_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.palette_open {
            return self.close_palette(window, cx);
        }
        self.palette_open = true;
        self.palette_query
            .update(cx, |input, cx| input.set_value(String::new(), window, cx));
        window.focus(&self.palette_query.focus_handle(cx), cx);
        cx.notify();
    }

    fn close_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.palette_open = false;
        window.focus(&self.focus, cx);
        cx.notify();
    }

    fn run(&mut self, command: Command, window: &mut Window, cx: &mut Context<Self>) {
        self.close_palette(window, cx);
        match command {
            Command::NewCopy => self.select(None, window, cx),
            Command::Select(index) => self.select(Some(index), window, cx),
            Command::Start => self.start(false, cx),
            Command::Update => self.start(true, cx),
            Command::Stop => self.stop(cx),
            Command::Show(tab) => self.show(tab, cx),
            Command::ExportMap => {
                self.tab = Tab::LinkMap;
                self.export_map(cx);
            }
            Command::OpenReport => {
                if let Some(report) = &self.progress.report {
                    cx.open_url(&format!("file://{}", report.display()));
                }
            }
        }
        cx.notify();
    }

    fn show(&mut self, tab: Tab, cx: &mut Context<Self>) {
        self.tab = tab;
        if tab == Tab::LinkMap {
            self.refresh_map(cx);
        }
        cx.notify();
    }

    fn export_map(&mut self, cx: &mut Context<Self>) {
        self.refresh_map(cx);
        let output = PathBuf::from(Self::text(&self.output, cx).trim());
        self.map_notice = match &self.link_map {
            Ok(lines) if !lines.is_empty() => {
                let path = output.join(LINK_MAP_SVG);
                match std::fs::write(&path, insight::link_map_svg(lines)) {
                    Ok(()) => {
                        cx.open_url(&format!("file://{}", path.display()));
                        format!("Saved {}", path.display())
                    }
                    Err(error) => format!("Could not save {}: {error}", path.display()),
                }
            }
            Ok(_) => "Nothing to export yet.".to_owned(),
            Err(message) => message.clone(),
        };
        cx.notify();
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
            workers: self.workers,
            per_host_delay: self.speed.delay(),
            respect_robots: self.respect_robots,
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
            self.refresh_statuses();
        }

        self.progress = Progress::started(running.resumed, running.workers);
        self.pane = Pane::Workers;
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
                // Idle workers say nothing once it's over; the log does.
                this.pane = Pane::Activity;
                this.stop = None;
                this.refresh_statuses();
                cx.notify();
            });
        })
        .detach();
        // Keeps the clock and speed moving between events.
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(TICK).await;
                let running = this.update(cx, |this, cx| {
                    cx.notify();
                    this.progress.running()
                });
                if !matches!(running, Ok(true)) {
                    break;
                }
            }
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
        self.refresh_statuses();
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

    fn open_folder(&self, cx: &mut Context<Self>) {
        let folder = PathBuf::from(Self::text(&self.output, cx).trim());
        cx.open_url(&format!("file://{}", folder.display()));
    }

    fn toggle_look(&mut self, cx: &mut Context<Self>) {
        let look = if Look::global(cx).dark {
            Look::light()
        } else {
            Look::dark()
        };
        look.apply(cx);
        self.changed(cx);
    }

    // ---- header, rail, sidebar, status bar ---------------------------------

    fn header(&self, look: &Look, cx: &mut Context<Self>) -> AnyElement {
        let search = div()
            .id("commands")
            .test_support()
            .aria_label("Commands")
            .cursor_pointer()
            .w(px(340.))
            .h(px(28.))
            .px_3()
            .flex()
            .items_center()
            .gap_2()
            .rounded_md()
            .bg(look.raised)
            .border_1()
            .border_color(look.border)
            .text_sm()
            .text_color(look.muted)
            .hover(|el| el.border_color(look.muted))
            .on_click(cx.listener(|this, _, window, cx| this.toggle_palette(window, cx)))
            .child(Icon::new(IconName::Search).small().text_color(look.muted))
            .child(div().flex_1().child("Search commands"))
            .child(kbd("Ctrl K", look));
        let theme_icon = if look.dark {
            IconName::Sun
        } else {
            IconName::Moon
        };
        div()
            .h(px(44.))
            .flex_none()
            .px_3()
            .flex()
            .items_center()
            .justify_between()
            .bg(look.chrome)
            .border_b_1()
            .border_color(look.border)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .size(px(24.))
                            .rounded_md()
                            .bg(look.accent)
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_color(look.on_accent)
                            .text_sm()
                            .font_weight(FontWeight::BOLD)
                            .child("M"),
                    )
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("MisterCopy"),
                    )
                    .child(chip(
                        concat!("v", env!("CARGO_PKG_VERSION")),
                        look.muted,
                        look.raised,
                    )),
            )
            .child(search)
            .child(
                Button::new("theme")
                    .ghost()
                    .small()
                    .icon(theme_icon)
                    .tooltip("Switch light / dark")
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_look(cx))),
            )
            .into_any_element()
    }

    fn rail(&self, look: &Look, cx: &mut Context<Self>) -> AnyElement {
        let item = |id: &'static str,
                    icon: IconName,
                    tip: &'static str,
                    active: bool,
                    cx: &mut Context<Self>,
                    action: fn(&mut Self, &mut Context<Self>)| {
            Button::new(id)
                .ghost()
                .icon(icon)
                .tooltip(tip)
                .when(active, |b| b.selected(true))
                .on_click(cx.listener(move |this, _, _, cx| action(this, cx)))
        };
        div()
            .w(px(48.))
            .flex_none()
            .h_full()
            .py_2()
            .flex()
            .flex_col()
            .items_center()
            .gap_1()
            .bg(look.chrome)
            .border_r_1()
            .border_color(look.border)
            .child(item(
                "rail-copy",
                IconName::Globe,
                "Copy options",
                self.tab == Tab::Copy,
                cx,
                |this, cx| this.show(Tab::Copy, cx),
            ))
            .child(item(
                "rail-rules",
                IconName::Settings2,
                "Rules",
                self.tab == Tab::Rules,
                cx,
                |this, cx| this.show(Tab::Rules, cx),
            ))
            .child(item(
                "rail-map",
                IconName::Network,
                "Link map",
                self.tab == Tab::LinkMap,
                cx,
                |this, cx| this.show(Tab::LinkMap, cx),
            ))
            .child(div().flex_1())
            .child(item(
                "rail-folder",
                IconName::FolderOpen,
                "Open the copy's folder",
                false,
                cx,
                |this, cx| this.open_folder(cx),
            ))
            .into_any_element()
    }

    fn sidebar(&self, look: &Look, cx: &mut Context<Self>) -> AnyElement {
        let busy = self.progress.running();
        let filter = Self::text(&self.filter, cx).to_lowercase();
        let items: Vec<AnyElement> =
            self.projects
                .items
                .iter()
                .enumerate()
                .filter(|(_, project)| {
                    filter.is_empty() || project.url.to_lowercase().contains(&filter)
                })
                .map(|(index, project)| {
                    let selected = self.selected == Some(index);
                    let status = self
                        .statuses
                        .get(index)
                        .copied()
                        .unwrap_or(ProjectStatus::NotStarted);
                    let running = busy && selected;
                    let (dot_color, label) = match status {
                        _ if running => (look.accent, "copying"),
                        ProjectStatus::Complete => (look.success, status.label()),
                        ProjectStatus::Unfinished => (look.warning, status.label()),
                        ProjectStatus::NotStarted => (look.muted, status.label()),
                    };
                    // Tells apart two copies of the same site.
                    let folder = project
                        .output
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    div()
                        .id(SharedString::from(format!("project-{index}")))
                        .test_support()
                        .aria_label(SharedString::from(project.name()))
                        .px_2()
                        .py(px(6.))
                        .flex()
                        .items_center()
                        .gap_2()
                        .rounded_md()
                        .border_l_2()
                        .border_color(if selected {
                            look.accent
                        } else {
                            gpui_kit::transparent_black()
                        })
                        .when(selected, |el| el.bg(look.raised))
                        .when(!busy, |el| {
                            el.cursor_pointer().hover(|el| el.bg(look.raised))
                        })
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.select(Some(index), window, cx)
                        }))
                        .child(dot(dot_color))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .flex()
                                .flex_col()
                                .child(
                                    div()
                                        .text_sm()
                                        .whitespace_nowrap()
                                        .overflow_hidden()
                                        .text_ellipsis()
                                        .child(project.name()),
                                )
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(look.muted)
                                        .whitespace_nowrap()
                                        .overflow_hidden()
                                        .text_ellipsis()
                                        .child(format!("{label} · {folder}")),
                                ),
                        )
                        .into_any_element()
                })
                .collect();
        let empty = items.is_empty();
        div()
            .id("projects")
            .w(px(236.))
            .flex_none()
            .h_full()
            .flex()
            .flex_col()
            .bg(look.surface)
            .border_r_1()
            .border_color(look.border)
            .child(
                div()
                    .px_3()
                    .pt_3()
                    .pb_2()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_xs()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(look.muted)
                            .child("COPIES"),
                    )
                    .child(
                        Button::new("new-copy")
                            .ghost()
                            .small()
                            .icon(IconName::Plus)
                            .tooltip("New copy")
                            .on_click(
                                cx.listener(|this, _, window, cx| this.select(None, window, cx)),
                            ),
                    ),
            )
            .child(
                div().px_3().pb_2().child(
                    Input::new(&self.filter)
                        .small()
                        .prefix(Icon::new(IconName::Search).small().text_color(look.muted))
                        .id("filter")
                        .aria_label("Filter copies"),
                ),
            )
            .child(
                div()
                    .id("project-list")
                    .flex_1()
                    .overflow_y_scroll()
                    .px_2()
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .children(items)
                    .when(empty, |el| {
                        el.child(
                            div()
                                .px_2()
                                .py_4()
                                .text_xs()
                                .text_color(look.muted)
                                .child("Copies you make show up here."),
                        )
                    }),
            )
            .into_any_element()
    }

    fn status_bar(&self, look: &Look, cx: &mut Context<Self>) -> AnyElement {
        let (color, phase) = phase_style(&self.progress, look);
        let mode = if self.render_js {
            "Chromium + HTTP"
        } else {
            "HTTP (Chrome TLS)"
        };
        div()
            .h(px(26.))
            .flex_none()
            .px_3()
            .flex()
            .items_center()
            .justify_between()
            .bg(look.chrome)
            .border_t_1()
            .border_color(look.border)
            .text_xs()
            .text_color(look.muted)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_1()
                            .child(dot(color))
                            .child(phase),
                    )
                    .child(format!(
                        "{} workers · {} · {mode}",
                        self.workers,
                        self.speed.label()
                    )),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .when(self.progress.running(), |el| {
                        el.child(format!("{} active", self.progress.active()))
                    })
                    .child(
                        div()
                            .id("status-folder")
                            .cursor_pointer()
                            .hover(|el| el.text_color(look.text))
                            .on_click(cx.listener(|this, _, _, cx| this.open_folder(cx)))
                            .child("Open folder"),
                    ),
            )
            .into_any_element()
    }

    // ---- request bar and its tabs ------------------------------------------

    fn request_bar(&self, look: &Look, cx: &mut Context<Self>) -> AnyElement {
        let running = self.progress.running();
        let (method, method_color) = if self.render_js {
            ("AUTO", look.info)
        } else {
            ("HTTP", look.success)
        };
        let method_chip = div()
            .id("requester")
            .test_support()
            .aria_label(SharedString::from(format!("Requester: {method}")))
            .h_full()
            .px_3()
            .flex()
            .items_center()
            .gap_1()
            .border_r_1()
            .border_color(look.border)
            .text_xs()
            .font_weight(FontWeight::BOLD)
            .text_color(method_color)
            .child(method)
            .when(job::browser_support() && !running, |el| {
                el.cursor_pointer()
                    .hover(|el| el.bg(look.border))
                    .child(
                        Icon::new(IconName::ChevronDown)
                            .xsmall()
                            .text_color(look.muted),
                    )
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.render_js = !this.render_js;
                        this.changed(cx);
                    }))
            });
        let action = if running {
            Button::new("stop")
                .danger()
                .icon(IconName::Square)
                .label("Stop")
                .on_click(cx.listener(|this, _, _, cx| this.stop(cx)))
        } else {
            Button::new("copy")
                .primary()
                .icon(IconName::Play)
                .label("Copy")
                .on_click(cx.listener(|this, _, _, cx| this.start(false, cx)))
        };
        div()
            .flex()
            .items_center()
            .gap_2()
            .child(
                div()
                    .flex_1()
                    .h(px(36.))
                    .flex()
                    .items_center()
                    .rounded_md()
                    .border_1()
                    .border_color(look.border)
                    .bg(look.raised)
                    .overflow_hidden()
                    .child(method_chip)
                    .child(
                        div().flex_1().child(
                            Input::new(&self.url)
                                .appearance(false)
                                .id("url")
                                .aria_label("Website address"),
                        ),
                    ),
            )
            .child(div().h(px(36.)).flex().items_center().child(action))
            .when(!running && self.can_update(), |el| {
                el.child(
                    Button::new("update")
                        .outline()
                        .icon(IconName::RefreshCw)
                        .label("Update")
                        .tooltip("Re-check the copy and download only what changed")
                        .on_click(cx.listener(|this, _, _, cx| this.start(true, cx))),
                )
            })
            .into_any_element()
    }

    fn tabs(&self, look: &Look, cx: &mut Context<Self>) -> AnyElement {
        let rules = [&self.exclude, &self.include]
            .iter()
            .filter(|input| !Self::text(input, cx).trim().is_empty())
            .count();
        let tab = |id: &'static str, label: &'static str, which: Tab, badge: usize| {
            underline_tab(
                id,
                label,
                self.tab == which,
                badge,
                look,
                cx.listener(move |this, _, _, cx| this.show(which, cx)),
            )
        };
        div()
            .flex()
            .gap_4()
            .border_b_1()
            .border_color(look.border)
            .child(tab("tab-copy", "Options", Tab::Copy, 0))
            .child(tab("tab-rules", "Rules", Tab::Rules, rules))
            .child(tab("tab-map", "Link map", Tab::LinkMap, 0))
            .into_any_element()
    }

    fn options_view(&self, look: &Look, cx: &mut Context<Self>) -> AnyElement {
        let running = self.progress.running();
        let stepper = div()
            .flex()
            .items_center()
            .h(px(30.))
            .rounded_md()
            .border_1()
            .border_color(look.border)
            .bg(look.raised)
            .child(
                Button::new("workers-dec")
                    .ghost()
                    .small()
                    .icon(IconName::Minus)
                    .disabled(running || self.workers <= 1)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.workers = this.workers.saturating_sub(1).max(1);
                        this.changed(cx);
                    })),
            )
            .child(
                div()
                    .id("workers")
                    .test_support()
                    .aria_label(SharedString::from(format!("Workers: {}", self.workers)))
                    .w(px(36.))
                    .flex()
                    .justify_center()
                    .text_sm()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(self.workers.to_string()),
            )
            .child(
                Button::new("workers-inc")
                    .ghost()
                    .small()
                    .icon(IconName::Plus)
                    .disabled(running || self.workers >= MAX_WORKERS)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.workers = (this.workers + 1).min(MAX_WORKERS);
                        this.changed(cx);
                    })),
            );
        let speeds = div()
            .flex()
            .p(px(2.))
            .rounded_md()
            .bg(look.raised)
            .border_1()
            .border_color(look.border)
            .children(Speed::ALL.map(|speed| {
                let active = self.speed == speed;
                div()
                    .id(SharedString::from(format!(
                        "speed-{}",
                        speed.label().to_lowercase()
                    )))
                    .test_support()
                    .aria_label(SharedString::from(speed.label()))
                    .px_3()
                    .py(px(3.))
                    .rounded(px(4.))
                    .text_sm()
                    .when(active, |el| el.bg(look.surface).text_color(look.text))
                    .when(!active, |el| el.text_color(look.muted))
                    .when(!running, |el| {
                        el.cursor_pointer()
                            .hover(|el| el.text_color(look.text))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.speed = speed;
                                this.changed(cx);
                            }))
                    })
                    .child(speed.label())
            }));
        let browser = job::browser_support();
        div()
            .flex()
            .flex_col()
            .gap_4()
            .child(labelled(
                "Save in",
                None,
                div()
                    .flex()
                    .gap_2()
                    .child(
                        div().flex_1().child(
                            Input::new(&self.output)
                                .id("output")
                                .aria_label("Folder to save in"),
                        ),
                    )
                    .child(
                        Button::new("open-folder")
                            .outline()
                            .icon(IconName::FolderOpen)
                            .tooltip("Open the folder")
                            .on_click(cx.listener(|this, _, _, cx| this.open_folder(cx))),
                    ),
                look,
            ))
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap_6()
                    .child(labelled(
                        "Parallel downloads",
                        Some("requests in flight at once"),
                        stepper,
                        look,
                    ))
                    .child(labelled("Speed", Some(self.speed.hint()), speeds, look)),
            )
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap_6()
                    .child(
                        Switch::new("robots")
                            .checked(self.respect_robots)
                            .label("Respect robots.txt")
                            .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                this.respect_robots = *checked;
                                this.changed(cx);
                            })),
                    )
                    .when(browser, |el| {
                        el.child(
                            Switch::new("render-js")
                                .checked(self.render_js)
                                .label("Render JavaScript with Chromium")
                                .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                    this.render_js = *checked;
                                    this.changed(cx);
                                })),
                        )
                    }),
            )
            .into_any_element()
    }

    fn rules_view(&self, look: &Look, cx: &mut Context<Self>) -> AnyElement {
        let verdict = SharedString::from(self.rule_verdict.clone());
        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(div().text_xs().text_color(look.muted).child(
                "Regular expressions matched against each full URL. They apply to the next copy.",
            ))
            .child(
                div()
                    .flex()
                    .gap_3()
                    .child(
                        div().flex_1().child(labelled(
                            "Skip URLs matching",
                            None,
                            Input::new(&self.exclude)
                                .id("exclude")
                                .aria_label("Skip pattern"),
                            look,
                        )),
                    )
                    .child(
                        div().flex_1().child(labelled(
                            "Also fetch (even other sites)",
                            None,
                            Input::new(&self.include)
                                .id("include")
                                .aria_label("Include pattern"),
                            look,
                        )),
                    ),
            )
            .child(labelled(
                "Test a URL",
                None,
                div()
                    .flex()
                    .gap_2()
                    .child(
                        div().flex_1().child(
                            Input::new(&self.test_url)
                                .id("test-url")
                                .aria_label("URL to test"),
                        ),
                    )
                    .child(
                        Button::new("test")
                            .outline()
                            .label("Test")
                            .on_click(cx.listener(|this, _, _, cx| this.test_rules(cx))),
                    ),
                look,
            ))
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

    fn map_view(&self, look: &Look, cx: &mut Context<Self>) -> AnyElement {
        let body = match &self.link_map {
            Err(message) => div()
                .text_sm()
                .text_color(look.muted)
                .child(message.clone()),
            Ok(lines) if lines.is_empty() => div()
                .text_sm()
                .text_color(look.muted)
                .child("No pages yet."),
            Ok(lines) => div().children(lines.iter().map(|line| {
                div()
                    .pl(px(line.depth as f32 * 16.))
                    .flex()
                    .gap_2()
                    .whitespace_nowrap()
                    .child(line.url.clone())
                    .when(!line.note.is_empty(), |el| {
                        el.child(div().text_color(look.danger).child(line.note.clone()))
                    })
            })),
        };
        let count = self.link_map.as_ref().map_or(0, Vec::len);
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        Button::new("refresh-map")
                            .outline()
                            .small()
                            .icon(IconName::RefreshCw)
                            .label("Refresh")
                            .on_click(cx.listener(|this, _, _, cx| this.refresh_map(cx))),
                    )
                    .child(
                        Button::new("export-map")
                            .outline()
                            .small()
                            .icon(IconName::ExternalLink)
                            .label("Export SVG")
                            .on_click(cx.listener(|this, _, _, cx| this.export_map(cx))),
                    )
                    .child(
                        div()
                            .id("map-count")
                            .test_support()
                            .aria_label(SharedString::from(format!("{count} pages")))
                            .text_xs()
                            .text_color(look.muted)
                            .child(format!(
                                "{count} pages, each under the page that first linked to it"
                            )),
                    ),
            )
            .when(!self.map_notice.is_empty(), |el| {
                el.child(
                    div()
                        .id("map-notice")
                        .test_support()
                        .aria_label(SharedString::from(self.map_notice.clone()))
                        .text_xs()
                        .text_color(look.muted)
                        .child(self.map_notice.clone()),
                )
            })
            .child(
                div()
                    .id("map")
                    .h(px(150.))
                    .overflow_y_scroll()
                    .rounded_md()
                    .border_1()
                    .border_color(look.border)
                    .bg(look.chrome)
                    .p_2()
                    .text_xs()
                    .font_family("monospace")
                    .child(body),
            )
            .into_any_element()
    }

    // ---- result panel ------------------------------------------------------

    fn result_panel(&self, look: &Look, cx: &mut Context<Self>) -> AnyElement {
        let progress = &self.progress;
        let status = SharedString::from(progress.status());
        let (phase_color, phase) = phase_style(progress, look);
        let idle = progress.phase == job::Phase::Idle;
        let failed_to_start =
            matches!(progress.phase, job::Phase::Failed(_)) && progress.started.is_none();

        let metrics = div()
            .flex()
            .items_center()
            .gap_4()
            .text_xs()
            .child(
                div()
                    .px_2()
                    .py(px(2.))
                    .rounded_md()
                    .bg(phase_color.opacity(0.15))
                    .text_color(phase_color)
                    .font_weight(FontWeight::SEMIBOLD)
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(dot(phase_color))
                    .child(phase),
            )
            .child(metric("Time", clock(progress.elapsed()), look))
            .child(metric("Size", human_bytes(progress.bytes), look))
            .child(metric(
                "Speed",
                format!("{}/s", human_bytes(progress.rate())),
                look,
            ));

        let fraction = progress.fraction();
        let bar_label = match fraction {
            Some(f) => format!(
                "{} / {} URLs · {:.0}%",
                progress.done,
                progress.total.max(progress.done),
                f * 100.0
            ),
            None if progress.running() => "Discovering pages…".to_owned(),
            None => String::new(),
        };
        let bar = div()
            .id("progress")
            .test_support()
            .role(Role::ProgressIndicator)
            .aria_label(SharedString::from(bar_label.clone()))
            .flex()
            .flex_col()
            .gap_1()
            .child(
                div()
                    .h(px(6.))
                    .w_full()
                    .rounded_full()
                    .bg(look.raised)
                    .overflow_hidden()
                    .child(
                        div()
                            .h_full()
                            .rounded_full()
                            .bg(match progress.phase {
                                job::Phase::Failed(_) => look.danger,
                                job::Phase::Finished => look.success,
                                _ => look.accent,
                            })
                            .when_some(fraction, |el, f| el.w(relative(f)))
                            .when(fraction.is_none() && progress.running(), |el| {
                                el.w_full().opacity(0.35)
                            })
                            .when(fraction.is_none() && !progress.running(), |el| el.w(px(0.))),
                    ),
            )
            .child(
                div()
                    .flex()
                    .justify_between()
                    .text_xs()
                    .text_color(look.muted)
                    .child(bar_label)
                    .child(workers_line(progress)),
            );

        let counters = div()
            .flex()
            .gap_2()
            .child(stat(
                "saved",
                "Saved",
                progress.saved.to_string(),
                look.success,
                look,
            ))
            .child(stat(
                "same",
                "Unchanged",
                progress.unchanged.to_string(),
                look.info,
                look,
            ))
            .child(stat(
                "failed",
                "Failed",
                progress.failed.to_string(),
                look.danger,
                look,
            ))
            .child(stat(
                "retries",
                "Retries",
                progress.retries.to_string(),
                look.warning,
                look,
            ))
            .child(stat(
                "size",
                "Downloaded",
                human_bytes(progress.bytes),
                look.text,
                look,
            ));

        let pane_tab = |id: &'static str, label: String, which: Pane, cx: &mut Context<Self>| {
            underline_tab(
                id,
                label,
                self.pane == which,
                0,
                look,
                cx.listener(move |this, _, _, cx| {
                    this.pane = which;
                    cx.notify();
                }),
            )
        };
        let tabs = div()
            .flex()
            .items_center()
            .justify_between()
            .border_b_1()
            .border_color(look.border)
            .child(
                div()
                    .flex()
                    .gap_4()
                    .child(pane_tab(
                        "pane-workers",
                        format!("Workers ({})", progress.workers.len()),
                        Pane::Workers,
                        cx,
                    ))
                    .child(pane_tab(
                        "pane-activity",
                        format!("Activity ({})", progress.log.len()),
                        Pane::Activity,
                        cx,
                    )),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .when(self.pane == Pane::Activity, |el| {
                        el.child(
                            Switch::new("errors-only")
                                .small()
                                .checked(self.errors_only)
                                .label("Errors only")
                                .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                    this.errors_only = *checked;
                                    cx.notify();
                                })),
                        )
                    })
                    .when_some(progress.report.clone(), |el, report| {
                        el.child(
                            Button::new("report")
                                .ghost()
                                .small()
                                .icon(IconName::FileText)
                                .label("Open report")
                                .on_click(move |_, _, cx| {
                                    cx.open_url(&format!("file://{}", report.display()))
                                }),
                        )
                    }),
            );

        let body = match self.pane {
            Pane::Workers => self.workers_view(look),
            Pane::Activity => self.activity_view(look),
        };

        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .gap_3()
            .p_4()
            .bg(look.surface)
            .rounded_lg()
            .border_1()
            .border_color(look.border)
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("Progress"),
                    )
                    .child(metrics),
            )
            .child(
                div()
                    .id("status")
                    .role(Role::Status)
                    .test_support()
                    .aria_label(status.clone())
                    .text_sm()
                    .text_color(match progress.phase {
                        job::Phase::Failed(_) => look.danger,
                        _ => look.muted,
                    })
                    .child(status),
            )
            .when(idle || failed_to_start, |el| el.child(empty_state(look)))
            .when(!(idle || failed_to_start), |el| {
                el.child(bar).child(counters).child(tabs).child(body)
            })
            .into_any_element()
    }

    fn workers_view(&self, look: &Look) -> AnyElement {
        let rows = self
            .progress
            .workers
            .iter()
            .enumerate()
            .map(|(index, worker)| {
                let busy = worker.url.is_some();
                let (verb, verb_color, dot_color) = match (busy, worker.waiting) {
                    (false, _) => ("idle", look.muted, look.border),
                    (true, true) => ("WAIT", look.warning, look.warning),
                    (true, false) => ("GET", look.success, look.accent),
                };
                let waited = worker
                    .since
                    .map(|since| format!("{} ms", since.elapsed().as_millis()))
                    .unwrap_or_default();
                div()
                    .id(SharedString::from(format!("worker-{index}")))
                    .test_support()
                    .aria_label(SharedString::from(format!(
                        "Worker {}: {}",
                        index + 1,
                        worker.url.as_deref().unwrap_or("idle")
                    )))
                    .h(px(28.))
                    .px_2()
                    .flex()
                    .items_center()
                    .gap_3()
                    .rounded_md()
                    .when(index % 2 == 1, |el| el.bg(look.chrome.opacity(0.5)))
                    .child(
                        div()
                            .w(px(34.))
                            .text_xs()
                            .font_family("monospace")
                            .text_color(look.muted)
                            .child(format!("#{:02}", index + 1)),
                    )
                    .child(dot(dot_color))
                    .child(
                        div()
                            .w(px(36.))
                            .text_xs()
                            .font_weight(FontWeight::BOLD)
                            .text_color(verb_color)
                            .child(verb),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_xs()
                            .font_family("monospace")
                            .whitespace_nowrap()
                            .overflow_hidden()
                            .text_ellipsis()
                            .text_color(if busy { look.text } else { look.muted })
                            .child(
                                worker
                                    .url
                                    .clone()
                                    .unwrap_or_else(|| "waiting for work".to_owned()),
                            ),
                    )
                    .child(
                        div()
                            .w(px(64.))
                            .flex()
                            .justify_end()
                            .text_xs()
                            .text_color(look.muted)
                            .child(waited),
                    )
                    .child(
                        div()
                            .w(px(64.))
                            .flex()
                            .justify_end()
                            .text_xs()
                            .text_color(look.muted)
                            .child(format!("{} req", worker.requests)),
                    )
            });
        div()
            .id("workers-list")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .children(rows)
            .into_any_element()
    }

    fn activity_view(&self, look: &Look) -> AnyElement {
        let rows = self
            .progress
            .log
            .iter()
            .rev()
            .filter(|line| {
                !self.errors_only || matches!(line.kind, LineKind::Failed | LineKind::Retry)
            })
            .take(ACTIVITY_ROWS)
            .map(|line| {
                let color = match line.kind {
                    LineKind::Saved => look.success,
                    LineKind::Unchanged => look.info,
                    LineKind::Failed => look.danger,
                    LineKind::Retry => look.warning,
                    LineKind::Info => look.muted,
                };
                div()
                    .min_h(px(24.))
                    .px_2()
                    .flex()
                    .items_center()
                    .gap_3()
                    .border_b_1()
                    .border_color(look.border.opacity(0.5))
                    .child(div().w(px(46.)).flex().child(chip(
                        line.kind.tag(),
                        color,
                        color.opacity(0.14),
                    )))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_xs()
                            .font_family("monospace")
                            .whitespace_nowrap()
                            .overflow_hidden()
                            .text_ellipsis()
                            .child(line.text.clone()),
                    )
                    .child(
                        div()
                            .max_w(px(260.))
                            .text_xs()
                            .whitespace_nowrap()
                            .overflow_hidden()
                            .text_ellipsis()
                            .text_color(if line.kind == LineKind::Failed {
                                look.danger
                            } else {
                                look.muted
                            })
                            .child(line.detail.clone()),
                    )
            });
        div()
            .id("log")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .children(rows)
            .into_any_element()
    }

    fn palette(&self, look: &Look, cx: &mut Context<Self>) -> AnyElement {
        let matches = self.palette_matches(cx);
        let rows =
            matches
                .into_iter()
                .take(PALETTE_ROWS)
                .enumerate()
                .map(|(index, (label, command))| {
                    div()
                        .id(SharedString::from(format!("cmd-{index}")))
                        .test_support()
                        .aria_label(SharedString::from(label.clone()))
                        .px_3()
                        .py(px(6.))
                        .rounded_md()
                        .text_sm()
                        .cursor_pointer()
                        .when(index == 0, |el| el.bg(look.raised))
                        .hover(|el| el.bg(look.raised))
                        .on_click(
                            cx.listener(move |this, _, window, cx| this.run(command, window, cx)),
                        )
                        .child(label)
                });
        div()
            .id("palette-backdrop")
            .absolute()
            .inset_0()
            .flex()
            .justify_center()
            .pt(px(60.))
            .bg(gpui_kit::black().opacity(0.35))
            .on_click(cx.listener(|this, _, window, cx| this.close_palette(window, cx)))
            .child(
                div()
                    .id("palette")
                    .test_support()
                    .w(px(480.))
                    .h_auto()
                    .p_2()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .bg(look.surface)
                    .border_1()
                    .border_color(look.border)
                    .rounded_lg()
                    .shadow_lg()
                    // Clicks inside don't reach the backdrop.
                    .on_click(|_, _, cx| cx.stop_propagation())
                    .child(
                        Input::new(&self.palette_query)
                            .prefix(Icon::new(IconName::Search).small().text_color(look.muted))
                            .id("palette-query")
                            .aria_label("Command"),
                    )
                    .children(rows)
                    .child(
                        div()
                            .px_1()
                            .pt_1()
                            .flex()
                            .gap_2()
                            .text_xs()
                            .text_color(look.muted)
                            .child(kbd("Enter", look))
                            .child("runs the first match")
                            .child(kbd("Esc", look))
                            .child("closes"),
                    ),
            )
            .into_any_element()
    }
}

impl Render for CopyWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.probed && std::env::var_os("MISTERCOPY_PROBE").is_some() {
            // Measures startup: report once the first frame is on screen, then quit.
            self.probed = true;
            window.on_next_frame(|_, cx| {
                println!(
                    "startup_ms={} rss_kb={}",
                    crate::startup_ms(),
                    crate::rss_kb().unwrap_or(0)
                );
                cx.quit();
            });
        }
        let look = Look::global(cx);
        let header = self.header(&look, cx);
        let rail = self.rail(&look, cx);
        let sidebar = self.sidebar(&look, cx);
        let request = self.request_bar(&look, cx);
        let tabs = self.tabs(&look, cx);
        let view = match self.tab {
            Tab::Copy => self.options_view(&look, cx),
            Tab::Rules => self.rules_view(&look, cx),
            Tab::LinkMap => self.map_view(&look, cx),
        };
        let result = self.result_panel(&look, cx);
        let status_bar = self.status_bar(&look, cx);
        let palette = self.palette_open.then(|| self.palette(&look, cx));
        div()
            .id("main-window")
            .track_focus(&self.focus)
            .key_context("MisterCopy")
            .on_action(
                cx.listener(|this, _: &TogglePalette, window, cx| this.toggle_palette(window, cx)),
            )
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                if this.palette_open && event.keystroke.key == "escape" {
                    this.close_palette(window, cx);
                }
            }))
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .bg(look.chrome)
            .text_color(look.text)
            .child(header)
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .child(rail)
                    .child(sidebar)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .flex()
                            .flex_col()
                            .gap_3()
                            .p_3()
                            .child(
                                div()
                                    .flex_none()
                                    .flex()
                                    .flex_col()
                                    .gap_3()
                                    .p_4()
                                    .bg(look.surface)
                                    .rounded_lg()
                                    .border_1()
                                    .border_color(look.border)
                                    .child(request)
                                    .child(tabs)
                                    .child(view),
                            )
                            .child(result),
                    ),
            )
            .child(status_bar)
            .children(palette)
    }
}

/// The phase as a colour and a word, for chips and the status bar.
fn phase_style(progress: &Progress, look: &Look) -> (Hsla, &'static str) {
    match progress.phase {
        job::Phase::Idle => (look.muted, "Ready"),
        job::Phase::Running => (look.accent, "Copying"),
        job::Phase::Stopping => (look.warning, "Stopping"),
        job::Phase::Finished if progress.failed > 0 => (look.warning, "Done with errors"),
        job::Phase::Finished => (look.success, "Done"),
        job::Phase::Stopped => (look.warning, "Stopped"),
        job::Phase::Failed(_) => (look.danger, "Failed"),
    }
}

/// `3 downloading · 1 waiting · 4 workers`
fn workers_line(progress: &Progress) -> String {
    let waiting = progress
        .workers
        .iter()
        .filter(|w| w.url.is_some() && w.waiting)
        .count();
    let mut parts = vec![format!("{} downloading", progress.active())];
    if waiting > 0 {
        parts.push(format!("{waiting} waiting their turn"));
    }
    parts.push(format!("{} workers", progress.workers.len()));
    parts.join(" · ")
}

/// `0:42` or `1:02:03`.
fn clock(elapsed: Duration) -> String {
    let secs = elapsed.as_secs();
    if secs >= 3600 {
        format!("{}:{:02}:{:02}", secs / 3600, secs % 3600 / 60, secs % 60)
    } else {
        format!("{}:{:02}", secs / 60, secs % 60)
    }
}

fn dot(color: Hsla) -> impl IntoElement {
    div().size(px(8.)).flex_none().rounded_full().bg(color)
}

fn chip(text: impl Into<SharedString>, color: Hsla, background: Hsla) -> impl IntoElement {
    div()
        .px(px(6.))
        .py(px(1.))
        .rounded(px(4.))
        .bg(background)
        .text_color(color)
        .text_xs()
        .font_weight(FontWeight::SEMIBOLD)
        .child(text.into())
}

fn kbd(text: &'static str, look: &Look) -> impl IntoElement {
    div()
        .px(px(5.))
        .rounded(px(4.))
        .border_1()
        .border_color(look.border)
        .bg(look.surface)
        .text_xs()
        .text_color(look.muted)
        .child(text)
}

fn count_badge(count: usize, look: &Look) -> impl IntoElement {
    div()
        .ml_1()
        .px(px(5.))
        .rounded_full()
        .bg(look.accent.opacity(0.18))
        .text_color(look.accent)
        .text_xs()
        .child(count.to_string())
}

fn underline_tab(
    id: &'static str,
    label: impl Into<SharedString>,
    active: bool,
    badge: usize,
    look: &Look,
    on_click: impl Fn(&gpui_kit::ClickEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    let label = label.into();
    div()
        .id(id)
        .on_click(on_click)
        .test_support()
        .aria_label(label.clone())
        .pb_2()
        .flex()
        .items_center()
        .cursor_pointer()
        .text_sm()
        .border_b_2()
        .border_color(if active {
            look.accent
        } else {
            gpui_kit::transparent_black()
        })
        .text_color(if active { look.text } else { look.muted })
        .when(active, |el| el.font_weight(FontWeight::MEDIUM))
        .hover(|el| el.text_color(look.text))
        .child(label)
        .when(badge > 0, |el| el.child(count_badge(badge, look)))
        .into_any_element()
}

fn labelled(
    label: &'static str,
    hint: Option<&'static str>,
    control: impl IntoElement,
    look: &Look,
) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .flex()
                .gap_2()
                .items_baseline()
                .child(
                    div()
                        .text_xs()
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(look.muted)
                        .child(label),
                )
                .when_some(hint, |el, hint| {
                    el.child(
                        div()
                            .text_xs()
                            .text_color(look.muted.opacity(0.7))
                            .child(hint),
                    )
                }),
        )
        .child(control)
}

fn metric(label: &'static str, value: String, look: &Look) -> impl IntoElement {
    div()
        .flex()
        .gap_1()
        .child(div().text_color(look.muted).child(label))
        .child(div().font_weight(FontWeight::SEMIBOLD).child(value))
}

fn stat(
    id: &'static str,
    label: &'static str,
    value: String,
    color: Hsla,
    look: &Look,
) -> AnyElement {
    div()
        .id(id)
        .test_support()
        .aria_label(SharedString::from(format!("{label}: {value}")))
        .flex_1()
        .min_w(px(80.))
        .px_3()
        .py_2()
        .flex()
        .flex_col()
        .rounded_md()
        .bg(look.chrome.opacity(0.6))
        .border_1()
        .border_color(look.border)
        .child(
            div()
                .text_lg()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(color)
                .child(value),
        )
        .child(div().text_xs().text_color(look.muted).child(label))
        .into_any_element()
}

fn empty_state(look: &Look) -> impl IntoElement {
    div()
        .flex_1()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap_2()
        .text_color(look.muted)
        .child(
            div()
                .size(px(48.))
                .rounded_full()
                .bg(look.raised)
                .flex()
                .items_center()
                .justify_center()
                .child(Icon::new(IconName::Globe).large().text_color(look.muted)),
        )
        .child(
            div()
                .text_sm()
                .text_color(look.text)
                .child("Enter a site address and press Copy"),
        )
        .child(
            div()
                .text_xs()
                .child("Pages, styles, images and fonts download in parallel; links are rewritten to work offline."),
        )
}
