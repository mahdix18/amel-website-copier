use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use mc_extract::{Link, LinkKind, extract_css, extract_html, rewrite_css, rewrite_html};
use mc_requester::{
    Body, FetchContext, FetchError, FetchRequest, FetchResponse, Requester, header,
};
use tokio::sync::broadcast;
use tokio::task::{self, JoinSet};
use tokio::time::Instant as Deadline;
use url::Url;

use crate::normalize::normalize;
use crate::path::{LocalPath, PathAllocator};
use crate::rules::{Decision, Policy, Rule, Scope, SkipReason};

/// Redirect hops followed between records before giving up on a cycle.
const MAX_ALIAS_HOPS: usize = 16;

#[derive(Debug, Clone)]
pub struct CrawlConfig {
    pub seeds: Vec<Url>,
    pub output_dir: PathBuf,
    /// Extra hosts treated as part of the site, besides the seeds' hosts.
    pub domain_aliases: Vec<String>,
    pub rules: Vec<Rule>,
    pub include_external_assets: bool,
    pub max_depth: Option<u32>,
    /// Stop fetching after this many URLs.
    pub max_urls: Option<usize>,
    pub concurrency: usize,
    /// Minimum gap between two requests to the same host.
    pub per_host_delay: Duration,
}

impl CrawlConfig {
    /// Polite defaults: 4 requests in flight, one per second per host.
    pub fn new(seed: Url, output_dir: impl Into<PathBuf>) -> Self {
        Self {
            seeds: vec![seed],
            output_dir: output_dir.into(),
            domain_aliases: Vec::new(),
            rules: Vec::new(),
            include_external_assets: false,
            max_depth: None,
            max_urls: None,
            concurrency: 4,
            per_host_delay: Duration::from_secs(1),
        }
    }
}

#[derive(Debug, Clone)]
pub enum CrawlEvent {
    Saved {
        url: Url,
        path: PathBuf,
        bytes: u64,
    },
    Failed {
        url: Url,
        reason: String,
    },
    Skipped {
        url: Url,
        reason: SkipReason,
    },
    /// The crawl finished; links in this many documents are being rewritten.
    Rewriting {
        documents: usize,
    },
}

#[derive(Debug, Clone, Default)]
pub struct CrawlSummary {
    pub saved: usize,
    pub bytes: u64,
    pub failed: Vec<(Url, String)>,
    pub skipped: usize,
    pub elapsed: Duration,
}

#[derive(Debug, thiserror::Error)]
pub enum CrawlError {
    #[error("no start URL given")]
    NoSeeds,
    #[error("start URL `{0}` must be an http or https URL with a host")]
    InvalidSeed(Url),
    #[error("concurrency must be at least 1")]
    ZeroConcurrency,
    #[error("cannot use output directory {}: {source}", path.display())]
    Output {
        path: PathBuf,
        source: std::io::Error,
    },
}

pub struct Crawler {
    config: CrawlConfig,
    requester: Arc<dyn Requester>,
    events: broadcast::Sender<CrawlEvent>,
}

impl Crawler {
    pub fn new(config: CrawlConfig, requester: Arc<dyn Requester>) -> Self {
        let (events, _) = broadcast::channel(1024);
        Self {
            config,
            requester,
            events,
        }
    }

    /// Progress events. A receiver that falls too far behind skips ahead.
    pub fn subscribe(&self) -> broadcast::Receiver<CrawlEvent> {
        self.events.subscribe()
    }

