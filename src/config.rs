//! Persistent configuration stored as TOML.
//!
//! [`Config::load`] reads `config.toml` from the platform config directory
//! (the same directory that [`crate::store::Store`] uses for favorites and
//! recents), returning [`Config::default`] when the file does not exist yet.
//! CLI arguments always take precedence over stored values.
//!
//! **Security note:** Xtream credentials are written in plaintext. Persistent
//! files are protected by the user's filesystem access controls and use mode
//! `0600` on Unix, but they are not encrypted.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::private_file;

const CONFIG_FILE: &str = "config.toml";

/// Top-level configuration file structure.
#[derive(Serialize, Deserialize)]
pub struct Config {
    /// Stored Xtream Codes account credentials.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub xtream: Option<XtreamConfig>,
    /// Path to the VLC executable, overriding auto-detection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vlc_path: Option<PathBuf>,
    /// `User-Agent` header for Xtream requests; some providers only
    /// answer to known player user agents.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_agent: Option<String>,
    /// XMLTV guide source (HTTP(S) URL or file path) used when `--epg` is
    /// not given on the command line. Takes precedence over a `url-tvg`
    /// header and the Xtream account's own guide.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub epg_url: Option<String>,
    /// Whether the channel filter (`/`) treats its input as a regular
    /// expression, falling back to plain substring matching when the
    /// pattern fails to compile. Enabled by default; set to `false` to
    /// always use plain substring matching.
    #[serde(default = "default_regex_filter")]
    pub regex_filter: bool,
    /// Whether playing a channel reuses a single running VLC instance
    /// (via `--one-instance`) instead of opening a new window per channel.
    /// Disabled by default.
    #[serde(default)]
    pub vlc_reuse_instance: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            xtream: None,
            vlc_path: None,
            user_agent: None,
            epg_url: None,
            regex_filter: default_regex_filter(),
            vlc_reuse_instance: false,
        }
    }
}

fn default_regex_filter() -> bool {
    true
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
        let text = private_file::read_to_string(path)?;
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
            private_file::create_dir_all(parent).map_err(|e| ConfigError::Write(e.to_string()))?;
        }
        let tmp = unique_tmp(path);
        let write_and_rename =
            private_file::write(&tmp, text.as_bytes()).and_then(|()| fs::rename(&tmp, path));
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
            user_agent: Some("VLC/3.0.20".to_owned()),
            epg_url: Some("http://example.com/epg.xml.gz".to_owned()),
            regex_filter: false,
            vlc_reuse_instance: true,
        };
        config.save(&path).unwrap();

        let loaded = Config::load(&path).unwrap();
        let xtream = loaded.xtream.unwrap();
        assert_eq!(xtream.server, "http://example.com:8080");
        assert_eq!(xtream.username, "user");
        assert_eq!(xtream.password, "s3cr3t");
        assert_eq!(loaded.vlc_path, Some(PathBuf::from("/usr/bin/vlc")));
        assert_eq!(loaded.user_agent, Some("VLC/3.0.20".to_owned()));
        assert_eq!(
            loaded.epg_url,
            Some("http://example.com/epg.xml.gz".to_owned())
        );
        assert!(!loaded.regex_filter);
        assert!(loaded.vlc_reuse_instance);

        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn missing_file_returns_default() {
        let path = std::env::temp_dir().join("m3u-viewer-config-nonexistent-99999.toml");
        let config = Config::load(&path).unwrap();
        assert!(config.xtream.is_none());
        assert!(config.vlc_path.is_none());
        assert!(config.regex_filter);
        assert!(!config.vlc_reuse_instance);
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
            vlc_path: Some(PathBuf::from("C:/tools/vlc.exe")),
            ..Config::default()
        };
        config.save(&path).unwrap();
        let loaded = Config::load(&path).unwrap();
        assert!(loaded.xtream.is_none());
        assert_eq!(loaded.vlc_path, Some(PathBuf::from("C:/tools/vlc.exe")));
        assert!(loaded.user_agent.is_none());
        assert!(loaded.epg_url.is_none());
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn config_written_before_vlc_reuse_instance_existed_defaults_to_disabled() {
        let path = temp_path("pre-reuse-instance");
        fs::write(&path, "vlc_path = \"/usr/bin/vlc\"\n").unwrap();
        let loaded = Config::load(&path).unwrap();
        assert!(!loaded.vlc_reuse_instance);
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn config_written_before_regex_filter_existed_defaults_to_enabled() {
        let path = temp_path("pre-regex");
        fs::write(&path, "vlc_path = \"/usr/bin/vlc\"\n").unwrap();
        let loaded = Config::load(&path).unwrap();
        assert!(loaded.regex_filter);
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }
}
