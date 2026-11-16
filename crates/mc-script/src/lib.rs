//! JavaScript hooks for copies, run in QuickJS: a script defines any of
//!
//! ```js
//! function onStart(info)          { log("copying " + info.url) }
//! function shouldFetch(url, kind) { return !url.includes("/tag/") }
//! function onFile(file)           { if (file.bytes > 1e7) log("big: " + file.url) }
//! function onError(url, reason)   { log(url + " failed: " + reason) }
//! function onFinish(summary)      { log(summary.saved + " files") }
//! ```
//!
//! `log(text)` shows a line in the copy's output. Each call may run for
//! [`CALL_LIMIT`]; a script that throws or runs too long is reported in the
//! log and the copy carries on as if the hook weren't there.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use mc_core::{CrawlSummary, Hooks, LinkKind, SavedInfo};
use rquickjs::{CatchResultExt, Context, Ctx, Function, IntoJs, Object, Runtime, Value};
use url::Url;

/// How long one hook call may run.
pub const CALL_LIMIT: Duration = Duration::from_secs(2);
/// Memory a script may use.
const MEMORY_LIMIT: usize = 64 * 1024 * 1024;

/// A starting point shown in the app's script editor.
pub const TEMPLATE: &str = r#"// Runs with the copy. Delete what you don't need.

function onStart(info) {
  log("Copying " + info.url + " into " + info.output);
}

// Return false to skip a URL. kind is "page" or "asset".
function shouldFetch(url, kind) {
  return true;
}

function onFile(file) {
  // file.url, file.path, file.bytes, file.mime, file.type
  if (file.bytes > 10 * 1024 * 1024) log("Large file: " + file.url);
}

function onError(url, reason) {
  log("Failed: " + url + " (" + reason + ")");
}

function onFinish(summary) {
  log("Done: " + summary.saved + " files, " + summary.failed + " failed");
}
"#;

pub struct ScriptHooks {
    runtime: Runtime,
    context: Context,
    logs: Arc<Mutex<Vec<String>>>,
    deadline: Arc<Mutex<Option<Instant>>>,
    /// Set once a call ran too long; hooks then do nothing.
    broken: AtomicBool,
}

impl ScriptHooks {
    pub fn from_file(path: &Path) -> Result<Self, String> {
        let source = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read script {}: {e}", path.display()))?;
        Self::new(&source).map_err(|e| format!("script {}: {e}", path.display()))
    }

    pub fn new(source: &str) -> Result<Self, String> {
        let runtime = Runtime::new().map_err(|e| e.to_string())?;
        runtime.set_memory_limit(MEMORY_LIMIT);
        let deadline: Arc<Mutex<Option<Instant>>> = Arc::new(Mutex::new(None));
        let watch = Arc::clone(&deadline);
        runtime.set_interrupt_handler(Some(Box::new(move || {
            watch
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .is_some_and(|until| Instant::now() > until)
        })));
        let context = Context::full(&runtime).map_err(|e| e.to_string())?;
        let hooks = Self {
            runtime,
            context,
            logs: Arc::default(),
            deadline,
            broken: AtomicBool::new(false),
        };
        let sink = Arc::clone(&hooks.logs);
        hooks.timed(|| {
            hooks.context.with(|ctx| {
                let log = Function::new(ctx.clone(), move |text: rquickjs::Coerced<String>| {
                    sink.lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .push(text.0);
                })
                .catch(&ctx)
                .map_err(|e| e.to_string())?;
                ctx.globals()
                    .set("log", log)
                    .catch(&ctx)
                    .map_err(|e| e.to_string())?;
                ctx.eval::<(), _>(source)
                    .catch(&ctx)
                    .map_err(|e| e.to_string())
            })
        })?;
        Ok(hooks)
    }

    /// Runs `work` with the call deadline set.
    fn timed<R>(&self, work: impl FnOnce() -> R) -> R {
        *self.deadline.lock().unwrap_or_else(PoisonError::into_inner) =
            Some(Instant::now() + CALL_LIMIT);
        let result = work();
        *self.deadline.lock().unwrap_or_else(PoisonError::into_inner) = None;
        result
    }

