//! External playback: locating a VLC executable and launching streams in
//! it as a detached process, so the viewer keeps running.

use std::env;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;

use thiserror::Error;

/// Why a stream could not be handed to VLC.
#[derive(Debug, Error)]
pub enum PlayerError {
    /// No usable VLC executable was found during discovery.
    #[error("VLC not found — checked --vlc, PATH, and standard install folders")]
    NotFound,
    /// A `--vlc` override was given but does not point at an executable.
    #[error("--vlc path is not a file: {0}")]
    BadOverride(PathBuf),
    /// VLC was found but could not be started.
    #[error("failed to launch VLC: {0}")]
    Spawn(#[from] std::io::Error),
}

/// A resolved external player.
pub struct Player {
    exe: PathBuf,
}

impl Player {
    /// Locates VLC: an explicit `--vlc` override wins, then `vlc` on
    /// `PATH`, then the platform's standard install locations.
    ///
    /// # Errors
    ///
    /// [`PlayerError::BadOverride`] if `override_path` is set but not a
    /// file; [`PlayerError::NotFound`] if discovery comes up empty.
    pub fn discover(override_path: Option<&Path>) -> Result<Self, PlayerError> {
        if let Some(path) = override_path {
            return if path.is_file() {
                log::info!("using VLC override: {}", path.display());
                Ok(Self {
                    exe: path.to_path_buf(),
                })
            } else {
                log::warn!("VLC override is not a file: {}", path.display());
                Err(PlayerError::BadOverride(path.to_path_buf()))
            };
        }
        let path_dirs = env::var_os("PATH")
            .map(|paths| env::split_paths(&paths).collect::<Vec<_>>())
            .unwrap_or_default();
        let result = find_executable(path_dirs.iter().map(PathBuf::as_path))
            .or_else(|| find_executable(standard_dirs().iter().map(PathBuf::as_path)))
            .map(|exe| Self { exe })
            .ok_or(PlayerError::NotFound);
        match &result {
            Ok(player) => log::info!("VLC found: {}", player.exe.display()),
            Err(_) => log::warn!("VLC not found"),
        }
        result
    }

    /// Full path of the executable that will be launched.
    #[must_use]
    pub fn exe(&self) -> &Path {
        &self.exe
    }

    /// Launches `url` in VLC, detached: output is discarded and a reaper
    /// thread waits on the child so the viewer neither blocks nor leaves
    /// zombies behind.
    ///
    /// # Errors
    ///
    /// [`PlayerError::Spawn`] if the process cannot be started.
    pub fn play(&self, url: &str) -> Result<(), PlayerError> {
        log::info!("launching VLC for playback");
        let mut child = Command::new(&self.exe)
            .arg(url)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        thread::spawn(move || {
            let _ = child.wait();
        });
        Ok(())
    }
}

/// First directory in `dirs` containing a VLC executable name.
fn find_executable<'a>(dirs: impl Iterator<Item = &'a Path>) -> Option<PathBuf> {
    let names: &[&str] = if cfg!(windows) {
        &["vlc.exe"]
    } else {
        &["vlc"]
    };
    for dir in dirs {
        for name in names {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// Standard VLC install directories for the current platform.
fn standard_dirs() -> Vec<PathBuf> {
    if cfg!(windows) {
        ["ProgramFiles", "ProgramFiles(x86)"]
            .iter()
            .filter_map(env::var_os)
            .map(|root| PathBuf::from(root).join("VideoLAN").join("VLC"))
            .collect()
    } else if cfg!(target_os = "macos") {
        vec![PathBuf::from("/Applications/VLC.app/Contents/MacOS")]
    } else {
        vec![
            PathBuf::from("/usr/bin"),
            PathBuf::from("/usr/local/bin"),
            PathBuf::from("/snap/bin"),
        ]
    }
}

#[cfg(test)]
// unwrap is fine in tests (see CLAUDE.md).
#[allow(clippy::unwrap_used)]
mod tests {
    use std::fs;

    use super::*;

    /// Creates a unique temp dir containing a fake VLC executable.
    fn fake_vlc_dir(tag: &str) -> PathBuf {
        let dir = env::temp_dir().join(format!("m3u-viewer-test-{tag}-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let name = if cfg!(windows) { "vlc.exe" } else { "vlc" };
        fs::write(dir.join(name), b"").unwrap();
        dir
    }

    #[test]
    fn override_must_point_at_a_file() {
        let missing = Path::new("Z:/definitely/not/here/vlc.exe");
        assert!(matches!(
            Player::discover(Some(missing)),
            Err(PlayerError::BadOverride(_))
        ));
    }

    #[test]
    fn override_wins_when_valid() {
        let dir = fake_vlc_dir("override");
        let exe = dir.join(if cfg!(windows) { "vlc.exe" } else { "vlc" });
        let player = Player::discover(Some(&exe)).unwrap();
        assert_eq!(player.exe(), exe);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn finds_executable_in_candidate_dirs() {
        let dir = fake_vlc_dir("dirs");
        let missing = env::temp_dir().join("m3u-viewer-test-empty-nonexistent");
        let found = find_executable([missing.as_path(), dir.as_path()].into_iter());
        assert_eq!(found.unwrap().parent().unwrap(), dir);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn missing_everywhere_is_none() {
        let missing = env::temp_dir().join("m3u-viewer-test-empty-nonexistent");
        assert!(find_executable(std::iter::once(missing.as_path())).is_none());
    }
}
