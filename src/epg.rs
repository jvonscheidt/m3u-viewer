//! Electronic programme guide (EPG) loaded from XMLTV.
//!
//! An XMLTV document — from a local file, an HTTP(S) URL, or an Xtream
//! panel's `xmltv.php` — is parsed in one streaming pass into a [`Guide`]:
//! programme lists per channel id plus a display-name index for playlist
//! entries without a `tvg-id`. Only programmes ending after "now" and
//! starting within the next 12 hours are kept, so multi-day
//! guides for very large playlists stay small in memory. [`spawn`] runs
//! the fetch and parse on a background thread, mirroring how the playlist
//! itself is loaded, and delivers a single [`EpgEvent`] over a channel.

use std::borrow::Cow;
use std::collections::HashMap;
use std::fmt;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, channel};
use std::thread;

use chrono::{DateTime, Local, NaiveDateTime, TimeZone, Utc};
use flate2::bufread::GzDecoder;
use quick_xml::Reader as XmlReader;
use quick_xml::events::attributes::Attribute;
use quick_xml::events::{BytesStart, Event as XmlEvent};
use thiserror::Error;

use crate::xtream::{HttpTimeouts, http_agent};

/// Programmes ending before "now" are dropped at parse time; so are ones
/// starting further ahead than this. Twelve hours keeps now/next working
/// through a long session without holding a full multi-day guide.
const KEEP_AHEAD_SECS: i64 = 12 * 60 * 60;

