//! Streaming parser and in-memory model for M3U/M3U8 playlists.
//!
//! The parser makes a single pass over a buffered reader: `#EXTINF`
//! directives are decoded into [`Channel`] entries, `group-title` values are
//! interned into a flat table, and malformed entries are counted in
//! [`Playlist::skipped`] instead of aborting the load. Only I/O failures
//! abort parsing: bytes that are not valid UTF-8 (common in Latin-1 /
//! Windows-1252 playlists) are decoded leniently rather than rejected.

use std::borrow::Cow;
use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::fmt;
use std::io::BufRead;

use thiserror::Error;

/// Error returned when reading a playlist fails.
///
/// Malformed playlist *content* is never an error — bad entries are skipped
/// and counted. Only failures of the underlying reader surface here.
#[derive(Debug, Error)]
pub enum ParseError {
    /// The underlying reader failed.
    #[error("failed to read playlist: {0}")]
    Io(#[from] std::io::Error),
}

/// Index into [`Playlist::groups`] identifying an interned group name.
pub type GroupId = usize;

/// A single playlist entry.
#[derive(Clone, PartialEq, Eq)]
pub struct Channel {
    /// Display name: the text after the comma in `#EXTINF`, or the URL
    /// itself for bare-URL entries and empty names.
    pub(crate) name: String,
    /// Stream URL (or file path) of the entry.
    pub(crate) url: String,
    /// `tvg-id` attribute, if present.
    pub(crate) tvg_id: Option<String>,
    /// Interned `group-title` attribute, if present.
    pub(crate) group: Option<GroupId>,
}

impl fmt::Debug for Channel {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Channel")
            .field("name", &self.name)
            .field("url", &"<redacted URL>")
            .field("tvg_id", &self.tvg_id)
            .field("group", &self.group)
            .finish()
    }
}

impl Channel {
    /// Display name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Stream URL or path.
    #[must_use]
    pub fn url(&self) -> &str {
        &self.url
    }

    /// XMLTV channel identifier.
    #[must_use]
    pub fn tvg_id(&self) -> Option<&str> {
        self.tvg_id.as_deref()
    }

    /// Interned group identifier.
    #[must_use]
    pub fn group(&self) -> Option<GroupId> {
        self.group
    }
}

/// A parsed playlist: a flat channel list plus interned group names.
#[derive(Debug, Default)]
pub struct Playlist {
    /// All successfully parsed channels, in file order.
    pub(crate) channels: Vec<Channel>,
    /// Number of malformed entries that were skipped.
    pub(crate) skipped: usize,
    groups: Vec<String>,
}

impl Playlist {
    /// Successfully parsed channels in file order.
    #[must_use]
    pub fn channels(&self) -> &[Channel] {
        &self.channels
    }

    /// Number of malformed entries skipped during parsing.
    #[must_use]
    pub fn skipped(&self) -> usize {
        self.skipped
    }
    /// Parses a playlist from a buffered reader in a single streaming pass.
    ///
    /// Accepts extended M3U (`#EXTINF` metadata followed by a URL line) as
    /// well as plain M3U (bare URL lines). Unknown `#` directives and blank
    /// lines are ignored; a UTF-8 BOM and CRLF line endings are handled.
    /// Bytes that are not valid UTF-8 are read as Windows-1252 (which
    /// covers Latin-1 text) instead of failing the whole load.
    ///
    /// # Errors
    ///
    /// Returns [`ParseError::Io`] if the reader fails. Malformed content is
    /// skipped and counted in [`Playlist::skipped`] instead of erroring.
    pub fn from_reader<R: BufRead>(mut reader: R) -> Result<Self, ParseError> {
        let mut builder = PlaylistBuilder::new();
        let mut line = Vec::new();
        loop {
            line.clear();
            if reader.read_until(b'\n', &mut line)? == 0 {
                break;
            }
            builder.push_line(&decode_line(&line));
        }
        Ok(builder.finish())
    }

    /// All interned group names, in order of first appearance.
    #[must_use]
    pub fn groups(&self) -> &[String] {
        &self.groups
    }

    /// Resolves an interned [`GroupId`] to its name.
    #[must_use]
    pub fn group_name(&self, id: GroupId) -> Option<&str> {
        self.groups.get(id).map(String::as_str)
    }
}

