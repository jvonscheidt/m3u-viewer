//! On-disk cache of the last successfully loaded Xtream playlist.
//!
//! [`crate::loader::load_xtream`] shows this cached copy immediately on
//! startup, if present, while a fresh copy is fetched live in the
//! background; once that fetch succeeds, [`promote`] overwrites the cache
//! so the next launch starts from up-to-date data. The cache is plain M3U
//! text — the same format the loader already parses — so no separate
//! (de)serialization format is needed.
//!
//! Every operation here is best-effort: a cache that cannot be read,
//! written, or promoted just means the next launch fetches live instead of
//! starting from a cached copy, never a reason to fail the load itself.

use std::fs::{self, File};
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

/// Opens a fresh temp file beside `path` to stream a new cache into
/// (creating the parent directory if needed), so a write that dies
/// halfway never corrupts the previous, still-valid cache. `None` if the
/// directory or file cannot be created.
#[must_use]
pub fn create_temp(path: &Path) -> Option<(File, PathBuf)> {
    let parent = path.parent()?;
    private_file::create_dir_all(parent).ok()?;
    let tmp = unique_tmp(path);
    let file = private_file::create(&tmp).ok()?;
    Some((file, tmp))
}

/// Atomically replaces the cache at `path` with the finished temp file
/// from [`create_temp`].
pub fn promote(tmp: &Path, path: &Path) {
    if fs::rename(tmp, path).is_err() {
        discard_temp(tmp);
    }
}

/// Removes a temp file whose write was aborted (parse failure, I/O
/// error) instead of promoted.
pub fn discard_temp(tmp: &Path) {
    let _ = fs::remove_file(tmp);
}

/// A sibling temp path unique to this process and moment, e.g.
/// `xtream-example.m3u.tmp.4711.1234567890`.
fn unique_tmp(path: &Path) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".tmp.{}.{nanos}", std::process::id()));
    path.with_file_name(name)
}

#[cfg(test)]
// unwrap is fine in tests (see CLAUDE.md).
#[allow(clippy::unwrap_used)]
mod tests {
    use std::io::{Read, Write};

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
    fn create_write_promote_and_reopen_round_trips() {
        let dir = temp_dir("roundtrip");
        let cache_path = path(&dir, "acct");
        let (mut file, tmp) = create_temp(&cache_path).unwrap();
        file.write_all(b"#EXTM3U\n").unwrap();
        drop(file);
        assert!(!cache_path.exists());
        promote(&tmp, &cache_path);
        assert!(cache_path.exists());

        let mut reopened = open(&cache_path).unwrap();
        let mut text = String::new();
        reopened.read_to_string(&mut text).unwrap();
        assert_eq!(text, "#EXTM3U\n");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn discarded_temp_never_reaches_the_cache_path() {
        let dir = temp_dir("discard");
        let cache_path = path(&dir, "acct");
        let (file, tmp) = create_temp(&cache_path).unwrap();
        drop(file);
        discard_temp(&tmp);
        assert!(!tmp.exists());
        assert!(!cache_path.exists());
        let _ = fs::remove_dir_all(&dir);
    }
}