/// Why the guide could not be loaded.
#[derive(Debug, Error)]
pub enum EpgError {
    /// Reading the source (file or response body) failed.
    #[error("could not read the XMLTV source: {0}")]
    Io(#[from] std::io::Error),
    /// The document is not well-formed XML.
    #[error("could not parse the XMLTV document: {0}")]
    Xml(#[from] quick_xml::Error),
    /// The server replied, but not with the guide.
    #[error("server returned HTTP {0}")]
    Status(u16),
    /// The request itself failed (DNS, connect, TLS, …).
    #[error("request failed: {0}")]
    Http(#[from] Box<ureq::Error>),
    /// `error` happened after a redirect had taken the request to another
    /// host (`to` is its `scheme://host[:port]`).
    #[error("redirected to {to}, a different host — {hint}; then {error}", hint = crate::http::DIVERTED_HINT)]
    Redirected {
        /// Where the redirect led.
        to: String,
        /// What then went wrong there.
        #[source]
        error: Box<EpgError>,
    },
}

impl From<crate::http::GetError> for EpgError {
    fn from(error: crate::http::GetError) -> Self {
        let base = match error.kind {
            crate::http::GetErrorKind::Status(code) => Self::Status(code),
            crate::http::GetErrorKind::Request(error) => Self::Http(error),
        };
        match error.diverted_to {
            Some(to) => Self::Redirected {
                to,
                error: Box::new(base),
            },
            None => base,
        }
    }
}

/// One programme (a scheduled broadcast) on one channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Programme {
    /// Start time, seconds since the Unix epoch.
    pub(crate) start: i64,
    /// End time, seconds since the Unix epoch.
    pub(crate) stop: i64,
    /// Programme title.
    pub(crate) title: String,
}

impl Programme {
    /// Start time in Unix seconds.
    #[must_use]
    pub fn start(&self) -> i64 {
        self.start
    }

    /// End time in Unix seconds.
    #[must_use]
    pub fn stop(&self) -> i64 {
        self.stop
    }

    /// Programme title.
    #[must_use]
    pub fn title(&self) -> &str {
        &self.title
    }
}

/// A parsed guide: per-channel programme lists keyed by XMLTV channel id,
/// plus a display-name index for channels whose playlist entry carries no
/// `tvg-id`.
#[derive(Debug, Default)]
pub struct Guide {
    /// XMLTV channel id → programmes sorted by start time.
    programmes: HashMap<String, Vec<Programme>>,
    /// Lowercased `<display-name>` → first XMLTV channel id using that name.
    display_names: HashMap<String, String>,
}

impl Guide {
    /// Number of channels that carry at least one programme.
    #[must_use]
    pub fn channel_count(&self) -> usize {
        self.programmes.len()
    }

    /// The programme airing at `now` and the one after it, for the
    /// channel identified by `tvg_id` (preferred) or, failing that, its
    /// display `name`. Either slot is `None` when the guide has no data
    /// there (e.g. a gap between programmes, or an unknown channel).
    #[must_use]
    pub fn now_next(
        &self,
        tvg_id: Option<&str>,
        name: &str,
        now: i64,
    ) -> (Option<&Programme>, Option<&Programme>) {
        let Some(programmes) = self.channel_programmes(tvg_id, name) else {
            return (None, None);
        };
        // First programme starting after `now`; the one before it is
        // current if it hasn't ended yet.
        let upcoming = programmes.partition_point(|p| p.start <= now);
        let current = upcoming
            .checked_sub(1)
            .map(|i| &programmes[i])
            .filter(|p| p.stop > now);
        (current, programmes.get(upcoming))
    }

    fn channel_programmes(&self, tvg_id: Option<&str>, name: &str) -> Option<&Vec<Programme>> {
        if let Some(id) = tvg_id
            && let Some(list) = self.programmes.get(id)
        {
            return Some(list);
        }
        let id = self.display_names.get(&name.to_lowercase())?;
        self.programmes.get(id)
    }
}

/// Formats Unix seconds as local wall-clock `HH:MM` for display.
#[must_use]
pub fn format_time(epoch: i64) -> String {
    DateTime::from_timestamp(epoch, 0).map_or_else(
        || "--:--".to_owned(),
        |time| time.with_timezone(&Local).format("%H:%M").to_string(),
    )
}

/// How the document's bytes map to characters, taken from its XML
/// declaration. quick-xml is built without its `encoding` feature, so it
/// hands over raw bytes and decoding happens here — leniently, so one bad
/// byte costs one character (U+FFFD), not the whole guide.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TextEncoding {
    /// UTF-8, the XML default.
    Utf8,
    /// ISO-8859-1 and its aliases, decoded as Windows-1252 (as browsers
    /// do): the same characters byte for byte, except that 0x80–0x9F hold
    /// `€`, curly quotes, and dashes instead of unused control codes.
    Windows1252,
}

impl TextEncoding {
    /// Maps an XML declaration's `encoding` label; labels other than the
    /// Latin-1 / Windows-1252 family fall back to UTF-8.
    fn from_label(label: &[u8]) -> Self {
        // The WHATWG Encoding Standard's labels for windows-1252, minus
        // the ASCII ones: a feed declaring ASCII yet carrying non-ASCII
        // bytes is far more likely UTF-8, the default anyway.
        const WINDOWS_1252_LABELS: [&[u8]; 14] = [
            b"cp1252",
            b"cp819",
            b"csisolatin1",
            b"ibm819",
            b"iso-8859-1",
            b"iso-ir-100",
            b"iso8859-1",
            b"iso88591",
            b"iso_8859-1",
            b"iso_8859-1:1987",
            b"l1",
            b"latin1",
            b"windows-1252",
            b"x-cp1252",
        ];
        let label = label.trim_ascii();
        if WINDOWS_1252_LABELS
            .iter()
            .any(|known| label.eq_ignore_ascii_case(known))
        {
            Self::Windows1252
        } else {
            Self::Utf8
        }
    }

    /// Decodes raw document bytes; never fails.
    fn decode(self, bytes: &[u8]) -> Cow<'_, str> {
        match self {
            Self::Windows1252 if !bytes.is_ascii() => {
                Cow::Owned(bytes.iter().copied().map(windows_1252_char).collect())
            }
            // Pure ASCII is identical in both encodings; this borrows.
            _ => String::from_utf8_lossy(bytes),
        }
    }
}

/// The character a Windows-1252 byte stands for. Bytes the code page
/// leaves undefined map to the same-valued code point, as in WHATWG.
fn windows_1252_char(byte: u8) -> char {
    const C1: [char; 32] = [
        '€', '\u{81}', '‚', 'ƒ', '„', '…', '†', '‡', 'ˆ', '‰', 'Š', '‹', 'Œ', '\u{8d}', 'Ž',
        '\u{8f}', '\u{90}', '‘', '’', '“', '”', '•', '–', '—', '˜', '™', 'š', '›', 'œ', '\u{9d}',
        'ž', 'Ÿ',
    ];
    match byte {
        0x80..=0x9f => C1[usize::from(byte - 0x80)],
        _ => char::from(byte),
    }
}

/// Which element's text is currently being collected.
enum TextTarget {
    None,
    DisplayName,
    Title,
}