/// Incremental playlist parser: feed lines one at a time, drain parsed
/// channels in batches (for streaming loads), then [`finish`](Self::finish).
///
/// [`Playlist::from_reader`] is a convenience wrapper around this type.
#[derive(Debug, Default)]
pub struct PlaylistBuilder {
    playlist: Playlist,
    group_ids: HashMap<String, GroupId>,
    /// Metadata of the `#EXTINF` line waiting for its URL line.
    pending: Option<ExtInf>,
    /// True after a malformed `#EXTINF`: its URL line is swallowed without
    /// producing a channel (the entry was already counted as skipped).
    pending_malformed: bool,
    /// Guide URL from the `#EXTM3U` header, when it carries one.
    tvg_url: Option<String>,
}

impl PlaylistBuilder {
    /// Creates an empty builder.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Consumes one line of playlist text (with or without the trailing
    /// newline). Malformed input never fails; it is counted instead.
    pub fn push_line(&mut self, line: &str) {
        let text = line.trim().trim_start_matches('\u{feff}');
        if text.is_empty() {
            return;
        }
        if let Some(rest) = text.strip_prefix("#EXTINF:") {
            if self.pending.take().is_some() {
                // Previous #EXTINF never got a URL line.
                self.playlist.skipped += 1;
            }
            self.pending_malformed = false;
            if let Some(info) = parse_extinf(rest) {
                self.pending = Some(info);
            } else {
                self.playlist.skipped += 1;
                self.pending_malformed = true;
            }
        } else if let Some(rest) = text.strip_prefix("#EXTM3U") {
            // The header may name an XMLTV guide (`url-tvg`, or the
            // `x-tvg-url` spelling some generators use).
            if self.tvg_url.is_none() {
                self.tvg_url = attribute(rest, "url-tvg").or_else(|| attribute(rest, "x-tvg-url"));
            }
        } else if text.starts_with('#') {
            // Unknown directives are ignored.
        } else if self.pending_malformed {
            self.pending_malformed = false;
        } else {
            let url = text.to_owned();
            let channel = match self.pending.take() {
                Some(info) => {
                    let name = if info.name.is_empty() {
                        url.clone()
                    } else {
                        info.name
                    };
                    let group = info
                        .group
                        .map(|g| intern(&mut self.playlist.groups, &mut self.group_ids, g));
                    Channel {
                        name,
                        url,
                        tvg_id: info.tvg_id,
                        group,
                    }
                }
                None => Channel {
                    // Plain M3U entry: the URL doubles as the name.
                    name: url.clone(),
                    url,
                    tvg_id: None,
                    group: None,
                },
            };
            self.playlist.channels.push(channel);
        }
    }

    /// Removes and returns the channels parsed since the last drain.
    /// Group ids in the returned channels keep referring to [`Self::groups`].
    pub fn drain_channels(&mut self) -> Vec<Channel> {
        std::mem::take(&mut self.playlist.channels)
    }

    /// Number of channels currently buffered (since the last drain).
    #[must_use]
    pub fn buffered_channels(&self) -> usize {
        self.playlist.channels.len()
    }

    /// All interned group names seen so far, in order of first appearance.
    #[must_use]
    pub fn groups(&self) -> &[String] {
        &self.playlist.groups
    }

    /// Number of malformed entries skipped so far.
    #[must_use]
    pub fn skipped(&self) -> usize {
        self.playlist.skipped
    }

    /// The XMLTV guide URL from the `#EXTM3U` header (`url-tvg` /
    /// `x-tvg-url`), once such a header line has been consumed.
    #[must_use]
    pub fn tvg_url(&self) -> Option<&str> {
        self.tvg_url.as_deref()
    }

    /// Finalizes parsing (a trailing `#EXTINF` with no URL counts as
    /// skipped) and returns the playlist with any undrained channels.
    #[must_use]
    pub fn finish(mut self) -> Playlist {
        if self.pending.is_some() {
            self.playlist.skipped += 1;
        }
        self.playlist
    }
}

