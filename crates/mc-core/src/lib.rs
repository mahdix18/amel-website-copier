//! The MisterCopy crawler.
//!
//! [`Crawler`] fetches through any [`Requester`](mc_requester::Requester),
//! saves each response under a path from [`map_url`], and once the crawl ends
//! rewrites every saved HTML and CSS file so its links point at the local
//! copies. With a [`StateStore`] it can stop and resume, or update an earlier
//! copy with conditional requests.

mod crawler;
mod normalize;
mod path;
mod report;
mod retry;
mod robots;
mod rules;
mod store;

pub use crawler::{
    CrawlConfig, CrawlError, CrawlEvent, CrawlSummary, Crawler, Credential, REPORT_HTML,
    REPORT_JSON, STATE_DIR, StopHandle, StopReason,
};
pub use mc_extract::LinkKind;
pub use normalize::normalize;
pub use path::{LocalPath, PathAllocator, map_url};
pub use report::{Report, human_bytes};
pub use rules::{Decision, Policy, Rule, RuleAction, Scope, SkipReason};
pub use store::{
    CrawlStatus, Document, EntryState, MemoryStore, SavedFile, StateStore, StoreError, StoredCrawl,
    UrlEntry,
};
