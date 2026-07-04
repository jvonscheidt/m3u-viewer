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
    // Local files stay lenient: plain M3U without the header is accepted.
    let summary = parse_stream(file, total, Header::Optional, tx).map_err(|e| e.to_string())?;
    log::info!("file playlist parsed: {} channels", summary.channels);
    Ok(())
}

fn load_xtream(account: &Account, tx: &Sender<LoadEvent>) -> Result<(), String> {
    let (reader, total) = account.fetch().map_err(|error| error.to_string())?;
    // get.php always answers with extended M3U, so anything else (CDN
    // challenge page, HTML error, panel notice) must abort with a look at
    // the body rather than turn into junk channels or an empty list.
    let summary =
        parse_stream(reader, total, Header::Required, tx).map_err(|error| error.to_string())?;
    if summary.channels == 0 {
        return Err(match summary.first_line {
            Some(line) => format!("server sent a playlist with no channels (starts: {line:?})"),
            None => "server sent an empty response — check that the account is active".to_owned(),
        });
    }
    log::info!("xtream playlist parsed: {} channels", summary.channels);
    Ok(())
}

/// Whether the input must start with the `#EXTM3U` header.
#[derive(Clone, Copy, PartialEq)]
enum Header {
    /// Plain M3U is fine (local files).
    Optional,
    /// Abort early when the first line is not `#EXTM3U` (HTTP responses).
    Required,
}

/// What [`parse_stream`] saw, for post-parse diagnostics.
struct ParseSummary {
    /// Total channels parsed across all batches.
    channels: usize,
    /// First non-blank line of the input (truncated), so an error message
    /// can show what a channel-less response actually contained.
    first_line: Option<String>,
}

/// Streams `input` through the parser, flushing a batch every
/// [`BATCH_SIZE`] channels.
///
/// With [`Header::Required`], input whose first non-blank line is not
/// `#EXTM3U` fails as [`std::io::ErrorKind::InvalidData`] before any
/// batch is sent.
fn parse_stream(
    input: impl Read,
    total_bytes: Option<u64>,
    header: Header,
    tx: &Sender<LoadEvent>,
) -> std::io::Result<ParseSummary> {
    let mut reader = BufReader::with_capacity(256 * 1024, input);
    let mut builder = PlaylistBuilder::new();
    let mut line = String::new();
    let mut bytes_read: u64 = 0;
    let mut groups_sent = 0;
    let mut channels = 0;
    let mut first_line: Option<String> = None;

    loop {
        line.clear();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            break;
        }
        bytes_read += n as u64;
        if first_line.is_none() {
            let trimmed = line.trim_start_matches('\u{feff}').trim();
            if !trimmed.is_empty() {
                let snippet: String = trimmed.chars().take(120).collect();
                if header == Header::Required && !trimmed.starts_with("#EXTM3U") {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("server did not send an M3U playlist; it starts: {snippet:?}"),
                    ));
                }
                first_line = Some(snippet);
            }
        }
        builder.push_line(&line);
        if builder.buffered_channels() >= BATCH_SIZE {
            channels += builder.buffered_channels();
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
    channels += playlist.channels.len();
    let _ = tx.send(LoadEvent::Batch {
        channels: std::mem::take(&mut playlist.channels),
        new_groups: playlist.groups()[groups_sent..].to_vec(),
        skipped: playlist.skipped,
        percent: Some(100),
    });
    Ok(ParseSummary {
        channels,
        first_line,
    })
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

#[cfg(test)]
// unwrap is fine in tests (see CLAUDE.md).
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// One-shot local HTTP server answering 200 OK with `body`.
    fn serve_once(body: &'static str) -> u16 {
        use std::io::Write;

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0_u8; 1024];
            let mut request = Vec::new();
            loop {
                let n = stream.read(&mut buf).unwrap();
                request.extend_from_slice(&buf[..n]);
                if n == 0 || request.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
        });
        port
    }

    /// Drains the loader until the terminal event, returning the total
    /// channel count and the failure message, if any.
    fn drain(rx: &Receiver<LoadEvent>) -> (usize, Option<String>) {
        let mut channels = 0;
        for event in rx {
            match event {
                LoadEvent::Batch {
                    channels: batch, ..
                } => channels += batch.len(),
                LoadEvent::Finished => return (channels, None),
                LoadEvent::Failed(message) => return (channels, Some(message)),
            }
        }
        panic!("loader hung up without a terminal event");
    }

    fn xtream_source(port: u16) -> Source {
        Source::Xtream(Account::new(
            &format!("127.0.0.1:{port}"),
            "u".into(),
            "p".into(),
        ))
    }

    #[test]
    fn xtream_html_response_fails_with_snippet() {
        // Regression: a 200 response that is not a playlist (challenge
        // page, HTML error, …) used to load as junk channels.
        let port = serve_once("<html><body>Access denied</body></html>\n");
        let (channels, error) = drain(&spawn(xtream_source(port)));
        assert_eq!(channels, 0);
        let error = error.unwrap();
        assert!(error.contains("not send an M3U"), "unexpected: {error}");
        assert!(error.contains("<html>"), "snippet missing: {error}");
    }

    #[test]
    fn xtream_empty_response_fails() {
        let port = serve_once("");
        let (channels, error) = drain(&spawn(xtream_source(port)));
        assert_eq!(channels, 0);
        assert!(error.unwrap().contains("empty response"));
    }

    #[test]
    fn xtream_header_only_playlist_fails_but_names_the_header() {
        // An account with zero channels is still a failure worth explaining.
        let port = serve_once("#EXTM3U\n");
        let (channels, error) = drain(&spawn(xtream_source(port)));
        assert_eq!(channels, 0);
        assert!(error.unwrap().contains("#EXTM3U"));
    }

    #[test]
    fn xtream_valid_playlist_still_loads() {
        let port = serve_once("#EXTM3U\n#EXTINF:-1 group-title=\"News\",One\nhttp://u/1\n");
        let (channels, error) = drain(&spawn(xtream_source(port)));
        assert_eq!(channels, 1);
        assert!(error.is_none());
    }

    #[test]
    fn empty_file_still_finishes_without_error() {
        // Only Xtream promotes “no channels” to a failure: opening an empty
        // local file deliberately keeps showing an empty list.
        let dir = std::env::temp_dir().join(format!("m3u-viewer-loader-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("empty.m3u");
        std::fs::write(&path, "#EXTM3U\n").unwrap();
        let (channels, error) = drain(&spawn(Source::File(path)));
        assert_eq!(channels, 0);
        assert!(error.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
