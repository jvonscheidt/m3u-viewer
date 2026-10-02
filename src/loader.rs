//! Background playlist loading.
//!
//! [`spawn`] starts a thread that streams a playlist — from a local file
//! or straight from an Xtream Codes server — through [`PlaylistBuilder`],
//! sending [`LoadEvent`]s over an mpsc channel so the UI can appear
//! immediately and fill in while the data is still arriving.
//!
//! For Xtream sources, `load_xtream` additionally shows a cached copy of
//! the last successful load first (if one exists in `cache_dir`), so the
//! list is populated instantly instead of waiting on the network; the
//! live fetch then runs as usual and, on arriving at its first real
//! batch, a [`LoadEvent::Reset`] clears the cached rows before the fresh
//! ones replace them. The private cache module handles the on-disk side.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::fmt;
use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread;

use crate::cache::{self, PendingCache};
use crate::playlist::{Channel, GroupId, PlaylistBuilder, decode_line};
use crate::xtream::Account;

/// Channels per [`LoadEvent::Batch`]; small enough for a responsive first
/// paint, large enough to keep channel overhead negligible.
const BATCH_SIZE: usize = 4096;

/// Where the playlist comes from.
#[derive(Debug)]
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
    /// The playlist's `#EXTM3U` header named an XMLTV guide (`url-tvg`).
    /// Consumed by the event loop in `main`, which owns EPG loading; may
    /// arrive more than once (cached copy, then the live refresh) and
    /// every occurrence after the first is ignored there.
    EpgUrl(String),
    /// A non-fatal problem the user should know about; loading still ends
    /// with [`LoadEvent::Finished`]. Sent when the live refresh failed
    /// and the cached playlist stays on screen, so its stale (possibly
    /// unplayable) URLs don't masquerade as a successful load. Contains
    /// no credentials.
    Warning(String),
    /// The whole playlist was parsed successfully.
    Finished,
    /// Loading aborted (I/O error, HTTP failure, bad credentials, …).
    Failed(String),
}

impl fmt::Debug for LoadEvent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Batch {
                channels,
                new_groups,
                skipped,
                percent,
            } => formatter
                .debug_struct("Batch")
                .field("channels", &channels.len())
                .field("new_groups", &new_groups)
                .field("skipped", skipped)
                .field("percent", percent)
                .finish(),
            Self::Reset => formatter.write_str("Reset"),
            Self::EpgUrl(_) => formatter
                .debug_tuple("EpgUrl")
                .field(&"<redacted URL>")
                .finish(),
            Self::Warning(message) => formatter.debug_tuple("Warning").field(message).finish(),
            Self::Finished => formatter.write_str("Finished"),
            Self::Failed(message) => formatter.debug_tuple("Failed").field(message).finish(),
        }
    }
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
        None,
        tx,
    )
    .map_err(|e| e.to_string())?;
    log::info!("file playlist parsed: {} channels", summary.delivered);
    Ok(())
}

/// Shows the last cached playlist (if any) immediately, for a fast first
/// paint while the live fetch below replaces it. Returns whether anything
/// was actually shown, so the caller knows a later [`LoadEvent::Reset`]
/// is needed once live data starts arriving.
fn load_cached(path: &Path, tx: &Sender<LoadEvent>) -> bool {
    cache::open(path).is_some_and(|file| show_cached(file, path, tx))
}

/// Streams the cache contents in `input` (read from `path`) to the UI;
/// see [`load_cached`]. A read error partway through still returns `true`
/// when batches already went out — those rows are on screen and must be
/// cleared by a [`LoadEvent::Reset`] before live data arrives, or the
/// fresh rows would be appended to them (and their group ids would index
/// the cached group table). The unreadable cache file is removed so the
/// next launch doesn't stumble over it again.
fn show_cached(input: impl Read, path: &Path, tx: &Sender<LoadEvent>) -> bool {
    let mut delivered = 0;
    let result = parse_stream(
        input,
        None,
        Header::Optional,
        &mut delivered,
        &mut false,
        None,
        tx,
    );
    if let Err(error) = result {
        log::warn!("cached playlist unreadable after {delivered} channels ({error}); removing it");
        cache::remove(path);
    } else if delivered > 0 {
        log::info!("showing {delivered} cached channels while refreshing");
    }
    delivered > 0
}

