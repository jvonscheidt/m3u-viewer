//! Core library for `m3u-viewer`: playlist parsing, background loading,
//! and the TUI's state and rendering. The binary in `main.rs` only wires
//! these pieces to the terminal.
//!
//! # Example
//!
//! ```
//! use m3u_viewer::playlist::Playlist;
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let input = b"#EXTM3U\n#EXTINF:-1 group-title=\"News\",World News\nhttp://example/news\n";
//! let playlist = Playlist::from_reader(&input[..])?;
//!
//! assert_eq!(playlist.channels()[0].name(), "World News");
//! assert_eq!(
//!     playlist.channels()[0]
//!         .group()
//!         .and_then(|id| playlist.group_name(id)),
//!     Some("News")
//! );
//! # Ok(())
//! # }
//! ```

pub mod app;
mod cache;
pub mod config;
pub mod epg;
pub mod loader;
pub mod player;
pub mod playlist;
mod private_file;
pub mod store;
pub mod ui;
pub mod xtream;