/// Decodes one raw playlist line (newline included or not).
///
/// Valid UTF-8 is borrowed as is. Anything else is decoded leniently:
/// valid UTF-8 runs stay UTF-8 and each invalid byte is read as
/// Windows-1252 — the encoding legacy (Latin-1-era) playlists are almost
/// always in, so `M\xFAsica` becomes "Música" instead of failing the load
/// or turning into U+FFFD. Every byte maps to some character, so this
/// never fails. A UTF-8 BOM decodes to U+FEFF, which
/// [`PlaylistBuilder::push_line`] strips.
pub(crate) fn decode_line(bytes: &[u8]) -> Cow<'_, str> {
    if let Ok(text) = std::str::from_utf8(bytes) {
        return Cow::Borrowed(text);
    }
    let mut text = String::with_capacity(bytes.len() * 2);
    for chunk in bytes.utf8_chunks() {
        text.push_str(chunk.valid());
        text.extend(chunk.invalid().iter().copied().map(windows_1252));
    }
    Cow::Owned(text)
}

/// Maps a byte to its Windows-1252 character. That is Latin-1 except for
/// 0x80–0x9F, where Windows-1252 has printable characters (€, curly
/// quotes, dashes, …) instead of C1 controls; its five unassigned bytes
/// keep their C1 code points, as in the WHATWG encoding standard.
fn windows_1252(byte: u8) -> char {
    const C1_RANGE: [char; 32] = [
        '€', '\u{81}', '‚', 'ƒ', '„', '…', '†', '‡', 'ˆ', '‰', 'Š', '‹', 'Œ', '\u{8d}', 'Ž',
        '\u{8f}', '\u{90}', '‘', '’', '“', '”', '•', '–', '—', '˜', '™', 'š', '›', 'œ', '\u{9d}',
        'ž', 'Ÿ',
    ];
    match byte {
        0x80..=0x9F => C1_RANGE[usize::from(byte - 0x80)],
        _ => char::from(byte),
    }
}

/// Metadata carried by one `#EXTINF` directive.
#[derive(Debug)]
struct ExtInf {
    name: String,
    tvg_id: Option<String>,
    group: Option<String>,
}

/// Decodes the payload of an `#EXTINF:` line (everything after the colon):
/// `<duration> [key="value" …],<display name>`.
///
/// Returns `None` when there is no attribute/name separator comma, which is
/// the one shape we treat as malformed. Attribute values may contain commas;
/// the separator is the first comma outside double quotes.
fn parse_extinf(payload: &str) -> Option<ExtInf> {
    let (meta, name) = split_at_unquoted_comma(payload)?;
    Some(ExtInf {
        name: decode_text(name.trim()),
        tvg_id: attribute(meta, "tvg-id"),
        group: attribute(meta, "group-title"),
    })
}

/// Splits at the first comma that is not inside double quotes.
fn split_at_unquoted_comma(s: &str) -> Option<(&str, &str)> {
    let mut in_quotes = false;
    for (i, c) in s.char_indices() {
        match c {
            '"' => in_quotes = !in_quotes,
            ',' if !in_quotes => return Some((&s[..i], &s[i + 1..])),
            _ => {}
        }
    }
    None
}

/// Extracts the value of a `key="value"` attribute from `#EXTINF` metadata.
///
/// The metadata is scanned as a sequence of tokens (the leading duration and
/// any tokens that are not `name="value"` pairs are skipped), so `key`
/// matches only as a whole attribute name — never as a substring of another
/// name (`x-tvg-id`) and never inside another attribute's quoted value.
/// A malformed quoted value is skipped so later attributes can still be
/// recovered. XML entities are decoded because generated and third-party M3U
/// files commonly use them to represent quotes and ampersands inside values.
fn attribute(meta: &str, key: &str) -> Option<String> {
    let bytes = meta.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // Skip whitespace between tokens.
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        // Read this token's name, up to '=' or the next whitespace.
        let name_start = i;
        while i < bytes.len() && bytes[i] != b'=' && !bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        let name = &meta[name_start..i];
        // Only `name="value"` tokens carry a value; anything else (the
        // leading duration, bare tokens) is skipped by the outer loop.
        if i < bytes.len() && bytes[i] == b'=' {
            i += 1;
            if i < bytes.len() && bytes[i] == b'"' {
                i += 1;
                let value_start = i;
                let mut next_attribute = None;
                while i < bytes.len() && bytes[i] != b'"' {
                    if bytes[i].is_ascii_whitespace() {
                        let mut next = i;
                        while next < bytes.len() && bytes[next].is_ascii_whitespace() {
                            next += 1;
                        }
                        let mut equals = next;
                        while equals < bytes.len()
                            && bytes[equals] != b'='
                            && !bytes[equals].is_ascii_whitespace()
                        {
                            equals += 1;
                        }
                        if equals + 1 < bytes.len()
                            && bytes[equals] == b'='
                            && bytes[equals + 1] == b'"'
                        {
                            next_attribute = Some(next);
                            break;
                        }
                    }
                    i += 1;
                }
                if let Some(next) = next_attribute {
                    i = next;
                    continue;
                }
                if i >= bytes.len() {
                    // Unterminated final attribute: treat it as absent.
                    break;
                }
                let value = &meta[value_start..i];
                i += 1; // Consume the closing quote.
                if name == key {
                    return Some(decode_text(value));
                }
            }
        }
    }
    None
}

