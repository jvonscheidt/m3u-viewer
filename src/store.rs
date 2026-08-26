//! Persistent favorites and recently played channels.
//!
//! Both are keyed by channel URL (so they survive playlist re-ordering)
//! and stored as small JSON arrays in the platform config directory,
//! written atomically (temp file + rename) on every change.

use std::collections::HashSet;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::private_file;

/// Maximum number of remembered recently played channels.
pub const RECENTS_CAP: usize = 50;

/// Failure to persist a change (the in-memory state is still updated).
#[derive(Debug, Error)]
pub enum StoreError {
    /// Reading a persisted list failed.
    #[error("could not read {}: {source}", path.display())]
    Read {
        /// File that could not be read.
        path: PathBuf,
        /// Underlying filesystem error.
        source: io::Error,
    },
    /// A persisted list was not valid JSON.
    #[error("could not parse {}: {source}", path.display())]
    Decode {
        /// File containing invalid data.
        path: PathBuf,
        /// Underlying JSON error.
        source: serde_json::Error,
    },
    /// Writing the JSON file failed.
    #[error("could not save: {0}")]
    Io(#[from] io::Error),
    /// Encoding to JSON failed.
    #[error("could not encode: {0}")]
    Json(#[from] serde_json::Error),
}

/// On-disk store for favorites and recents.
pub struct Store {
    dir: PathBuf,
    favorites: HashSet<String>,
    /// Newest first, deduplicated, at most [`RECENTS_CAP`] entries.
    recents: Vec<String>,
}

impl fmt::Debug for Store {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Store")
            .field("favorites", &self.favorites.len())
            .field("recents", &self.recents.len())
            .finish_non_exhaustive()
    }
}

impl Store {
    /// The default per-user config directory, if the platform has one.
    #[must_use]
    pub fn default_dir() -> Option<PathBuf> {
        directories::ProjectDirs::from("", "", "m3u-viewer")
            .map(|dirs| dirs.config_dir().to_path_buf())
    }

    /// Opens the store in `dir`, reading whatever state exists there.
    ///
    /// Missing files yield empty lists.
    ///
    /// # Errors
    ///
    /// [`StoreError::Read`] or [`StoreError::Decode`] when an existing file
    /// cannot be read or decoded. Callers should disable persistence rather
    /// than replace unreadable data with an empty list.
    pub fn load(dir: PathBuf) -> Result<Self, StoreError> {
        let favorites = read_list(&dir.join(FAVORITES_FILE))?
            .into_iter()
            .collect::<HashSet<_>>();
        let mut recents = read_list(&dir.join(RECENTS_FILE))?;
        recents.truncate(RECENTS_CAP);
        Ok(Self {
            dir,
            favorites,
            recents,
        })
    }

    /// Whether `url` is currently a favorite.
    #[must_use]
    pub fn is_favorite(&self, url: &str) -> bool {
        self.favorites.contains(url)
    }

    /// Toggles favorite status for `url` and persists the change.
    /// Returns the new status (`true` = now a favorite).
    ///
    /// # Errors
    ///
    /// [`StoreError`] if saving fails; the in-memory toggle sticks anyway.
    pub fn toggle_favorite(&mut self, url: &str) -> Result<bool, StoreError> {
        let now_favorite = if self.favorites.remove(url) {
            false
        } else {
            self.favorites.insert(url.to_owned());
            true
        };
        // Sorted for a stable, diff-friendly file.
        let mut list = self.favorites.iter().cloned().collect::<Vec<_>>();
        list.sort_unstable();
        self.save(FAVORITES_FILE, &list)?;
        Ok(now_favorite)
    }