/// Xtream loading: a cached copy (if any) is shown first for an instant
/// first paint, then the M3U download (`get.php`) is tried live; panels
/// that disable it get the channel list rebuilt from the JSON player API
/// instead. Either live path clears the cached rows (via
/// [`LoadEvent::Reset`]) only once it actually has fresh data to replace
/// them with, so a live fetch that never gets that far leaves the cached
/// copy on screen instead of clearing it for nothing — with a
/// [`LoadEvent::Warning`] saying so, since its URLs may no longer play.
///
/// Error messages pass through [`redact_credentials`] before they are
/// logged or reach the UI.
fn load_xtream(
    account: &Account,
    cache_dir: Option<&Path>,
    tx: &Sender<LoadEvent>,
) -> Result<(), String> {
    let cache_path = cache_dir.map(|dir| cache::path(dir, &account.cache_key()));
    if let Some(dir) = cache_path.as_deref().and_then(Path::parent) {
        // Before this load creates a temp file of its own.
        cache::sweep_stale_temps(dir);
    }
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
        Err(error) => redact_credentials(&error),
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
            let api_error = redact_credentials(&api_error);
            let combined = format!("M3U download failed: {m3u_error}; player API: {api_error}");
            if cache_shown {
                // Both live paths failed before producing anything, so the
                // cached copy was never cleared — keep showing it instead
                // of replacing a list with an error, but say that it is
                // stale: after e.g. a password change its URLs won't play.
                log::warn!("xtream refresh failed ({combined}); keeping cached playlist");
                let _ = tx.send(LoadEvent::Warning(format!(
                    "showing cached playlist — refresh failed: {combined}"
                )));
                Ok(())
            } else {
                Err(combined)
            }
        }
    }
}

/// Masks the value of every `password=` query parameter in `message`.
/// Some HTTP client errors (ureq's `BadUri`, `RequireHttpsOnly`) quote the
/// full request URL, and Xtream request URLs carry the password in the
/// query string; this keeps it out of the status bar and the log.
fn redact_credentials(message: &str) -> String {
    const KEY: &str = "password=";

    let mut redacted = String::with_capacity(message.len());
    let mut rest = message;
    while let Some(start) = rest.find(KEY) {
        let value_start = start + KEY.len();
        redacted.push_str(&rest[..value_start]);
        redacted.push_str("<redacted>");
        let value = &rest[value_start..];
        let end = value
            .find(|c: char| matches!(c, '&' | '"' | '\'' | '#') || c.is_whitespace())
            .unwrap_or(value.len());
        rest = &value[end..];
    }
    redacted.push_str(rest);
    redacted
}

fn load_xtream_m3u(
    account: &Account,
    delivered: &mut usize,
    reset_pending: &mut bool,
    cache_path: Option<&Path>,
    tx: &Sender<LoadEvent>,
) -> Result<(), String> {
    let (reader, total) = account.fetch().map_err(|error| error.to_string())?;
    // Dropped (and so discarded) on every early return below.
    let mut sink = cache_path.and_then(PendingCache::create);
    // get.php always answers with extended M3U, so anything else (CDN
    // challenge page, HTML error, panel notice) must abort with a look at
    // the body rather than turn into junk channels or an empty list.
    let summary = parse_stream(
        reader,
        total,
        Header::Required,
        delivered,
        reset_pending,
        sink.as_mut(),
        tx,
    )
    .map_err(|error| error.to_string())?;
    if summary.delivered == 0 {
        return Err(match summary.first_line {
            Some(line) => format!("server sent a playlist with no channels (starts: {line:?})"),
            None => "server sent an empty response — check that the account is active".to_owned(),
        });
    }
    commit_cache(sink);
    log::info!("xtream playlist parsed: {} channels", summary.delivered);
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

    let mut cache_sink = cache_path.and_then(PendingCache::create);
    if let Some(sink) = &mut cache_sink {
        sink.write(b"#EXTM3U\n");
    }

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
            cache_sink.as_mut(),
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
    commit_cache(cache_sink);
    Ok(())
}

