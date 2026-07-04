//! Background playlist loading.
//!
//! [`spawn`] starts a thread that streams a playlist file through
//! [`PlaylistBuilder`], sending [`LoadEvent`]s over an mpsc channel so the
//! UI can appear immediately and fill in while the file is still parsing.

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread;

use crate::playlist::{Channel, PlaylistBuilder};

/// Channels per [`LoadEvent::Batch`]; small enough for a responsive first
/// paint, large enough to keep channel overhead negligible.
const BATCH_SIZE: usize = 4096;

/// Progress message from the loader thread to the UI.
pub enum LoadEvent {
    /// A batch of parsed channels.
    Batch {
        /// Channels parsed since the previous batch.
        channels: Vec<Channel>,
        /// Group names interned since the previous batch, in id order:
        /// appending them to the receiver's group table keeps the
        /// [`Channel::group`] ids in `channels` valid.
        new_groups: Vec<String>,
        /// Total malformed entries skipped so far (cumulative).
        skipped: usize,
        /// Rough progress through the file, 0–100.
        percent: u8,
    },
    /// The whole file was parsed successfully.
    Finished,
    /// Loading aborted (I/O error, unreadable file, …).
    Failed(String),
}

/// Spawns the loader thread for `path` and returns the event receiver.
///
/// The thread finishes on its own; failures are reported as
/// [`LoadEvent::Failed`] rather than panics.
#[must_use]
pub fn spawn(path: PathBuf) -> Receiver<LoadEvent> {
    let (tx, rx) = channel();
    thread::spawn(move || {
        let result = load(&path, &tx);
        // A send failure just means the UI is gone; nothing left to do.
        let _ = match result {
            Ok(()) => tx.send(LoadEvent::Finished),
            Err(err) => tx.send(LoadEvent::Failed(err.to_string())),
        };
    });
    rx
}

/// Streams `path` through the parser, flushing a batch every
/// [`BATCH_SIZE`] channels.
fn load(path: &Path, tx: &Sender<LoadEvent>) -> std::io::Result<()> {
    let file = File::open(path)?;
    let total_bytes = file.metadata()?.len();
    let mut reader = BufReader::with_capacity(256 * 1024, file);
    let mut builder = PlaylistBuilder::new();
    let mut line = String::new();
    let mut bytes_read: u64 = 0;
    let mut groups_sent = 0;

    loop {
        line.clear();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            break;
        }
        bytes_read += n as u64;
        builder.push_line(&line);
        if builder.buffered_channels() >= BATCH_SIZE {
            flush(
                &mut builder,
                &mut groups_sent,
                percent(bytes_read, total_bytes),
                tx,
            );
        }
    }

    // finish() folds a trailing URL-less #EXTINF into the skipped count, so
    // always send the tail batch even when it carries no channels.
    let mut playlist = builder.finish();
    let _ = tx.send(LoadEvent::Batch {
        channels: std::mem::take(&mut playlist.channels),
        new_groups: playlist.groups()[groups_sent..].to_vec(),
        skipped: playlist.skipped,
        percent: 100,
    });
    Ok(())
}

/// Sends the currently buffered channels and any newly seen groups.
fn flush(
    builder: &mut PlaylistBuilder,
    groups_sent: &mut usize,
    percent: u8,
    tx: &Sender<LoadEvent>,
) {
    let new_groups = builder.groups()[*groups_sent..].to_vec();
    *groups_sent = builder.groups().len();
    let _ = tx.send(LoadEvent::Batch {
        channels: builder.drain_channels(),
        new_groups,
        skipped: builder.skipped(),
        percent,
    });
}

/// Integer progress percentage, clamped to 0–100.
fn percent(read: u64, total: u64) -> u8 {
    read.saturating_mul(100)
        .checked_div(total)
        .map_or(100, |p| u8::try_from(p.min(100)).unwrap_or(100))
}
