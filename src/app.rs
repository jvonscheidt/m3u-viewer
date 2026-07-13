//! Application state and key handling for the TUI.
//!
//! [`App`] owns the (growing) channel list, the active filter and group
//! restriction, and the selection. Rendering lives in [`crate::ui`]; the
//! binary's event loop feeds keys and [`LoadEvent`]s in here.

use std::collections::HashMap;

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use regex::{Regex, RegexBuilder};

use crate::epg::{EpgEvent, Guide};
use crate::loader::LoadEvent;
use crate::playlist::{Channel, GroupId};
use crate::store::Store;

/// How the current filter text is matched against a channel's search key.
/// Rebuilt by [`App::rebuild_filter_matcher`] whenever the filter text or
/// the regex-filter setting changes.
enum FilterMatcher {
    /// No filter text: everything matches.
    None,
    /// Plain case-insensitive substring match — either regex mode is off,
    /// or the typed text failed to compile as a regex (most often because
    /// the user is still mid-way through typing a pattern).
    Substring(String),
    /// Case-insensitive regular expression match.
    Regex(Regex),
}

/// Input mode: decides how key presses are interpreted and what is drawn
/// on top of the channel list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Browsing the channel list.
    Normal,
    /// Editing the filter string (entered with `/`).
    Filter,
    /// Choosing a group restriction in the popup (entered with `g`).
    Groups,
    /// Help overlay (entered with `?`).
    Help,
}

/// Which subset of the playlist the channel list shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    /// Every channel.
    All,
    /// Only favorites (in playlist order).
    Favorites,
    /// Only recently played channels, newest first.
    Recents,
}

/// State of the (optional) background EPG load.
pub enum EpgState {
    /// No EPG source was configured or discovered.
    Absent,
    /// A guide is being fetched and parsed in the background.
    Loading,
    /// The guide is ready for now/next lookups.
    Ready(Guide),
    /// Loading failed; the detailed error is in the log file.
    Failed,
}

/// A channel the user asked to play, handed from [`App::handle_key`] to
/// the event loop (which owns the external player).
pub struct PlayRequest {
    /// Display name, for the status-bar confirmation.
    pub name: String,
    /// Stream URL to hand to the player.
    pub url: String,
}

/// Top-level TUI state.
// The bools are independent flags (filter mode, load progress, EPG
// visibility, quit); folding them into one state machine would be false
// structure.
#[allow(clippy::struct_excessive_bools)]
pub struct App {
    pub(crate) channels: Vec<Channel>,
    /// Lowercase "name group" per channel, precomputed so a filter pass
    /// over a million entries stays within the latency budget.
    search_keys: Vec<String>,
    /// Lowercase channel name per channel, cached so sorting never
    /// re-lowercases a name it has already seen.
    name_keys: Vec<String>,
    /// All channel indices, sorted alphabetically by name (case
    /// insensitive) and merge-updated as batches arrive — see
    /// [`Self::absorb_channels`]. The source of iteration order for both
    /// the "all channels" and favorites views.
    sorted_channels: Vec<usize>,
    pub(crate) groups: Vec<String>,
    /// `groups` ids in alphabetical order, for the group popup. Rebuilt
    /// from scratch whenever groups change: the interned group table
    /// stays orders of magnitude smaller than the channel list, so a
    /// full resort here is cheap even at playlist scale.
    pub(crate) sorted_groups: Vec<GroupId>,
    pub(crate) filter: String,
    /// Compiled form of `filter`, rebuilt whenever it or `regex_filter`
    /// changes; kept as state so a batch absorb doesn't recompile it per
    /// channel.
    filter_matcher: FilterMatcher,
    /// Whether `filter` is interpreted as a regular expression (with a
    /// substring fallback when it fails to compile). Mirrors
    /// [`crate::config::Config::regex_filter`]; set once at startup via
    /// [`Self::set_regex_filter`].
    regex_filter: bool,
    pub(crate) group_filter: Option<GroupId>,
    /// Indices into `channels` that pass the filter and group restriction.
    pub(crate) filtered: Vec<usize>,
    /// Selection as an index into `filtered`.
    pub(crate) selected: usize,
    /// First visible row (index into `filtered`).
    pub(crate) offset: usize,
    /// Rows in the channel viewport as of the last render; used for
    /// PageUp/PageDown.
    pub(crate) page_rows: usize,
    pub(crate) mode: Mode,
    pub(crate) loading: bool,
    /// Load progress 0–100; `None` when the source size is unknown.
    pub(crate) percent: Option<u8>,
    pub(crate) skipped: usize,
    pub(crate) error: Option<String>,
    pub(crate) file_name: String,
    /// Cursor in the group popup: 0 is "(all groups)", `n + 1` is group `n`.
    pub(crate) group_cursor: usize,
    /// Transient status-bar notice (playback confirmations and errors);
    /// cleared by the next key press.
    pub(crate) message: Option<String>,
    pub(crate) view: View,
    /// Favorites/recents persistence; `None` when the platform has no
    /// config directory (the features degrade to a status message).
    pub(crate) store: Option<Store>,
    /// First channel index per URL, for resolving recents to rows.
    url_index: HashMap<String, usize>,
    play_request: Option<PlayRequest>,
    /// Programme guide, once an EPG source was found and loaded.
    pub(crate) epg: EpgState,
    /// Whether EPG data is drawn (`e` toggles); meaningless until a
    /// guide is ready.
    pub(crate) epg_visible: bool,
    quit: bool,
}

