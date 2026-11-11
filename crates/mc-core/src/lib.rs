//! The MisterCopy crawler.
//!
//! [`Crawler`] fetches through any [`Requester`](mc_requester::Requester),
//! saves each response under a path from [`map_url`], and once the crawl ends
//! rewrites every saved HTML and CSS file so its links point at the local
//! copies.

mod crawler;
mod normalize;
mod path;
mod rules;

pub use crawler::{CrawlConfig, CrawlError, CrawlEvent, CrawlSummary, Crawler};
pub use normalize::normalize;
pub use path::{LocalPath, PathAllocator, map_url};
pub use rules::{Decision, Policy, Rule, RuleAction, Scope, SkipReason};
