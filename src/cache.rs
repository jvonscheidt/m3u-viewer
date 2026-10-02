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
//!
//! # Temp files left behind
//!
//! A viewer that quits (or crashes) mid-download never gets to drop its
//! [`PendingCache`], so its playlist-sized temp file stays behind.
//! [`sweep_stale_temps`] removes such leftovers at the start of the next
//! Xtream load. To tell them apart from the temp file of a second viewer
//! that is still downloading, every [`PendingCache`] holds an exclusive
//! lock on its temp file for as long as it is open; the OS releases that
//! lock when the process exits, however it exits. See
//! [`sweep_stale_temps`] for the exact rule.
//!
//! # Location
//!
//! The cache lives in the platform's per-user *cache* directory (see
//! [`CacheDirs::platform_default`]): it is big, re-downloadable data that
//! must not be synced along with a roaming profile. Versions up to 0.9.1
//! kept it in a `cache/` subdirectory of the config directory; the first
//! load with this version deletes the playlists found there (see
//! [`CacheDirs::tidy`]) instead of moving them, since they carry stream
//! URLs with possibly outdated credentials and would be re-keyed anyway.

use std::fs::{self, File, TryLockError};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::private_file;

/// Directories used by the Xtream playlist cache.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheDirs {
    /// Where cached playlists are stored.
    dir: PathBuf,
    /// Where older versions stored them; cleaned up by [`Self::tidy`].
    legacy_dir: Option<PathBuf>,
}

impl CacheDirs {
    /// Caches playlists in `dir`, with no older location to clean up.
    #[must_use]
    pub fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            legacy_dir: None,
        }
    }

    /// Caches playlists in `dir`, and deletes the playlists an older
    /// version left in `legacy_dir` (see [`Self::tidy`]).
    #[must_use]
    pub fn with_legacy_dir(dir: PathBuf, legacy_dir: PathBuf) -> Self {
        Self {
            dir,
            legacy_dir: Some(legacy_dir),
        }
    }

    /// The per-user cache directory — `%LOCALAPPDATA%\m3u-viewer\cache` on
    /// Windows (not the roaming profile), `~/.cache/m3u-viewer` on Linux,
    /// `~/Library/Caches/m3u-viewer` on macOS — with the `cache/`
    /// subdirectory of the config directory as the legacy location.
    /// `None` on platforms without a home directory.
    #[must_use]
    pub fn platform_default() -> Option<Self> {
        directories::ProjectDirs::from("", "", "m3u-viewer").map(|dirs| {
            Self::with_legacy_dir(
                dirs.cache_dir().to_path_buf(),
                dirs.config_dir().join("cache"),
            )
        })
    }

    /// Where cached playlists are stored.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Housekeeping before a load: deletes the playlist caches an older
    /// version left in the legacy directory, then sweeps abandoned temp
    /// files from the cache directory ([`sweep_stale_temps`]).
    ///
    /// In the legacy directory only `xtream-*.m3u` playlists and their
    /// temp files are deleted, and the directory itself only if that
    /// leaves it empty; nothing else in the config directory is touched.
    /// Older versions do not lock their temp files, so a legacy temp file
    /// is deleted only once it is unmodified for [`STALE_AFTER`] — a
    /// younger one may still be written by an older viewer running
    /// alongside, and is left for a later launch.
    pub fn tidy(&self) {
        if let Some(legacy) = &self.legacy_dir
            && *legacy != self.dir
        {
            remove_legacy_caches(legacy);
        }
        sweep_stale_temps(&self.dir);
    }
}

/// See [`CacheDirs::tidy`].
fn remove_legacy_caches(legacy: &Path) {
    let Ok(entries) = fs::read_dir(legacy) else {
        return;
    };
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let Some(name) = file_name.to_str() else {
            continue;
        };
        let path = entry.path();
        let remove = if is_cache_file_name(name) {
            true
        } else if let Some((target, _)) = private_file::parse_tmp_name(name) {
            is_cache_file_name(target) && unmodified_for(&path, STALE_AFTER)
        } else {
            false
        };
        if remove {
            match fs::remove_file(&path) {
                Ok(()) => log::info!("removed legacy playlist cache {}", path.display()),
                Err(error) => log::debug!("could not remove {}: {error}", path.display()),
            }
        }
    }
    // Only succeeds once the directory is empty, i.e. holds nothing else.
    if fs::remove_dir(legacy).is_ok() {
        log::info!("removed legacy cache directory {}", legacy.display());
    }
}

