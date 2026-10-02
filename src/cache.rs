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
//!
//! # File names and superseded caches
//!
//! A cached playlist is named `xtream-<account>-<key>.m3u`: `<key>` is
//! [`Account::cache_key`] (readable host and username plus a hash that
//! also covers the password), `<account>` a separate 16-hex-digit hash of
//! the unsanitized host and username alone. After a password change the
//! account gets a new `<key>`, and the cache under the old one is useless
//! — its stream URLs embed the old password. [`PendingCache::commit`]
//! therefore deletes the account's other caches; see [`prune_superseded`]
//! for why that never hits another account's cache.

use std::fs::{self, File, TryLockError};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::private_file;
use crate::xtream::Account;

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

/// Where `account`'s cached playlist lives in the cache directory
/// ([`CacheDirs::dir`]): `xtream-<account>-<key>.m3u`, see the
/// [module docs](self#file-names-and-superseded-caches).
#[must_use]
pub fn path(cache_dir: &Path, account: &Account) -> PathBuf {
    cache_dir.join(format!(
        "{FILE_PREFIX}{:016x}-{}{FILE_SUFFIX}",
        account_hash(account),
        account.cache_key()
    ))
}

/// Stable non-cryptographic (FNV-1a) hash of the unsanitized host and
/// username — the account's identity without its password, so it stays
/// the same across password changes. Hashed like the identity part of
/// [`Account::cache_key`]: scheme stripped from the server URL, fields
/// NUL-separated.
fn account_hash(account: &Account) -> u64 {
    const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;

    let (server, username, _) = account.credentials();
    let host = server.split_once("://").map_or(server, |(_, rest)| rest);
    host.bytes()
        .chain(std::iter::once(0))
        .chain(username.bytes())
        .fold(OFFSET_BASIS, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(PRIME)
        })
}

/// Whether `name` is the file name of a cached playlist (not a temp file).
fn is_cache_file_name(name: &str) -> bool {
    name.starts_with(FILE_PREFIX) && name.ends_with(FILE_SUFFIX)
}

/// The parts of a cached playlist's file name that identify its account.
#[derive(Debug, PartialEq, Eq)]
struct AccountFile<'a> {
    /// `<account>`: hash of the unsanitized host and username.
    account_hash: &'a str,
    /// `<key>` without its trailing hash: the sanitized host and username.
    readable: &'a str,
}

impl<'a> AccountFile<'a> {
    /// Parses `xtream-<16 hex>-<readable>-<16 hex>.m3u`; `None` for any
    /// other name, including temp files and pre-0.9.2 names.
    fn parse(name: &'a str) -> Option<Self> {
        let is_hash = |text: &str| text.len() == 16 && text.bytes().all(|b| b.is_ascii_hexdigit());
        let stem = name.strip_prefix(FILE_PREFIX)?.strip_suffix(FILE_SUFFIX)?;
        let (account_hash, key) = stem.split_once('-')?;
        let (readable, credentials_hash) = key.rsplit_once('-')?;
        (is_hash(account_hash) && is_hash(credentials_hash) && !readable.is_empty()).then_some(
            Self {
                account_hash,
                readable,
            },
        )
    }
}

