//! Binary entry point: parse the playlist named on the command line and
//! print a summary. Temporary CLI until the TUI lands.

use std::fs::File;
use std::io::BufReader;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use m3u_viewer::playlist::Playlist;

fn main() -> Result<()> {
    let Some(path) = std::env::args_os().nth(1).map(PathBuf::from) else {
        bail!("usage: m3u-viewer <playlist.m3u>");
    };
    let file = File::open(&path).with_context(|| format!("cannot open {}", path.display()))?;
    let playlist = Playlist::from_reader(BufReader::new(file))
        .with_context(|| format!("failed to parse {}", path.display()))?;
    println!(
        "{} channels in {} groups ({} malformed entries skipped)",
        playlist.channels.len(),
        playlist.groups().len(),
        playlist.skipped
    );
    Ok(())
}
