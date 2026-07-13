//! Private filesystem helpers for credential-bearing application data.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;

/// Creates `path`, restricting the final directory to the current user on Unix.
pub(crate) fn create_dir_all(path: &Path) -> io::Result<()> {
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;

        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
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

/// Writes a complete private file, truncating any previous contents.
pub(crate) fn write(path: &Path, contents: &[u8]) -> io::Result<()> {
    let mut file = create(path)?;
    file.write_all(contents)
}

/// Opens a private file after tightening permissions inherited from older versions.
pub(crate) fn open(path: &Path) -> io::Result<File> {
    #[cfg(unix)]
    restrict_file(path)?;
    File::open(path)
}

/// Reads a private UTF-8 file after tightening legacy permissions.
pub(crate) fn read_to_string(path: &Path) -> io::Result<String> {
    #[cfg(unix)]
    restrict_file(path)?;
    fs::read_to_string(path)
}

#[cfg(unix)]
fn restrict_file(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;

    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
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
