//! Window preferences remembered across launches, in `prefs.json` next to
//! the project list: parallel downloads, speed, switches and the theme.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::job::{MAX_WORKERS, Speed};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Prefs {
    pub workers: usize,
    pub speed: Speed,
    pub respect_robots: bool,
    pub render_js: bool,
    /// Browser profile and platform to impersonate; `None` for defaults.
    pub profile: Option<String>,
    pub platform: Option<String>,
    /// Also queue the pages the site's sitemaps list.
    pub sitemaps: bool,
    /// Check for missed files after each copy.
    pub verify: bool,
    /// Slow down automatically on 429/503.
    pub adaptive: bool,
    /// File types not downloaded.
    pub skip_types: Vec<mc_core::FileType>,
    /// Run the script with each copy, and its text.
    pub script_on: bool,
    pub script: Option<String>,
    /// A desktop notification when a copy finishes.
    pub notify: bool,
    /// The log panel is open.
    pub console_open: bool,
    pub dark: bool,
}

impl Default for Prefs {
    fn default() -> Self {
        Self {
            workers: 4,
            speed: Speed::default(),
            respect_robots: true,
            render_js: false,
            profile: None,
            platform: None,
            sitemaps: true,
            verify: true,
            adaptive: true,
            skip_types: Vec::new(),
            script_on: false,
            script: None,
            notify: true,
            console_open: false,
            dark: true,
        }
    }
}

impl Prefs {
    /// Loads `path`; anything missing or unreadable falls back to defaults.
    pub fn load(path: Option<&PathBuf>) -> Self {
        let mut prefs: Self = path
            .and_then(|p| std::fs::read(p).ok())
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        prefs.workers = prefs.workers.clamp(1, MAX_WORKERS);
        prefs
    }

    pub fn save(&self, path: Option<&PathBuf>) -> std::io::Result<()> {
        let Some(path) = path else {
            return Ok(());
        };
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let json = serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?;
        std::fs::write(path, json)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_repairs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("prefs.json");
        assert_eq!(Prefs::load(Some(&path)), Prefs::default());

        let prefs = Prefs {
            workers: 12,
            speed: Speed::Fast,
            dark: false,
            ..Prefs::default()
        };
        prefs.save(Some(&path)).unwrap();
        assert_eq!(Prefs::load(Some(&path)), prefs);
        assert!(std::fs::read_to_string(&path).unwrap().contains("\"fast\""));

        std::fs::write(&path, r#"{"workers": 999}"#).unwrap();
        let repaired = Prefs::load(Some(&path));
        assert_eq!(repaired.workers, MAX_WORKERS);
        assert!(
            repaired.respect_robots,
            "missing fields keep their defaults"
        );
    }
}
