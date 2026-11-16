//! Crawl state in SQLite, at `<output>/.amel/state.db`.
//!
//! One row per normalized URL, written through as the crawl goes, so an
//! interrupted copy can be resumed and a finished one updated.

use std::path::{Path, PathBuf};

use mc_core::{
    CrawlStatus, Document, EntryState, LinkKind, LocalPath, STATE_DIR, SavedFile, SkipReason,
    StateStore, StoreError, StoredCrawl, UrlEntry,
};
use mc_requester::{Url, Validators};
use rusqlite::{Connection, OptionalExtension, Row, params};

const DB_FILE: &str = "state.db";
const SCHEMA_VERSION: &str = "1";

const COLUMNS: &str = "url TEXT PRIMARY KEY, depth INTEGER NOT NULL, kind TEXT NOT NULL, \
    referrer TEXT, follow INTEGER NOT NULL, state TEXT NOT NULL, detail TEXT, path TEXT, \
    mime TEXT, document TEXT, charset TEXT, bytes INTEGER, etag TEXT, last_modified TEXT";
const SELECT: &str = "SELECT url, depth, kind, referrer, follow, state, detail, path, mime, \
    document, charset, bytes, etag, last_modified FROM";

pub struct SqliteStore {
    conn: Connection,
}

fn store_error(error: impl std::error::Error + Send + Sync + 'static) -> StoreError {
    StoreError::new(error)
}

/// Copies made before the rename keep their state in `.mistercopy`; it
/// moves to the new name the first time the copy is opened, so `resume`
/// and `update` go on working.
fn migrate_legacy(output: &Path) {
    let legacy = output.join(LEGACY_STATE_DIR);
    let current = output.join(STATE_DIR);
    if legacy.is_dir() && !current.exists() {
        let _ = std::fs::rename(legacy, current);
    }
}

/// The state folder's name before the rename.
const LEGACY_STATE_DIR: &str = ".mistercopy";

impl SqliteStore {
    pub fn path_in(output: &Path) -> PathBuf {
        output.join(STATE_DIR).join(DB_FILE)
    }

    /// Whether `output` holds state from an earlier crawl.
    pub fn exists_in(output: &Path) -> bool {
        migrate_legacy(output);
        Self::path_in(output).is_file()
    }

    /// Opens, or creates, the state for the copy in `output`.
    pub fn open(output: &Path) -> Result<Self, StoreError> {
        migrate_legacy(output);
        let path = Self::path_in(output);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(store_error)?;
        }
        let conn = Connection::open(&path).map_err(store_error)?;
        conn.execute_batch(&format!(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS urls ({COLUMNS});
             CREATE TABLE IF NOT EXISTS previous ({COLUMNS});"
        ))
        .map_err(store_error)?;

        let store = Self { conn };
        match store.meta("schema")? {
            None => store.set_meta("schema", SCHEMA_VERSION)?,
            Some(version) if version == SCHEMA_VERSION => {}
            Some(version) => {
                return Err(StoreError::new(format!(
                    "{} was written by an incompatible Amel (schema {version})",
                    path.display()
                )));
            }
        }
        Ok(store)
    }

    pub fn status(&self) -> Result<Option<CrawlStatus>, StoreError> {
        Ok(self.meta("status")?.as_deref().and_then(CrawlStatus::parse))
    }

    pub fn meta(&self, key: &str) -> Result<Option<String>, StoreError> {
        self.conn
            .query_row("SELECT value FROM meta WHERE key = ?1", [key], |row| {
                row.get(0)
            })
            .optional()
            .map_err(store_error)
    }

    pub fn set_meta(&self, key: &str, value: &str) -> Result<(), StoreError> {
        self.conn
            .execute(
                "INSERT INTO meta (key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                [key, value],
            )
            .map(drop)
            .map_err(store_error)
    }

    /// Before an update: keeps the saved files of the current copy for
    /// conditional requests and starts a fresh crawl.
    pub fn begin_update(&mut self) -> Result<(), StoreError> {
        let tx = self.conn.transaction().map_err(store_error)?;
        tx.execute_batch(
            "DELETE FROM previous;
             INSERT INTO previous SELECT * FROM urls WHERE state = 'saved';
             DELETE FROM urls;",
        )
        .map_err(store_error)?;
        tx.commit().map_err(store_error)
    }

    /// Forgets everything but the schema version, for a fresh copy.
    pub fn reset(&mut self) -> Result<(), StoreError> {
        self.conn
            .execute_batch(
                "DELETE FROM urls; DELETE FROM previous; DELETE FROM meta WHERE key != 'schema';",
            )
            .map_err(store_error)
    }

    fn load_table(&self, table: &str) -> Result<Vec<UrlEntry>, StoreError> {
        let mut statement = self
            .conn
            .prepare(&format!("{SELECT} {table} ORDER BY rowid"))
            .map_err(store_error)?;
        let rows = statement.query_map([], decode).map_err(store_error)?;
        rows.map(|row| row.map_err(store_error)?)
            .collect::<Result<Vec<_>, _>>()
    }
}

impl StateStore for SqliteStore {
    fn load(&mut self) -> Result<StoredCrawl, StoreError> {
        Ok(StoredCrawl {
            entries: self.load_table("urls")?,
            previous: self.load_table("previous")?,
        })
    }