    /// Records a successful playback: `url` moves to the front of the
    /// recents list (deduplicated, capped) and the change is persisted.
    ///
    /// # Errors
    ///
    /// [`StoreError`] if saving fails; the in-memory update sticks anyway.
    pub fn push_recent(&mut self, url: &str) -> Result<(), StoreError> {
        self.recents.retain(|existing| existing != url);
        self.recents.insert(0, url.to_owned());
        self.recents.truncate(RECENTS_CAP);
        let list = self.recents.clone();
        self.save(RECENTS_FILE, &list)
    }

    /// Recently played URLs, newest first.
    #[must_use]
    pub fn recents(&self) -> &[String] {
        &self.recents
    }

    fn save(&self, file: &str, list: &[String]) -> Result<(), StoreError> {
        private_file::create_dir_all(&self.dir)?;
        let json = serde_json::to_string_pretty(list)?;
        private_file::atomic_write(&self.dir.join(file), json.as_bytes())?;
        Ok(())
    }
}

const FAVORITES_FILE: &str = "favorites.json";
const RECENTS_FILE: &str = "recents.json";

/// Reads a JSON string array, treating only a missing file as empty.
fn read_list(path: &Path) -> Result<Vec<String>, StoreError> {
    let json = match private_file::read_to_string(path) {
        Ok(json) => json,
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => {
            return Err(StoreError::Read {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    serde_json::from_str(&json).map_err(|source| StoreError::Decode {
        path: path.to_path_buf(),
        source,
    })
}

#[cfg(test)]
// unwrap is fine in tests (see CLAUDE.md).
#[allow(clippy::unwrap_used)]
mod tests {
    use std::fs;

    use super::*;

    /// Fresh directory under the system temp dir, unique per test.
    fn temp_store(tag: &str) -> Store {
        let dir =
            std::env::temp_dir().join(format!("m3u-viewer-store-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Store::load(dir).unwrap()
    }

    #[test]
    fn favorites_toggle_and_persist() {
        let mut store = temp_store("fav");
        assert!(!store.is_favorite("http://u/1"));
        assert!(store.toggle_favorite("http://u/1").unwrap());
        assert!(store.is_favorite("http://u/1"));

        // A fresh load from the same directory sees the favorite.
        let reloaded = Store::load(store.dir.clone()).unwrap();
        assert!(reloaded.is_favorite("http://u/1"));

        assert!(!store.toggle_favorite("http://u/1").unwrap());
        let reloaded = Store::load(store.dir.clone()).unwrap();
        assert!(!reloaded.is_favorite("http://u/1"));
        let _ = fs::remove_dir_all(&store.dir);
    }

    #[test]
    fn recents_dedupe_newest_first_and_cap() {
        let mut store = temp_store("rec");
        for i in 0..60 {
            store.push_recent(&format!("http://u/{i}")).unwrap();
        }
        assert_eq!(store.recents().len(), RECENTS_CAP);
        assert_eq!(store.recents()[0], "http://u/59");

        // Replaying an old URL moves it to the front without duplicating.
        store.push_recent("http://u/30").unwrap();
        assert_eq!(store.recents()[0], "http://u/30");
        assert_eq!(
            store
                .recents()
                .iter()
                .filter(|u| *u == "http://u/30")
                .count(),
            1
        );

        let reloaded = Store::load(store.dir.clone()).unwrap();
        assert_eq!(reloaded.recents(), store.recents());
        let _ = fs::remove_dir_all(&store.dir);
    }

    #[test]
    fn corrupt_files_return_an_error_without_overwriting_data() {
        let dir =
            std::env::temp_dir().join(format!("m3u-viewer-store-corrupt-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(FAVORITES_FILE), "not json at all").unwrap();
        fs::write(dir.join(RECENTS_FILE), "{\"wrong\": \"shape\"}").unwrap();
        let error = Store::load(dir.clone()).unwrap_err();
        assert!(matches!(error, StoreError::Decode { .. }));
        assert_eq!(
            fs::read_to_string(dir.join(FAVORITES_FILE)).unwrap(),
            "not json at all"
        );
        let _ = fs::remove_dir_all(&dir);
    }
}