    fn log(&self, text: String) {
        self.logs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(text);
    }

    /// Calls the global function `name`, if the script defines it, with the
    /// arguments `build` makes; returns its answer as a boolean (or `None`).
    fn call(
        &self,
        name: &str,
        build: impl for<'js> FnOnce(&Ctx<'js>) -> rquickjs::Result<Vec<Value<'js>>>,
    ) -> Option<bool> {
        if self.broken.load(Ordering::Relaxed) {
            return None;
        }
        let outcome = self.timed(|| {
            self.context.with(|ctx| -> Result<Option<bool>, String> {
                let Ok(function) = ctx.globals().get::<_, Function>(name) else {
                    return Ok(None);
                };
                let args = build(&ctx).catch(&ctx).map_err(|e| e.to_string())?;
                let mut call = rquickjs::function::Args::new(ctx.clone(), args.len());
                for arg in args {
                    call.push_arg(arg).catch(&ctx).map_err(|e| e.to_string())?;
                }
                let answer: Value = function
                    .call_arg(call)
                    .catch(&ctx)
                    .map_err(|e| e.to_string())?;
                Ok(answer.as_bool())
            })
        });
        self.runtime.run_gc();
        match outcome {
            Ok(answer) => answer,
            Err(error) => {
                let timed_out = error.contains("interrupted");
                self.log(if timed_out {
                    format!(
                        "script: {name} ran longer than {}s; the script is turned off",
                        CALL_LIMIT.as_secs()
                    )
                } else {
                    format!("script error in {name}: {error}")
                });
                if timed_out {
                    self.broken.store(true, Ordering::Relaxed);
                }
                None
            }
        }
    }
}

