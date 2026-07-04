//! Persistent configuration stored as TOML.
//!
//! [`Config::load`] reads `config.toml` from the platform config directory
//! (the same directory that [`crate::store::Store`] uses for favorites and
//! recents), returning [`Config::default`] when the file does not exist yet.
//! CLI arguments always take precedence over stored values.
//!
//! **Security note:** Xtream credentials are written in plaintext. The file
//! lives in the user's private config directory; treat it accordingly.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

const CONFIG_FILE: &str = "config.toml";

/// Top-level configuration file structure.
#[derive(Default, Serialize, Deserialize)]
pub struct Config {
    /// Stored Xtream Codes account credentials.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub xtream: Option<XtreamConfig>,
    /// Path to the VLC executable, overriding auto-detection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vlc_path: Option<PathBuf>,
}

/// Xtream Codes credentials stored in the config file.
///
/// Not `Debug`-derived to prevent accidental password exposure in logs or
/// error messages.
#[derive(Serialize, Deserialize)]
pub struct XtreamConfig {
    /// Provider base URL, e.g. `http://provider.example:8080`.
    pub server: String,
    /// Account username.
    pub username: String,
    /// Account password (stored in plaintext).
    pub password: String,
}

/// Errors that can occur while loading or saving the config file.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// Reading the config file failed.
    #[error("could not read config: {0}")]
    Read(#[from] std::io::Error),
    /// The config file is not valid TOML.
    #[error("could not parse config: {0}")]
    Parse(#[from] toml::de::Error),
    /// Writing the config file failed.
    #[error("could not save config: {0}")]
    Write(String),
}

impl Config {
    /// Loads config from `path`, returning [`Config::default`] when the file
    /// does not exist.
    ///
    /// # Errors
    ///
    /// [`ConfigError`] if the file exists but cannot be read or parsed.
    #[must_use = "the loaded config is not used"]
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = fs::read_to_string(path)?;
        Ok(toml::from_str(&text)?)
    }

    /// Saves the config to `path` atomically (temp file → rename).
    ///
    /// The parent directory is created if it does not exist.
    ///
    /// # Errors
    ///
    /// [`ConfigError::Write`] if serialisation or any I/O step fails.
    pub fn save(&self, path: &Path) -> Result<(), ConfigError> {
        let text = toml::to_string_pretty(self).map_err(|e| ConfigError::Write(e.to_string()))?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| ConfigError::Write(e.to_string()))?;
        }
        let tmp = unique_tmp(path);
        let write_and_rename = fs::write(&tmp, &text).and_then(|()| fs::rename(&tmp, path));
        if let Err(e) = write_and_rename {
            let _ = fs::remove_file(&tmp);
            return Err(ConfigError::Write(e.to_string()));
        }
        Ok(())
    }

    /// Returns the default config file path, or `None` on platforms without a
    /// per-user config directory.
    #[must_use]
    pub fn default_path() -> Option<PathBuf> {
        directories::ProjectDirs::from("", "", "m3u-viewer")
            .map(|dirs| dirs.config_dir().join(CONFIG_FILE))
    }
}

/// A sibling temp path unique to this process and moment, so concurrent
/// writers cannot clobber each other's temp file before their renames.
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
    use std::fs;

    use super::*;

    fn temp_path(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("m3u-viewer-config-{tag}-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        dir.join(CONFIG_FILE)
    }

    #[test]
    fn round_trip_xtream_and_vlc() {
        let path = temp_path("rt");
        let config = Config {
            xtream: Some(XtreamConfig {
                server: "http://example.com:8080".to_owned(),
                username: "user".to_owned(),
                password: "s3cr3t".to_owned(),
            }),
            vlc_path: Some(PathBuf::from("/usr/bin/vlc")),
        };
        config.save(&path).unwrap();

        let loaded = Config::load(&path).unwrap();
        let xtream = loaded.xtream.unwrap();
        assert_eq!(xtream.server, "http://example.com:8080");
        assert_eq!(xtream.username, "user");
        assert_eq!(xtream.password, "s3cr3t");
        assert_eq!(loaded.vlc_path, Some(PathBuf::from("/usr/bin/vlc")));

        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn missing_file_returns_default() {
        let path = std::env::temp_dir().join("m3u-viewer-config-nonexistent-99999.toml");
        let config = Config::load(&path).unwrap();
        assert!(config.xtream.is_none());
        assert!(config.vlc_path.is_none());
    }

    #[test]
    fn corrupt_file_returns_error() {
        let path = temp_path("corrupt");
        fs::write(&path, "not valid toml [[[").unwrap();
        assert!(Config::load(&path).is_err());
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn partial_config_round_trips() {
        let path = temp_path("partial");
        let config = Config {
            xtream: None,
            vlc_path: Some(PathBuf::from("C:/tools/vlc.exe")),
        };
        config.save(&path).unwrap();
        let loaded = Config::load(&path).unwrap();
        assert!(loaded.xtream.is_none());
        assert_eq!(loaded.vlc_path, Some(PathBuf::from("C:/tools/vlc.exe")));
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }
}