impl App {
    /// Creates the state for a freshly opened, still-loading playlist.
    /// `store` carries persisted favorites/recents; `None` disables both.
    #[must_use]
    pub fn new(file_name: String, store: Option<Store>) -> Self {
        Self {
            channels: Vec::new(),
            search_keys: Vec::new(),
            name_keys: Vec::new(),
            sorted_channels: Vec::new(),
            groups: Vec::new(),
            sorted_groups: Vec::new(),
            filter: String::new(),
            filter_matcher: FilterMatcher::None,
            regex_filter: true,
            group_filter: None,
            filtered: Vec::new(),
            selected: 0,
            offset: 0,
            page_rows: 1,
            mode: Mode::Normal,
            loading: true,
            percent: None,
            skipped: 0,
            error: None,
            file_name,
            group_cursor: 0,
            message: None,
            view: View::All,
            store,
            url_index: HashMap::new(),
            play_request: None,
            epg: EpgState::Absent,
            epg_visible: true,
            quit: false,
        }
    }

    /// True once the user asked to exit.
    #[must_use]
    pub fn should_quit(&self) -> bool {
        self.quit
    }

    /// Applies a loader event: appends channels/groups or records the end
    /// of loading.
    pub fn on_load_event(&mut self, event: LoadEvent) {
        match event {
            LoadEvent::Batch {
                channels,
                new_groups,
                skipped,
                percent,
            } => {
                if !new_groups.is_empty() {
                    self.groups.extend(new_groups);
                    self.rebuild_sorted_groups();
                }
                self.skipped = skipped;
                self.percent = percent;
                let start = self.channels.len();
                for channel in &channels {
                    let name_lower = channel.name.to_lowercase();
                    self.search_keys
                        .push(self.search_key(&name_lower, channel.group));
                    self.name_keys.push(name_lower);
                }
                self.channels.extend(channels);
                for index in start..self.channels.len() {
                    self.url_index
                        .entry(self.channels[index].url.clone())
                        .or_insert(index);
                }
                self.absorb_channels(start);
            }
            LoadEvent::Reset => {
                self.channels.clear();
                self.search_keys.clear();
                self.name_keys.clear();
                self.sorted_channels.clear();
                self.groups.clear();
                self.sorted_groups.clear();
                self.url_index.clear();
                self.skipped = 0;
                self.percent = None;
                self.selected = 0;
                self.offset = 0;
                // A GroupId is only meaningful for the batch of groups it
                // was assigned alongside; group order depends on
                // first-seen order, so a restriction chosen while a
                // cached playlist was shown could silently point at the
                // wrong group once fresh data replaces it. The text
                // filter is a plain string and stays safe to keep.
                self.group_filter = None;
                self.recompute_filter();
            }
            // Consumed by the event loop in `main`, which owns EPG loading.
            LoadEvent::EpgUrl(_) => {}
            LoadEvent::Finished => {
                self.loading = false;
                self.percent = Some(100);
            }
            LoadEvent::Failed(message) => {
                self.loading = false;
                self.error = Some(message);
            }
        }
    }

    /// Marks that an EPG load has started (the status bar shows it).
    pub fn set_epg_loading(&mut self) {
        self.epg = EpgState::Loading;
    }

    /// Applies the result of a background EPG load.
    pub fn on_epg_event(&mut self, event: EpgEvent) {
        self.epg = match event {
            EpgEvent::Loaded(guide) => EpgState::Ready(guide),
            EpgEvent::Failed(_) => EpgState::Failed,
        };
    }

    /// The loaded guide, when one is ready and EPG display is enabled.
    pub(crate) fn visible_guide(&self) -> Option<&Guide> {
        match &self.epg {
            EpgState::Ready(guide) if self.epg_visible => Some(guide),
            _ => None,
        }
    }

    /// Takes the pending playback request, if the last key press created
    /// one. The event loop consumes this and talks to the player.
    pub fn take_play_request(&mut self) -> Option<PlayRequest> {
        self.play_request.take()
    }

    /// Puts a transient notice (e.g. playback confirmation or error) in
    /// the status bar; the next key press clears it.
    pub fn set_message(&mut self, message: String) {
        self.message = Some(message);
    }

    /// Records a successful playback in the recents list.
    pub fn record_played(&mut self, url: &str) {
        if let Some(store) = &mut self.store {
            if let Err(error) = store.push_recent(url) {
                self.message = Some(format!("✗ recents: {error}"));
            }
            if self.view == View::Recents {
                self.recompute_filter();
            }
        }
    }

    /// Whether the filter text is currently applied as a compiled regex
    /// (as opposed to a plain substring match).
    #[must_use]
    pub fn filter_is_regex(&self) -> bool {
        matches!(self.filter_matcher, FilterMatcher::Regex(_))
    }

    /// Whether regex mode is on but the typed text does not currently
    /// compile as a regex, so filtering has fallen back to a plain
    /// substring match.
    #[must_use]
    pub fn filter_regex_invalid(&self) -> bool {
        self.regex_filter && !self.filter.is_empty() && !self.filter_is_regex()
    }

    /// Whether the channel at `index` is a favorite.
    pub(crate) fn is_favorite(&self, index: usize) -> bool {
        self.store
            .as_ref()
            .is_some_and(|store| store.is_favorite(&self.channels[index].url))
    }

