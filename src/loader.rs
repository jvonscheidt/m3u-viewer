//! Background playlist loading.
//!
//! [`spawn`] starts a thread that streams a playlist — from a local file
//! or straight from an Xtream Codes server — through [`PlaylistBuilder`],
//! sending [`LoadEvent`]s over an mpsc channel so the UI can appear
//! immediately and fill in while the data is still arriving.
//!
//! For Xtream sources, [`load_xtream`] additionally shows a cached copy of
//! the last successful load first (if one exists in `cache_dir`), so the
//! list is populated instantly instead of waiting on the network; the
//! live fetch then runs as usual and, on arriving at its first real
//! batch, a [`LoadEvent::Reset`] clears the cached rows before the fresh
//! ones replace them. See [`crate::cache`] for the on-disk side of this.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread;

use crate::cache;
use crate::playlist::{Channel, GroupId, PlaylistBuilder};
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
    /// Discards everything loaded so far. Sent only when a cached
    /// playlist was shown first and the live fetch has now reached its
    /// first real batch of fresh data, replacing it.
    Reset,
    /// The whole playlist was parsed successfully.
    Finished,
    /// Loading aborted (I/O error, HTTP failure, bad credentials, …).
    Failed(String),
}

/// Spawns the loader thread for `source` and returns the event receiver.
///
/// `cache_dir` is the app's config directory (see [`crate::store::Store::default_dir`]);
/// `None` on platforms without one simply disables Xtream playlist caching.
/// The thread finishes on its own; failures are reported as
/// [`LoadEvent::Failed`] rather than panics.
#[must_use]
pub fn spawn(source: Source, cache_dir: Option<PathBuf>) -> Receiver<LoadEvent> {
    let (tx, rx) = channel();
    thread::spawn(move || {
        let result = match source {
            Source::File(path) => {
                log::info!("loading playlist from file: {}", path.display());
                load_file(&path, &tx)
            }
            Source::Xtream(account) => {
                log::info!("loading Xtream playlist: {}", account.display_name());
                load_xtream(&account, cache_dir.as_deref(), &tx)
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
    let mut delivered = 0;
    let summary = parse_stream(
        file,
        total,
        Header::Optional,
        &mut delivered,
        &mut false,
        &mut None,
        tx,
    )
    .map_err(|e| e.to_string())?;
    log::info!("file playlist parsed: {} channels", summary.channels);
    Ok(())
}

/// Shows the last cached playlist (if any) immediately, for a fast first
/// paint while the live fetch below replaces it. Returns whether anything
/// was actually shown, so the caller knows a later [`LoadEvent::Reset`]
/// is needed once live data starts arriving.
fn load_cached(path: &Path, tx: &Sender<LoadEvent>) -> bool {
    let Some(file) = cache::open(path) else {
        return false;
    };
    let mut delivered = 0;
    match parse_stream(
        file,
        None,
        Header::Optional,
        &mut delivered,
        &mut false,
        &mut None,
        tx,
    ) {
        Ok(summary) if summary.channels > 0 => {
            log::info!(
                "showing {} cached channels while refreshing",
                summary.channels
            );
            true
        }
        Ok(_) => false,
        Err(error) => {
            log::warn!("cached playlist unreadable ({error}); ignoring");
            false
        }
    }
}

/// Xtream loading: a cached copy (if any) is shown first for an instant
/// first paint, then the M3U download (`get.php`) is tried live; panels
/// that disable it get the channel list rebuilt from the JSON player API
/// instead. Either live path clears the cached rows (via
/// [`LoadEvent::Reset`]) only once it actually has fresh data to replace
/// them with, so a live fetch that never gets that far leaves the cached
/// copy on screen instead of clearing it for nothing.
fn load_xtream(
    account: &Account,
    cache_dir: Option<&Path>,
    tx: &Sender<LoadEvent>,
) -> Result<(), String> {
    let cache_path = cache_dir.map(|dir| cache::path(dir, &account.cache_key()));
    let cache_shown = cache_path
        .as_deref()
        .is_some_and(|path| load_cached(path, tx));
    let mut reset_pending = cache_shown;

    let mut delivered = 0;
    let m3u_error = match load_xtream_m3u(
        account,
        &mut delivered,
        &mut reset_pending,
        cache_path.as_deref(),
        tx,
    ) {
        Ok(()) => return Ok(()),
        Err(error) => error,
    };
    if delivered > 0 {
        // Channels already reached the UI (download died mid-stream); a
        // second full load would duplicate every entry.
        return Err(m3u_error);
    }
    log::warn!("M3U download failed ({m3u_error}); trying the player API instead");
    match load_xtream_api(account, &mut reset_pending, cache_path.as_deref(), tx) {
        Ok(()) => Ok(()),
        Err(api_error) => {
            let combined = format!("M3U download failed: {m3u_error}; player API: {api_error}");
            if cache_shown {
                // Both live paths failed before producing anything, so the
                // cached copy was never cleared — keep showing it instead
                // of replacing a working list with an error.
                log::warn!("xtream refresh failed ({combined}); keeping cached playlist");
                Ok(())
            } else {
                Err(combined)
            }
        }
    }
}

fn load_xtream_m3u(
    account: &Account,
    delivered: &mut usize,
    reset_pending: &mut bool,
    cache_path: Option<&Path>,
    tx: &Sender<LoadEvent>,
) -> Result<(), String> {
    let (reader, total) = account.fetch().map_err(|error| error.to_string())?;
    let (mut sink, tmp_path) = match cache_path.and_then(cache::create_temp) {
        Some((file, tmp)) => (Some(file), Some(tmp)),
        None => (None, None),
    };
    // get.php always answers with extended M3U, so anything else (CDN
    // challenge page, HTML error, panel notice) must abort with a look at
    // the body rather than turn into junk channels or an empty list.
    let summary = match parse_stream(
        reader,
        total,
        Header::Required,
        delivered,
        reset_pending,
        &mut sink,
        tx,
    ) {
        Ok(summary) => summary,
        Err(error) => {
            if let Some(tmp) = &tmp_path {
                cache::discard_temp(tmp);
            }
            return Err(error.to_string());
        }
    };
    if summary.channels == 0 {
        if let Some(tmp) = &tmp_path {
            cache::discard_temp(tmp);
        }
        return Err(match summary.first_line {
            Some(line) => format!("server sent a playlist with no channels (starts: {line:?})"),
            None => "server sent an empty response — check that the account is active".to_owned(),
        });
    }
    if let (Some(tmp), Some(path)) = (&tmp_path, cache_path) {
        cache::promote(tmp, path);
    }
    log::info!("xtream playlist parsed: {} channels", summary.channels);
    Ok(())
}

/// Builds the channel list from the player API: categories become
/// groups, and each live stream's URL is synthesized from its id. Panels
/// that don't serve `get.php` (see [`load_xtream_m3u`]) still get a
/// working cache: every channel is mirrored into `cache_path` as it's
/// built, in the same M3U form [`crate::playlist`] parses back.
fn load_xtream_api(
    account: &Account,
    reset_pending: &mut bool,
    cache_path: Option<&Path>,
    tx: &Sender<LoadEvent>,
) -> Result<(), String> {
    let categories = account
        .fetch_live_categories()
        .map_err(|error| error.to_string())?;
    let streams = account
        .fetch_live_streams()
        .map_err(|error| error.to_string())?;
    log::info!(
        "player API: {} live streams in {} categories",
        streams.len(),
        categories.len()
    );
    if streams.is_empty() {
        return Err("the player API returned no live streams".to_owned());
    }

    let (mut cache_sink, tmp_path) = match cache_path.and_then(cache::create_temp) {
        Some((mut file, tmp)) => {
            if file.write_all(b"#EXTM3U\n").is_ok() {
                (Some(file), Some(tmp))
            } else {
                (None, None)
            }
        }
        None => (None, None),
    };

    let category_names: HashMap<&str, &str> = categories
        .iter()
        .map(|category| (category.id.as_str(), category.name.as_str()))
        .collect();
    let total = streams.len();
    let mut groups: Vec<String> = Vec::new();
    let mut group_ids: HashMap<String, GroupId> = HashMap::new();
    let mut groups_sent = 0;
    let mut channels: Vec<Channel> = Vec::new();
    for (done, stream) in streams.into_iter().enumerate() {
        let group_name = stream
            .category_id
            .as_deref()
            .and_then(|id| category_names.get(id).copied());
        let group = group_name.map(|name| match group_ids.entry(name.to_owned()) {
            Entry::Occupied(entry) => *entry.get(),
            Entry::Vacant(entry) => {
                groups.push(name.to_owned());
                *entry.insert(groups.len() - 1)
            }
        });
        let url = account.live_stream_url(stream.stream_id);
        // Panels without a stream name get the URL, like bare M3U entries.
        let name = stream.name.unwrap_or_else(|| url.clone());
        write_m3u_entry(
            &mut cache_sink,
            &name,
            &url,
            stream.epg_channel_id.as_deref(),
            group_name,
        );
        channels.push(Channel {
            name,
            url,
            tvg_id: stream.epg_channel_id,
            group,
        });
        if channels.len() >= BATCH_SIZE {
            let new_groups = groups[groups_sent..].to_vec();
            groups_sent = groups.len();
            let percent = u8::try_from(((done + 1) * 100 / total).min(100)).unwrap_or(100);
            send_batch(
                reset_pending,
                tx,
                std::mem::take(&mut channels),
                new_groups,
                0,
                Some(percent),
            );
        }
    }
    send_batch(
        reset_pending,
        tx,
        channels,
        groups[groups_sent..].to_vec(),
        0,
        Some(100),
    );
    drop(cache_sink);
    if let (Some(tmp), Some(path)) = (&tmp_path, cache_path) {
        cache::promote(tmp, path);
    }
    Ok(())
}

/// Appends one channel as an `#EXTINF`/URL pair to `sink`, if present. A
/// write failure disables the sink for the rest of the load — mirroring
/// the same file that's about to be shown to the user isn't worth
/// failing over.
fn write_m3u_entry(
    sink: &mut Option<File>,
    name: &str,
    url: &str,
    tvg_id: Option<&str>,
    group: Option<&str>,
) {
    use std::fmt::Write as _;

    let Some(file) = sink else { return };
    let mut line = String::from("#EXTINF:-1");
    if let Some(id) = tvg_id {
        let _ = write!(line, " tvg-id=\"{id}\"");
    }
    if let Some(group) = group {
        let _ = write!(line, " group-title=\"{group}\"");
    }
    line.push(',');
    line.push_str(name);
    line.push('\n');
    line.push_str(url);
    line.push('\n');
    if file.write_all(line.as_bytes()).is_err() {
        *sink = None;
    }
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
/// [`BATCH_SIZE`] channels. `delivered` counts the channels sent to the
/// UI so far — kept caller-visible so an error mid-stream can tell
/// whether a retry through another source would duplicate entries.
///
/// With [`Header::Required`], input whose first non-blank line is not
/// `#EXTM3U` fails as [`std::io::ErrorKind::InvalidData`] before any
/// batch is sent.
///
/// `reset_pending` and `cache_sink` support the Xtream cache-then-refresh
/// flow (see [`load_xtream`]): when `*reset_pending` is set, a
/// [`LoadEvent::Reset`] is sent right before the first non-empty batch —
/// not any earlier, so a fetch that never gets that far never clears a
/// cached copy already on screen. When `cache_sink` holds a file, every
/// line read is mirrored into it, so a stream that parses successfully
/// leaves behind an exact copy to cache; the caller decides whether to
/// keep it. A write failure just stops the mirroring silently — caching
/// is never a reason to fail the load.
fn parse_stream(
    input: impl Read,
    total_bytes: Option<u64>,
    header: Header,
    delivered: &mut usize,
    reset_pending: &mut bool,
    cache_sink: &mut Option<File>,
    tx: &Sender<LoadEvent>,
) -> std::io::Result<ParseSummary> {
    let mut reader = BufReader::with_capacity(256 * 1024, input);
    let mut builder = PlaylistBuilder::new();
    let mut line = String::new();
    let mut bytes_read: u64 = 0;
    let mut groups_sent = 0;
    let mut first_line: Option<String> = None;

    loop {
        line.clear();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            break;
        }
        bytes_read += n as u64;
        if let Some(sink) = cache_sink
            && sink.write_all(line.as_bytes()).is_err()
        {
            *cache_sink = None;
        }
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
            *delivered += builder.buffered_channels();
            flush(
                &mut builder,
                &mut groups_sent,
                percent(bytes_read, total_bytes),
                reset_pending,
                tx,
            );
        }
    }

    // finish() folds a trailing URL-less #EXTINF into the skipped count, so
    // always send the tail batch even when it carries no channels.
    let mut playlist = builder.finish();
    *delivered += playlist.channels.len();
    send_batch(
        reset_pending,
        tx,
        std::mem::take(&mut playlist.channels),
        playlist.groups()[groups_sent..].to_vec(),
        playlist.skipped,
        Some(100),
    );
    Ok(ParseSummary {
        channels: *delivered,
        first_line,
    })
}

/// Sends the currently buffered channels and any newly seen groups.
fn flush(
    builder: &mut PlaylistBuilder,
    groups_sent: &mut usize,
    percent: Option<u8>,
    reset_pending: &mut bool,
    tx: &Sender<LoadEvent>,
) {
    let new_groups = builder.groups()[*groups_sent..].to_vec();
    *groups_sent = builder.groups().len();
    send_batch(
        reset_pending,
        tx,
        builder.drain_channels(),
        new_groups,
        builder.skipped(),
        percent,
    );
}

/// Sends `channels` as a [`LoadEvent::Batch`], first sending
/// [`LoadEvent::Reset`] if `*reset_pending` is set — but only when this
/// batch actually carries channels, so an empty administrative batch
/// (e.g. the always-sent tail of an otherwise-empty stream) never clears
/// a cached copy for nothing. Consumes `reset_pending` on that first use.
fn send_batch(
    reset_pending: &mut bool,
    tx: &Sender<LoadEvent>,
    channels: Vec<Channel>,
    new_groups: Vec<String>,
    skipped: usize,
    percent: Option<u8>,
) {
    if !channels.is_empty() && std::mem::take(reset_pending) {
        let _ = tx.send(LoadEvent::Reset);
    }
    let _ = tx.send(LoadEvent::Batch {
        channels,
        new_groups,
        skipped,
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
    use std::fs;

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
                // Cached rows are being replaced by fresh ones.
                LoadEvent::Reset => channels = 0,
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

    /// Mock Xtream panel: serves `hits` sequential connections, routing
    /// by request path — `get.php` is blocked with a custom status code
    /// (as real panels do), the player API answers with JSON.
    fn serve_panel(hits: usize) -> u16 {
        use std::io::Write;

        const CATEGORIES: &str = r#"[{"category_id":1,"category_name":"News"},{"category_id":"2","category_name":"Sports"}]"#;
        const STREAMS: &str = r#"[
            {"name":"One","stream_id":11,"category_id":"1","epg_channel_id":"one.tv"},
            {"name":"Two","stream_id":"22","category_id":2},
            {"name":"Three","stream_id":33,"category_id":null}
        ]"#;

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for _ in 0..hits {
                let (mut stream, _) = listener.accept().unwrap();
                let mut buf = [0_u8; 2048];
                let mut request = Vec::new();
                loop {
                    let n = stream.read(&mut buf).unwrap();
                    request.extend_from_slice(&buf[..n]);
                    if n == 0 || request.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                let request = String::from_utf8_lossy(&request);
                let (status, body) = if request.contains("get.php") {
                    ("HTTP/1.1 884 Blocked", "")
                } else if request.contains("action=get_live_categories") {
                    ("HTTP/1.1 200 OK", CATEGORIES)
                } else if request.contains("action=get_live_streams") {
                    ("HTTP/1.1 200 OK", STREAMS)
                } else {
                    ("HTTP/1.1 404 Not Found", "")
                };
                let response = format!(
                    "{status}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).unwrap();
            }
        });
        port
    }

    #[test]
    fn falls_back_to_player_api_when_m3u_download_is_blocked() {
        let port = serve_panel(3);
        let rx = spawn(xtream_source(port), None);
        let mut channels = Vec::new();
        let mut groups = Vec::new();
        for event in &rx {
            match event {
                LoadEvent::Batch {
                    channels: batch,
                    new_groups,
                    ..
                } => {
                    channels.extend(batch);
                    groups.extend(new_groups);
                }
                LoadEvent::Reset => panic!("unexpected reset: no cache was primed"),
                LoadEvent::Finished => break,
                LoadEvent::Failed(message) => panic!("load failed: {message}"),
            }
        }
        assert_eq!(groups, ["News", "Sports"]);
        let summary: Vec<(&str, Option<usize>)> = channels
            .iter()
            .map(|c| (c.name.as_str(), c.group))
            .collect();
        assert_eq!(
            summary,
            [
                ("One", Some(0)),
                ("Two", Some(1)),
                ("Three", None) // null category → no group
            ]
        );
        assert_eq!(
            channels[0].url,
            format!("http://127.0.0.1:{port}/live/u/p/11.ts")
        );
        assert_eq!(
            channels[1].url,
            format!("http://127.0.0.1:{port}/live/u/p/22.ts")
        );
        assert_eq!(channels[0].tvg_id.as_deref(), Some("one.tv"));
    }

    #[test]
    fn api_failure_reports_both_errors() {
        // One-shot server: get.php gets the HTML page, then the listener
        // is gone, so the player API fallback cannot connect — the final
        // error must name both failures.
        let port = serve_once("<html>blocked</html>\n");
        let (channels, error) = drain(&spawn(xtream_source(port), None));
        assert_eq!(channels, 0);
        let error = error.unwrap();
        assert!(error.contains("M3U download failed"), "got: {error}");
        assert!(error.contains("player API"), "got: {error}");
    }

    #[test]
    fn xtream_html_response_fails_with_snippet() {
        // Regression: a 200 response that is not a playlist (challenge
        // page, HTML error, …) used to load as junk channels.
        let port = serve_once("<html><body>Access denied</body></html>\n");
        let (channels, error) = drain(&spawn(xtream_source(port), None));
        assert_eq!(channels, 0);
        let error = error.unwrap();
        assert!(error.contains("not send an M3U"), "unexpected: {error}");
        assert!(error.contains("<html>"), "snippet missing: {error}");
    }

    #[test]
    fn xtream_empty_response_fails() {
        let port = serve_once("");
        let (channels, error) = drain(&spawn(xtream_source(port), None));
        assert_eq!(channels, 0);
        assert!(error.unwrap().contains("empty response"));
    }

    #[test]
    fn xtream_header_only_playlist_fails_but_names_the_header() {
        // An account with zero channels is still a failure worth explaining.
        let port = serve_once("#EXTM3U\n");
        let (channels, error) = drain(&spawn(xtream_source(port), None));
        assert_eq!(channels, 0);
        assert!(error.unwrap().contains("#EXTM3U"));
    }

    #[test]
    fn xtream_valid_playlist_still_loads() {
        let port = serve_once("#EXTM3U\n#EXTINF:-1 group-title=\"News\",One\nhttp://u/1\n");
        let (channels, error) = drain(&spawn(xtream_source(port), None));
        assert_eq!(channels, 1);
        assert!(error.is_none());
    }

    /// Unique temp dir to use as a cache directory, cleaned up by the
    /// caller once the test is done with it.
    fn temp_cache_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "m3u-viewer-loader-cache-{tag}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    /// Pre-populates the on-disk cache for `cache_key` with `body`, as if
    /// left behind by a previous successful load.
    fn seed_cache(dir: &Path, cache_key: &str, body: &str) {
        let path = cache::path(dir, cache_key);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, body).unwrap();
    }

    #[test]
    fn cached_playlist_survives_a_totally_failed_live_refresh() {
        let dir = temp_cache_dir("survive");
        // get.php answers with an HTML block page (M3U parse fails before
        // any batch), then the listener is gone, so the player API
        // fallback cannot connect either — both live paths fail before
        // ever clearing the cache.
        let port = serve_once("<html>blocked</html>\n");
        let account = Account::new(&format!("127.0.0.1:{port}"), "u".into(), "p".into());
        seed_cache(
            &dir,
            &account.cache_key(),
            "#EXTM3U\n#EXTINF:-1,Cached\nhttp://u/cached\n",
        );

        let (channels, error) = drain(&spawn(Source::Xtream(account), Some(dir.clone())));
        assert_eq!(channels, 1, "the cached channel should still be showing");
        assert!(
            error.is_none(),
            "expected success (cache kept), got: {error:?}"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn successful_live_refresh_replaces_cache_and_updates_it_on_disk() {
        let dir = temp_cache_dir("refresh");
        let body = "#EXTM3U\n#EXTINF:-1 group-title=\"News\",Fresh\nhttp://u/fresh\n";
        let port = serve_once(body);
        let account = Account::new(&format!("127.0.0.1:{port}"), "u".into(), "p".into());
        let cache_key = account.cache_key();
        seed_cache(
            &dir,
            &cache_key,
            "#EXTM3U\n#EXTINF:-1,Cached\nhttp://u/cached\n",
        );

        let rx = spawn(Source::Xtream(account), Some(dir.clone()));
        let mut saw_cached_batch = false;
        let mut saw_reset = false;
        let mut names_after_reset = Vec::new();
        for event in &rx {
            match event {
                LoadEvent::Batch { channels, .. } => {
                    if saw_reset {
                        names_after_reset.extend(channels.into_iter().map(|c| c.name));
                    } else if channels.iter().any(|c| c.name == "Cached") {
                        saw_cached_batch = true;
                    }
                }
                LoadEvent::Reset => saw_reset = true,
                LoadEvent::Finished => break,
                LoadEvent::Failed(message) => panic!("load failed: {message}"),
            }
        }
        assert!(saw_cached_batch, "cached channel should have shown first");
        assert!(saw_reset, "live refresh should reset before replacing");
        assert_eq!(names_after_reset, ["Fresh"]);

        let cached_text = fs::read_to_string(cache::path(&dir, &cache_key)).unwrap();
        assert!(
            cached_text.contains("Fresh"),
            "cache not updated: {cached_text}"
        );
        assert!(
            !cached_text.contains("Cached"),
            "stale cache kept: {cached_text}"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn player_api_fallback_also_writes_the_cache() {
        // Regression: panels that always reject get.php (so every load
        // falls back to load_xtream_api) never got a cache file written,
        // since only the M3U download path mirrored to disk.
        let dir = temp_cache_dir("api-fallback");
        let port = serve_panel(3);
        let account = Account::new(&format!("127.0.0.1:{port}"), "u".into(), "p".into());
        let cache_key = account.cache_key();

        let (channels, error) = drain(&spawn(Source::Xtream(account), Some(dir.clone())));
        assert_eq!(channels, 3);
        assert!(error.is_none());

        let cached_text = fs::read_to_string(cache::path(&dir, &cache_key)).unwrap();
        assert!(cached_text.starts_with("#EXTM3U\n"));
        assert!(cached_text.contains("tvg-id=\"one.tv\""));
        assert!(cached_text.contains("group-title=\"News\""));
        assert!(cached_text.contains(",One\n"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn empty_file_still_finishes_without_error() {
        // Only Xtream promotes “no channels” to a failure: opening an empty
        // local file deliberately keeps showing an empty list.
        let dir = std::env::temp_dir().join(format!("m3u-viewer-loader-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("empty.m3u");
        std::fs::write(&path, "#EXTM3U\n").unwrap();
        let (channels, error) = drain(&spawn(Source::File(path), None));
        assert_eq!(channels, 0);
        assert!(error.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
