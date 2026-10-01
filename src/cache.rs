//! On-disk cache of the last successfully loaded Xtream playlist.
//!
//! [`crate::loader::load_xtream`] shows this cached copy immediately on
//! startup, if present, while a fresh copy is fetched live in the
//! background. The fresh copy is streamed into a [`PendingCache`] as it
//! arrives; once the fetch succeeds, [`PendingCache::commit`] replaces the
//! cache so the next launch starts from up-to-date data. The cache is plain
//! M3U text — the same format the loader already parses — so no separate
//! (de)serialization format is needed.
//!
//! Every operation here is best-effort: a cache that cannot be read,
//! written, or promoted just means the next launch fetches live instead of
//! starting from a cached copy, never a reason to fail the load itself.
//! What it must never do is replace a good cache with an incomplete one.

use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::private_file;

/// Where an account's cached playlist lives under the app's config
/// directory, keyed by [`crate::xtream::Account::cache_key`].
#[must_use]
pub fn path(config_dir: &Path, account_key: &str) -> PathBuf {
    config_dir
        .join("cache")
        .join(format!("xtream-{account_key}.m3u"))
}

/// Opens the cached playlist for reading, if it exists.
#[must_use]
pub fn open(path: &Path) -> Option<File> {
    private_file::open(path).ok()
}

/// A replacement cache being streamed into a temp file beside its final
/// path, so a write that dies halfway never corrupts the previous,
/// still-valid cache.
///
/// Write errors are remembered rather than returned: after the first one
/// every later write is a no-op and [`commit`](Self::commit) discards the
/// temp file instead of promoting it. Dropping an uncommitted cache (an
/// aborted load) discards it too.
#[derive(Debug)]
pub struct PendingCache {
    /// Open temp file; `None` once a write to it has failed.
    file: Option<File>,
    tmp: PathBuf,
    path: PathBuf,
    /// Set once the temp file has been renamed onto `path`, so [`Drop`]
    /// leaves it alone.
    committed: bool,
}

impl PendingCache {
    /// Opens a fresh temp file beside `path` (creating the parent
    /// directory if needed). `None` if the directory or file cannot be
    /// created.
    #[must_use]
    pub fn create(path: &Path) -> Option<Self> {
        let parent = path.parent()?;
        private_file::create_dir_all(parent).ok()?;
        let tmp = private_file::unique_tmp(path);
        let file = private_file::create(&tmp).ok()?;
        Some(Self {
            file: Some(file),
            tmp,
            path: path.to_owned(),
            committed: false,
        })
    }

    /// Appends `bytes` to the temp file. The first failure (disk full,
    /// I/O error) poisons this cache: the file is closed, later writes
    /// are ignored, and [`commit`](Self::commit) will not promote it.
    pub fn write(&mut self, bytes: &[u8]) {
        if let Some(file) = &mut self.file
            && let Err(error) = file.write_all(bytes)
        {
            log::warn!("playlist cache write failed ({error}); keeping the previous cache");
            self.file = None;
        }
    }

    /// Atomically replaces the cache with the temp file — only if every
    /// write succeeded; otherwise the temp file is discarded and the
    /// previous cache stays untouched. Returns whether the cache was
    /// replaced.
    pub fn commit(mut self) -> bool {
        let Some(file) = self.file.take() else {
            return false;
        };
        // Close the handle first: Windows cannot rename an open file.
        drop(file);
        // promote() removes the temp file itself when it fails.
        self.committed = private_file::promote(&self.tmp, &self.path).is_ok();
        self.committed
    }

    /// A pending cache for `path` whose temp file is open read-only, so
    /// every write to it fails the way a full disk would.
    #[cfg(test)]
    // unwrap is fine in test helpers (see CLAUDE.md).
    #[allow(clippy::unwrap_used)]
    pub(crate) fn failing_for_test(path: &Path) -> Self {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let tmp = private_file::unique_tmp(path);
        fs::write(&tmp, b"").unwrap();
        Self {
            file: Some(File::open(&tmp).unwrap()),
            tmp,
            path: path.to_owned(),
            committed: false,
        }
    }
}

impl Drop for PendingCache {
    fn drop(&mut self) {
        if !self.committed {
            // Close before removing: Windows cannot delete an open file.
            self.file = None;
            let _ = fs::remove_file(&self.tmp);
        }
    }
}

#[cfg(test)]
// unwrap is fine in tests (see AGENTS.md).
#[allow(clippy::unwrap_used)]
mod tests {
    use std::io::Read;

    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("m3u-viewer-cache-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn path_is_namespaced_under_a_cache_subdirectory() {
        let dir = PathBuf::from("C:/config");
        assert_eq!(
            path(&dir, "example.com-user"),
            PathBuf::from("C:/config/cache/xtream-example.com-user.m3u")
        );
    }

    #[test]
    fn missing_cache_opens_as_none() {
        let dir = temp_dir("missing");
        assert!(open(&path(&dir, "acct")).is_none());
    }

    #[test]
    fn create_write_commit_and_reopen_round_trips() {
        let dir = temp_dir("roundtrip");
        let cache_path = path(&dir, "acct");
        let mut pending = PendingCache::create(&cache_path).unwrap();
        pending.write(b"#EXTM3U\n");
        assert!(!cache_path.exists());
        assert!(pending.commit());
        assert!(cache_path.exists());

        let mut reopened = open(&cache_path).unwrap();
        let mut text = String::new();
        reopened.read_to_string(&mut text).unwrap();
        assert_eq!(text, "#EXTM3U\n");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn dropped_cache_never_reaches_the_cache_path() {
        let dir = temp_dir("discard");
        let cache_path = path(&dir, "acct");
        let pending = PendingCache::create(&cache_path).unwrap();
        let tmp = pending.tmp.clone();
        drop(pending);
        assert!(!tmp.exists());
        assert!(!cache_path.exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn failed_write_keeps_the_previous_cache() {
        // Regression: a write error (disk full) mid-download only stopped
        // the mirroring, and the truncated temp file was still promoted
        // over the good cache.
        let dir = temp_dir("write-failure");
        let cache_path = path(&dir, "acct");
        let good = "#EXTM3U\n#EXTINF:-1,Good\nhttp://u/good\n";
        fs::create_dir_all(cache_path.parent().unwrap()).unwrap();
        fs::write(&cache_path, good).unwrap();

        let mut pending = PendingCache::failing_for_test(&cache_path);
        let tmp = pending.tmp.clone();
        pending.write(b"#EXTM3U\n");
        assert!(!pending.commit(), "a failed write must not be promoted");

        assert!(!tmp.exists(), "temp file left behind");
        assert_eq!(fs::read_to_string(&cache_path).unwrap(), good);
        let _ = fs::remove_dir_all(&dir);
    }
}