    fn put(&mut self, entry: &UrlEntry) -> Result<(), StoreError> {
        let (state, detail, saved) = match &entry.state {
            EntryState::Queued => ("queued", None, None),
            EntryState::Saved(saved) => ("saved", None, Some(saved)),
            EntryState::Alias(to) => ("alias", Some(to.to_string()), None),
            EntryState::Remote(to) => ("remote", Some(to.to_string()), None),
            EntryState::Failed(reason) => ("failed", Some(reason.clone()), None),
            EntryState::Skipped(reason) => ("skipped", Some(reason.as_str().to_owned()), None),
        };
        let (document, charset) = match saved.and_then(|s| s.document.as_ref()) {
            Some(Document::Html { charset }) => (Some("html"), charset.clone()),
            Some(Document::Css) => (Some("css"), None),
            None => (None, None),
        };
        let mut statement = self
            .conn
            .prepare_cached(
                "INSERT INTO urls (url, depth, kind, referrer, follow, state, detail, path, mime,
                     document, charset, bytes, etag, last_modified)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)
                 ON CONFLICT(url) DO UPDATE SET depth = excluded.depth, kind = excluded.kind,
                     referrer = excluded.referrer, follow = excluded.follow, state = excluded.state,
                     detail = excluded.detail, path = excluded.path, mime = excluded.mime,
                     document = excluded.document, charset = excluded.charset,
                     bytes = excluded.bytes, etag = excluded.etag,
                     last_modified = excluded.last_modified",
            )
            .map_err(store_error)?;
        statement
            .execute(params![
                entry.url.as_str(),
                entry.depth,
                kind_name(entry.kind),
                entry.referrer.as_ref().map(Url::as_str),
                entry.follow_links,
                state,
                detail,
                saved.map(|s| s.path.to_string()),
                saved.and_then(|s| s.mime.clone()),
                document,
                charset,
                saved.map(|s| s.bytes as i64),
                saved.and_then(|s| s.validators.etag.clone()),
                saved.and_then(|s| s.validators.last_modified.clone()),
            ])
            .map(drop)
            .map_err(store_error)
    }

    fn set_status(&mut self, status: CrawlStatus) -> Result<(), StoreError> {
        self.set_meta("status", status.as_str())
    }
}

fn kind_name(kind: LinkKind) -> &'static str {
    match kind {
        LinkKind::Page => "page",
        LinkKind::Asset => "asset",
    }
}

/// Decodes one row. A row that can't be understood is an error rather than
/// something silently dropped, since resuming from bad state would mislead.
fn decode(row: &Row<'_>) -> rusqlite::Result<Result<UrlEntry, StoreError>> {
    let url: String = row.get(0)?;
    let corrupt = |what: &str| StoreError::new(format!("corrupt state for {url}: {what}"));
    let parse_url = |text: &str| Url::parse(text).map_err(|_| corrupt("invalid URL"));

    let depth: u32 = row.get(1)?;
    let kind: String = row.get(2)?;
    let referrer: Option<String> = row.get(3)?;
    let follow_links: bool = row.get(4)?;
    let state: String = row.get(5)?;
    let detail: Option<String> = row.get(6)?;
    let path: Option<String> = row.get(7)?;
    let mime: Option<String> = row.get(8)?;
    let document: Option<String> = row.get(9)?;
    let charset: Option<String> = row.get(10)?;
    let bytes: Option<i64> = row.get(11)?;
    let etag: Option<String> = row.get(12)?;
    let last_modified: Option<String> = row.get(13)?;

    let decoded = (|| {
        let kind = match kind.as_str() {
            "page" => LinkKind::Page,
            "asset" => LinkKind::Asset,
            _ => return Err(corrupt("unknown kind")),
        };
        let detail = || detail.clone().ok_or_else(|| corrupt("missing detail"));
        let state = match state.as_str() {
            "queued" => EntryState::Queued,
            "alias" => EntryState::Alias(parse_url(&detail()?)?),
            "remote" => EntryState::Remote(parse_url(&detail()?)?),
            "failed" => EntryState::Failed(detail()?),
            "skipped" => EntryState::Skipped(
                SkipReason::parse(&detail()?).ok_or_else(|| corrupt("unknown skip reason"))?,
            ),
            "saved" => EntryState::Saved(SavedFile {
                path: path
                    .as_deref()
                    .and_then(LocalPath::parse)
                    .ok_or_else(|| corrupt("invalid path"))?,
                mime: mime.clone(),
                document: match document.as_deref() {
                    Some("html") => Some(Document::Html {
                        charset: charset.clone(),
                    }),
                    Some("css") => Some(Document::Css),
                    None => None,
                    Some(_) => return Err(corrupt("unknown document kind")),
                },
                bytes: bytes.unwrap_or_default().max(0) as u64,
                validators: Validators {
                    etag: etag.clone(),
                    last_modified: last_modified.clone(),
                },
            }),
            _ => return Err(corrupt("unknown state")),
        };
        Ok(UrlEntry {
            url: parse_url(&url)?,
            depth,
            kind,
            referrer: referrer.as_deref().map(parse_url).transpose()?,
            follow_links,
            state,
        })
    })();
    Ok(decoded)
}