/// Replaces the on-disk cache with a fully loaded `sink`, if caching is
/// on. A sink that hit a write error is discarded instead (see
/// [`PendingCache::commit`]), so a truncated copy never replaces a good
/// cache.
fn commit_cache(sink: Option<PendingCache>) {
    if let Some(sink) = sink
        && !sink.commit()
    {
        log::warn!("playlist cache not updated; the previous copy (if any) is kept");
    }
}

/// Appends one channel as an `#EXTINF`/URL pair to `sink`, if present. A
/// write failure poisons the sink for the rest of the load (it is then
/// discarded, not committed) — mirroring the same list that's being shown
/// to the user isn't worth failing the load over.
fn write_m3u_entry(
    sink: Option<&mut PendingCache>,
    name: &str,
    url: &str,
    tvg_id: Option<&str>,
    group: Option<&str>,
) {
    if let Some(sink) = sink {
        sink.write(format_m3u_entry(name, url, tvg_id, group).as_bytes());
    }
}

fn format_m3u_entry(name: &str, url: &str, tvg_id: Option<&str>, group: Option<&str>) -> String {
    use std::fmt::Write as _;

    let mut line = String::from("#EXTINF:-1");
    if let Some(id) = tvg_id {
        let id = encode_m3u_attribute(id);
        let _ = write!(line, " tvg-id=\"{id}\"");
    }
    if let Some(group) = group {
        let group = encode_m3u_attribute(group);
        let _ = write!(line, " group-title=\"{group}\"");
    }
    line.push(',');
    line.push_str(&single_line_m3u_text(name));
    line.push('\n');
    line.push_str(url);
    line.push('\n');
    line
}

fn encode_m3u_attribute(text: &str) -> String {
    single_line_m3u_text(text).replace('"', "&quot;")
}