/// Parses an XMLTV document, keeping only programmes that overlap the
/// window from `now` through the configured 12-hour lookahead. Programmes with
/// missing or malformed attributes are skipped, not errors. Text is read
/// as UTF-8, or as Windows-1252 when the XML declaration names Latin-1;
/// invalid UTF-8 becomes U+FFFD instead of failing the document.
///
/// # Errors
///
/// [`EpgError::Xml`] when the document is not well-formed XML (which
/// includes I/O failures of the underlying reader).
pub fn parse_xmltv<R: BufRead>(input: R, now: i64) -> Result<Guide, EpgError> {
    // No trim_text: per-event trimming could eat spaces around separately
    // reported entity references. Collected text is trimmed once, when its
    // element ends.
    let mut reader = XmlReader::from_reader(input);
    let mut guide = Guide::default();
    let mut buf = Vec::new();
    // Id of the <channel> being read, while inside one.
    let mut channel_id: Option<String> = None;
    // Channel id and partially built programme of the <programme> being
    // read — `None` when it was dropped (bad attributes or out of window).
    let mut pending: Option<(String, Programme)> = None;
    let mut target = TextTarget::None;
    let mut text = String::new();
    let mut encoding = TextEncoding::Utf8;

    loop {
        match reader.read_event_into(&mut buf)? {
            XmlEvent::Decl(declaration) => {
                if let Some(Ok(label)) = declaration.encoding() {
                    encoding = TextEncoding::from_label(&label);
                }
            }
            XmlEvent::Start(element) => match element.local_name().as_ref() {
                b"channel" => channel_id = attr_value(&element, b"id", encoding),
                b"display-name" if channel_id.is_some() => {
                    target = TextTarget::DisplayName;
                    text.clear();
                }
                b"programme" => pending = programme_from_attrs(&element, now, encoding),
                // Only the first <title> counts; feeds often repeat it
                // once per language.
                b"title" if pending.as_ref().is_some_and(|(_, p)| p.title.is_empty()) => {
                    target = TextTarget::Title;
                    text.clear();
                }
                _ => {}
            },
            // Entity references arrive as separate GeneralRef events, so
            // text and CDATA only need decoding, not unescaping.
            XmlEvent::Text(t) => {
                if !matches!(target, TextTarget::None) {
                    text.push_str(&encoding.decode(&t));
                }
            }
            XmlEvent::CData(t) => {
                if !matches!(target, TextTarget::None) {
                    text.push_str(&encoding.decode(&t));
                }
            }
            // Handle references when quick-xml reports them separately;
            // versions/configurations that expand them into Text work too.
            XmlEvent::GeneralRef(reference) => {
                if !matches!(target, TextTarget::None)
                    && let Some(ch) = resolve_reference(&reference)
                {
                    text.push(ch);
                }
            }
            XmlEvent::End(element) => match element.local_name().as_ref() {
                b"display-name" => {
                    let name = text.trim();
                    if matches!(target, TextTarget::DisplayName)
                        && let Some(id) = &channel_id
                        && !name.is_empty()
                    {
                        guide
                            .display_names
                            .entry(name.to_lowercase())
                            .or_insert_with(|| id.clone());
                    }
                    target = TextTarget::None;
                }
                b"title" => {
                    if matches!(target, TextTarget::Title)
                        && let Some((_, programme)) = &mut pending
                    {
                        text.trim().clone_into(&mut programme.title);
                    }
                    target = TextTarget::None;
                }
                b"channel" => channel_id = None,
                b"programme" => {
                    if let Some((channel, programme)) = pending.take()
                        && !programme.title.is_empty()
                    {
                        guide.programmes.entry(channel).or_default().push(programme);
                    }
                }
                _ => {}
            },
            XmlEvent::Eof => break,
            _ => {}
        }
        buf.clear();
    }
    for list in guide.programmes.values_mut() {
        list.sort_by_key(|programme| programme.start);
    }
    Ok(guide)
}

/// Resolves a character reference (`&#…;`) or one of XML's five
/// predefined named entities. `None` for custom entities, which are
/// dropped from the collected text.
fn resolve_reference(reference: &quick_xml::events::BytesRef) -> Option<char> {
    if let Ok(Some(ch)) = reference.resolve_char_ref() {
        return Some(ch);
    }
    match reference.decode().ok()?.as_ref() {
        "amp" => Some('&'),
        "lt" => Some('<'),
        "gt" => Some('>'),
        "quot" => Some('"'),
        "apos" => Some('\''),
        _ => None,
    }
}

