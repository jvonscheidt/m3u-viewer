//! Background playlist loading.
//!
//! [`spawn`] starts a thread that streams a playlist — from a local file
//! or straight from an Xtream Codes server — through [`PlaylistBuilder`],
//! sending [`LoadEvent`]s over an mpsc channel so the UI can appear
//! immediately and fill in while the data is still arriving.

use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread;

use crate::playlist::{Channel, PlaylistBuilder};
use crate::xtream::Account;

/// Channels per [`LoadEvent::Batch`]; small enough for a responsive first
/// paint, large enough to keep channel overhead negligible.
const BATCH_SIZE: usize = 4096;

/// Where the playlist comes from.
pub enum Source {
    /// A local `.m3u`/`.m3u8` file.
    File(PathBuf),
    /// An Xtream Codes account (playlist downloaded via `get.php`).
    Xtream(Account),
}

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
        /// Rough progress, 0–100; `None` when the total size is unknown
        /// (e.g. a chunked HTTP response).
        percent: Option<u8>,
    },
    /// The whole playlist was parsed successfully.
    Finished,
    /// Loading aborted (I/O error, HTTP failure, bad credentials, …).
    Failed(String),
}

/// Spawns the loader thread for `source` and returns the event receiver.
///
/// The thread finishes on its own; failures are reported as
/// [`LoadEvent::Failed`] rather than panics.
#[must_use]
pub fn spawn(source: Source) -> Receiver<LoadEvent> {
    let (tx, rx) = channel();
    thread::spawn(move || {
        let result = match source {
            Source::File(path) => {
                log::info!("loading playlist from file: {}", path.display());
                load_file(&path, &tx)
            }
            Source::Xtream(account) => {
                log::info!("loading Xtream playlist: {}", account.display_name());
                load_xtream(&account, &tx)
            }
        };
        // A send failure just means the UI is gone; nothing left to do.
        let _ = match result {
            Ok(()) => {
                log::info!("playlist loading complete");
                tx.send(LoadEvent::Finished)
            }
            Err(message) => {
                log::error!("playlist loading failed: {message}");
                tx.send(LoadEvent::Failed(message))
            }
        };
    });
    rx
}

fn load_file(path: &Path, tx: &Sender<LoadEvent>) -> Result<(), String> {
    let file = File::open(path).map_err(|error| error.to_string())?;
    let total = file.metadata().map(|meta| meta.len()).ok();
    parse_stream(file, total, tx).map_err(|error| error.to_string())
}

fn load_xtream(account: &Account, tx: &Sender<LoadEvent>) -> Result<(), String> {
    let (reader, total) = account.fetch().map_err(|error| error.to_string())?;
    parse_stream(reader, total, tx).map_err(|error| error.to_string())
}

/// Streams `input` through the parser, flushing a batch every
/// [`BATCH_SIZE`] channels.
fn parse_stream(
    input: impl Read,
    total_bytes: Option<u64>,
    tx: &Sender<LoadEvent>,
) -> std::io::Result<()> {
    let mut reader = BufReader::with_capacity(256 * 1024, input);
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
        percent: Some(100),
    });
    Ok(())
}

/// Sends the currently buffered channels and any newly seen groups.
fn flush(
    builder: &mut PlaylistBuilder,
    groups_sent: &mut usize,
    percent: Option<u8>,
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

/// Integer progress percentage, clamped to 0–100; `None` without a total.
fn percent(read: u64, total: Option<u64>) -> Option<u8> {
    let total = total.filter(|&t| t > 0)?;
    Some(u8::try_from((read.saturating_mul(100) / total).min(100)).unwrap_or(100))
}
