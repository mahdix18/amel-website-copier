//! The copies the app knows about, remembered across launches in
//! `~/.config/mistercopy/projects.json`.

use std::path::{Path, PathBuf};

use mc_core::CrawlStatus;
use mc_store::SqliteStore;
use serde::{Deserialize, Serialize};

/// One copy: the site and the folder it was saved into.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Project {
    pub url: String,
    pub output: PathBuf,
}

impl Project {
    /// The site's host, for the sidebar.
    pub fn name(&self) -> String {
        mc_requester::Url::parse(&self.url)
            .ok()
            .and_then(|url| url.host_str().map(str::to_owned))
            .unwrap_or_else(|| self.url.clone())
    }

    pub fn status(&self) -> ProjectStatus {
        if !SqliteStore::exists_in(&self.output) {
            return ProjectStatus::NotStarted;
        }
        match SqliteStore::open(&self.output)
            .ok()
            .and_then(|s| s.status().ok().flatten())
        {
            Some(CrawlStatus::Complete) => ProjectStatus::Complete,
            Some(_) => ProjectStatus::Unfinished,
            None => ProjectStatus::NotStarted,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectStatus {
    NotStarted,
    Unfinished,
    Complete,
}

impl ProjectStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::NotStarted => "not copied yet",
            Self::Unfinished => "unfinished",
            Self::Complete => "complete",
        }
    }
}

/// Most recently used first.
#[derive(Debug, Clone, Default)]
pub struct Projects {
    path: Option<PathBuf>,
    pub items: Vec<Project>,
}

impl Projects {
    /// `$XDG_CONFIG_HOME/mistercopy/projects.json`, else under `~/.config`.
    pub fn default_path() -> Option<PathBuf> {
        let config = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?;
        Some(config.join("mistercopy").join("projects.json"))
    }

    /// Loads the list; a missing or unreadable file is an empty list, so a
    /// damaged file never stops the app from starting.
    pub fn load(path: Option<PathBuf>) -> Self {
        let items = path
            .as_deref()
            .and_then(|p| std::fs::read(p).ok())
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        Self { path, items }
    }

    /// Puts `project` first, dropping an older entry for the same folder.
    pub fn remember(&mut self, project: Project) {
        self.items.retain(|p| p.output != project.output);
        self.items.insert(0, project);
    }

    pub fn forget(&mut self, index: usize) {
        if index < self.items.len() {
            self.items.remove(index);
        }
    }

    pub fn save(&self) -> std::io::Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let json = serde_json::to_vec_pretty(&self.items).map_err(std::io::Error::other)?;
        std::fs::write(path, json)
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(url: &str, output: &str) -> Project {
        Project {
            url: url.to_owned(),
            output: PathBuf::from(output),
        }
    }

    #[test]
    fn remembers_most_recent_first_and_persists() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join("projects.json");
        let mut projects = Projects::load(Some(path.clone()));
        assert!(projects.items.is_empty());

        projects.remember(project("https://a.test/", "/out/a"));
        projects.remember(project("https://b.test/", "/out/b"));
        projects.remember(project("https://a.test/docs", "/out/a"));
        assert_eq!(projects.items.len(), 2);
        assert_eq!(projects.items[0].name(), "a.test");
        projects.save().unwrap();

        let mut reloaded = Projects::load(Some(path.clone()));
        assert_eq!(reloaded.items, projects.items);
        reloaded.forget(0);
        reloaded.forget(9);
        assert_eq!(reloaded.items.len(), 1);

        std::fs::write(&path, "not json").unwrap();
        assert!(Projects::load(Some(path)).items.is_empty());
    }

    #[test]
    fn reads_status_from_the_copy() {
        let dir = tempfile::tempdir().unwrap();
        let p = project("https://a.test/", dir.path().to_str().unwrap());
        assert_eq!(p.status(), ProjectStatus::NotStarted);
        let mut store = SqliteStore::open(dir.path()).unwrap();
        mc_core::StateStore::set_status(&mut store, CrawlStatus::Stopped).unwrap();
        assert_eq!(p.status(), ProjectStatus::Unfinished);
        mc_core::StateStore::set_status(&mut store, CrawlStatus::Complete).unwrap();
        assert_eq!(p.status(), ProjectStatus::Complete);
    }
}