    pub async fn run(self) -> Result<CrawlSummary, CrawlError> {
        let started = Instant::now();
        let seeds = self.validate().await?;

        let scope = Scope::new(
            seeds
                .iter()
                .filter_map(|seed| seed.host_str().map(str::to_owned))
                .chain(self.config.domain_aliases.iter().cloned()),
        );
        let policy = Policy {
            scope,
            rules: self.config.rules.clone(),
            include_external_assets: self.config.include_external_assets,
            max_depth: self.config.max_depth,
        };

        let mut state = State::default();
        for url in seeds {
            if state.seen.insert(url.clone()) {
                state.queue.push_back(Job {
                    url,
                    depth: 0,
                    kind: LinkKind::Page,
                    referrer: None,
                    follow_links: true,
                });
            }
        }

        let mut tasks: JoinSet<(Job, Result<FetchResponse, FetchError>)> = JoinSet::new();
        let mut in_flight: HashMap<task::Id, Url> = HashMap::new();
        loop {
            while tasks.len() < self.config.concurrency {
                let Some(job) = state.queue.pop_front() else {
                    break;
                };
                if self.config.max_urls.is_some_and(|max| state.fetched >= max) {
                    self.skip(&mut state, job.url, SkipReason::UrlLimit);
                    continue;
                }
                state.fetched += 1;
                let slot = state.reserve_slot(&job.url, self.config.per_host_delay);
                let requester = Arc::clone(&self.requester);
                let url = job.url.clone();
                let handle = tasks.spawn(async move {
                    tokio::time::sleep_until(slot).await;
                    let context = FetchContext {
                        referrer: job.referrer.clone(),
                        depth: job.depth,
                        attempt: 0,
                    };
                    let result = requester
                        .fetch(FetchRequest::get(job.url.clone()), &context)
                        .await;
                    (job, result)
                });
                in_flight.insert(handle.id(), url);
            }

            let Some(joined) = tasks.join_next_with_id().await else {
                break;
            };
            match joined {
                Ok((id, (job, result))) => {
                    in_flight.remove(&id);
                    self.handle(&mut state, &policy, job, result).await;
                }
                Err(error) => {
                    // A requester panicked; the crawl carries on without it.
                    if let Some(url) = in_flight.remove(&error.id()) {
                        self.fail(&mut state, url, format!("requester crashed: {error}"));
                    }
                }
            }
        }

        self.rewrite_documents(&mut state).await;
        state.summary.elapsed = started.elapsed();
        Ok(state.summary)
    }

    async fn validate(&self) -> Result<Vec<Url>, CrawlError> {
        if self.config.concurrency == 0 {
            return Err(CrawlError::ZeroConcurrency);
        }
        if self.config.seeds.is_empty() {
            return Err(CrawlError::NoSeeds);
        }
        let seeds = self
            .config
            .seeds
            .iter()
            .map(|seed| normalize(seed).ok_or_else(|| CrawlError::InvalidSeed(seed.clone())))
            .collect::<Result<Vec<_>, _>>()?;
        tokio::fs::create_dir_all(&self.config.output_dir)
            .await
            .map_err(|source| CrawlError::Output {
                path: self.config.output_dir.clone(),
                source,
            })?;
        Ok(seeds)
    }

    async fn handle(
        &self,
        state: &mut State,
        policy: &Policy,
        job: Job,
        result: Result<FetchResponse, FetchError>,
    ) {
        let response = match result {
            Ok(response) => response,
            Err(error) => return self.fail(state, job.url, error.to_string()),
        };
        if !response.status.is_success() {
            return self.fail(state, job.url, format!("HTTP {}", response.status));
        }
        let Some(final_url) = normalize(&response.final_url) else {
            let reason = format!("redirected to unsupported URL {}", response.final_url);
            return self.fail(state, job.url, reason);
        };

        if final_url != job.url {
            if state.records.contains_key(&final_url) {
                state.records.insert(job.url, Record::Alias(final_url));
                return;
            }
            if let Decision::Skip(reason) = policy.decide(&final_url, job.kind, job.depth) {
                state
                    .records
                    .insert(job.url, Record::Remote(final_url.clone()));
                return self.skip(state, final_url, reason);
            }
            state.seen.insert(final_url.clone());
            state
                .records
                .insert(job.url.clone(), Record::Alias(final_url.clone()));
        }

        let body = match read_body(&response.body).await {
            Ok(body) => body,
            Err(error) => {
                return self.fail(state, final_url, format!("could not read body: {error}"));
            }
        };
        let mime = response.mime_type().or_else(|| {
            mime_guess::from_path(final_url.path())
                .first_raw()
                .map(str::to_owned)
        });
        let path = state.allocator.allocate(&final_url, mime.as_deref());
        let file = self.config.output_dir.join(path.to_path_buf());
        if let Err(error) = write_atomic(&file, &body).await {
            let reason = format!("could not write {}: {error}", file.display());
            return self.fail(state, final_url, reason);
        }

        let document = match mime.as_deref() {
            Some("text/html" | "application/xhtml+xml") => Some(Document::Html {
                charset: charset(&response),
            }),
            Some("text/css") => Some(Document::Css),
            _ => None,
        };
        let links: Vec<Link> = match &document {
            Some(Document::Html { charset }) => {
                extract_html(&body, charset.as_deref(), &final_url).unwrap_or_default()
            }
            Some(Document::Css) => extract_css(&String::from_utf8_lossy(&body), &final_url),
            None => Vec::new(),
        };
        let runtime_links = response.discovered_links.iter().map(|url| Link {
            url: url.clone(),
            kind: LinkKind::Page,
        });
        for link in links.into_iter().chain(runtime_links) {
            self.enqueue(state, policy, link, &job, &final_url);
        }

        let bytes = body.len() as u64;
        state
            .records
            .insert(final_url.clone(), Record::Saved { path, document });
        state.summary.saved += 1;
        state.summary.bytes += bytes;
        self.emit(CrawlEvent::Saved {
            url: final_url,
            path: file,
            bytes,
        });
    }

