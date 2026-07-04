//! Core library for `m3u-viewer`: playlist parsing, background loading,
//! and the TUI's state and rendering. The binary in `main.rs` only wires
//! these pieces to the terminal.

pub mod app;
pub mod loader;
pub mod playlist;
pub mod ui;
