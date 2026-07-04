//! Streaming parser and in-memory model for M3U/M3U8 playlists.
//!
//! The parser makes a single pass over a buffered reader: `#EXTINF`
//! directives are decoded into [`Channel`] entries, `group-title` values are
//! interned into a flat table, and malformed entries are counted in
//! [`Playlist::skipped`] instead of aborting the load. Only I/O failures
//! abort parsing.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Channel {
    /// Display name: the text after the comma in `#EXTINF`, or the URL
    /// itself for bare-URL entries and empty names.
    pub name: String,
    /// Stream URL (or file path) of the entry.
    pub url: String,
    /// `tvg-id` attribute, if present.
    pub tvg_id: Option<String>,
    /// Interned `group-title` attribute, if present.
    pub group: Option<GroupId>,
}

/// A parsed playlist: a flat channel list plus interned group names.
#[derive(Debug, Default)]
pub struct Playlist {
    /// All successfully parsed channels, in file order.
    pub channels: Vec<Channel>,
    /// Number of malformed entries that were skipped.
    pub skipped: usize,
    groups: Vec<String>,
}

impl Playlist {
    /// Parses a playlist from a buffered reader in a single streaming pass.
    ///
    /// Accepts extended M3U (`#EXTINF` metadata followed by a URL line) as
    /// well as plain M3U (bare URL lines). Unknown `#` directives and blank
    /// lines are ignored; a UTF-8 BOM and CRLF line endings are handled.
    ///
    /// # Errors
    ///
    /// Returns [`ParseError::Io`] if the reader fails. Malformed content is
    /// skipped and counted in [`Playlist::skipped`] instead of erroring.
    pub fn from_reader<R: BufRead>(reader: R) -> Result<Self, ParseError> {
        let mut builder = PlaylistBuilder::new();
        for line in reader.lines() {
            builder.push_line(&line?);
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
#[derive(Default)]
pub struct PlaylistBuilder {
    playlist: Playlist,
    group_ids: HashMap<String, GroupId>,
    /// Metadata of the `#EXTINF` line waiting for its URL line.
    pending: Option<ExtInf>,
    /// True after a malformed `#EXTINF`: its URL line is swallowed without
    /// producing a channel (the entry was already counted as skipped).
    pending_malformed: bool,
    seen_first_line: bool,
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
        let mut text = line.trim();
        if !self.seen_first_line {
            text = text.trim_start_matches('\u{feff}');
            self.seen_first_line = true;
        }
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
        } else if text.starts_with('#') {
            // #EXTM3U header and unknown directives are ignored.
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

/// Metadata carried by one `#EXTINF` directive.
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
        name: name.trim().to_owned(),
        tvg_id: attribute(meta, "tvg-id").map(str::to_owned),
        group: attribute(meta, "group-title").map(str::to_owned),
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
/// The key must start the string or follow whitespace, so that e.g.
/// `tvg-id` does not match inside `x-tvg-id`. Attributes without a closing
/// quote are treated as absent.
fn attribute<'a>(meta: &'a str, key: &str) -> Option<&'a str> {
    let mut from = 0;
    while let Some(pos) = meta[from..].find(key) {
        let start = from + pos;
        let at_boundary = start == 0 || meta.as_bytes()[start - 1].is_ascii_whitespace();
        let after = &meta[start + key.len()..];
        if at_boundary
            && let Some(value) = after.strip_prefix("=\"")
            && let Some(end) = value.find('"')
        {
            return Some(&value[..end]);
        }
        from = start + key.len();
    }
    None
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
// unwrap is fine in tests (see CLAUDE.md).
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
    fn handles_bom_crlf_blank_lines_and_comments() {
        let playlist =
            parse("\u{feff}#EXTM3U\r\n\r\n# a comment\r\n#EXTINF:-1,A\r\nhttp://u/a\r\n");
        assert_eq!(playlist.channels.len(), 1);
        assert_eq!(playlist.channels[0].name, "A");
        assert_eq!(playlist.channels[0].url, "http://u/a");
        assert_eq!(playlist.skipped, 0);
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