    fn enqueue(
        &self,
        state: &mut State,
        policy: &Policy,
        link: Link,
        parent: &Job,
        page_url: &Url,
    ) {
        let Some(url) = normalize(&link.url) else {
            return;
        };
        if state.seen.contains(&url) {
            return;
        }
        if link.kind == LinkKind::Page && !parent.follow_links {
            return;
        }
        let depth = match link.kind {
            LinkKind::Page => parent.depth + 1,
            LinkKind::Asset => parent.depth,
        };
        match policy.decide(&url, link.kind, depth) {
            Decision::Fetch { follow_links } => {
                state.seen.insert(url.clone());
                state.queue.push_back(Job {
                    url,
                    depth,
                    kind: link.kind,
                    referrer: Some(page_url.clone()),
                    follow_links,
                });
            }
            // A shallower path may still reach this URL later.
            Decision::Skip(SkipReason::TooDeep) => self.skip(state, url, SkipReason::TooDeep),
            Decision::Skip(reason) => {
                state.seen.insert(url.clone());
                self.skip(state, url, reason);
            }
        }
    }

    /// Points every reference in saved documents at its local copy, and
    /// every reference to something not copied at its absolute URL.
    async fn rewrite_documents(&self, state: &mut State) {
        let documents: Vec<(Url, LocalPath, Document)> = state
            .records
            .iter()
            .filter_map(|(url, record)| match record {
                Record::Saved {
                    path,
                    document: Some(document),
                } => Some((url.clone(), path.clone(), document.clone())),
                _ => None,
            })
            .collect();
        self.emit(CrawlEvent::Rewriting {
            documents: documents.len(),
        });

        for (url, path, document) in documents {
            let file = self.config.output_dir.join(path.to_path_buf());
            let original = match tokio::fs::read(&file).await {
                Ok(bytes) => bytes,
                Err(error) => {
                    self.fail(
                        state,
                        url,
                        format!("could not re-read {}: {error}", file.display()),
                    );
                    continue;
                }
            };
            let map = |target: &Url, _: LinkKind| Some(state.href_for(&path, target));
            let rewritten = match &document {
                Document::Html { charset } => {
                    rewrite_html(&original, charset.as_deref(), &url, map)
                        .map_err(|error| error.to_string())
                }
                Document::Css => {
                    Ok(rewrite_css(&String::from_utf8_lossy(&original), &url, map).into_bytes())
                }
            };
            let result = match rewritten {
                Ok(bytes) => write_atomic(&file, &bytes)
                    .await
                    .map_err(|error| format!("could not write {}: {error}", file.display())),
                Err(error) => Err(error),
            };
            if let Err(reason) = result {
                self.fail(state, url, format!("links not rewritten: {reason}"));
            }
        }
    }