    /// Routes a key press according to the current [`Mode`].
    pub fn handle_key(&mut self, key: KeyEvent) {
        self.message = None;
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.quit = true;
            return;
        }
        match self.mode {
            Mode::Normal => self.key_normal(key),
            Mode::Filter => self.key_filter(key),
            Mode::Groups => self.key_groups(key),
            Mode::Help => self.mode = Mode::Normal,
        }
    }

    fn key_normal(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Enter => {
                if let Some(&index) = self.filtered.get(self.selected) {
                    let channel = &self.channels[index];
                    self.play_request = Some(PlayRequest {
                        name: channel.name.clone(),
                        url: channel.url.clone(),
                    });
                }
            }
            KeyCode::Char('/') => self.mode = Mode::Filter,
            KeyCode::Char('g') => {
                self.group_cursor = self
                    .group_filter
                    .map_or(0, |id| self.group_display_position(id) + 1);
                self.mode = Mode::Groups;
            }
            KeyCode::Char('?') => self.mode = Mode::Help,
            KeyCode::Char('e') => match self.epg {
                EpgState::Absent => {
                    self.message =
                        Some("✗ no EPG source (--epg, url-tvg, or an Xtream account)".to_owned());
                }
                _ => self.epg_visible = !self.epg_visible,
            },
            KeyCode::Char('f') => self.toggle_favorite(),
            KeyCode::Char('F') => {
                // Pressing the view's key again returns to the full list.
                self.switch_view(if self.view == View::Favorites {
                    View::All
                } else {
                    View::Favorites
                });
            }
            KeyCode::Char('R') => {
                self.switch_view(if self.view == View::Recents {
                    View::All
                } else {
                    View::Recents
                });
            }
            KeyCode::Tab => self.switch_view(match self.view {
                View::All => View::Favorites,
                View::Favorites => View::Recents,
                View::Recents => View::All,
            }),
            KeyCode::Esc => {
                self.filter.clear();
                self.group_filter = None;
                self.recompute_filter();
            }
            KeyCode::Up => self.move_up(1),
            KeyCode::Down => self.move_down(1),
            KeyCode::PageUp => self.move_up(self.page_rows),
            KeyCode::PageDown => self.move_down(self.page_rows),
            KeyCode::Home => self.selected = 0,
            KeyCode::End => self.selected = self.filtered.len().saturating_sub(1),
            _ => {}
        }
    }

    fn key_filter(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.filter.clear();
                self.mode = Mode::Normal;
                self.recompute_filter();
            }
            KeyCode::Enter => self.mode = Mode::Normal,
            KeyCode::Backspace => {
                self.filter.pop();
                self.recompute_filter();
            }
            KeyCode::Char(c) => {
                self.filter.push(c);
                self.recompute_filter();
            }
            _ => {}
        }
    }

    fn key_groups(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => self.mode = Mode::Normal,
            KeyCode::Up => self.group_cursor = self.group_cursor.saturating_sub(1),
            KeyCode::Down => {
                self.group_cursor = (self.group_cursor + 1).min(self.groups.len());
            }
            KeyCode::Enter => {
                self.group_filter = self
                    .group_cursor
                    .checked_sub(1)
                    .map(|position| self.sorted_groups[position]);
                self.mode = Mode::Normal;
                self.recompute_filter();
            }
            _ => {}
        }
    }

    /// Toggles favorite status of the selection and persists it.
    fn toggle_favorite(&mut self) {
        let Some(&index) = self.filtered.get(self.selected) else {
            return;
        };
        let Some(store) = &mut self.store else {
            self.message = Some("✗ favorites unavailable (no config directory)".to_owned());
            return;
        };
        if let Err(error) = store.toggle_favorite(&self.channels[index].url) {
            self.message = Some(format!("✗ favorites: {error}"));
        }
        if self.view == View::Favorites {
            // The row may have just left this view.
            self.recompute_filter();
        }
    }

    fn switch_view(&mut self, target: View) {
        if target != View::All && self.store.is_none() {
            self.message = Some("✗ favorites/recents unavailable (no config directory)".to_owned());
            return;
        }
        self.view = target;
        self.selected = 0;
        if target == View::Favorites {
            // Opening favorites should show them all, not whatever text
            // filter or group restriction was left over from browsing
            // another view.
            self.filter.clear();
            self.group_filter = None;
        }
        self.recompute_filter();
    }

    /// Switches between regex and plain substring filtering (mirrors
    /// [`crate::config::Config::regex_filter`]) and re-applies the filter.
    pub fn set_regex_filter(&mut self, enabled: bool) {
        self.regex_filter = enabled;
        self.recompute_filter();
    }

    /// Rebuilds the filtered index list from scratch and clamps the
    /// selection.
    fn recompute_filter(&mut self) {
        self.rebuild_filter_matcher();
        self.filtered = match (self.view, &self.store) {
            // Recents ordering comes from the store (newest first), not
            // alphabetically.
            (View::Recents, Some(store)) => store
                .recents()
                .iter()
                .filter_map(|url| self.url_index.get(url).copied())
                .filter(|&index| self.matches(index))
                .collect(),
            (View::All, _) => self
                .sorted_channels
                .iter()
                .copied()
                .filter(|&index| self.matches(index))
                .collect(),
            (View::Favorites, Some(store)) => self
                .sorted_channels
                .iter()
                .copied()
                .filter(|&index| {
                    store.is_favorite(&self.channels[index].url) && self.matches(index)
                })
                .collect(),
            // Unreachable via switch_view, but a storeless favorites or
            // recents view must show nothing, not everything.
            (View::Favorites | View::Recents, None) => Vec::new(),
        };
        self.selected = self.selected.min(self.filtered.len().saturating_sub(1));
        self.offset = self.offset.min(self.selected);
    }

    /// Merge-updates [`Self::sorted_channels`] (and, for the "all
    /// channels" view, [`Self::filtered`]) with the channels appended at
    /// `start..self.channels.len()`.
    ///
    /// The new slice is sorted once (cheap: one batch) and merged into
    /// the already-sorted running lists in a single linear pass, so
    /// absorbing a batch costs O(n) rather than re-sorting everything —
    /// the same budget the previous plain-append approach spent, now
    /// spent keeping alphabetical order instead of arrival order.
    fn absorb_channels(&mut self, start: usize) {
        let selected_channel = self.filtered.get(self.selected).copied();
        let mut new_indices: Vec<usize> = (start..self.channels.len()).collect();
        new_indices.sort_by(|&a, &b| self.name_keys[a].cmp(&self.name_keys[b]));
        self.sorted_channels = merge_by_key(&self.sorted_channels, &new_indices, &self.name_keys);
        if self.view == View::All {
            let matching: Vec<usize> = new_indices
                .iter()
                .copied()
                .filter(|&index| self.matches(index))
                .collect();
            self.filtered = merge_by_key(&self.filtered, &matching, &self.name_keys);
        } else {
            // Favorites/recents views need the store checks and (for
            // recents) store-defined ordering.
            self.recompute_filter();
        }
        if let Some(selected_channel) = selected_channel
            && let Some(position) = self
                .filtered
                .iter()
                .position(|&index| index == selected_channel)
        {
            self.selected = position;
        }
    }

    /// Rebuilds the alphabetical group order shown in the group popup.
    fn rebuild_sorted_groups(&mut self) {
        self.sorted_groups = (0..self.groups.len()).collect();
        let groups = &self.groups;
        self.sorted_groups
            .sort_by(|&a, &b| groups[a].to_lowercase().cmp(&groups[b].to_lowercase()));
    }

    /// Row of `id` in the group popup (its position in
    /// [`Self::sorted_groups`]).
    fn group_display_position(&self, id: GroupId) -> usize {
        self.sorted_groups
            .iter()
            .position(|&group_id| group_id == id)
            .unwrap_or(0)
    }

    /// Group restriction and text filter (view membership is handled in
    /// [`Self::recompute_filter`]).
    fn matches(&self, index: usize) -> bool {
        let group_ok = self
            .group_filter
            .is_none_or(|id| self.channels[index].group == Some(id));
        group_ok
            && match &self.filter_matcher {
                FilterMatcher::None => true,
                FilterMatcher::Substring(needle) => self.search_keys[index].contains(needle),
                FilterMatcher::Regex(re) => re.is_match(&self.search_keys[index]),
            }
    }

    /// Recompiles [`Self::filter_matcher`] from the current filter text and
    /// `regex_filter` setting. Search keys are already lowercased, so plain
    /// substring matching stays a lowercase-needle `contains`; regex
    /// patterns are compiled case-insensitively for the same effect. A
    /// pattern that fails to compile — most often because the user is
    /// still mid-way through typing it — falls back to a substring match
    /// instead of showing "no matches" for a currently-invalid regex.
    fn rebuild_filter_matcher(&mut self) {
        self.filter_matcher = if self.filter.is_empty() {
            FilterMatcher::None
        } else if self.regex_filter {
            RegexBuilder::new(&self.filter)
                .case_insensitive(true)
                .build()
                .map_or_else(
                    |_| FilterMatcher::Substring(self.filter.to_lowercase()),
                    FilterMatcher::Regex,
                )
        } else {
            FilterMatcher::Substring(self.filter.to_lowercase())
        };
    }

    /// Lowercase haystack for filtering: `name_lower` plus the group name.
    fn search_key(&self, name_lower: &str, group: Option<GroupId>) -> String {
        let mut key = name_lower.to_owned();
        if let Some(name) = group.and_then(|id| self.groups.get(id)) {
            key.push(' ');
            key.push_str(&name.to_lowercase());
        }
        key
    }

    fn move_up(&mut self, by: usize) {
        self.selected = self.selected.saturating_sub(by);
    }

    fn move_down(&mut self, by: usize) {
        let last = self.filtered.len().saturating_sub(1);
        self.selected = (self.selected + by).min(last);
    }

    /// Records the viewport height and scrolls `offset` so the selection
    /// stays visible. Called from the renderer each frame.
    pub(crate) fn ensure_visible(&mut self, rows: usize) {
        self.page_rows = rows.max(1);
        if self.selected < self.offset {
            self.offset = self.selected;
        } else if self.selected >= self.offset + self.page_rows {
            self.offset = self.selected + 1 - self.page_rows;
        }
        // Never scroll past the point where the last row sits at the bottom
        // of the viewport: otherwise a stale (large) offset left over from a
        // longer list — after a narrowing filter or a terminal enlarge —
        // would render matching rows behind a band of blank lines.
        self.offset = self
            .offset
            .min(self.filtered.len().saturating_sub(self.page_rows));
    }
}

