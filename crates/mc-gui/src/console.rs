//! The log panel's model: every crawl event and app action as a timestamped
//! line with a level and a source, kept in memory for the panel and written
//! to a daily file so a problem can be looked at after the app is closed.

use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{DateTime, Local};
use mc_core::CrawlEvent;

/// Lines kept in memory; older ones remain in the file.
const KEEP: usize = 5000;
/// Daily files older than this are removed at startup.
const KEEP_DAYS: u64 = 14;

pub use mc_core::LogLevel as Level;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub time: DateTime<Local>,
    pub level: Level,
    /// `copy`, `script`, `proxy`, `login`, `app`…
    pub source: &'static str,
    pub message: String,
}

impl Entry {
    /// `12:03:44.512` for the panel.
    pub fn clock(&self) -> String {
        self.time.format("%H:%M:%S%.3f").to_string()
    }

    /// One line of the log file and of "copy all".
    pub fn line(&self) -> String {
        format!(
            "{} {:<5} {:<7} {}",
            self.time.format("%Y-%m-%d %H:%M:%S%.3f"),
            self.level.as_str(),
            self.source,
            self.message
        )
    }
}

/// What the panel shows: entries at or above a level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Filter {
    #[default]
    All,
    Info,
    Warnings,
    Errors,
}

impl Filter {
    pub const ALL: [Self; 4] = [Self::All, Self::Info, Self::Warnings, Self::Errors];

    pub fn label(self) -> &'static str {
        match self {
            Self::All => "All",
            Self::Info => "Info",
            Self::Warnings => "Warnings",
            Self::Errors => "Errors",
        }
    }

    fn admits(self, level: Level) -> bool {
        level
            >= match self {
                Self::All => Level::Debug,
                Self::Info => Level::Info,
                Self::Warnings => Level::Warn,
                Self::Errors => Level::Error,
            }
    }
}

#[derive(Default)]
pub struct Console {
    entries: VecDeque<Entry>,
    dir: Option<PathBuf>,
    file: Option<(String, BufWriter<File>)>,
    /// Counts since the last clear, for the status bar.
    pub warnings: usize,
    pub errors: usize,
}

impl Console {
    /// A console writing daily files into `dir` (none: memory only).
    pub fn new(dir: Option<PathBuf>) -> Self {
        if let Some(dir) = &dir {
            prune(dir);
        }
        Self {
            entries: VecDeque::new(),
            dir,
            file: None,
            warnings: 0,
            errors: 0,
        }
    }

    /// `$XDG_STATE_HOME/amel/logs`, else `~/.local/state/amel/logs`.
    pub fn default_dir() -> Option<PathBuf> {
        let state = std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state"))
            })?;
        Some(state.join("amel").join("logs"))
    }

    pub fn dir(&self) -> Option<&Path> {
        self.dir.as_deref()
    }

    pub fn push(&mut self, level: Level, source: &'static str, message: impl Into<String>) {
        let entry = Entry {
            time: Local::now(),
            level,
            source,
            message: message.into(),
        };
        match level {
            Level::Warn => self.warnings += 1,
            Level::Error => self.errors += 1,
            _ => {}
        }
        self.write(&entry);
        if self.entries.len() == KEEP {
            self.entries.pop_front();
        }
        self.entries.push_back(entry);
    }

    /// Records a crawl event (progress counts are left out).
    pub fn event(&mut self, event: &CrawlEvent) {
        if let Some((level, source, message)) = event.describe() {
            self.push(level, source, message);
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Entries the filter and search admit, oldest first.
    pub fn matching<'a>(
        &'a self,
        filter: Filter,
        query: &str,
    ) -> impl DoubleEndedIterator<Item = &'a Entry> + 'a {
        let query = query.trim().to_lowercase();
        self.entries.iter().filter(move |entry| {
            filter.admits(entry.level)
                && (query.is_empty()
                    || entry.message.to_lowercase().contains(&query)
                    || entry.source.contains(query.as_str()))
        })
    }

    /// The matching lines as text, for "copy all".
    pub fn text(&self, filter: Filter, query: &str) -> String {
        self.matching(filter, query)
            .map(Entry::line)
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Empties the panel (the file keeps everything).
    pub fn clear(&mut self) {
        self.entries.clear();
        self.warnings = 0;
        self.errors = 0;
    }

    /// Pushes buffered lines to the file; called on the UI's tick.
    pub fn flush(&mut self) {
        if let Some((_, writer)) = &mut self.file {
            let _ = writer.flush();
        }
    }

    fn write(&mut self, entry: &Entry) {
        let Some(dir) = self.dir.clone() else {
            return;
        };
        let day = entry.time.format("%Y-%m-%d").to_string();
        if self.file.as_ref().is_none_or(|(open, _)| *open != day) {
            self.flush();
            let path = dir.join(format!("amel-{day}.log"));
            let opened = std::fs::create_dir_all(&dir)
                .and_then(|()| OpenOptions::new().create(true).append(true).open(&path));
            self.file = opened.ok().map(|file| (day, BufWriter::new(file)));
        }
        if let Some((_, writer)) = &mut self.file {
            let _ = writeln!(writer, "{}", entry.line());
        }
    }
}

impl Drop for Console {
    fn drop(&mut self) {
        self.flush();
    }
}

/// Removes daily files older than [`KEEP_DAYS`].
fn prune(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let limit = Duration::from_secs(KEEP_DAYS * 24 * 3600);
    for entry in entries.flatten() {
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|age| age > limit);
        let ours = entry.file_name().to_string_lossy().starts_with("amel-");
        if old && ours {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mc_requester::Url;

    #[test]
    fn records_filters_and_writes_a_daily_file() {
        let dir = tempfile::tempdir().unwrap();
        let mut console = Console::new(Some(dir.path().to_path_buf()));
        let url = Url::parse("https://s.test/a.css").unwrap();
        console.push(Level::Info, "app", "copy started");
        console.event(&CrawlEvent::Fetching {
            worker: 0,
            url: url.clone(),
        });
        console.event(&CrawlEvent::Failed {
            url: url.clone(),
            reason: "HTTP 404 Not Found".to_owned(),
        });
        console.event(&CrawlEvent::Progress {
            done: 1,
            total: 2,
            active: 0,
        });
        assert_eq!(console.len(), 3, "progress counts are left out");
        assert_eq!((console.warnings, console.errors), (0, 1));

        let errors: Vec<_> = console.matching(Filter::Errors, "").collect();
        assert_eq!(errors.len(), 1);
        assert!(errors[0].message.contains("HTTP 404"));
        assert_eq!(
            console.matching(Filter::Info, "").count(),
            2,
            "debug left out"
        );
        assert_eq!(console.matching(Filter::All, "worker 1").count(), 1);
        assert_eq!(console.matching(Filter::All, "APP").count(), 1, "by source");

        let text = console.text(Filter::All, "");
        assert_eq!(text.lines().count(), 3);
        assert!(
            text.lines()
                .nth(2)
                .unwrap()
                .contains("ERROR copy    failed")
        );

        console.flush();
        let file = std::fs::read_dir(dir.path())
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let name = file.file_name().unwrap().to_string_lossy().into_owned();
        assert!(
            name.starts_with("amel-") && name.ends_with(".log"),
            "{name}"
        );
        assert_eq!(std::fs::read_to_string(&file).unwrap(), format!("{text}\n"));

        console.clear();
        assert!(console.is_empty());
        assert_eq!(console.errors, 0);
        assert_eq!(
            std::fs::read_to_string(&file).unwrap().lines().count(),
            3,
            "the file keeps what the panel cleared"
        );
    }
}