    fn fail(&self, state: &mut State, url: Url, reason: String) {
        state.summary.failed.push((url.clone(), reason.clone()));
        self.emit(CrawlEvent::Failed { url, reason });
    }

    fn skip(&self, state: &mut State, url: Url, reason: SkipReason) {
        if state.skipped.insert(url.clone()) {
            state.summary.skipped += 1;
            self.emit(CrawlEvent::Skipped { url, reason });
        }
    }

    fn emit(&self, event: CrawlEvent) {
        // No subscribers is fine.
        let _ = self.events.send(event);
    }
}

struct Job {
    url: Url,
    depth: u32,
    kind: LinkKind,
    referrer: Option<Url>,
    follow_links: bool,
}

#[derive(Clone)]
enum Document {
    Html { charset: Option<String> },
    Css,
}

enum Record {
    Saved {
        path: LocalPath,
        document: Option<Document>,
    },
    /// Redirected to another URL that has (or will have) its own record.
    Alias(Url),
    /// Not copied; links point at this absolute URL.
    Remote(Url),
}

enum Target<'a> {
    Local(&'a LocalPath),
    Remote(&'a Url),
}

#[derive(Default)]
struct State {
    queue: VecDeque<Job>,
    /// URLs queued or handled, by normalized form.
    seen: HashSet<Url>,
    skipped: HashSet<Url>,
    records: HashMap<Url, Record>,
    allocator: PathAllocator,
    summary: CrawlSummary,
    fetched: usize,
    next_slot: HashMap<String, Deadline>,
}

impl State {
    /// The earliest moment a request to `url`'s host may start, keeping the
    /// per-host gap.
    fn reserve_slot(&mut self, url: &Url, gap: Duration) -> Deadline {
        let host = format!(
            "{}:{}",
            url.host_str().unwrap_or_default(),
            url.port_or_known_default().unwrap_or(0)
        );
        let now = Deadline::now();
        let slot = self
            .next_slot
            .get(&host)
            .copied()
            .filter(|next| *next > now)
            .unwrap_or(now);
        self.next_slot.insert(host, slot + gap);
        slot
    }

    fn target(&self, url: &Url) -> Option<Target<'_>> {
        let mut key = url;
        for _ in 0..MAX_ALIAS_HOPS {
            match self.records.get(key)? {
                Record::Saved { path, .. } => return Some(Target::Local(path)),
                Record::Remote(remote) => return Some(Target::Remote(remote)),
                Record::Alias(next) => key = next,
            }
        }
        None
    }

    fn href_for(&self, from: &LocalPath, target: &Url) -> String {
        let fragment = target
            .fragment()
            .map(|f| format!("#{f}"))
            .unwrap_or_default();
        let Some(key) = normalize(target) else {
            return target.to_string();
        };
        match self.target(&key) {
            Some(Target::Local(path)) => format!("{}{fragment}", from.relative_href(path)),
            Some(Target::Remote(remote)) => format!("{remote}{fragment}"),
            None => target.to_string(),
        }
    }
}

fn charset(response: &FetchResponse) -> Option<String> {
    let value = response.headers.get(header::CONTENT_TYPE)?.to_str().ok()?;
    value.split(';').skip(1).find_map(|param| {
        let (name, value) = param.split_once('=')?;
        name.trim()
            .eq_ignore_ascii_case("charset")
            .then(|| value.trim().trim_matches('"').to_owned())
    })
}

async fn read_body(body: &Body) -> std::io::Result<Vec<u8>> {
    match body {
        Body::Bytes(bytes) => Ok(bytes.to_vec()),
        Body::File(path) => tokio::fs::read(path).await,
    }
}

/// Writes via a temporary sibling and a rename, so an interrupted crawl never
/// leaves a half-written file under the final name.
async fn write_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy())
        .unwrap_or_default();
    let temp = path.with_file_name(format!(".{name}.mc-tmp"));
    tokio::fs::write(&temp, data).await?;
    tokio::fs::rename(&temp, path).await
}