/// Deletes the other caches of the account whose fresh cache was just
/// committed at `path` — copies keyed on an older password, full of
/// stream URLs that no longer play and that still embed it.
///
/// A file counts as the same account only when its name parses (see
/// [`AccountFile::parse`]) and both its account hash and its readable
/// host-and-username part equal `path`'s. The readable part alone is not
/// enough: sanitizing maps different accounts (`a.b`/`u-1` and `a_b`/`u_1`)
/// to the same text, and those differ in the account hash, which covers
/// the unsanitized host and username. Two different accounts would need
/// both an identical sanitized name and a 64-bit hash collision.
/// Temp files, unparsable names, and legacy-format names are never
/// touched. Best-effort, like everything here.
fn prune_superseded(path: &Path) {
    let (Some(dir), Some(own_name)) = (path.parent(), path.file_name().and_then(|n| n.to_str()))
    else {
        return;
    };
    let Some(own) = AccountFile::parse(own_name) else {
        return;
    };
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let Some(name) = file_name.to_str() else {
            continue;
        };
        if name != own_name && AccountFile::parse(name).as_ref() == Some(&own) {
            let superseded = entry.path();
            match fs::remove_file(&superseded) {
                Ok(()) => log::info!("removed superseded cache {}", superseded.display()),
                Err(error) => log::debug!("could not remove {}: {error}", superseded.display()),
            }
        }
    }
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
    /// previous cache stays untouched. Once replaced, the account's caches
    /// under older passwords are deleted ([`prune_superseded`]). Returns
    /// whether the cache was replaced.
    pub fn commit(mut self) -> bool {
        let Some(file) = self.file.take() else {
            return false;
        };
        // Close the handle first: Windows cannot rename an open file.
        drop(file);
        // promote() removes the temp file itself when it fails.
        self.committed = private_file::promote(&self.tmp, &self.path).is_ok();
        if self.committed {
            prune_superseded(&self.path);
        }
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

    /// An account on `example.com` (never contacted) with `username`.
    fn account(username: &str) -> Account {
        Account::new("example.com", username.into(), "p".into())
    }

    #[test]
    fn path_names_the_account_and_its_cache_key() {
        let dir = PathBuf::from("C:/cache");
        let account = account("user");
        let cache_path = path(&dir, &account);
        assert_eq!(cache_path.parent(), Some(dir.as_path()));
        let name = cache_path.file_name().unwrap().to_str().unwrap();
        let key = account.cache_key();
        let hash = name
            .strip_prefix("xtream-")
            .and_then(|rest| rest.strip_suffix(&format!("-{key}.m3u")))
            .unwrap();
        assert_eq!(hash, format!("{:016x}", account_hash(&account)));
        assert_eq!(
            AccountFile::parse(name),
            Some(AccountFile {
                account_hash: hash,
                readable: "example_com-user",
            })
        );
    }

    #[test]
    fn account_hash_ignores_the_password_and_scheme_only() {
        let base = account_hash(&Account::new("example.com", "u".into(), "one".into()));
        for same in [
            Account::new("example.com", "u".into(), "two".into()),
            Account::new("http://example.com", "u".into(), "one".into()),
        ] {
            assert_eq!(account_hash(&same), base);
        }
        for other in [
            Account::new("example.org", "u".into(), "one".into()),
            Account::new("example.com", "v".into(), "one".into()),
            // Same bytes, split differently between host and username.
            Account::new("example.co", "mu".into(), "one".into()),
        ] {
            assert_ne!(account_hash(&other), base);
        }
    }

    #[test]
    fn only_current_format_names_parse_as_account_files() {
        for name in [
            "xtream-example_com-u-0123456789abcdef.m3u",
            "xtream-0123456789abcdef-example_com-u.m3u",
            "xtream-0123456789abcdef--0123456789abcdef.m3u",
            "xtream-0123456789abcdef-example_com-u-0123456789abcdef.m3u.tmp.1.2.3",
            "other-0123456789abcdef-example_com-u-0123456789abcdef.m3u",
        ] {
            assert_eq!(AccountFile::parse(name), None, "{name}");
        }
    }

    #[test]
    fn commit_deletes_the_same_accounts_caches_under_older_passwords() {
        // Regression: after a password change (which re-keys the cache) the
        // old cache, full of stream URLs embedding the old password, stayed
        // on disk forever.
        let dir = temp_dir("prune");
        fs::create_dir_all(&dir).unwrap();
        let old_passwords = ["old-1", "old-2"].map(|password| {
            path(
                &dir,
                &Account::new("example.com", "u".into(), password.into()),
            )
        });
        for old in &old_passwords {
            fs::write(old, "#EXTM3U\n").unwrap();
        }
        let current = Account::new("https://example.com", "u".into(), "new".into());
        let cache_path = path(&dir, &current);

        let mut pending = PendingCache::create(&cache_path).unwrap();
        pending.write(b"#EXTM3U\n");
        assert!(pending.commit());

        assert!(cache_path.exists());
        for old in &old_passwords {
            assert!(!old.exists(), "{} left behind", old.display());
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn commit_keeps_other_accounts_even_with_the_same_readable_name() {
        // Review note: sanitizing maps "a.b.com"/"u-1" and "a_b_com"/"u_1"
        // to the same readable name; pruning by that prefix alone would
        // delete the other account's cache.
        let dir = temp_dir("prune-keep");
        fs::create_dir_all(&dir).unwrap();
        let lookalike = path(&dir, &Account::new("a_b_com", "u_1".into(), "p".into()));
        let other_user = path(&dir, &Account::new("a.b.com", "u-2".into(), "p".into()));
        let legacy_name = dir.join("xtream-a_b_com-u_1-0123456789abcdef.m3u");
        let unrelated = dir.join("notes.txt");
        let current = Account::new("a.b.com", "u-1".into(), "p".into());
        let cache_path = path(&dir, &current);
        let in_flight = foreign_tmp(&cache_path, other_pid());
        for kept in [
            &lookalike,
            &other_user,
            &legacy_name,
            &unrelated,
            &in_flight,
        ] {
            fs::write(kept, "#EXTM3U\n").unwrap();
        }
        let readable = |path: &Path| {
            let name = path.file_name().unwrap().to_str().unwrap().to_owned();
            AccountFile::parse(&name).unwrap().readable.to_owned()
        };
        assert_eq!(readable(&lookalike), readable(&cache_path));

        let mut pending = PendingCache::create(&cache_path).unwrap();
        pending.write(b"#EXTM3U\n");
        assert!(pending.commit());

        for kept in [
            &lookalike,
            &other_user,
            &legacy_name,
            &unrelated,
            &in_flight,
        ] {
            assert!(kept.exists(), "{} was removed", kept.display());
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn failed_commit_prunes_nothing() {
        let dir = temp_dir("prune-failed");
        fs::create_dir_all(&dir).unwrap();
        let old = path(&dir, &Account::new("example.com", "u".into(), "old".into()));
        fs::write(&old, "#EXTM3U\n").unwrap();
        let cache_path = path(&dir, &Account::new("example.com", "u".into(), "new".into()));

        let mut pending = PendingCache::failing_for_test(&cache_path);
        pending.write(b"#EXTM3U\n");
        assert!(!pending.commit());

        assert!(old.exists(), "the only usable cache must survive");
        let _ = fs::remove_dir_all(&dir);
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
        let current = path(&dir, &account("acct"));
        fs::write(&current, "#EXTM3U\n").unwrap();

        CacheDirs::with_legacy_dir(dir.clone(), dir.clone()).tidy();

        assert!(current.exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_cache_opens_as_none() {
        let dir = temp_dir("missing");
        assert!(open(&path(&dir, &account("acct"))).is_none());
    }

    #[test]
    fn create_write_commit_and_reopen_round_trips() {
        let dir = temp_dir("roundtrip");
        let cache_path = path(&dir, &account("acct"));
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
        let cache_path = path(&dir, &account("acct"));
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
        let cache_path = path(&dir, &account("acct"));
        let cache_dir = cache_path.parent().unwrap();
        fs::create_dir_all(cache_dir).unwrap();
        let abandoned = [
            foreign_tmp(&cache_path, other_pid()),
            foreign_tmp(&path(&dir, &account("other")), other_pid().wrapping_add(1)),
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
        let cache_path = path(&dir, &account("acct"));
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
        let pending = PendingCache::create(&path(&dir, &account("acct"))).unwrap();
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
        let cache_path = path(&dir, &account("acct"));
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