fn single_line_m3u_text(text: &str) -> String {
    text.replace("\r\n", " ").replace(['\r', '\n'], " ")
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
    /// Total channels delivered across all batches.
    delivered: usize,
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
/// batch is sent. Lines that are not valid UTF-8 are decoded leniently
/// ([`decode_line`]), so the only other errors are reader I/O failures.
///
/// `reset_pending` and `cache_sink` support the Xtream cache-then-refresh
/// flow (see [`load_xtream`]): when `*reset_pending` is set, a
/// [`LoadEvent::Reset`] is sent right before the first non-empty batch —
/// not any earlier, so a fetch that never gets that far never clears a
/// cached copy already on screen. When `cache_sink` is given, every line
/// read is mirrored into it, so a stream that parses successfully leaves
/// behind an exact copy to cache; the caller decides whether to commit
/// it. A write failure poisons the sink (it then refuses to commit) but
/// never fails the load — caching is not worth failing over.
fn parse_stream(
    input: impl Read,
    total_bytes: Option<u64>,
    header: Header,
    delivered: &mut usize,
    reset_pending: &mut bool,
    mut cache_sink: Option<&mut PendingCache>,
    tx: &Sender<LoadEvent>,
) -> std::io::Result<ParseSummary> {
    let mut reader = BufReader::with_capacity(256 * 1024, input);
    let mut builder = PlaylistBuilder::new();
    let mut raw_line = Vec::new();
    let mut bytes_read: u64 = 0;
    let mut groups_sent = 0;
    let mut first_line: Option<String> = None;
    let mut epg_url_sent = false;

    loop {
        raw_line.clear();
        let n = reader.read_until(b'\n', &mut raw_line)?;
        if n == 0 {
            break;
        }
        bytes_read += n as u64;
        // Mirror the bytes as received: the cache is re-read through the
        // same decoding, so it parses back to exactly the same channels.
        if let Some(sink) = cache_sink.as_deref_mut() {
            sink.write(&raw_line);
        }
        let line = decode_line(&raw_line);
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
        if !epg_url_sent && let Some(url) = builder.tvg_url() {
            epg_url_sent = true;
            let _ = tx.send(LoadEvent::EpgUrl(url.to_owned()));
        }
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
        percent(bytes_read, total_bytes),
    );
    Ok(ParseSummary {
        delivered: *delivered,
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
// unwrap is fine in tests (see AGENTS.md).
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

    /// One-shot response that closes before its declared body length,
    /// producing a read error after the supplied playlist bytes.
    fn serve_truncated(body: String) -> u16 {
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
                if n == 0 || request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len() + 1024
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
                LoadEvent::EpgUrl(_) | LoadEvent::Warning(_) => {}
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
                LoadEvent::EpgUrl(_) => {}
                LoadEvent::Warning(message) => panic!("unexpected warning: {message}"),
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

    #[test]
    fn partial_m3u_delivery_does_not_fall_back_to_the_player_api() {
        use std::fmt::Write as _;

        let mut body = String::from("#EXTM3U\n");
        for index in 0..BATCH_SIZE {
            writeln!(
                body,
                "#EXTINF:-1,Channel {index}\nhttp://example.com/{index}"
            )
            .unwrap();
        }
        let port = serve_truncated(body);
        let (channels, error) = drain(&spawn(xtream_source(port), None));

        assert_eq!(channels, BATCH_SIZE);
        let error = error.unwrap();
        assert!(
            !error.contains("player API"),
            "fallback duplicated a partial load: {error}"
        );
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

        let (channels, warnings, error) =
            drain_with_warnings(&spawn(Source::Xtream(account), Some(dir.clone())));
        assert_eq!(channels, 1, "the cached channel should still be showing");
        assert!(
            error.is_none(),
            "expected success (cache kept), got: {error:?}"
        );
        // Regression: the failed refresh used to be only logged, so a
        // stale cache (e.g. URLs with an old password) looked current.
        assert_eq!(warnings.len(), 1, "got: {warnings:?}");
        assert!(
            warnings[0].starts_with("showing cached playlist — refresh failed:"),
            "got: {warnings:?}"
        );
        assert!(warnings[0].contains("player API"), "got: {warnings:?}");
        let _ = fs::remove_dir_all(&dir);
    }

    /// Like [`drain`], but also collects every [`LoadEvent::Warning`].
    fn drain_with_warnings(rx: &Receiver<LoadEvent>) -> (usize, Vec<String>, Option<String>) {
        let mut channels = 0;
        let mut warnings = Vec::new();
        for event in rx {
            match event {
                LoadEvent::Batch {
                    channels: batch, ..
                } => channels += batch.len(),
                LoadEvent::Reset => channels = 0,
                LoadEvent::EpgUrl(_) => {}
                LoadEvent::Warning(message) => warnings.push(message),
                LoadEvent::Finished => return (channels, warnings, None),
                LoadEvent::Failed(message) => return (channels, warnings, Some(message)),
            }
        }
        panic!("loader hung up without a terminal event");
    }

    #[test]
    fn refresh_failure_warning_does_not_leak_the_password() {
        // A panel error page echoing the request URL ends up in the
        // "did not send an M3U" snippet; HTTP client errors can quote the
        // URL the same way.
        let dir = temp_cache_dir("redact");
        let port = serve_once(
            "<html>bad request: /get.php?username=u&password=s3cret-pw&type=m3u</html>\n",
        );
        let account = Account::new(&format!("127.0.0.1:{port}"), "u".into(), "s3cret-pw".into());
        seed_cache(
            &dir,
            &account.cache_key(),
            "#EXTM3U\n#EXTINF:-1,Cached\nhttp://u/cached\n",
        );
        let (channels, warnings, error) =
            drain_with_warnings(&spawn(Source::Xtream(account), Some(dir.clone())));
        assert_eq!(channels, 1);
        assert!(error.is_none(), "got: {error:?}");
        assert_eq!(warnings.len(), 1, "got: {warnings:?}");
        assert!(!warnings[0].contains("s3cret"), "leaked: {}", warnings[0]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn redact_credentials_masks_password_query_values() {
        assert_eq!(
            redact_credentials(
                "bad uri: http://h/get.php?username=u&password=p%26w&type=m3u is missing host"
            ),
            "bad uri: http://h/get.php?username=u&password=<redacted>&type=m3u is missing host"
        );
        assert_eq!(
            redact_credentials("a password=one b \"password=two\" password="),
            "a password=<redacted> b \"password=<redacted>\" password=<redacted>"
        );
        // The auth-failure hint mentions the word, but carries no value.
        let hint = "check username, password, and account status";
        assert_eq!(redact_credentials(hint), hint);
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
                LoadEvent::EpgUrl(_) => {}
                LoadEvent::Warning(message) => panic!("unexpected warning: {message}"),
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
    fn xtream_load_sweeps_temp_files_abandoned_by_an_earlier_run() {
        // Regression: a viewer quit mid-download left its playlist-sized
        // temp file behind, and no later run ever removed it.
        let dir = temp_cache_dir("sweep");
        let port = serve_once("#EXTM3U\n#EXTINF:-1,Fresh\nhttp://u/fresh\n");
        let account = Account::new(&format!("127.0.0.1:{port}"), "u".into(), "p".into());
        let cache_path = cache::path(&dir, &account.cache_key());
        let mut name = cache_path.file_name().unwrap().to_os_string();
        let crashed_pid = std::process::id().wrapping_add(1);
        name.push(format!(".tmp.{crashed_pid}.1700000000000000000.0"));
        let abandoned = cache_path.with_file_name(name);
        fs::create_dir_all(cache_path.parent().unwrap()).unwrap();
        fs::write(&abandoned, "#EXTM3U\n#EXTINF:-1,Half\n").unwrap();

        let (channels, error) = drain(&spawn(Source::Xtream(account), Some(dir.clone())));
        assert_eq!(channels, 1);
        assert!(error.is_none(), "got: {error:?}");
        assert!(!abandoned.exists(), "abandoned temp file left behind");
        let leftovers = fs::read_dir(cache_path.parent().unwrap()).unwrap().count();
        assert_eq!(leftovers, 1, "only the fresh cache should remain");
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
    fn cache_write_failure_mid_load_keeps_the_previous_cache() {
        // Regression: a write error (disk full) mid-download dropped the
        // cache sink but the caller still promoted the truncated temp
        // file over the good cache. Covers both mirroring paths: the
        // get.php stream (parse_stream) and the player API (write_m3u_entry).
        let dir = temp_cache_dir("write-failure");
        let cache_path = cache::path(&dir, "acct");
        let good = "#EXTM3U\n#EXTINF:-1,Good\nhttp://u/good\n";
        fs::create_dir_all(cache_path.parent().unwrap()).unwrap();
        fs::write(&cache_path, good).unwrap();

        let (tx, rx) = channel();
        let mut sink = PendingCache::failing_for_test(&cache_path);
        let mut delivered = 0;
        let summary = parse_stream(
            "#EXTM3U\n#EXTINF:-1,Fresh\nhttp://u/fresh\n".as_bytes(),
            None,
            Header::Required,
            &mut delivered,
            &mut false,
            Some(&mut sink),
            &tx,
        )
        .unwrap();
        assert_eq!(summary.delivered, 1, "the load itself must still succeed");
        commit_cache(Some(sink));
        assert_eq!(fs::read_to_string(&cache_path).unwrap(), good);

        let mut sink = PendingCache::failing_for_test(&cache_path);
        write_m3u_entry(Some(&mut sink), "Fresh", "http://u/fresh", None, None);
        commit_cache(Some(sink));
        assert_eq!(fs::read_to_string(&cache_path).unwrap(), good);

        drop(tx);
        assert!(rx.iter().count() > 0);
        let leftovers = fs::read_dir(cache_path.parent().unwrap()).unwrap().count();
        assert_eq!(leftovers, 1, "temp files were left behind");
        let _ = fs::remove_dir_all(&dir);
    }

    /// Reader that fails like a dying disk once its data runs out.
    struct FailingReader;

    impl Read for FailingReader {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("simulated read failure"))
        }
    }

    #[test]
    fn cache_read_failure_after_a_batch_still_requests_a_reset() {
        // Regression: a cache that failed to read partway through reported
        // "nothing shown", so the live refresh skipped its Reset and
        // appended fresh rows to the partial cached ones (with group ids
        // indexing the cached group table).
        use std::fmt::Write as _;

        let dir = temp_cache_dir("read-failure");
        let cache_path = cache::path(&dir, "acct");
        let mut body = String::from("#EXTM3U\n");
        for index in 0..=BATCH_SIZE {
            writeln!(
                body,
                "#EXTINF:-1 group-title=\"Cached\",Channel {index}\nhttp://u/{index}"
            )
            .unwrap();
        }
        fs::create_dir_all(cache_path.parent().unwrap()).unwrap();
        fs::write(&cache_path, &body).unwrap();

        let (tx, rx) = channel();
        let input = body.as_bytes().chain(FailingReader);
        assert!(
            show_cached(input, &cache_path, &tx),
            "rows reached the UI, so the live refresh must reset them"
        );
        drop(tx);
        let shown: usize = rx
            .iter()
            .map(|event| match event {
                LoadEvent::Batch { channels, .. } => channels.len(),
                _ => 0,
            })
            .sum();
        assert_eq!(shown, BATCH_SIZE, "only the first full batch went out");
        assert!(!cache_path.exists(), "unreadable cache should be removed");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn cache_read_failure_before_any_batch_shows_nothing() {
        let dir = temp_cache_dir("read-failure-early");
        let cache_path = cache::path(&dir, "acct");
        let (tx, rx) = channel();
        let input = "#EXTM3U\n#EXTINF:-1,A\nhttp://u/a\n"
            .as_bytes()
            .chain(FailingReader);
        assert!(!show_cached(input, &cache_path, &tx));
        drop(tx);
        assert_eq!(rx.iter().count(), 0, "a failed read sends no tail batch");
        let _ = fs::remove_dir_all(&dir);
    }

    /// Latin-1 playlist (as written by legacy tools) for the encoding tests.
    const LATIN1_PLAYLIST: &[u8] = b"#EXTM3U\n\
        #EXTINF:-1 group-title=\"M\xfasica\",Caf\xe9 Radio\n\
        http://u/1\n\
        #EXTINF:-1 group-title=\"Noticias\",Espa\xf1a 24h\n\
        http://u/2\n";

    /// Channel names and group names delivered by `rx`, in order.
    fn names_and_groups(rx: &Receiver<LoadEvent>) -> (Vec<String>, Vec<String>) {
        let mut names = Vec::new();
        let mut groups = Vec::new();
        for event in rx {
            match event {
                LoadEvent::Batch {
                    channels,
                    new_groups,
                    ..
                } => {
                    names.extend(channels.into_iter().map(|c| c.name));
                    groups.extend(new_groups);
                }
                LoadEvent::Failed(message) => panic!("load failed: {message}"),
                LoadEvent::Finished => break,
                LoadEvent::Reset | LoadEvent::EpgUrl(_) | LoadEvent::Warning(_) => {}
            }
        }
        (names, groups)
    }

    #[test]
    fn latin1_file_loads_instead_of_failing() {
        // Regression: the first non-UTF-8 byte failed the whole load with
        // "stream did not contain valid UTF-8".
        let dir = temp_cache_dir("latin1-file");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("latin1.m3u");
        fs::write(&path, LATIN1_PLAYLIST).unwrap();

        let (names, groups) = names_and_groups(&spawn(Source::File(path), None));
        assert_eq!(names, ["Café Radio", "España 24h"]);
        assert_eq!(groups, ["Música", "Noticias"]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn latin1_stream_is_cached_byte_for_byte_and_reads_back_identically() {
        let dir = temp_cache_dir("latin1-cache");
        let cache_path = cache::path(&dir, "acct");
        let (tx, rx) = channel();
        let mut sink = PendingCache::create(&cache_path).unwrap();
        let mut delivered = 0;
        parse_stream(
            LATIN1_PLAYLIST,
            Some(LATIN1_PLAYLIST.len() as u64),
            Header::Required,
            &mut delivered,
            &mut false,
            Some(&mut sink),
            &tx,
        )
        .unwrap();
        assert!(sink.commit());
        assert_eq!(fs::read(&cache_path).unwrap(), LATIN1_PLAYLIST);

        assert!(load_cached(&cache_path, &tx));
        drop(tx);
        let mut percents = Vec::new();
        let mut names = Vec::new();
        for event in &rx {
            if let LoadEvent::Batch {
                channels, percent, ..
            } = event
            {
                percents.push(percent);
                names.extend(channels.into_iter().map(|c| c.name));
            }
        }
        assert_eq!(percents[0], Some(100), "progress counts raw bytes");
        assert_eq!(
            names,
            ["Café Radio", "España 24h", "Café Radio", "España 24h"],
            "live and cached loads must decode identically"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn generated_cache_entries_round_trip_special_characters() {
        let text = format!(
            "#EXTM3U\n{}",
            format_m3u_entry(
                "One\r\n\"Prime\" & Live",
                "http://u/one",
                Some("one\"&.tv"),
                Some("Kids \"R\" & News\nLive"),
            )
        );

        assert!(text.contains(",One \"Prime\" & Live\n"));
        assert!(text.contains("tvg-id=\"one&quot;&.tv\""));
        assert!(text.contains("group-title=\"Kids &quot;R&quot; & News Live\""));
        assert!(!text.contains("&amp;"));

        let playlist = crate::playlist::Playlist::from_reader(text.as_bytes()).unwrap();
        assert_eq!(playlist.channels.len(), 1);
        let channel = &playlist.channels[0];
        assert_eq!(channel.name, "One \"Prime\" & Live");
        assert_eq!(channel.url, "http://u/one");
        assert_eq!(channel.tvg_id.as_deref(), Some("one\"&.tv"));
        assert_eq!(
            playlist.group_name(channel.group.unwrap()),
            Some("Kids \"R\" & News Live")
        );
    }

    #[test]
    fn header_url_tvg_is_forwarded_as_an_epg_event() {
        let dir =
            std::env::temp_dir().join(format!("m3u-viewer-loader-epg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("with-epg.m3u");
        std::fs::write(
            &path,
            "#EXTM3U url-tvg=\"http://example.com/epg.xml\"\n#EXTINF:-1,A\nhttp://u/a\n",
        )
        .unwrap();
        let rx = spawn(Source::File(path), None);
        let mut epg_urls = Vec::new();
        for event in &rx {
            match event {
                LoadEvent::EpgUrl(url) => epg_urls.push(url),
                LoadEvent::Finished | LoadEvent::Failed(_) => break,
                LoadEvent::Batch { .. } | LoadEvent::Reset | LoadEvent::Warning(_) => {}
            }
        }
        assert_eq!(epg_urls, ["http://example.com/epg.xml"]);
        let _ = std::fs::remove_dir_all(&dir);
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