/// Merges two channel-index lists, each already sorted by `keys[index]`,
/// into one sorted list — the linear-time counterpart to re-sorting the
/// concatenation, used to fold a newly arrived batch into a running
/// alphabetical order without re-touching the entries already placed.
fn merge_by_key(a: &[usize], b: &[usize], keys: &[String]) -> Vec<usize> {
    let mut merged = Vec::with_capacity(a.len() + b.len());
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        if keys[a[i]] <= keys[b[j]] {
            merged.push(a[i]);
            i += 1;
        } else {
            merged.push(b[j]);
            j += 1;
        }
    }
    merged.extend_from_slice(&a[i..]);
    merged.extend_from_slice(&b[j..]);
    merged
}

#[cfg(test)]
// unwrap is fine in tests (see CLAUDE.md).
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn channel(name: &str, group: Option<GroupId>) -> Channel {
        Channel {
            name: name.to_owned(),
            url: format!("http://example.com/{name}"),
            tvg_id: None,
            group,
        }
    }

    /// Unique temp dir for a store-backed test; second element is the dir
    /// for cleanup.
    fn temp_store(tag: &str) -> (Store, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("m3u-viewer-app-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        (Store::load(dir.clone()), dir)
    }

    fn loaded_app() -> App {
        loaded_app_with(None)
    }

    fn loaded_app_with(store: Option<Store>) -> App {
        let mut app = App::new("test.m3u".into(), store);
        app.on_load_event(LoadEvent::Batch {
            channels: vec![
                channel("BBC News", Some(0)),
                channel("CNN", Some(0)),
                channel("Eurosport", Some(1)),
            ],
            new_groups: vec!["News".into(), "Sports".into()],
            skipped: 0,
            percent: Some(100),
        });
        app.on_load_event(LoadEvent::Finished);
        app
    }

    #[test]
    fn batches_extend_channels_and_filtered() {
        let app = loaded_app();
        assert_eq!(app.channels.len(), 3);
        assert_eq!(app.filtered, vec![0, 1, 2]);
        assert!(!app.loading);
    }

    /// Reads back channel names in `app.filtered` order.
    fn filtered_names(app: &App) -> Vec<&str> {
        app.filtered
            .iter()
            .map(|&i| app.channels[i].name.as_str())
            .collect()
    }

    #[test]
    fn channels_display_alphabetically_regardless_of_arrival_order() {
        let mut app = App::new("test.m3u".into(), None);
        app.on_load_event(LoadEvent::Batch {
            channels: vec![
                channel("Zebra", None),
                channel("apple", None),
                channel("Mango", None),
            ],
            new_groups: Vec::new(),
            skipped: 0,
            percent: Some(100),
        });
        app.on_load_event(LoadEvent::Finished);
        // Case-insensitive: "apple" sorts before "Mango" despite the case.
        assert_eq!(filtered_names(&app), ["apple", "Mango", "Zebra"]);
    }

    #[test]
    fn later_batches_merge_into_the_existing_alphabetical_order() {
        let mut app = App::new("test.m3u".into(), None);
        app.on_load_event(LoadEvent::Batch {
            channels: vec![channel("Mango", None), channel("Zebra", None)],
            new_groups: Vec::new(),
            skipped: 0,
            percent: Some(100),
        });
        app.on_load_event(LoadEvent::Batch {
            channels: vec![channel("apple", None), channel("Kiwi", None)],
            new_groups: Vec::new(),
            skipped: 0,
            percent: Some(100),
        });
        app.on_load_event(LoadEvent::Finished);
        assert_eq!(filtered_names(&app), ["apple", "Kiwi", "Mango", "Zebra"]);
    }

    #[test]
    fn later_batches_preserve_the_selected_channel() {
        let mut app = App::new("test.m3u".into(), None);
        app.on_load_event(LoadEvent::Batch {
            channels: vec![channel("Mango", None), channel("Zebra", None)],
            new_groups: Vec::new(),
            skipped: 0,
            percent: Some(50),
        });
        app.selected = 1;

        app.on_load_event(LoadEvent::Batch {
            channels: vec![channel("apple", None), channel("Kiwi", None)],
            new_groups: Vec::new(),
            skipped: 0,
            percent: Some(100),
        });

        assert_eq!(app.selected, 3);
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(
            app.take_play_request().unwrap().url,
            "http://example.com/Zebra"
        );
    }

    #[test]
    fn favorites_view_is_also_alphabetical() {
        let (store, dir) = temp_store("fav-alpha");
        let mut app = App::new("test.m3u".into(), Some(store));
        app.on_load_event(LoadEvent::Batch {
            channels: vec![channel("Zebra", None), channel("apple", None)],
            new_groups: Vec::new(),
            skipped: 0,
            percent: Some(100),
        });
        app.on_load_event(LoadEvent::Finished);
        // Favorite them out of alphabetical order: row 1 (Zebra) first,
        // then row 0 (apple).
        app.selected = 1;
        app.handle_key(key(KeyCode::Char('f')));
        app.selected = 0;
        app.handle_key(key(KeyCode::Char('f')));
        app.handle_key(key(KeyCode::Char('F')));
        assert_eq!(app.view, View::Favorites);
        assert_eq!(filtered_names(&app), ["apple", "Zebra"]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn group_popup_lists_groups_alphabetically() {
        let mut app = App::new("test.m3u".into(), None);
        app.on_load_event(LoadEvent::Batch {
            channels: vec![channel("A", Some(0)), channel("B", Some(1))],
            new_groups: vec!["Zeta".into(), "Alpha".into()],
            skipped: 0,
            percent: Some(100),
        });
        let names: Vec<&str> = app
            .sorted_groups
            .iter()
            .map(|&id| app.groups[id].as_str())
            .collect();
        assert_eq!(names, ["Alpha", "Zeta"]);
    }

    #[test]
    fn group_cursor_finds_the_active_filter_after_reordering() {
        let mut app = App::new("test.m3u".into(), None);
        app.on_load_event(LoadEvent::Batch {
            channels: vec![channel("A", Some(0)), channel("B", Some(1))],
            // "Zeta" is group id 0 but sorts after "Alpha" (id 1).
            new_groups: vec!["Zeta".into(), "Alpha".into()],
            skipped: 0,
            percent: Some(100),
        });
        app.group_filter = Some(0); // Zeta
        app.handle_key(key(KeyCode::Char('g')));
        // Popup rows: 0 "(all groups)", 1 Alpha, 2 Zeta.
        assert_eq!(app.group_cursor, 2);
    }

    #[test]
    fn typed_filter_narrows_by_name_case_insensitively() {
        let mut app = loaded_app();
        app.handle_key(key(KeyCode::Char('/')));
        for c in "bbc".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        assert_eq!(app.mode, Mode::Filter);
        assert_eq!(app.filtered, vec![0]);
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.mode, Mode::Normal);
    }

    #[test]
    fn filter_also_matches_group_names() {
        let mut app = loaded_app();
        app.handle_key(key(KeyCode::Char('/')));
        for c in "sports".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        assert_eq!(app.filtered, vec![2]);
    }

    #[test]
    fn regex_filter_supports_alternation() {
        let mut app = loaded_app();
        app.handle_key(key(KeyCode::Char('/')));
        for c in "bbc|eurosport".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        assert!(app.filter_is_regex());
        assert_eq!(app.filtered, vec![0, 2]);
    }

    #[test]
    fn regex_metacharacters_are_interpreted_as_regex_by_default() {
        let mut app = App::new("test.m3u".into(), None);
        app.on_load_event(LoadEvent::Batch {
            channels: vec![channel("ESPN+", None), channel("ESPN", None)],
            new_groups: Vec::new(),
            skipped: 0,
            percent: Some(100),
        });
        app.on_load_event(LoadEvent::Finished);
        app.handle_key(key(KeyCode::Char('/')));
        for c in "espn+".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        assert!(app.filter_is_regex());
        // "+" is a quantifier on "N" here, not a literal character, so both
        // "ESPN" and "ESPN+" match.
        assert_eq!(app.filtered.len(), 2);
    }

    #[test]
    fn regex_filter_can_be_disabled_for_literal_substring_matching() {
        let mut app = App::new("test.m3u".into(), None);
        app.on_load_event(LoadEvent::Batch {
            channels: vec![channel("ESPN+", None), channel("ESPN", None)],
            new_groups: Vec::new(),
            skipped: 0,
            percent: Some(100),
        });
        app.on_load_event(LoadEvent::Finished);
        app.set_regex_filter(false);
        app.handle_key(key(KeyCode::Char('/')));
        for c in "espn+".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        assert!(!app.filter_is_regex());
        // Literal match: only the channel actually named "ESPN+" qualifies.
        assert_eq!(app.filtered.len(), 1);
    }

    #[test]
    fn invalid_regex_falls_back_to_substring_match() {
        let mut app = App::new("test.m3u".into(), None);
        app.on_load_event(LoadEvent::Batch {
            channels: vec![channel("ESPN[HD]", None)],
            new_groups: Vec::new(),
            skipped: 0,
            percent: Some(100),
        });
        app.on_load_event(LoadEvent::Finished);
        app.handle_key(key(KeyCode::Char('/')));
        // "espn[" does not compile as a regex (unterminated character
        // class); this is the common case of typing a pattern that isn't
        // finished yet, and must still narrow by literal substring instead
        // of showing "no matches".
        for c in "espn[".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        assert!(app.filter_regex_invalid());
        assert!(!app.filter_is_regex());
        assert_eq!(app.filtered, vec![0]);
    }

    #[test]
    fn group_selection_combines_with_filter() {
        let mut app = loaded_app();
        // Pick group "News" (cursor 1) in the popup.
        app.handle_key(key(KeyCode::Char('g')));
        app.handle_key(key(KeyCode::Down));
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.group_filter, Some(0));
        assert_eq!(app.filtered, vec![0, 1]);
        // Add a text filter on top.
        app.handle_key(key(KeyCode::Char('/')));
        app.handle_key(key(KeyCode::Char('c')));
        assert_eq!(app.filtered, vec![0, 1]); // both contain 'c' ("bbc", "cnn")
        app.handle_key(key(KeyCode::Char('n')));
        assert_eq!(app.filtered, vec![1]);
    }

    #[test]
    fn escape_clears_filter_and_group() {
        let mut app = loaded_app();
        app.handle_key(key(KeyCode::Char('g')));
        app.handle_key(key(KeyCode::Down));
        app.handle_key(key(KeyCode::Enter));
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.group_filter, None);
        assert_eq!(app.filtered.len(), 3);
    }

    #[test]
    fn navigation_clamps_to_bounds() {
        let mut app = loaded_app();
        app.handle_key(key(KeyCode::Up));
        assert_eq!(app.selected, 0);
        app.handle_key(key(KeyCode::End));
        assert_eq!(app.selected, 2);
        app.handle_key(key(KeyCode::Down));
        assert_eq!(app.selected, 2);
        app.handle_key(key(KeyCode::Home));
        assert_eq!(app.selected, 0);
    }

    #[test]
    fn appended_batches_respect_active_filter() {
        let mut app = loaded_app();
        app.handle_key(key(KeyCode::Char('/')));
        app.handle_key(key(KeyCode::Char('c')));
        assert_eq!(app.filtered, vec![0, 1]);
        app.on_load_event(LoadEvent::Batch {
            channels: vec![channel("Comedy Central", None), channel("Arte", None)],
            new_groups: Vec::new(),
            skipped: 0,
            percent: Some(100),
        });
        // Only the matching newcomer joins the filtered list.
        assert_eq!(app.filtered, vec![0, 1, 3]);
    }

    #[test]
    fn reset_event_keeps_the_text_filter_but_clears_the_group_restriction() {
        // Regression: a cache-then-refresh Reset (see `crate::loader`)
        // must drop the group restriction — its GroupId only makes sense
        // for the batch of groups it was assigned alongside, and group
        // order depends on first-seen order in the (now-replaced) data —
        // but the plain-string text filter is safe to keep applying.
        let mut app = loaded_app();
        app.handle_key(key(KeyCode::Char('/')));
        for c in "bbc".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        app.handle_key(key(KeyCode::Enter));
        app.group_filter = Some(0);

        app.on_load_event(LoadEvent::Reset);

        assert!(app.channels.is_empty());
        assert!(app.groups.is_empty());
        assert!(app.filtered.is_empty());
        assert_eq!(app.filter, "bbc");
        assert_eq!(app.group_filter, None);
    }

    #[test]
    fn reset_then_batch_replaces_cached_channels_with_fresh_ones() {
        let mut app = App::new("test.m3u".into(), None);
        app.on_load_event(LoadEvent::Batch {
            channels: vec![channel("Cached", None)],
            new_groups: Vec::new(),
            skipped: 0,
            percent: Some(100),
        });
        assert_eq!(filtered_names(&app), ["Cached"]);

        app.on_load_event(LoadEvent::Reset);
        assert!(app.channels.is_empty());

        app.on_load_event(LoadEvent::Batch {
            channels: vec![channel("Fresh", None)],
            new_groups: Vec::new(),
            skipped: 0,
            percent: Some(100),
        });
        assert_eq!(filtered_names(&app), ["Fresh"]);
    }

    #[test]
    fn enter_requests_playback_of_the_selection() {
        let mut app = loaded_app();
        app.handle_key(key(KeyCode::Down));
        app.handle_key(key(KeyCode::Enter));
        let request = app.take_play_request().unwrap();
        assert_eq!(request.name, "CNN");
        assert_eq!(request.url, "http://example.com/CNN");
        // Consumed: a second take yields nothing.
        assert!(app.take_play_request().is_none());
    }

    #[test]
    fn enter_on_empty_list_requests_nothing() {
        let mut app = App::new("empty.m3u".into(), None);
        app.handle_key(key(KeyCode::Enter));
        assert!(app.take_play_request().is_none());
    }

    #[test]
    fn next_key_clears_transient_message() {
        let mut app = loaded_app();
        app.set_message("▶ CNN in VLC".into());
        assert!(app.message.is_some());
        app.handle_key(key(KeyCode::Down));
        assert!(app.message.is_none());
    }

    #[test]
    fn failed_load_surfaces_error() {
        let mut app = App::new("gone.m3u".into(), None);
        app.on_load_event(LoadEvent::Failed("boom".into()));
        assert!(!app.loading);
        assert_eq!(app.error.as_deref(), Some("boom"));
    }

    #[test]
    fn favorite_toggle_and_favorites_view() {
        let (store, dir) = temp_store("fav-view");
        let mut app = loaded_app_with(Some(store));
        // Favorite the first channel (BBC News), then open the view.
        app.handle_key(key(KeyCode::Char('f')));
        assert!(app.is_favorite(0));
        app.handle_key(key(KeyCode::Char('F')));
        assert_eq!(app.view, View::Favorites);
        assert_eq!(app.filtered, vec![0]);
        // Unfavoriting inside the view empties it immediately.
        app.handle_key(key(KeyCode::Char('f')));
        assert!(app.filtered.is_empty());
        // Pressing F again returns to the full list.
        app.handle_key(key(KeyCode::Char('F')));
        assert_eq!(app.view, View::All);
        assert_eq!(app.filtered.len(), 3);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn opening_favorites_clears_a_leftover_text_filter() {
        let (store, dir) = temp_store("fav-filter-reset");
        let mut app = loaded_app_with(Some(store));
        // Favorite BBC News and CNN.
        app.selected = 0;
        app.handle_key(key(KeyCode::Char('f')));
        app.selected = 1;
        app.handle_key(key(KeyCode::Char('f')));
        // Narrow the All view down to Eurosport, which isn't a favorite.
        app.handle_key(key(KeyCode::Char('/')));
        for c in "eurosport".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.filter, "eurosport");
        // Opening favorites must show both favorites, not the filtered
        // (empty) subset carried over from the All view.
        app.handle_key(key(KeyCode::Char('F')));
        assert_eq!(app.view, View::Favorites);
        assert!(app.filter.is_empty());
        assert_eq!(filtered_names(&app), ["BBC News", "CNN"]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn opening_favorites_clears_a_leftover_group_restriction() {
        // Regression: a group restriction (unlike the text filter) was
        // carried into the favorites view, hiding favorites from every
        // other group.
        let (store, dir) = temp_store("fav-group-reset");
        let mut app = loaded_app_with(Some(store));
        // Favorite BBC News (News) and Eurosport (Sports).
        app.selected = 0;
        app.handle_key(key(KeyCode::Char('f')));
        app.selected = 2;
        app.handle_key(key(KeyCode::Char('f')));
        // Restrict to the News group via the popup (row 1).
        app.handle_key(key(KeyCode::Char('g')));
        app.handle_key(key(KeyCode::Down));
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.group_filter, Some(0));
        // Opening favorites must show both favorites, not just News ones.
        app.handle_key(key(KeyCode::Char('F')));
        assert_eq!(app.view, View::Favorites);
        assert_eq!(app.group_filter, None);
        assert_eq!(filtered_names(&app), ["BBC News", "Eurosport"]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn recents_view_is_newest_first() {
        let (store, dir) = temp_store("rec-view");
        let mut app = loaded_app_with(Some(store));
        app.record_played("http://example.com/CNN");
        app.record_played("http://example.com/BBC News");
        app.handle_key(key(KeyCode::Char('R')));
        assert_eq!(app.view, View::Recents);
        assert_eq!(app.filtered, vec![0, 1]); // BBC (newest), then CNN
        app.record_played("http://example.com/CNN");
        assert_eq!(app.filtered, vec![1, 0]); // replay reorders
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn tab_cycles_the_three_views() {
        let (store, dir) = temp_store("tab");
        let mut app = loaded_app_with(Some(store));
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.view, View::Favorites);
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.view, View::Recents);
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.view, View::All);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn storeless_app_reports_unavailable_instead_of_switching() {
        let mut app = loaded_app();
        app.handle_key(key(KeyCode::Char('F')));
        assert_eq!(app.view, View::All);
        assert!(app.message.as_deref().unwrap().contains("unavailable"));
        app.handle_key(key(KeyCode::Char('f')));
        assert!(app.message.as_deref().unwrap().contains("unavailable"));
    }

    #[test]
    fn epg_toggle_without_a_source_reports_instead_of_flipping() {
        let mut app = loaded_app();
        app.handle_key(key(KeyCode::Char('e')));
        assert!(app.epg_visible);
        assert!(app.message.as_deref().unwrap().contains("no EPG source"));
    }

    #[test]
    fn epg_toggle_flips_visibility_once_a_guide_is_ready() {
        let mut app = loaded_app();
        app.set_epg_loading();
        assert!(matches!(app.epg, EpgState::Loading));
        assert!(app.visible_guide().is_none(), "loading is not ready");
        app.on_epg_event(EpgEvent::Loaded(Guide::default()));
        assert!(app.visible_guide().is_some());
        app.handle_key(key(KeyCode::Char('e')));
        assert!(app.visible_guide().is_none(), "toggled off");
        app.handle_key(key(KeyCode::Char('e')));
        assert!(app.visible_guide().is_some(), "toggled back on");
    }

    #[test]
    fn failed_epg_load_is_recorded_without_a_guide() {
        let mut app = loaded_app();
        app.set_epg_loading();
        app.on_epg_event(EpgEvent::Failed("boom".into()));
        assert!(matches!(app.epg, EpgState::Failed));
        assert!(app.visible_guide().is_none());
    }

    #[test]
    fn scrolling_keeps_selection_visible() {
        let mut app = loaded_app();
        app.ensure_visible(2);
        assert_eq!(app.offset, 0);
        app.handle_key(key(KeyCode::End));
        app.ensure_visible(2);
        assert_eq!(app.offset, 1); // rows 1..=2 visible, selection on 2
        app.handle_key(key(KeyCode::Home));
        app.ensure_visible(2);
        assert_eq!(app.offset, 0);
    }

    #[test]
    fn narrowing_filter_does_not_strand_offset_below_the_list() {
        // Regression: scrolling to the end of a long list left a large
        // offset that a subsequent narrowing filter did not pull back up,
        // hiding the matches behind blank rows.
        let mut app = App::new("test.m3u".into(), None);
        let channels = (0..1000)
            .map(|i| channel(&format!("Channel {i}"), None))
            .collect();
        app.on_load_event(LoadEvent::Batch {
            channels,
            new_groups: Vec::new(),
            skipped: 0,
            percent: Some(100),
        });
        app.on_load_event(LoadEvent::Finished);
        // Scroll to the bottom in a 20-row viewport.
        app.handle_key(key(KeyCode::End));
        app.ensure_visible(20);
        assert_eq!(app.offset, 980);
        // Filter down to the five "Channel 1", "10".."13"-style matches that
        // start with "Channel 1" and are short — pick "Channel 999" only.
        app.handle_key(key(KeyCode::Char('/')));
        for c in "channel 999".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        assert_eq!(app.filtered.len(), 1);
        // Offset must fall back so the single match is visible, not stranded
        // at row 980 with an empty viewport.
        app.ensure_visible(20);
        assert_eq!(app.offset, 0);
    }

    #[test]
    fn enlarging_viewport_pulls_offset_up_to_fill_it() {
        // Regression: growing the terminal must not leave the bottom rows
        // anchored high with blank space beneath them.
        let mut app = App::new("test.m3u".into(), None);
        let channels = (0..100)
            .map(|i| channel(&format!("Channel {i}"), None))
            .collect();
        app.on_load_event(LoadEvent::Batch {
            channels,
            new_groups: Vec::new(),
            skipped: 0,
            percent: Some(100),
        });
        app.on_load_event(LoadEvent::Finished);
        app.handle_key(key(KeyCode::End));
        app.ensure_visible(5); // small terminal
        assert_eq!(app.offset, 95);
        app.ensure_visible(40); // enlarged terminal
        assert_eq!(app.offset, 60); // 100 rows - 40 visible
    }
}