/// Reads one attribute of `element`, decoded like text (see
/// [`TextEncoding`]) and then unescaped and normalized; `None` when absent
/// or when it references an unknown entity.
fn attr_value(element: &BytesStart, name: &[u8], encoding: TextEncoding) -> Option<String> {
    let attr = element
        .attributes()
        .flatten()
        .find(|attr| attr.key.as_ref() == name)?;
    // quick-xml's unescaping/normalization insists on UTF-8, so hand it
    // the already decoded value rather than the raw bytes.
    let decoded = Attribute {
        key: attr.key,
        value: Cow::Owned(encoding.decode(&attr.value).into_owned().into_bytes()),
    };
    decoded
        .normalized_value(quick_xml::XmlVersion::Implicit1_0)
        .ok()
        .map(Cow::into_owned)
}

/// Builds the programme skeleton from a `<programme>` start tag; `None`
/// when attributes are missing/malformed or the programme lies outside
/// the kept window.
fn programme_from_attrs(
    element: &BytesStart,
    now: i64,
    encoding: TextEncoding,
) -> Option<(String, Programme)> {
    let channel = attr_value(element, b"channel", encoding)?;
    let start = parse_xmltv_time(&attr_value(element, b"start", encoding)?)?;
    let stop = parse_xmltv_time(&attr_value(element, b"stop", encoding)?)?;
    (stop > now && start <= now + KEEP_AHEAD_SECS).then(|| {
        (
            channel,
            Programme {
                start,
                stop,
                title: String::new(),
            },
        )
    })
}

/// Decodes an XMLTV timestamp — `YYYYMMDDHHMMSS ±HHMM`, where seconds and
/// the offset may be omitted; a missing offset is read as UTC — into Unix
/// seconds. `None` for anything malformed (the programme is skipped).
fn parse_xmltv_time(value: &str) -> Option<i64> {
    let value = value.trim();
    let (digits, offset) = match value.split_once(' ') {
        Some((digits, offset)) => (digits, Some(offset.trim())),
        None => (value, None),
    };
    if !matches!(digits.len(), 12 | 14) || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let mut padded = digits.to_owned();
    while padded.len() < 14 {
        padded.push('0');
    }
    match offset {
        Some(offset) => DateTime::parse_from_str(&format!("{padded} {offset}"), "%Y%m%d%H%M%S %z")
            .ok()
            .map(|time| time.timestamp()),
        None => NaiveDateTime::parse_from_str(&padded, "%Y%m%d%H%M%S")
            .ok()
            .map(|naive| Utc.from_utc_datetime(&naive).timestamp()),
    }
}

/// Where the XMLTV guide comes from.
pub enum EpgSource {
    /// A local XMLTV file (optionally gzip-compressed).
    File(PathBuf),
    /// An HTTP(S) URL serving XMLTV (optionally gzip-compressed).
    Url(String),
}

impl fmt::Debug for EpgSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::File(path) => formatter.debug_tuple("File").field(path).finish(),
            Self::Url(_) => formatter
                .debug_tuple("Url")
                .field(&"<redacted URL>")
                .finish(),
        }
    }
}

impl EpgSource {
    /// Interprets a CLI/config value: anything with an `http(s)://`
    /// scheme is a URL, everything else a local file path.
    #[must_use]
    pub fn from_arg(value: &str) -> Self {
        if value.starts_with("http://") || value.starts_with("https://") {
            Self::Url(value.to_owned())
        } else {
            Self::File(PathBuf::from(value))
        }
    }

    /// Loggable description that never leaks credentials embedded in a
    /// URL's query string (Xtream's `xmltv.php` carries them there).
    fn describe(&self) -> String {
        match self {
            Self::File(path) => path.display().to_string(),
            Self::Url(url) => url.split('?').next().unwrap_or(url).to_owned(),
        }
    }
}

/// Result of a background EPG load; exactly one is sent per [`spawn`].
#[derive(Debug)]
pub enum EpgEvent {
    /// The guide was fetched and parsed.
    Loaded(Guide),
    /// Loading failed; the message is also written to the log.
    Failed(String),
}