fn decode_text(text: &str) -> String {
    let mut decoded = String::with_capacity(text.len());
    let mut remaining = text;
    while let Some(start) = remaining.find('&') {
        decoded.push_str(&remaining[..start]);
        let entity = &remaining[start..];
        let Some(end) = entity.find(';').filter(|&end| end <= 16) else {
            decoded.push('&');
            remaining = &entity[1..];
            continue;
        };
        let candidate = &entity[..=end];
        if let Ok(value) = quick_xml::escape::unescape(candidate) {
            decoded.push_str(&value);
            remaining = &entity[end + 1..];
        } else {
            decoded.push('&');
            remaining = &entity[1..];
        }
    }
    decoded.push_str(remaining);
    decoded
}

/// Interns `name`, returning the id of an existing entry when possible.
fn intern(groups: &mut Vec<String>, ids: &mut HashMap<String, GroupId>, name: String) -> GroupId {
    match ids.entry(name) {
        Entry::Occupied(occupied) => *occupied.get(),
        Entry::Vacant(vacant) => {
            let id = groups.len();
            // One clone per *distinct* group name (a handful per playlist):
            // the map owns the key, the table owns the display copy.
            groups.push(vacant.key().clone());
            vacant.insert(id);
            id
        }
    }
}

#[cfg(test)]
// unwrap is fine in tests (see AGENTS.md).
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn parse(input: &str) -> Playlist {
        Playlist::from_reader(input.as_bytes()).unwrap()
    }

    #[test]
    fn parses_extinf_entry_with_attributes() {
        let playlist = parse(
            "#EXTM3U\n\
             #EXTINF:-1 tvg-id=\"one.tv\" tvg-logo=\"http://x/l.png\" group-title=\"News\",Channel One\n\
             http://example.com/one\n",
        );
        assert_eq!(playlist.channels.len(), 1);
        assert_eq!(playlist.skipped, 0);
        let channel = &playlist.channels[0];
        assert_eq!(channel.name, "Channel One");
        assert_eq!(channel.url, "http://example.com/one");
        assert_eq!(channel.tvg_id.as_deref(), Some("one.tv"));
        assert_eq!(playlist.group_name(channel.group.unwrap()), Some("News"));
    }

    #[test]
    fn interns_repeated_group_names() {
        let playlist = parse(
            "#EXTINF:-1 group-title=\"News\",A\nhttp://u/a\n\
             #EXTINF:-1 group-title=\"Sports\",B\nhttp://u/b\n\
             #EXTINF:-1 group-title=\"News\",C\nhttp://u/c\n",
        );
        assert_eq!(playlist.groups(), ["News", "Sports"]);
        assert_eq!(playlist.channels[0].group, playlist.channels[2].group);
        assert_ne!(playlist.channels[0].group, playlist.channels[1].group);
    }

    #[test]
    fn accepts_bare_url_lines_as_plain_m3u() {
        let playlist = parse("http://example.com/a\nhttp://example.com/b\n");
        assert_eq!(playlist.channels.len(), 2);
        assert_eq!(playlist.channels[0].name, "http://example.com/a");
        assert_eq!(playlist.channels[0].group, None);
        assert_eq!(playlist.skipped, 0);
    }

    #[test]
    fn skips_malformed_extinf_and_swallows_its_url() {
        let playlist = parse(
            "#EXTINF:no comma here\nhttp://example.com/bad\n\
             #EXTINF:-1,Good\nhttp://example.com/good\n",
        );
        assert_eq!(playlist.skipped, 1);
        assert_eq!(playlist.channels.len(), 1);
        assert_eq!(playlist.channels[0].name, "Good");
    }

    #[test]
    fn counts_extinf_without_url() {
        // One #EXTINF displaced by a second one, one dangling at EOF.
        let playlist = parse("#EXTINF:-1,First\n#EXTINF:-1,Second\nhttp://u/2\n#EXTINF:-1,Last\n");
        assert_eq!(playlist.skipped, 2);
        assert_eq!(playlist.channels.len(), 1);
        assert_eq!(playlist.channels[0].name, "Second");
    }

    #[test]
    fn attribute_values_may_contain_commas() {
        let playlist = parse("#EXTINF:-1 group-title=\"News, Local\",Name\nhttp://u\n");
        let channel = &playlist.channels[0];
        assert_eq!(channel.name, "Name");
        assert_eq!(
            playlist.group_name(channel.group.unwrap()),
            Some("News, Local")
        );
    }

    #[test]
    fn decodes_xml_entities_in_metadata_and_names() {
        let playlist = parse(
            "#EXTINF:-1 tvg-id=\"one&amp;&quot;.tv\" group-title=\"Kids &quot;R&quot; Us\",A &amp; B\nhttp://u\n",
        );
        let channel = &playlist.channels[0];
        assert_eq!(channel.name, "A & B");
        assert_eq!(channel.tvg_id.as_deref(), Some("one&\".tv"));
        assert_eq!(
            playlist.group_name(channel.group.unwrap()),
            Some("Kids \"R\" Us")
        );
    }

    #[test]
    fn decodes_entities_next_to_literal_ampersands() {
        let playlist =
            parse("#EXTINF:-1 group-title=\"Rock & Roll &quot;Live&quot;\",A & B\nhttp://u\n");
        let channel = &playlist.channels[0];
        assert_eq!(channel.name, "A & B");
        assert_eq!(
            playlist.group_name(channel.group.unwrap()),
            Some("Rock & Roll \"Live\"")
        );
    }

    #[test]
    fn handles_bom_crlf_blank_lines_and_comments() {
        let playlist =
            parse("\u{feff}#EXTM3U\r\n\r\n# a comment\r\n#EXTINF:-1,A\r\nhttp://u/a\r\n");
        assert_eq!(playlist.channels.len(), 1);
        assert_eq!(playlist.channels[0].name, "A");
        assert_eq!(playlist.channels[0].url, "http://u/a");
        assert_eq!(playlist.skipped, 0);
    }

    #[test]
    fn latin1_names_and_groups_do_not_abort_the_load() {
        // Regression: one non-UTF-8 byte failed the whole load with
        // InvalidData; Latin-1/Windows-1252 playlists are common.
        let input = b"#EXTM3U\n\
            #EXTINF:-1 group-title=\"M\xfasica\",Caf\xe9 \x80 \x93Live\x94\n\
            http://u/1\n\
            #EXTINF:-1 group-title=\"News\",Plain\n\
            http://u/2\n";
        let playlist = Playlist::from_reader(&input[..]).unwrap();
        assert_eq!(playlist.channels.len(), 2);
        let channel = &playlist.channels[0];
        assert_eq!(channel.name, "Café € “Live”");
        assert_eq!(playlist.group_name(channel.group.unwrap()), Some("Música"));
        assert_eq!(playlist.channels[1].name, "Plain");
        assert_eq!(playlist.skipped, 0);
    }

    #[test]
    fn decode_line_keeps_utf8_and_reads_stray_bytes_as_windows_1252() {
        assert!(matches!(
            decode_line("Música\n".as_bytes()),
            Cow::Borrowed("Música\n")
        ));
        // Valid UTF-8 runs survive next to an invalid byte on the same line.
        assert_eq!(decode_line(b"Caf\xc3\xa9 / Caf\xe9"), "Café / Café");
        assert_eq!(decode_line(b"\x80\x81\x9f\xa0\xff"), "€\u{81}Ÿ\u{a0}ÿ");
        // A BOM still decodes to U+FEFF for push_line to strip.
        assert_eq!(
            decode_line(b"\xef\xbb\xbf#EXTM3U \xe9"),
            "\u{feff}#EXTM3U é"
        );
    }

    #[test]
    fn bom_with_latin1_content_is_still_stripped() {
        let input =
            b"\xef\xbb\xbf#EXTM3U url-tvg=\"http://x/epg\"\r\n#EXTINF:-1,Ni\xf1o\r\nhttp://u/a\r\n";
        let playlist = Playlist::from_reader(&input[..]).unwrap();
        assert_eq!(playlist.channels.len(), 1);
        assert_eq!(playlist.channels[0].name, "Niño");
        assert_eq!(playlist.channels[0].url, "http://u/a");
    }

    #[test]
    fn bom_after_leading_blank_line_is_accepted() {
        let mut builder = PlaylistBuilder::new();
        builder.push_line("");
        builder.push_line("\u{feff}#EXTM3U url-tvg=\"http://example.com/epg.xml\"");
        builder.push_line("#EXTINF:-1,A");
        builder.push_line("http://u/a");

        assert_eq!(builder.tvg_url(), Some("http://example.com/epg.xml"));
        assert_eq!(builder.finish().channels.len(), 1);
    }

    #[test]
    fn empty_display_name_falls_back_to_url() {
        let playlist = parse("#EXTINF:-1,\nhttp://example.com/x\n");
        assert_eq!(playlist.channels[0].name, "http://example.com/x");
    }

    #[test]
    fn key_must_be_a_whole_word() {
        let playlist = parse("#EXTINF:-1 x-tvg-id=\"wrong\",A\nhttp://u\n");
        assert_eq!(playlist.channels[0].tvg_id, None);
    }

    #[test]
    fn key_inside_another_quoted_value_is_not_matched() {
        // Regression: a key name embedded in another attribute's quoted
        // value must not be mistaken for the real attribute.
        let playlist =
            parse("#EXTINF:-1 tvg-name=\"x group-title=Fake\" group-title=\"Real\",N\nhttp://u\n");
        let channel = &playlist.channels[0];
        assert_eq!(playlist.group_name(channel.group.unwrap()), Some("Real"));
    }

    #[test]
    fn attributes_need_not_be_whitespace_separated() {
        // Regression: an attribute glued to the previous closing quote was
        // dropped because the old parser required leading whitespace.
        let playlist = parse("#EXTINF:-1 group-title=\"A\"tvg-id=\"B\",N\nhttp://u\n");
        let channel = &playlist.channels[0];
        assert_eq!(channel.tvg_id.as_deref(), Some("B"));
        assert_eq!(playlist.group_name(channel.group.unwrap()), Some("A"));
    }

    #[test]
    fn unterminated_attribute_quote_is_absent() {
        assert_eq!(attribute("-1 tvg-id=\"abc", "tvg-id"), None);
    }

    #[test]
    fn malformed_attribute_does_not_hide_a_later_attribute() {
        let metadata = "-1 tvg-id=\"broken group-title=\"News\"";
        assert_eq!(attribute(metadata, "tvg-id"), None);
        assert_eq!(attribute(metadata, "group-title"), Some("News".to_owned()));
    }

    #[test]
    fn header_url_tvg_is_captured() {
        let mut builder = PlaylistBuilder::new();
        builder.push_line("#EXTM3U url-tvg=\"http://example.com/epg.xml.gz\"");
        builder.push_line("#EXTINF:-1,A");
        builder.push_line("http://u/a");
        assert_eq!(
            builder.tvg_url(),
            Some("http://example.com/epg.xml.gz"),
            "url-tvg should be read from the header"
        );
    }

    #[test]
    fn header_x_tvg_url_spelling_is_accepted() {
        let mut builder = PlaylistBuilder::new();
        builder.push_line("#EXTM3U x-tvg-url=\"http://example.com/guide.xml\"");
        assert_eq!(builder.tvg_url(), Some("http://example.com/guide.xml"));
    }

    #[test]
    fn plain_header_yields_no_tvg_url() {
        let mut builder = PlaylistBuilder::new();
        builder.push_line("#EXTM3U");
        builder.push_line("#EXTINF:-1,A");
        builder.push_line("http://u/a");
        assert_eq!(builder.tvg_url(), None);
    }

    #[test]
    fn parses_large_generated_playlist() {
        use std::fmt::Write as _;

        let mut input = String::from("#EXTM3U\n");
        for i in 0..100_000 {
            let group = i % 50;
            writeln!(
                input,
                "#EXTINF:-1 tvg-id=\"ch{i}.tv\" group-title=\"Group {group}\",Channel {i}\nhttp://example.com/{i}"
            )
            .unwrap();
        }
        let playlist = parse(&input);
        assert_eq!(playlist.channels.len(), 100_000);
        assert_eq!(playlist.groups().len(), 50);
        assert_eq!(playlist.skipped, 0);
        assert_eq!(playlist.channels[99_999].name, "Channel 99999");
    }
}