/// Whether the file at `path` was last modified more than `age` ago.
fn unmodified_for(path: &Path, age: Duration) -> bool {
    fs::metadata(path)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|modified| modified.elapsed().ok())
        .is_some_and(|elapsed| elapsed > age)
}

/// File-name prefix of every cached playlist.
const FILE_PREFIX: &str = "xtream-";
/// File-name extension of every cached playlist.
const FILE_SUFFIX: &str = ".m3u";

/// Age (since last modification) after which a temp file that cannot be
/// lock-checked is considered abandoned. A live download touches its temp
/// file at least every 30 s (the HTTP idle timeout) and is cut off after
/// one hour in total (see `xtream::HttpTimeouts::STANDARD`), so twice that
/// can only be a leftover.
const STALE_AFTER: Duration = Duration::from_hours(2);

/// Where an account's cached playlist lives in the cache directory
/// ([`CacheDirs::dir`]), keyed by [`crate::xtream::Account::cache_key`].
#[must_use]
pub fn path(cache_dir: &Path, account_key: &str) -> PathBuf {
    cache_dir.join(format!("{FILE_PREFIX}{account_key}{FILE_SUFFIX}"))
}

/// Whether `name` is the file name of a cached playlist (not a temp file).
fn is_cache_file_name(name: &str) -> bool {
    name.starts_with(FILE_PREFIX) && name.ends_with(FILE_SUFFIX)
}

/// Deletes playlist temp files in `dir` that no running viewer is writing:
/// leftovers of a viewer that quit or crashed mid-download.
///
/// A temp file (of any account) is deleted only when both hold:
///
/// - its name embeds a process id other than this process's — this
///   process's own temps belong to its own loads, which clean up after
///   themselves;
/// - its exclusive lock can be taken, i.e. no live [`PendingCache`] in
///   any process holds it. Locks die with their process, so a second
///   viewer that is still downloading keeps its temp file. Only where the
///   filesystem cannot lock at all does the file's age decide instead: it
///   must be unmodified for more than [`STALE_AFTER`].
///
/// Cached playlists themselves and unrelated files are never touched.
/// Best-effort: anything that cannot be inspected or removed is left for
/// a later sweep.
pub fn sweep_stale_temps(dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let own_pid = std::process::id();
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let Some((target, pid)) = file_name.to_str().and_then(private_file::parse_tmp_name) else {
            continue;
        };
        if pid == own_pid || !is_cache_file_name(target) {
            continue;
        }
        let path = entry.path();
        if is_abandoned(&path) {
            match fs::remove_file(&path) {
                Ok(()) => log::info!("removed abandoned playlist temp file {}", path.display()),
                Err(error) => log::debug!("could not remove {}: {error}", path.display()),
            }
        }
    }
}

/// Whether no live [`PendingCache`] is writing the temp file at `path`
/// (see [`sweep_stale_temps`]). The probe handle is closed on return, so
/// the file can be deleted right after.
fn is_abandoned(path: &Path) -> bool {
    let Ok(file) = File::open(path) else {
        return false;
    };
    match file.try_lock() {
        // Nobody holds it; the probe lock is released when `file` closes.
        Ok(()) => true,
        Err(TryLockError::WouldBlock) => false,
        Err(TryLockError::Error(error)) => {
            log::debug!("cannot lock-check {}: {error}", path.display());
            unmodified_for(path, STALE_AFTER)
        }
    }
}

/// Opens the cached playlist for reading, if it exists.
#[must_use]
pub fn open(path: &Path) -> Option<File> {
    private_file::open(path).ok()
}