/// Spawns a thread that loads the guide from `source` and returns the
/// event receiver. `user_agent` is sent on HTTP requests when set, as
/// some providers only answer to known player user agents.
#[must_use]
pub fn spawn(source: EpgSource, user_agent: Option<String>) -> Receiver<EpgEvent> {
    let (tx, rx) = channel();
    thread::spawn(move || {
        let described = source.describe();
        log::info!("loading EPG from {described}");
        let now = Utc::now().timestamp();
        let event = match load(&source, user_agent.as_deref(), now, HttpTimeouts::STANDARD) {
            Ok(guide) => {
                log::info!(
                    "EPG loaded: {} channels with programmes",
                    guide.channel_count()
                );
                EpgEvent::Loaded(guide)
            }
            Err(error) => {
                log::warn!("EPG load failed ({described}): {error}");
                EpgEvent::Failed(error.to_string())
            }
        };
        // A send failure just means the UI is gone; nothing left to do.
        let _ = tx.send(event);
    });
    rx
}

/// Fetches and parses the guide. URLs go through the shared download
/// agent configured with `timeouts`, so a server that accepts and then
/// hangs fails the load instead of blocking this thread forever.
fn load(
    source: &EpgSource,
    user_agent: Option<&str>,
    now: i64,
    timeouts: HttpTimeouts,
) -> Result<Guide, EpgError> {
    let reader: Box<dyn BufRead> = match source {
        EpgSource::File(path) => Box::new(BufReader::new(File::open(path)?)),
        EpgSource::Url(url) => {
            // Non-2xx replies (panels use custom codes ureq lets through)
            // are errors, so they are never parsed as XML.
            let response = crate::http::get(&http_agent(timeouts), url, user_agent)?;
            // Unlimited body: full guides routinely exceed ureq's 10 MB
            // default.
            Box::new(BufReader::new(
                response
                    .into_body()
                    .into_with_config()
                    .limit(u64::MAX)
                    .reader(),
            ))
        }
    };
    parse_xmltv(decompress_if_gzip(reader)?, now)
}

/// Transparently unwraps gzip — many XMLTV feeds ship as `.xml.gz` — by
/// sniffing the magic bytes rather than trusting file names.
fn decompress_if_gzip(mut reader: Box<dyn BufRead>) -> std::io::Result<Box<dyn BufRead>> {
    if reader.fill_buf()?.starts_with(&[0x1f, 0x8b]) {
        Ok(Box::new(BufReader::new(GzDecoder::new(reader))))
    } else {
        Ok(reader)
    }
}

