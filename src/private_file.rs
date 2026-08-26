//! Private filesystem helpers for credential-bearing application data.
//!
//! Unix permissions are set explicitly; on Windows the application relies on
//! the user-scoped ACLs of the profile's application-data directory.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static TEMP_FILE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Creates `path`, restricting the final directory to the current user on Unix.
pub(crate) fn create_dir_all(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        use std::os::unix::fs::PermissionsExt as _;

        let mut builder = fs::DirBuilder::new();
        builder.recursive(true).mode(0o700).create(path)?;
        // Also tighten a directory inherited from an older version.
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    #[cfg(not(unix))]
    fs::create_dir_all(path)?;
    Ok(())
}

/// Creates or truncates a file with user-only permissions on Unix.
pub(crate) fn create(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;

        options.mode(0o600);
    }
    let file = options.open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;

        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    Ok(file)
}

/// Writes and synchronizes a complete private file, truncating any previous contents.
pub(crate) fn write(path: &Path, contents: &[u8]) -> io::Result<()> {
    let mut file = create(path)?;
    file.write_all(contents)?;
    file.sync_all()
}

/// Returns a sibling temp path unique across concurrent calls and processes.
pub(crate) fn unique_tmp(path: &Path) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    let sequence = TEMP_FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".tmp.{}.{nanos}.{sequence}", std::process::id()));
    path.with_file_name(name)
}

/// Atomically replaces `path` with synchronized `contents`.
pub(crate) fn atomic_write(path: &Path, contents: &[u8]) -> io::Result<()> {
    let tmp = unique_tmp(path);
    let result = write(&tmp, contents).and_then(|()| rename_temp(&tmp, path));
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// Synchronizes and atomically promotes an existing sibling temp file.
pub(crate) fn promote(tmp: &Path, path: &Path) -> io::Result<()> {
    let result = OpenOptions::new()
        .write(true)
        .open(tmp)
        .and_then(|file| file.sync_all())
        .and_then(|()| rename_temp(tmp, path));
    if result.is_err() {
        let _ = fs::remove_file(tmp);
    }
    result
}

fn rename_temp(tmp: &Path, path: &Path) -> io::Result<()> {
    fs::rename(tmp, path)
}

/// Opens a private file, best-effort tightening permissions from older versions.
pub(crate) fn open(path: &Path) -> io::Result<File> {
    #[cfg(unix)]
    tighten_legacy_permissions(path);
    File::open(path)
}

/// Reads a private UTF-8 file, best-effort tightening legacy permissions.
pub(crate) fn read_to_string(path: &Path) -> io::Result<String> {
    #[cfg(unix)]
    tighten_legacy_permissions(path);
    fs::read_to_string(path)
}

#[cfg(unix)]
fn tighten_legacy_permissions(path: &Path) {
    use std::os::unix::fs::PermissionsExt as _;

    if let Err(error) = fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        && error.kind() != io::ErrorKind::NotFound
    {
        log::warn!(
            "could not tighten private file permissions for {}: {error}",
            path.display()
        );
    }
}

#[cfg(test)]
// unwrap is fine in tests (see CLAUDE.md).
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn private_write_round_trips() {
        let dir =
            std::env::temp_dir().join(format!("m3u-viewer-private-file-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        create_dir_all(&dir).unwrap();
        let path = dir.join("secret");

        write(&path, b"credentials").unwrap();

        assert_eq!(read_to_string(&path).unwrap(), "credentials");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn consecutive_temp_paths_do_not_collide() {
        let path = Path::new("config.toml");
        let first = unique_tmp(path);
        let second = unique_tmp(path);

        assert_ne!(first, second);
        assert_eq!(first.parent(), path.parent());
        assert_eq!(second.parent(), path.parent());
    }

    #[test]
    fn atomic_write_replaces_complete_contents() {
        let dir =
            std::env::temp_dir().join(format!("m3u-viewer-atomic-file-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        create_dir_all(&dir).unwrap();
        let path = dir.join("state");

        atomic_write(&path, b"first").unwrap();
        atomic_write(&path, b"second").unwrap();

        assert_eq!(read_to_string(&path).unwrap(), "second");
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
        let _ = fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn files_and_directories_are_user_only() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir =
            std::env::temp_dir().join(format!("m3u-viewer-private-mode-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        create_dir_all(&dir).unwrap();
        let path = dir.join("secret");
        write(&path, b"credentials").unwrap();

        assert_eq!(
            fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );

        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(read_to_string(&path).unwrap(), "credentials");
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let _ = fs::remove_dir_all(dir);
    }
}