/// Deletes the cache at `path` (e.g. one that could not be read back), so
/// the next launch fetches live instead of tripping over it again.
pub fn remove(path: &Path) {
    let _ = fs::remove_file(path);
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
    /// directory if needed) and locks it exclusively, marking it as in use
    /// for [`sweep_stale_temps`] in other viewers. `None` if the directory
    /// or file cannot be created.
    #[must_use]
    pub fn create(path: &Path) -> Option<Self> {
        let parent = path.parent()?;
        private_file::create_dir_all(parent).ok()?;
        let tmp = private_file::unique_tmp(path);
        let file = private_file::create(&tmp).ok()?;
        if let Err(error) = file.try_lock() {
            // Without the lock, sweeps fall back to the file's age, which
            // a live download keeps fresh.
            log::debug!("could not lock {}: {error}", tmp.display());
        }
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
    // unwrap is fine in test helpers (see AGENTS.md).
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
    fn path_is_a_prefixed_file_in_the_cache_directory() {
        let dir = PathBuf::from("C:/cache");
        assert_eq!(
            path(&dir, "example.com-user"),
            PathBuf::from("C:/cache/xtream-example.com-user.m3u")
        );
    }

    #[test]
    fn platform_cache_is_outside_the_config_directory() {
        // Regression: the cache lived under the config directory, which
        // on Windows is the roaming profile, so tens of MB of playlist
        // got synced to network profile storage. Paths only — nothing is
        // created or deleted in the real user directories.
        let Some(dirs) = CacheDirs::platform_default() else {
            return;
        };
        let config_dir = directories::ProjectDirs::from("", "", "m3u-viewer")
            .unwrap()
            .config_dir()
            .to_path_buf();
        assert!(
            !dirs.dir().starts_with(&config_dir),
            "{} is inside {}",
            dirs.dir().display(),
            config_dir.display()
        );
        assert_eq!(dirs.legacy_dir, Some(config_dir.join("cache")));
    }

    /// Writes `contents` to `path`, backdating its modification time by
    /// `age`.
    fn write_aged(path: &Path, contents: &[u8], age: Duration) {
        fs::write(path, contents).unwrap();
        let modified = std::time::SystemTime::now() - age;
        File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(modified)
            .unwrap();
    }

    #[test]
    fn tidy_deletes_legacy_playlists_but_nothing_else_in_the_config_dir() {
        // Regression: after the move to the cache directory, the old
        // config_dir/cache/ playlists — full of stream URLs with possibly
        // outdated credentials — would have stayed on disk forever.
        let root = temp_dir("legacy-mixed");
        let config_dir = root.join("config");
        let legacy = config_dir.join("cache");
        fs::create_dir_all(&legacy).unwrap();
        fs::write(config_dir.join("config.toml"), "user = 1\n").unwrap();
        fs::write(config_dir.join("favorites.json"), "[]").unwrap();
        let old_caches = [
            legacy.join("xtream-example_com-u-0123456789abcdef.m3u"),
            legacy.join("xtream-example_com-u.m3u"),
        ];
        for cache in &old_caches {
            fs::write(cache, "#EXTM3U\n").unwrap();
        }
        let stale_tmp = foreign_tmp(&old_caches[0], other_pid());
        write_aged(&stale_tmp, b"#EXTM3U\n", STALE_AFTER * 2);
        // An older viewer may still be writing this one.
        let fresh_tmp = foreign_tmp(&old_caches[1], other_pid());
        fs::write(&fresh_tmp, b"#EXTM3U\n").unwrap();
        let unrelated = legacy.join("notes.txt");
        fs::write(&unrelated, "mine").unwrap();

        let dirs = CacheDirs::with_legacy_dir(root.join("new-cache"), legacy.clone());
        dirs.tidy();

        for gone in old_caches.iter().chain([&stale_tmp]) {
            assert!(!gone.exists(), "{} left behind", gone.display());
        }
        for kept in [&fresh_tmp, &unrelated] {
            assert!(kept.exists(), "{} was removed", kept.display());
        }
        assert_eq!(
            fs::read_to_string(config_dir.join("config.toml")).unwrap(),
            "user = 1\n"
        );
        assert!(config_dir.join("favorites.json").exists());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn tidy_removes_the_legacy_directory_once_it_is_empty() {
        let root = temp_dir("legacy-empty");
        let config_dir = root.join("config");
        let legacy = config_dir.join("cache");
        fs::create_dir_all(&legacy).unwrap();
        fs::write(config_dir.join("config.toml"), "").unwrap();
        fs::write(legacy.join("xtream-a-b-0123456789abcdef.m3u"), "#EXTM3U\n").unwrap();

        let dirs = CacheDirs::with_legacy_dir(root.join("new-cache"), legacy.clone());
        dirs.tidy();
        // Nothing left to do on later launches.
        dirs.tidy();

        assert!(!legacy.exists());
        assert!(config_dir.join("config.toml").exists());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn tidy_never_treats_the_cache_directory_as_its_own_legacy() {
        let dir = temp_dir("legacy-same");
        fs::create_dir_all(&dir).unwrap();
        let current = path(&dir, "acct");
        fs::write(&current, "#EXTM3U\n").unwrap();

        CacheDirs::with_legacy_dir(dir.clone(), dir.clone()).tidy();

        assert!(current.exists());
        let _ = fs::remove_dir_all(&dir);
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

    /// A temp-file name for `target` as if created by process `pid`.
    fn foreign_tmp(target: &Path, pid: u32) -> PathBuf {
        let mut name = target.file_name().unwrap().to_os_string();
        name.push(format!(".tmp.{pid}.1700000000000000000.0"));
        target.with_file_name(name)
    }

    /// A process id that is not this process's.
    fn other_pid() -> u32 {
        std::process::id().wrapping_add(1)
    }

    #[test]
    fn sweep_removes_temps_abandoned_by_other_processes() {
        // Regression: quitting mid-download (or a crash) skipped the
        // PendingCache drop, and the playlist-sized temp file stayed in
        // the cache directory forever.
        let dir = temp_dir("sweep-abandoned");
        let cache_path = path(&dir, "acct");
        let cache_dir = cache_path.parent().unwrap();
        fs::create_dir_all(cache_dir).unwrap();
        let abandoned = [
            foreign_tmp(&cache_path, other_pid()),
            foreign_tmp(&path(&dir, "other"), other_pid().wrapping_add(1)),
        ];
        for tmp in &abandoned {
            fs::write(tmp, b"#EXTM3U\n").unwrap();
        }

        sweep_stale_temps(cache_dir);

        for tmp in &abandoned {
            assert!(!tmp.exists(), "{} left behind", tmp.display());
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn sweep_keeps_caches_unrelated_files_and_temps_in_use() {
        let dir = temp_dir("sweep-keep");
        let cache_path = path(&dir, "acct");
        let cache_dir = cache_path.parent().unwrap();
        fs::create_dir_all(cache_dir).unwrap();
        fs::write(&cache_path, b"#EXTM3U\n").unwrap();
        // Same shape of name, but not a playlist temp file.
        let unrelated = foreign_tmp(&cache_dir.join("notes.txt"), other_pid());
        fs::write(&unrelated, b"").unwrap();
        // This process's own temps belong to its own (live) loads.
        let own = foreign_tmp(&cache_path, std::process::id());
        fs::write(&own, b"").unwrap();
        // Another viewer still downloading: its temp file is locked.
        let in_use = foreign_tmp(&cache_path, other_pid());
        fs::write(&in_use, b"").unwrap();
        let writer = File::open(&in_use).unwrap();
        writer.try_lock().unwrap();

        sweep_stale_temps(cache_dir);

        for kept in [&cache_path, &unrelated, &own, &in_use] {
            assert!(kept.exists(), "{} was removed", kept.display());
        }
        drop(writer);
        sweep_stale_temps(cache_dir);
        assert!(!in_use.exists(), "unlocked once its writer is gone");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn pending_cache_locks_its_temp_file_against_sweeps() {
        let dir = temp_dir("sweep-pending");
        let pending = PendingCache::create(&path(&dir, "acct")).unwrap();
        let probe = File::open(&pending.tmp).unwrap();
        assert!(
            matches!(probe.try_lock(), Err(TryLockError::WouldBlock)),
            "another viewer's sweep must see the temp file as in use"
        );
        drop(probe);
        drop(pending);
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
