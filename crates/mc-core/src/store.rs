//! What a crawl remembers, so it can resume after an interruption and update
//! an earlier copy with conditional requests.

use std::sync::{Arc, Mutex, PoisonError};

use mc_extract::LinkKind;
use mc_requester::Validators;
use url::Url;

use crate::path::LocalPath;
use crate::rules::SkipReason;

/// A saved file the crawler rewrites links in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Document {
    Html { charset: Option<String> },
    Css,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedFile {
    pub path: LocalPath,
    pub mime: Option<String>,
    pub document: Option<Document>,
    pub bytes: u64,
    /// From the response, for conditional requests on update.
    pub validators: Validators,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntryState {
    /// Waiting to be fetched (or in flight when the crawl stopped).
    Queued,
    Saved(SavedFile),
    /// Redirected to another URL with its own entry.
    Alias(Url),
    /// Not copied; links point at this absolute URL.
    Remote(Url),
    Failed(String),
    Skipped(SkipReason),
}

/// Everything known about one normalized URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UrlEntry {
    pub url: Url,
    pub depth: u32,
    pub kind: LinkKind,
    pub referrer: Option<Url>,
    pub follow_links: bool,
    pub state: EntryState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrawlStatus {
    Running,
    /// Stopped early: by request, or a time or size limit.
    Stopped,
    Complete,
}

impl CrawlStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Stopped => "stopped",
            Self::Complete => "complete",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        [Self::Running, Self::Stopped, Self::Complete]
            .into_iter()
            .find(|status| status.as_str() == name)
    }
}

/// What a store hands back when a crawl starts.
#[derive(Debug, Clone, Default)]
pub struct StoredCrawl {
    /// The crawl in progress, in the order URLs were first seen.
    pub entries: Vec<UrlEntry>,
    /// Saved files from the copy being updated, used for conditional requests.
    pub previous: Vec<UrlEntry>,
}

#[derive(Debug, thiserror::Error)]
#[error("crawl state: {0}")]
pub struct StoreError(pub Box<dyn std::error::Error + Send + Sync>);

impl StoreError {
    pub fn new(error: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> Self {
        Self(error.into())
    }
}

/// Persistence for a crawl. Every change to an entry is written through, so
/// a crash loses at most the requests that were in flight.
pub trait StateStore: Send {
    fn load(&mut self) -> Result<StoredCrawl, StoreError>;
    /// Inserts or replaces the entry for `entry.url`, keeping its first-seen order.
    fn put(&mut self, entry: &UrlEntry) -> Result<(), StoreError>;
    fn set_status(&mut self, status: CrawlStatus) -> Result<(), StoreError>;
}

/// An in-memory store. Clones share state, so a test can hand one to a
/// crawler and inspect or resume from it afterwards.
#[derive(Debug, Clone, Default)]
pub struct MemoryStore {
    inner: Arc<Mutex<MemoryInner>>,
}

#[derive(Debug, Default)]
struct MemoryInner {
    stored: StoredCrawl,
    status: Option<CrawlStatus>,
}

impl MemoryStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn entries(&self) -> Vec<UrlEntry> {
        self.lock().stored.entries.clone()
    }

    pub fn status(&self) -> Option<CrawlStatus> {
        self.lock().status
    }

    /// Moves saved entries to `previous` and clears the rest, as before an update.
    pub fn begin_update(&self) {
        let mut inner = self.lock();
        let entries = std::mem::take(&mut inner.stored.entries);
        inner.stored.previous = entries
            .into_iter()
            .filter(|entry| matches!(entry.state, EntryState::Saved(_)))
            .collect();
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, MemoryInner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl StateStore for MemoryStore {
    fn load(&mut self) -> Result<StoredCrawl, StoreError> {
        Ok(self.lock().stored.clone())
    }

    fn put(&mut self, entry: &UrlEntry) -> Result<(), StoreError> {
        let mut inner = self.lock();
        let entries = &mut inner.stored.entries;
        match entries
            .iter_mut()
            .find(|existing| existing.url == entry.url)
        {
            Some(existing) => *existing = entry.clone(),
            None => entries.push(entry.clone()),
        }
        Ok(())
    }

    fn set_status(&mut self, status: CrawlStatus) -> Result<(), StoreError> {
        self.lock().status = Some(status);
        Ok(())
    }
}
