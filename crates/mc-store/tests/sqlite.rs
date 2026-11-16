#![allow(clippy::unwrap_used)]

use mc_core::{
    CrawlStatus, Document, EntryState, LinkKind, LocalPath, SavedFile, SkipReason, StateStore,
    UrlEntry,
};
use mc_requester::{Url, Validators};
use mc_store::SqliteStore;

fn entry(path: &str, state: EntryState) -> UrlEntry {
    UrlEntry {
        url: Url::parse(&format!("https://s.test{path}")).unwrap(),
        depth: 2,
        kind: LinkKind::Asset,
        referrer: Some(Url::parse("https://s.test/").unwrap()),
        follow_links: false,
        state,
    }
}

fn saved(path: &str, document: Option<Document>) -> EntryState {
    EntryState::Saved(SavedFile {
        path: LocalPath::parse(path).unwrap(),
        mime: Some("text/html".to_owned()),
        document,
        bytes: 1234,
        validators: Validators {
            etag: Some("\"v1\"".to_owned()),
            last_modified: Some("Wed, 21 Oct 2015 07:28:00 GMT".to_owned()),
        },
    })
}

fn all_states() -> Vec<UrlEntry> {
    vec![
        entry("/q", EntryState::Queued),
        entry(
            "/",
            saved(
                "s.test/index.html",
                Some(Document::Html {
                    charset: Some("utf-8".into()),
                }),
            ),
        ),
        entry("/s.css", saved("s.test/s.css", Some(Document::Css))),
        entry("/img", saved("s.test/café img.png", None)),
        entry(
            "/old",
            EntryState::Alias(Url::parse("https://s.test/").unwrap()),
        ),
        entry(
            "/out",
            EntryState::Remote(Url::parse("https://x.test/").unwrap()),
        ),
        entry("/bad", EntryState::Failed("HTTP 404 Not Found".to_owned())),
        entry("/robots", EntryState::Skipped(SkipReason::Robots)),
    ]
}

#[test]
fn round_trips_every_state_and_keeps_order() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = SqliteStore::open(dir.path()).unwrap();
    for e in all_states() {
        store.put(&e).unwrap();
    }
    // Updating an entry keeps its first-seen position.
    let mut first = all_states().remove(0);
    first.state = EntryState::Failed("timed out".into());
    store.put(&first).unwrap();
    drop(store);

    let mut reopened = SqliteStore::open(dir.path()).unwrap();
    let loaded = reopened.load().unwrap();
    let mut expected = all_states();
    expected[0] = first;
    assert_eq!(loaded.entries, expected);
    assert!(loaded.previous.is_empty());
}

#[test]
fn tracks_status_and_meta() {
    let dir = tempfile::tempdir().unwrap();
    assert!(!SqliteStore::exists_in(dir.path()));
    let mut store = SqliteStore::open(dir.path()).unwrap();
    assert!(SqliteStore::exists_in(dir.path()));
    assert_eq!(store.status().unwrap(), None);
    store.set_status(CrawlStatus::Stopped).unwrap();
    store.set_meta("options", "{}").unwrap();
    assert_eq!(store.status().unwrap(), Some(CrawlStatus::Stopped));
    assert_eq!(store.meta("options").unwrap().as_deref(), Some("{}"));

    store.put(&all_states()[1]).unwrap();
    store.reset().unwrap();
    assert_eq!(store.status().unwrap(), None);
    assert!(store.load().unwrap().entries.is_empty());
}

#[test]
fn begin_update_keeps_saved_files_as_previous() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = SqliteStore::open(dir.path()).unwrap();
    for e in all_states() {
        store.put(&e).unwrap();
    }
    store.begin_update().unwrap();
    let loaded = store.load().unwrap();
    assert!(loaded.entries.is_empty());
    assert_eq!(loaded.previous.len(), 3);
    assert!(
        loaded
            .previous
            .iter()
            .all(|e| matches!(e.state, EntryState::Saved(_)))
    );
}

#[test]
fn refuses_an_incompatible_schema() {
    let dir = tempfile::tempdir().unwrap();
    SqliteStore::open(dir.path())
        .unwrap()
        .set_meta("schema", "999")
        .unwrap();
    let err = SqliteStore::open(dir.path()).err().unwrap();
    assert!(err.to_string().contains("incompatible Amel"), "{err}");
}

#[test]
fn copies_from_before_the_rename_move_their_state() {
    let dir = tempfile::tempdir().unwrap();
    {
        let mut store = SqliteStore::open(dir.path()).unwrap();
        mc_core::StateStore::set_status(&mut store, mc_core::CrawlStatus::Stopped).unwrap();
    }
    // As a copy made before the rename would have it.
    std::fs::rename(dir.path().join(".amel"), dir.path().join(".mistercopy")).unwrap();
    assert!(
        SqliteStore::exists_in(dir.path()),
        "found under the old name"
    );
    assert!(dir.path().join(".amel").is_dir() && !dir.path().join(".mistercopy").exists());
    let store = SqliteStore::open(dir.path()).unwrap();
    assert_eq!(store.status().unwrap(), Some(mc_core::CrawlStatus::Stopped));
}