fn object<'js>(ctx: &Ctx<'js>, fields: &[(&str, Value<'js>)]) -> rquickjs::Result<Value<'js>> {
    let object = Object::new(ctx.clone())?;
    for (name, value) in fields {
        object.set(*name, value.clone())?;
    }
    Ok(object.into_value())
}

fn js<'js, T: IntoJs<'js>>(ctx: &Ctx<'js>, value: T) -> rquickjs::Result<Value<'js>> {
    value.into_js(ctx)
}

impl Hooks for ScriptHooks {
    fn on_start(&self, seeds: &[Url], output: &Path) {
        let url = seeds.first().map(Url::to_string).unwrap_or_default();
        let output = output.to_string_lossy().into_owned();
        self.call("onStart", |ctx| {
            Ok(vec![object(
                ctx,
                &[("url", js(ctx, url)?), ("output", js(ctx, output)?)],
            )?])
        });
    }

    fn should_fetch(&self, url: &Url, kind: LinkKind) -> bool {
        let url = url.to_string();
        let kind = match kind {
            LinkKind::Page => "page",
            LinkKind::Asset => "asset",
        };
        self.call("shouldFetch", |ctx| Ok(vec![js(ctx, url)?, js(ctx, kind)?])) != Some(false)
    }

    fn on_file(&self, file: &SavedInfo) {
        let url = file.url.to_string();
        let path = file.path.to_string_lossy().into_owned();
        let mime = file.mime.clone().unwrap_or_default();
        let kind = file.file_type.as_str();
        let bytes = file.bytes as f64;
        self.call("onFile", |ctx| {
            Ok(vec![object(
                ctx,
                &[
                    ("url", js(ctx, url)?),
                    ("path", js(ctx, path)?),
                    ("bytes", js(ctx, bytes)?),
                    ("mime", js(ctx, mime)?),
                    ("type", js(ctx, kind)?),
                ],
            )?])
        });
    }

    fn on_error(&self, url: &Url, reason: &str) {
        let url = url.to_string();
        let reason = reason.to_owned();
        self.call("onError", |ctx| Ok(vec![js(ctx, url)?, js(ctx, reason)?]));
    }

    fn on_finish(&self, summary: &CrawlSummary) {
        let saved = summary.saved as f64;
        let failed = summary.failed.len() as f64;
        let skipped = summary.skipped as f64;
        let bytes = summary.bytes as f64;
        let seconds = summary.elapsed.as_secs_f64();
        let stopped = summary.stopped.is_some();
        self.call("onFinish", |ctx| {
            Ok(vec![object(
                ctx,
                &[
                    ("saved", js(ctx, saved)?),
                    ("failed", js(ctx, failed)?),
                    ("skipped", js(ctx, skipped)?),
                    ("bytes", js(ctx, bytes)?),
                    ("seconds", js(ctx, seconds)?),
                    ("stopped", js(ctx, stopped)?),
                ],
            )?])
        });
    }

    fn take_logs(&self) -> Vec<String> {
        std::mem::take(&mut *self.logs.lock().unwrap_or_else(PoisonError::into_inner))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[test]
    fn calls_the_hooks_the_script_defines() {
        let hooks = ScriptHooks::new(
            r#"
            let files = 0;
            function onStart(info) { log("start " + info.url + " -> " + info.output) }
            function shouldFetch(url, kind) { return !url.includes("/skip/") }
            function onFile(f) { files += 1; log(f.type + " " + f.bytes + " " + f.url) }
            function onFinish(s) { log("done " + s.saved + " files, seen " + files) }
            "#,
        )
        .unwrap();
        hooks.on_start(&[url("https://s.test/")], Path::new("/out"));
        assert!(hooks.should_fetch(&url("https://s.test/a"), LinkKind::Page));
        assert!(!hooks.should_fetch(&url("https://s.test/skip/b"), LinkKind::Asset));
        hooks.on_file(&SavedInfo {
            url: url("https://s.test/a.png"),
            path: "/out/a.png".into(),
            bytes: 2048,
            mime: Some("image/png".to_owned()),
            file_type: mc_core::FileType::Image,
        });
        // onError isn't defined: nothing happens.
        hooks.on_error(&url("https://s.test/x"), "HTTP 404");
        hooks.on_finish(&CrawlSummary {
            saved: 7,
            ..CrawlSummary::default()
        });
        assert_eq!(
            hooks.take_logs(),
            [
                "start https://s.test/ -> /out",
                "image 2048 https://s.test/a.png",
                "done 7 files, seen 1"
            ]
        );
        assert!(hooks.take_logs().is_empty(), "logs are taken once");
    }

    #[test]
    fn reports_errors_and_stops_runaway_scripts() {
        assert!(ScriptHooks::new("function (").is_err());

        let hooks = ScriptHooks::new(
            r#"
            function onFile(f) { throw new Error("boom") }
            function shouldFetch(url) { while (true) {} }
            "#,
        )
        .unwrap();
        hooks.on_file(&SavedInfo {
            url: url("https://s.test/a"),
            path: "/a".into(),
            bytes: 1,
            mime: None,
            file_type: mc_core::FileType::Other,
        });
        let started = Instant::now();
        // Runs too long: fetched anyway, and the script is turned off.
        assert!(hooks.should_fetch(&url("https://s.test/b"), LinkKind::Page));
        assert!(started.elapsed() < CALL_LIMIT * 3);
        assert!(hooks.should_fetch(&url("https://s.test/c"), LinkKind::Page));
        let logs = hooks.take_logs();
        assert!(
            logs[0].contains("onFile") && logs[0].contains("boom"),
            "{logs:?}"
        );
        assert!(logs[1].contains("turned off"), "{logs:?}");
        assert_eq!(logs.len(), 2, "{logs:?}");
    }

    #[test]
    fn the_template_loads() {
        let hooks = ScriptHooks::new(TEMPLATE).unwrap();
        hooks.on_start(&[url("https://s.test/")], Path::new("/out"));
        assert_eq!(hooks.take_logs(), ["Copying https://s.test/ into /out"]);
    }
}