#[cfg(test)]
// unwrap is fine in tests (see AGENTS.md).
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// Fixed "now" for deterministic window checks: 2026-07-05 12:00 UTC.
    const NOW: i64 = 1_783_080_000;

    fn hours(n: i64) -> i64 {
        n * 3600
    }

    fn parse(xml: &str) -> Guide {
        parse_xmltv(xml.as_bytes(), NOW).unwrap()
    }

    /// XMLTV timestamp (UTC, explicit offset) `n` hours from NOW.
    fn stamp(offset_hours: i64) -> String {
        let time = DateTime::from_timestamp(NOW + hours(offset_hours), 0).unwrap();
        time.format("%Y%m%d%H%M%S +0000").to_string()
    }

    fn stamp_seconds(offset_seconds: i64) -> String {
        let time = DateTime::from_timestamp(NOW + offset_seconds, 0).unwrap();
        time.format("%Y%m%d%H%M%S +0000").to_string()
    }

    fn sample_guide() -> Guide {
        let xml = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<tv>
  <channel id="one.tv"><display-name>Channel One</display-name></channel>
  <channel id="two.tv"><display-name>Channel Two</display-name></channel>
  <programme start="{}" stop="{}" channel="one.tv"><title>Morning Show</title></programme>
  <programme start="{}" stop="{}" channel="one.tv"><title lang="en">News &amp; Weather</title></programme>
  <programme start="{}" stop="{}" channel="one.tv"><title>Evening Film</title></programme>
  <programme start="{}" stop="{}" channel="two.tv"><title><![CDATA[Match <Live>]]></title></programme>
</tv>"#,
            stamp(-3),
            stamp(-1), // Morning Show: already over → dropped by the window
            stamp(-1),
            stamp(1), // News & Weather: airing now
            stamp(1),
            stamp(2), // Evening Film: next
            stamp(-1),
            stamp(1), // Match <Live>: airing now on two.tv
        );
        parse(&xml)
    }

    #[test]
    fn now_next_by_tvg_id() {
        let guide = sample_guide();
        let (current, next) = guide.now_next(Some("one.tv"), "ignored", NOW);
        assert_eq!(current.unwrap().title, "News & Weather");
        assert_eq!(next.unwrap().title, "Evening Film");
    }

    #[test]
    fn past_programmes_are_dropped_by_the_window() {
        let guide = sample_guide();
        let (current, _) = guide.now_next(Some("one.tv"), "x", NOW - hours(2));
        // Morning Show was airing then, but it ended before NOW and was
        // never kept.
        assert!(current.is_none());
    }

    #[test]
    fn far_future_programmes_are_dropped_by_the_window() {
        let xml = format!(
            r#"<tv><programme start="{}" stop="{}" channel="one.tv"><title>Too Far</title></programme></tv>"#,
            stamp(20),
            stamp(21),
        );
        assert_eq!(parse(&xml).channel_count(), 0);
    }

    #[test]
    fn programme_window_boundaries_are_exact() {
        let xml = format!(
            r#"<tv>
<programme start="{}" stop="{}" channel="one.tv"><title>Just Ended</title></programme>
<programme start="{}" stop="{}" channel="one.tv"><title>Lookahead Edge</title></programme>
</tv>"#,
            stamp_seconds(-3600),
            stamp_seconds(0),
            stamp_seconds(KEEP_AHEAD_SECS),
            stamp_seconds(KEEP_AHEAD_SECS + 3600),
        );
        let guide = parse(&xml);
        let (current, next) = guide.now_next(Some("one.tv"), "x", NOW);
        assert!(current.is_none());
        assert_eq!(next.unwrap().title, "Lookahead Edge");
    }

    #[test]
    fn display_name_fallback_matches_case_insensitively() {
        let guide = sample_guide();
        let (current, _) = guide.now_next(None, "CHANNEL TWO", NOW);
        assert_eq!(current.unwrap().title, "Match <Live>");
    }

    #[test]
    fn duplicate_display_names_keep_the_first_channel() {
        let xml = format!(
            r#"<tv>
<channel id="first.tv"><display-name>Shared</display-name></channel>
<channel id="second.tv"><display-name>Shared</display-name></channel>
<programme start="{}" stop="{}" channel="first.tv"><title>First</title></programme>
<programme start="{}" stop="{}" channel="second.tv"><title>Second</title></programme>
</tv>"#,
            stamp(-1),
            stamp(1),
            stamp(-1),
            stamp(1),
        );
        let guide = parse(&xml);
        let (current, _) = guide.now_next(None, "shared", NOW);
        assert_eq!(current.unwrap().title, "First");
    }

    #[test]
    fn unknown_channel_yields_nothing() {
        let guide = sample_guide();
        let (current, next) = guide.now_next(Some("nope.tv"), "Nope", NOW);
        assert!(current.is_none());
        assert!(next.is_none());
    }

    #[test]
    fn gap_between_programmes_has_next_but_no_current() {
        let xml = format!(
            r#"<tv>
<programme start="{}" stop="{}" channel="one.tv"><title>Later</title></programme>
</tv>"#,
            stamp(2),
            stamp(3),
        );
        let guide = parse(&xml);
        let (current, next) = guide.now_next(Some("one.tv"), "x", NOW);
        assert!(current.is_none());
        assert_eq!(next.unwrap().title, "Later");
    }

    #[test]
    fn programmes_sort_by_start_regardless_of_document_order() {
        let xml = format!(
            r#"<tv>
<programme start="{}" stop="{}" channel="one.tv"><title>Second</title></programme>
<programme start="{}" stop="{}" channel="one.tv"><title>First</title></programme>
</tv>"#,
            stamp(1),
            stamp(2),
            stamp(-1),
            stamp(1),
        );
        let guide = parse(&xml);
        let (current, next) = guide.now_next(Some("one.tv"), "x", NOW);
        assert_eq!(current.unwrap().title, "First");
        assert_eq!(next.unwrap().title, "Second");
    }

    #[test]
    fn malformed_timestamps_skip_the_programme_not_the_document() {
        let xml = format!(
            r#"<tv>
<programme start="not a time" stop="also bad" channel="one.tv"><title>Broken</title></programme>
<programme start="{}" stop="{}" channel="one.tv"><title>Fine</title></programme>
</tv>"#,
            stamp(-1),
            stamp(1),
        );
        let guide = parse(&xml);
        let (current, _) = guide.now_next(Some("one.tv"), "x", NOW);
        assert_eq!(current.unwrap().title, "Fine");
    }

    #[test]
    fn timestamps_parse_offsets_and_default_to_utc() {
        // 12:00 +0200 is 10:00 UTC.
        assert_eq!(
            parse_xmltv_time("20260705120000 +0200"),
            parse_xmltv_time("20260705100000"),
        );
        // Truncated to minutes: seconds pad to zero.
        assert_eq!(
            parse_xmltv_time("202607051000"),
            parse_xmltv_time("20260705100000"),
        );
        assert_eq!(parse_xmltv_time(""), None);
        assert_eq!(parse_xmltv_time("20260705"), None);
        assert_eq!(parse_xmltv_time("2026070510000"), None);
    }

    #[test]
    fn numeric_character_references_resolve() {
        let xml = format!(
            r#"<tv><programme start="{}" stop="{}" channel="one.tv"><title>50&#37; Extra&#x21;</title></programme></tv>"#,
            stamp(-1),
            stamp(1),
        );
        let guide = parse(&xml);
        let (current, _) = guide.now_next(Some("one.tv"), "x", NOW);
        assert_eq!(current.unwrap().title, "50% Extra!");
    }

    #[test]
    fn gzipped_input_is_detected_and_decompressed() {
        use std::io::Write as _;

        let xml = format!(
            r#"<tv><programme start="{}" stop="{}" channel="one.tv"><title>Zipped</title></programme></tv>"#,
            stamp(-1),
            stamp(1),
        );
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(xml.as_bytes()).unwrap();
        let compressed = encoder.finish().unwrap();

        let reader: Box<dyn BufRead> = Box::new(BufReader::new(std::io::Cursor::new(compressed)));
        let guide = parse_xmltv(decompress_if_gzip(reader).unwrap(), NOW).unwrap();
        let (current, _) = guide.now_next(Some("one.tv"), "x", NOW);
        assert_eq!(current.unwrap().title, "Zipped");
    }

    #[test]
    fn source_from_arg_distinguishes_urls_and_files() {
        assert!(matches!(
            EpgSource::from_arg("https://example.com/epg.xml.gz"),
            EpgSource::Url(_)
        ));
        assert!(matches!(
            EpgSource::from_arg("C:/guides/epg.xml"),
            EpgSource::File(_)
        ));
    }

    #[test]
    fn url_description_hides_the_query_string() {
        let source = EpgSource::from_arg("http://host/xmltv.php?username=u&password=p");
        assert_eq!(source.describe(), "http://host/xmltv.php");
    }

    #[test]
    fn untitled_programmes_are_dropped() {
        let xml = format!(
            r#"<tv><programme start="{}" stop="{}" channel="one.tv"></programme></tv>"#,
            stamp(-1),
            stamp(1),
        );
        assert_eq!(parse(&xml).channel_count(), 0);
    }

    /// Encodes `text` as ISO-8859-1; every char must be below U+0100.
    fn latin1(text: &str) -> Vec<u8> {
        text.chars()
            .map(|c| u8::try_from(u32::from(c)).unwrap())
            .collect()
    }

    #[test]
    fn latin1_declared_feed_decodes_accented_text_and_attributes() {
        // Regression: text was decoded as strict UTF-8 and one Latin-1
        // byte failed the whole guide.
        let xml = format!(
            r#"<?xml version="1.0" encoding="ISO-8859-1"?>
<tv>
  <channel id="café&amp;co.tv"><display-name>Télé Café</display-name></channel>
  <programme start="{}" stop="{}" channel="café&amp;co.tv"><title>Café &amp; Crème</title></programme>
  <programme start="{}" stop="{}" channel="café&amp;co.tv"><title><![CDATA[Où est Gérard?]]></title></programme>
</tv>"#,
            stamp(-1),
            stamp(1),
            stamp(1),
            stamp(2),
        );
        let guide = parse_xmltv(latin1(&xml).as_slice(), NOW).unwrap();
        let (current, next) = guide.now_next(Some("café&co.tv"), "x", NOW);
        assert_eq!(current.unwrap().title, "Café & Crème");
        assert_eq!(next.unwrap().title, "Où est Gérard?");
        let (by_name, _) = guide.now_next(None, "TÉLÉ CAFÉ", NOW);
        assert_eq!(by_name.unwrap().title, "Café & Crème");
    }

    #[test]
    fn invalid_utf8_byte_costs_one_character_not_the_guide() {
        // No declaration, so UTF-8 — but one Latin-1 byte slipped in.
        let xml = format!(
            r#"<tv>
<programme start="{}" stop="{}" channel="one.tv"><title>Caf@ Bar</title></programme>
<programme start="{}" stop="{}" channel="two.tv"><title>Fine</title></programme>
</tv>"#,
            stamp(-1),
            stamp(1),
            stamp(-1),
            stamp(1),
        );
        // Latin-1 "é" (0xE9) in place of `@`: invalid UTF-8.
        let (before, after) = xml.split_once('@').unwrap();
        let bytes = [before.as_bytes(), b"\xe9", after.as_bytes()].concat();
        assert!(std::str::from_utf8(&bytes).is_err());
        let guide = parse_xmltv(bytes.as_slice(), NOW).unwrap();
        let (one, _) = guide.now_next(Some("one.tv"), "x", NOW);
        assert_eq!(one.unwrap().title, "Caf\u{fffd} Bar");
        let (two, _) = guide.now_next(Some("two.tv"), "x", NOW);
        assert_eq!(two.unwrap().title, "Fine");
    }

    #[test]
    fn windows_1252_labels_and_c1_characters_decode() {
        for label in ["ISO-8859-1", "latin1", " windows-1252 ", "CP1252"] {
            assert_eq!(
                TextEncoding::from_label(label.as_bytes()),
                TextEncoding::Windows1252,
                "{label}"
            );
        }
        for label in ["UTF-8", "US-ASCII"] {
            assert_eq!(
                TextEncoding::from_label(label.as_bytes()),
                TextEncoding::Utf8
            );
        }
        assert_eq!(
            TextEncoding::Windows1252.decode(b"\x80 \x92s \x96 \xe9\x81"),
            "€ ’s – é\u{81}"
        );
    }

    #[test]
    fn stalled_epg_server_fails_instead_of_hanging() {
        // Regression: EPG downloads used ureq's default agent without any
        // timeouts, so a server that accepted and then went silent blocked
        // the EPG thread forever — both before the headers and mid-body.
        use crate::xtream::test_server::{FAST, ok_head, serve};
        use std::time::{Duration, Instant};

        let partial = vec![b"<tv><programme start=".to_vec()];
        for (head, chunks) in [(String::new(), Vec::new()), (ok_head(10_000), partial)] {
            let (port, server) = serve(head, chunks, Duration::ZERO);
            let source = EpgSource::Url(format!("http://127.0.0.1:{port}/xmltv.php"));
            let started = Instant::now();
            assert!(load(&source, None, NOW, FAST).is_err());
            assert!(started.elapsed() < Duration::from_secs(5));
            server.join().unwrap();
        }
    }

    #[test]
    fn diverted_epg_request_error_names_the_new_host() {
        // Regression: like the playlist, a guide URL diverted to another
        // host failed with no hint that it never reached the provider.
        use crate::xtream::test_server::{FAST, serve};
        use std::time::Duration;

        let (target, target_server) = serve(
            "HTTP/1.1 503 Service Unavailable\r\ncontent-length: 0\r\n\r\n".into(),
            vec![],
            Duration::ZERO,
        );
        let (start, start_server) = serve(
            format!(
                "HTTP/1.1 301 Moved Permanently\r\nlocation: \
                 http://localhost:{target}//n\r\ncontent-length: 0\r\n\r\n"
            ),
            vec![],
            Duration::ZERO,
        );
        let source = EpgSource::Url(format!("http://127.0.0.1:{start}/xmltv.php"));
        let error = load(&source, None, NOW, FAST).unwrap_err();
        assert!(
            matches!(&error, EpgError::Redirected { error, .. } if matches!(**error, EpgError::Status(503))),
            "{error:?}"
        );
        let message = error.to_string();
        assert!(
            message.contains(&format!(
                "redirected to http://localhost:{target}, a different host"
            )),
            "{message}"
        );
        assert!(
            message.contains("DNS filtering or DNS spoofing"),
            "{message}"
        );
        start_server.join().unwrap();
        target_server.join().unwrap();
    }

    #[test]
    fn spawn_reports_a_missing_file_as_failed() {
        let rx = spawn(
            EpgSource::File(PathBuf::from("Z:/does/not/exist.xml")),
            None,
        );
        match rx.recv().unwrap() {
            EpgEvent::Failed(message) => assert!(message.contains("XMLTV"), "got: {message}"),
            EpgEvent::Loaded(_) => panic!("expected a failure"),
        }
    }
}
