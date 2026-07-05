//! Core library for `m3u-viewer`: playlist parsing, background loading,
//! and the TUI's state and rendering. The binary in `main.rs` only wires
//! these pieces to the terminal.

pub mod app;
mod cache;
pub mod config;
pub mod epg;
pub mod loader;
pub mod player;
pub mod playlist;
pub mod store;
pub mod ui;
pub mod xtream;
