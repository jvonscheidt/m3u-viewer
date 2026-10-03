//! Application state and key handling for the TUI.
//!
//! [`App`] owns the (growing) channel list, the active filter and group
//! restriction, and the selection. Rendering lives in [`crate::ui`]; the
//! binary's event loop feeds keys and [`LoadEvent`]s in here.

use std::collections::HashMap;
use std::collections::hash_map::{Entry, RandomState};
use std::fmt;
use std::hash::BuildHasher;

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use regex::{Regex, RegexBuilder};

use crate::epg::{EpgEvent, Guide};
use crate::loader::LoadEvent;
use crate::playlist::{Channel, GroupId};
use crate::store::Store;

/// How the current filter text is matched against a channel's name and
/// group (each on its own — see [`App::matches`]).
/// Rebuilt by [`App::rebuild_filter_matcher`] whenever the filter text or
/// the regex-filter setting changes.
#[derive(Debug)]
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
    /// Editing the group search string from the group popup.
    GroupSearch,
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
#[derive(Debug)]
pub enum EpgState {
    /// No EPG source was configured or discovered.
    Absent,
    /// A guide is being fetched and parsed in the background.
    Loading,
    /// The guide is ready for now/next lookups.
    Ready(Guide),
    /// Loading failed; retains the error for diagnostics.
    Failed(String),
}

/// A channel the user asked to play, handed from [`App::handle_key`] to
/// the event loop (which owns the external player).
pub struct PlayRequest {
    /// Display name, for the status-bar confirmation.
    pub(crate) name: String,
    /// Stream URL to hand to the player.
    pub(crate) url: String,
}

impl fmt::Debug for PlayRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PlayRequest")
            .field("name", &self.name)
            .field("url", &"<redacted URL>")
            .finish()
    }
}

impl PlayRequest {
    /// Channel display name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Stream URL to pass to the player.
    #[must_use]
    pub fn url(&self) -> &str {
        &self.url
    }
}

/// Top-level TUI state.
// The bools are independent flags (filter mode, load progress, EPG
// visibility, quit); folding them into one state machine would be false
// structure.
#[allow(clippy::struct_excessive_bools)]
pub struct App {
    pub(crate) channels: Vec<Channel>,
    /// Lowercase channel name per channel, cached so neither sorting nor a
    /// filter pass over a million entries re-lowercases a name it has
    /// already seen.
    name_keys: Vec<String>,
    /// All channel indices, sorted alphabetically by name (case
    /// insensitive) and merge-updated as batches arrive — see
    /// [`Self::absorb_channels`]. The source of iteration order for both
    /// the "all channels" and favorites views.
    sorted_channels: Vec<usize>,
    /// Reused merge destinations for [`Self::absorb_channels`], swapped in
    /// place of `sorted_channels`/`filtered` each batch so folding a batch
    /// into the running order reuses one growing allocation instead of
    /// allocating a fresh Vec per batch (the difference between O(n) and
    /// O(n²) allocation traffic across a large streaming load).
    sorted_scratch: Vec<usize>,
    filtered_scratch: Vec<usize>,
    pub(crate) groups: Vec<String>,
    /// Lowercase name per group id (parallel to `groups`), computed once
    /// per group rather than once per channel in it.
    group_keys: Vec<String>,
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
    /// Whether the user has picked a channel (navigated to it, played or
    /// favorited it). Only then does a streaming load keep the selection
    /// on that channel as earlier-sorting entries arrive; until then the
    /// cursor stays on the top row instead of drifting down the list.
    selection_pinned: bool,
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
    /// Non-fatal loader notice ([`LoadEvent::Warning`], e.g. a failed
    /// refresh behind a cached playlist). Unlike `message` it survives key
    /// presses; unlike `error` it leaves the rest of the status bar visible.
    pub(crate) warning: Option<String>,
    pub(crate) file_name: String,
    /// Cursor in the visible group popup rows. Row zero is the synthetic
    /// "(all groups)" entry without a search, but the first real match while
    /// searching.
    pub(crate) group_cursor: usize,
    /// Case-insensitive substring search applied inside the group popup.
    pub(crate) group_search: String,
    /// Group ids matching `group_search`, kept in alphabetical order.
    pub(crate) visible_groups: Vec<GroupId>,
    /// Rows in the group popup as of the last render; used for fast scrolling.
    pub(crate) group_page_rows: usize,
    /// Transient status-bar notice (playback confirmations and errors);
    /// cleared by the next key press.
    pub(crate) message: Option<String>,
    pub(crate) view: View,
    /// Favorites/recents persistence; `None` when the platform has no
    /// config directory (the features degrade to a status message).
    pub(crate) store: Option<Store>,
    /// First channel index per URL, for resolving recents to rows.
    url_index: UrlIndex,
    play_request: Option<PlayRequest>,
    /// Programme guide, once an EPG source was found and loaded.
    pub(crate) epg: EpgState,
    /// Whether EPG data is drawn (`e` toggles); meaningless until a
    /// guide is ready.
    pub(crate) epg_visible: bool,
    quit: bool,
}

impl fmt::Debug for App {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("App")
            .field("channels", &self.channels.len())
            .field("groups", &self.groups.len())
            .field("filtered", &self.filtered.len())
            .field("selected", &self.selected)
            .field("selection_pinned", &self.selection_pinned)
            .field("mode", &self.mode)
            .field("loading", &self.loading)
            .field("percent", &self.percent)
            .field("skipped", &self.skipped)
            .field("view", &self.view)
            .field("store", &self.store)
            .field("epg", &self.epg)
            .field("epg_visible", &self.epg_visible)
            .field("quit", &self.quit)
            .finish_non_exhaustive()
    }
}

impl App {
    /// Creates the state for a freshly opened, still-loading playlist.
    /// `store` carries persisted favorites/recents; `None` disables both.
    #[must_use]
    pub fn new(file_name: String, store: Option<Store>) -> Self {
        Self {
            channels: Vec::new(),
            name_keys: Vec::new(),
            sorted_channels: Vec::new(),
            sorted_scratch: Vec::new(),
            filtered_scratch: Vec::new(),
            groups: Vec::new(),
            group_keys: Vec::new(),
            sorted_groups: Vec::new(),
            filter: String::new(),
            filter_matcher: FilterMatcher::None,
            regex_filter: true,
            group_filter: None,
            filtered: Vec::new(),
            selected: 0,
            selection_pinned: false,
            offset: 0,
            page_rows: 1,
            mode: Mode::Normal,
            loading: true,
            percent: None,
            skipped: 0,
            error: None,
            warning: None,
            file_name,
            group_cursor: 0,
            group_search: String::new(),
            visible_groups: Vec::new(),
            group_page_rows: 1,
            message: None,
            view: View::All,
            store,
            url_index: UrlIndex::default(),
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
                    self.group_keys
                        .extend(new_groups.iter().map(|name| name.to_lowercase()));
                    self.groups.extend(new_groups);
                    self.rebuild_sorted_groups();
                }
                self.skipped = skipped;
                self.percent = percent;
                let start = self.channels.len();
                self.name_keys
                    .extend(channels.iter().map(|channel| channel.name.to_lowercase()));
                self.channels.extend(channels);
                for index in start..self.channels.len() {
                    self.url_index.insert(&self.channels, index);
                }
                self.absorb_channels(start);
            }
            LoadEvent::Reset => {
                self.channels.clear();
                self.name_keys.clear();
                self.sorted_channels.clear();
                self.groups.clear();
                self.group_keys.clear();
                self.sorted_groups.clear();
                self.visible_groups.clear();
                self.group_search.clear();
                self.group_cursor = 0;
                self.url_index.clear();
                self.skipped = 0;
                self.percent = None;
                self.selected = 0;
                self.selection_pinned = false;
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
            LoadEvent::Warning(message) => self.warning = Some(message),
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
            EpgEvent::Failed(message) => EpgState::Failed(message),
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
    ///
    /// In the recents view the played channel moves to the top of the
    /// list, so the selection follows it there: otherwise the cursor would
    /// stay on the old row index — now a different channel — and the next
    /// `Enter` would play something else.
    pub fn record_played(&mut self, url: &str) {
        if let Some(store) = &mut self.store {
            if let Err(error) = store.push_recent(url) {
                self.message = Some(format!("✗ recents: {error}"));
            }
            if self.view == View::Recents {
                self.recompute_filter();
                if let Some(position) = self
                    .filtered
                    .iter()
                    .position(|&index| self.channels[index].url == url)
                {
                    self.selected = position;
                    self.clamp_selection();
                }
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
            Mode::GroupSearch => self.key_group_search(key),
            Mode::Help => self.mode = Mode::Normal,
        }
    }

    fn key_normal(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Enter => {
                if let Some(&index) = self.filtered.get(self.selected) {
                    self.selection_pinned = true;
                    let channel = &self.channels[index];
                    self.play_request = Some(PlayRequest {
                        name: channel.name.clone(),
                        url: channel.url.clone(),
                    });
                }
            }
            KeyCode::Char('/') => self.mode = Mode::Filter,
            KeyCode::Char('g') => {
                self.group_search.clear();
                self.rebuild_visible_groups();
                self.group_cursor = self
                    .group_filter
                    .and_then(|id| self.group_row(id))
                    .unwrap_or(0);
                self.mode = Mode::Groups;
            }
            KeyCode::Char('?') => self.mode = Mode::Help,
            KeyCode::Char('e') => match &self.epg {
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
                self.filter_changed();
            }
            KeyCode::Up => self.move_up(1),
            KeyCode::Down => self.move_down(1),
            KeyCode::PageUp => self.move_up(self.page_rows),
            KeyCode::PageDown => self.move_down(self.page_rows),
            KeyCode::Home => {
                // Back to following the top of the list.
                self.selected = 0;
                self.selection_pinned = false;
            }
            KeyCode::End => {
                self.selected = self.filtered.len().saturating_sub(1);
                self.selection_pinned = true;
            }
            _ => {}
        }
    }

    fn key_filter(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.filter.clear();
                self.mode = Mode::Normal;
                self.filter_changed();
            }
            KeyCode::Enter => self.mode = Mode::Normal,
            KeyCode::Backspace => {
                self.filter.pop();
                self.filter_changed();
            }
            KeyCode::Char(c) if is_text_input(c, key.modifiers) => {
                self.filter.push(c);
                self.filter_changed();
            }
            _ => {}
        }
    }

    fn key_groups(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => self.mode = Mode::Normal,
            KeyCode::Char('/') => self.mode = Mode::GroupSearch,
            KeyCode::Enter => self.select_group(),
            code => self.navigate_groups(code),
        }
    }

    /// Group search input. Navigation keys keep moving the cursor through
    /// the matches while typing, so `Enter` can pick any of them, not just
    /// the first; `Esc` ends the search but keeps the highlighted group
    /// under the cursor in the full list.
    fn key_group_search(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                let highlighted = self.group_under_cursor();
                self.group_search.clear();
                self.rebuild_visible_groups();
                self.group_cursor = highlighted.and_then(|id| self.group_row(id)).unwrap_or(0);
                self.mode = Mode::Groups;
            }
            KeyCode::Enter => self.select_group(),
            KeyCode::Backspace => {
                self.group_search.pop();
                self.rebuild_visible_groups();
                self.group_cursor = 0;
            }
            KeyCode::Char(c) if is_text_input(c, key.modifiers) => {
                self.group_search.push(c);
                self.rebuild_visible_groups();
                self.group_cursor = 0;
            }
            code => self.navigate_groups(code),
        }
    }

    /// Moves the group popup cursor for the arrow/paging keys; any other
    /// key is ignored.
    fn navigate_groups(&mut self, code: KeyCode) {
        match code {
            KeyCode::Up => self.group_cursor = self.group_cursor.saturating_sub(1),
            KeyCode::Down => self.move_group_down(1),
            KeyCode::PageUp => {
                self.group_cursor = self.group_cursor.saturating_sub(self.group_page_rows);
            }
            KeyCode::PageDown => self.move_group_down(self.group_page_rows),
            KeyCode::Home => self.group_cursor = 0,
            KeyCode::End => self.group_cursor = self.group_item_count().saturating_sub(1),
            _ => {}
        }
    }

    /// Group shown on the popup row under [`Self::group_cursor`]; `None`
    /// for the synthetic "(all groups)" row or an empty search result.
    fn group_under_cursor(&self) -> Option<GroupId> {
        if self.group_search.is_empty() {
            self.group_cursor
                .checked_sub(1)
                .and_then(|position| self.visible_groups.get(position).copied())
        } else {
            self.visible_groups.get(self.group_cursor).copied()
        }
    }

    /// Popup row currently showing `id`, if it is visible at all (the
    /// "(all groups)" row shifts real groups down by one without a search).
    fn group_row(&self, id: GroupId) -> Option<usize> {
        let position = self.visible_groups.iter().position(|&group| group == id)?;
        Some(position + usize::from(self.group_search.is_empty()))
    }

    fn select_group(&mut self) {
        let selected = self.group_under_cursor();
        if !self.group_search.is_empty() && selected.is_none() {
            self.message = Some("✗ no matching groups".to_owned());
            return;
        }
        self.group_filter = selected;
        self.mode = Mode::Normal;
        self.recompute_filter();
    }

    /// Toggles favorite status of the selection and persists it.
    fn toggle_favorite(&mut self) {
        let Some(&index) = self.filtered.get(self.selected) else {
            return;
        };
        self.selection_pinned = true;
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
        self.selection_pinned = false;
        if target == View::Favorites {
            // Opening favorites should show them all, not whatever text
            // filter or group restriction was left over from browsing
            // another view.
            self.filter.clear();
            self.group_filter = None;
        }
        self.filter_changed();
    }

    /// Switches between regex and plain substring filtering (mirrors
    /// [`crate::config::Config::regex_filter`]) and re-applies the filter.
    pub fn set_regex_filter(&mut self, enabled: bool) {
        self.regex_filter = enabled;
        self.filter_changed();
    }

    /// Recompiles the matcher for a changed filter text or mode, then
    /// re-applies it. The only place the matcher is rebuilt, so a regex is
    /// compiled once per edit rather than once per list rebuild or batch.
    fn filter_changed(&mut self) {
        self.rebuild_filter_matcher();
        self.recompute_filter();
    }

    /// Rebuilds the filtered index list from scratch with the current
    /// matcher and clamps the selection.
    fn recompute_filter(&mut self) {
        self.filtered = match (self.view, &self.store) {
            // Recents ordering comes from the store (newest first), not
            // alphabetically.
            (View::Recents, Some(store)) => store
                .recents()
                .iter()
                .filter_map(|url| self.url_index.get(&self.channels, url))
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
        self.clamp_selection();
    }

    /// Merge-updates [`Self::sorted_channels`] and [`Self::filtered`] with
    /// the channels appended at `start..self.channels.len()`.
    ///
    /// The new slice is sorted once (cheap: one batch) and merged into
    /// the already-sorted running lists in a single linear pass, so
    /// absorbing a batch costs O(n) rather than re-sorting everything —
    /// the same budget the previous plain-append approach spent, now
    /// spent keeping alphabetical order instead of arrival order.
    ///
    /// A pinned selection (see [`Self::selection_pinned`]) follows its
    /// channel to its new row, keeping the same on-screen row so the
    /// viewport doesn't jump; an unpinned one stays where it is.
    fn absorb_channels(&mut self, start: usize) {
        let selected_channel = if self.selection_pinned {
            self.filtered.get(self.selected).copied()
        } else {
            None
        };
        let screen_row = self.selected.saturating_sub(self.offset);
        let mut new_indices: Vec<usize> = (start..self.channels.len()).collect();
        new_indices.sort_by(|&a, &b| self.name_keys[a].cmp(&self.name_keys[b]));
        merge_by_key_into(
            &mut self.sorted_scratch,
            &self.sorted_channels,
            &new_indices,
            &self.name_keys,
        );
        std::mem::swap(&mut self.sorted_channels, &mut self.sorted_scratch);
        match (self.view, &self.store) {
            // Store-defined (newest first) order, not alphabetical; the
            // rebuild walks at most RECENTS_CAP entries, so it stays cheap
            // no matter how large the playlist grows.
            (View::Recents, Some(_)) => self.recompute_filter(),
            // Only the new batch can add rows: the alphabetical views
            // filter just those and merge them in, instead of re-checking
            // (and, for favorites, re-hashing the URL of) every channel
            // already loaded on each batch.
            (View::All, _) | (View::Favorites, Some(_)) => {
                let favorites_only = self.store.as_ref().filter(|_| self.view == View::Favorites);
                let matching: Vec<usize> = new_indices
                    .iter()
                    .copied()
                    .filter(|&index| {
                        favorites_only
                            .is_none_or(|store| store.is_favorite(&self.channels[index].url))
                            && self.matches(index)
                    })
                    .collect();
                merge_by_key_into(
                    &mut self.filtered_scratch,
                    &self.filtered,
                    &matching,
                    &self.name_keys,
                );
                std::mem::swap(&mut self.filtered, &mut self.filtered_scratch);
            }
            // A storeless favorites/recents view stays empty.
            (View::Favorites | View::Recents, None) => {}
        }
        if let Some(selected_channel) = selected_channel
            && let Some(position) = self
                .filtered
                .iter()
                .position(|&index| index == selected_channel)
        {
            self.selected = position;
            self.offset = position.saturating_sub(screen_row);
        }
        self.clamp_selection();
    }

    /// Rebuilds the alphabetical group order shown in the group popup.
    ///
    /// Runs whenever a batch brings new groups — possibly while the popup
    /// is open — so the cursor stays on the group it was on (which may
    /// have shifted rows) instead of jumping back to "(all groups)".
    fn rebuild_sorted_groups(&mut self) {
        let cursor_group = self.group_under_cursor();
        self.sorted_groups = (0..self.groups.len()).collect();
        // Compare the cached lowercase keys: lowercasing inside the
        // comparator allocated two strings per comparison.
        let keys = &self.group_keys;
        self.sorted_groups.sort_by(|&a, &b| keys[a].cmp(&keys[b]));
        self.rebuild_visible_groups();
        self.group_cursor = cursor_group.and_then(|id| self.group_row(id)).unwrap_or(0);
    }

    /// Recomputes [`Self::visible_groups`] from the current search. Leaves
    /// [`Self::group_cursor`] alone: each caller decides where it belongs.
    fn rebuild_visible_groups(&mut self) {
        if self.group_search.is_empty() {
            self.visible_groups.clone_from(&self.sorted_groups);
        } else {
            let needle = self.group_search.to_lowercase();
            self.visible_groups = self
                .sorted_groups
                .iter()
                .copied()
                .filter(|&id| self.group_keys[id].contains(&needle))
                .collect();
        }
    }

    /// Group restriction and text filter (view membership is handled in
    /// [`Self::recompute_filter`]).
    ///
    /// The filter is tried against the channel name and the group name
    /// separately — a channel matches if either does — so anchors like
    /// `hd$` apply to each, and no pattern can match across the boundary
    /// between the two.
    fn matches(&self, index: usize) -> bool {
        let group = self.channels[index].group;
        if self.group_filter.is_some_and(|id| group != Some(id)) {
            return false;
        }
        let name = self.name_keys[index].as_str();
        let group_key = group.and_then(|id| self.group_keys.get(id));
        match &self.filter_matcher {
            FilterMatcher::None => true,
            FilterMatcher::Substring(needle) => {
                name.contains(needle.as_str())
                    || group_key.is_some_and(|key| key.contains(needle.as_str()))
            }
            FilterMatcher::Regex(re) => {
                re.is_match(name) || group_key.is_some_and(|key| re.is_match(key))
            }
        }
    }

    /// Recompiles [`Self::filter_matcher`] from the current filter text and
    /// `regex_filter` setting. Name and group keys are already lowercased, so plain
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

    fn move_up(&mut self, by: usize) {
        self.selected = self.selected.saturating_sub(by);
        self.selection_pinned = true;
    }

    fn clamp_selection(&mut self) {
        self.selected = self.selected.min(self.filtered.len().saturating_sub(1));
        self.offset = self.offset.min(self.selected);
    }

    fn move_down(&mut self, by: usize) {
        let last = self.filtered.len().saturating_sub(1);
        self.selected = (self.selected + by).min(last);
        self.selection_pinned = true;
    }

    fn group_item_count(&self) -> usize {
        self.visible_groups.len() + usize::from(self.group_search.is_empty())
    }

    fn move_group_down(&mut self, by: usize) {
        let last = self.group_item_count().saturating_sub(1);
        self.group_cursor = self.group_cursor.saturating_add(by).min(last);
    }

    /// Updates channel and group-popup viewport sizes for the current terminal
    /// height, keeping the channel selection visible.
    pub fn update_viewports(&mut self, terminal_rows: usize) {
        let reserved_rows = 1 + usize::from(self.visible_guide().is_some());
        self.ensure_visible(terminal_rows.saturating_sub(reserved_rows).max(1));
        self.group_page_rows = terminal_rows.min(17).saturating_sub(3).max(1);
    }

    fn ensure_visible(&mut self, rows: usize) {
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

/// Maps each distinct channel URL to the first channel index carrying it.
///
/// Keyed by a hash of the URL rather than the URL itself, so the index
/// holds no second copy of every URL (around 100 MB at a million
/// channels); the URL is read back from the channel list to confirm a
/// hit. The rare URL whose hash collides with a different URL's goes to a
/// small side table keyed by the full string, so collisions cost memory,
/// never correctness.
#[derive(Debug, Default)]
struct UrlIndex<S = RandomState> {
    hasher: S,
    by_hash: HashMap<u64, usize>,
    collisions: HashMap<String, usize>,
}

impl<S: BuildHasher> UrlIndex<S> {
    /// Records `channels[index]`, unless its URL is already indexed.
    fn insert(&mut self, channels: &[Channel], index: usize) {
        let url = &channels[index].url;
        match self.by_hash.entry(self.hasher.hash_one(url)) {
            Entry::Vacant(slot) => {
                slot.insert(index);
            }
            Entry::Occupied(slot) => {
                if channels[*slot.get()].url != *url {
                    // A genuine 64-bit hash collision between distinct
                    // URLs: the only case that stores a copy of the URL.
                    self.collisions.entry(url.clone()).or_insert(index);
                }
            }
        }
    }

    /// First channel index whose URL is `url`.
    fn get(&self, channels: &[Channel], url: &str) -> Option<usize> {
        let &index = self.by_hash.get(&self.hasher.hash_one(url))?;
        if channels[index].url == url {
            Some(index)
        } else {
            self.collisions.get(url).copied()
        }
    }

    fn clear(&mut self) {
        self.by_hash.clear();
        self.collisions.clear();
    }
}

/// Merges two channel-index lists, each already sorted by `keys[index]`,
/// into `out` — the counterpart to re-sorting the concatenation, used to
/// fold a newly arrived batch into a running alphabetical order without
/// re-comparing the entries already placed.
///
/// `a` is the long running list and `b` the short new batch: each `b`
/// entry binary-searches its slot in the rest of `a`, and the run of `a`
/// before it is block-copied. That costs O(|b| log |a|) key comparisons
/// plus one memcpy-speed pass over `a`, instead of a comparison — two
/// pointer-chasing string reads — per entry of `a` on every batch. On
/// equal keys, entries of `a` come first.
///
/// `out` is cleared first and must be distinct from `a` and `b`; the
/// caller passes a reused scratch buffer and swaps it into place, so a
/// streaming load reuses one growing allocation rather than allocating a
/// fresh Vec per batch.
fn merge_by_key_into(out: &mut Vec<usize>, a: &[usize], b: &[usize], keys: &[String]) {
    out.clear();
    out.reserve(a.len() + b.len());
    let mut rest = a;
    for &incoming in b {
        let key = &keys[incoming];
        let run = rest.partition_point(|&placed| keys[placed] <= *key);
        out.extend_from_slice(&rest[..run]);
        out.push(incoming);
        rest = &rest[run..];
    }
    out.extend_from_slice(rest);
}

fn is_text_input(character: char, modifiers: KeyModifiers) -> bool {
    let control = modifiers.contains(KeyModifiers::CONTROL);
    let alt = modifiers.contains(KeyModifiers::ALT);
    !character.is_control() && control == alt
}

#[cfg(test)]
// unwrap is fine in tests (see AGENTS.md).
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn modified_key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
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
        (Store::load(dir.clone()).unwrap(), dir)
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

    /// Hashes everything to the same value, forcing every distinct URL
    /// into a hash collision.
    #[derive(Default)]
    struct CollidingHasher;

    impl std::hash::Hasher for CollidingHasher {
        fn finish(&self) -> u64 {
            0
        }

        fn write(&mut self, _bytes: &[u8]) {}
    }

    #[test]
    fn url_index_resolves_urls_despite_hash_collisions() {
        let channels = vec![
            channel("A", None),
            channel("B", None),
            channel("A", None), // duplicate URL: first index wins
            channel("C", None),
        ];
        let mut index = UrlIndex::<std::hash::BuildHasherDefault<CollidingHasher>>::default();
        for i in 0..channels.len() {
            index.insert(&channels, i);
        }
        assert_eq!(index.get(&channels, "http://example.com/A"), Some(0));
        assert_eq!(index.get(&channels, "http://example.com/B"), Some(1));
        assert_eq!(index.get(&channels, "http://example.com/C"), Some(3));
        assert_eq!(index.get(&channels, "http://example.com/D"), None);
        index.clear();
        assert_eq!(index.get(&channels, "http://example.com/A"), None);
    }

    #[test]
    fn url_index_keeps_the_first_of_duplicate_urls() {
        let channels = vec![channel("A", None), channel("A", None)];
        let mut index = UrlIndex::<RandomState>::default();
        index.insert(&channels, 0);
        index.insert(&channels, 1);
        assert_eq!(index.get(&channels, "http://example.com/A"), Some(0));
        assert_eq!(index.get(&channels, "http://example.com/B"), None);
    }

    #[test]
    fn merge_matches_a_stable_sort_of_the_concatenation() {
        let keys: Vec<String> = (0..200_u64)
            .map(|i| format!("{:02}", i.wrapping_mul(0x9E37_79B9_7F4A_7C15) % 37))
            .collect();
        let sorted = |range: std::ops::Range<usize>| {
            let mut list: Vec<usize> = range.collect();
            list.sort_by(|&x, &y| keys[x].cmp(&keys[y]));
            list
        };
        for split in [0, 1, 50, 150, 199, 200] {
            let (a, b) = (sorted(0..split), sorted(split..200));
            let mut out = Vec::new();
            merge_by_key_into(&mut out, &a, &b, &keys);
            // A stable sort of a ++ b keeps a's entries first on ties.
            let mut expected: Vec<usize> = a.iter().chain(&b).copied().collect();
            expected.sort_by(|&x, &y| keys[x].cmp(&keys[y]));
            assert_eq!(out, expected, "split {split}");
        }
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
        app.handle_key(key(KeyCode::Down)); // Zebra
        app.offset = 1;

        app.on_load_event(LoadEvent::Batch {
            channels: vec![channel("apple", None), channel("Kiwi", None)],
            new_groups: Vec::new(),
            skipped: 0,
            percent: Some(100),
        });

        assert_eq!(app.selected, 3);
        // Same screen row as before the batch: the viewport moved with it.
        assert_eq!(app.offset, 3);
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(
            app.take_play_request().unwrap().url,
            "http://example.com/Zebra"
        );
    }

    #[test]
    fn later_batches_keep_an_untouched_selection_at_the_top() {
        // Regression: the selection was pinned to whichever channel sat
        // on row 0, so as earlier-sorting entries streamed in the cursor
        // (and viewport) drifted deep into the list without any input.
        let mut app = App::new("test.m3u".into(), None);
        app.on_load_event(LoadEvent::Batch {
            channels: vec![channel("Mango", None), channel("Zebra", None)],
            new_groups: Vec::new(),
            skipped: 0,
            percent: Some(50),
        });
        app.on_load_event(LoadEvent::Batch {
            channels: vec![channel("apple", None), channel("Kiwi", None)],
            new_groups: Vec::new(),
            skipped: 0,
            percent: Some(100),
        });

        assert_eq!(app.selected, 0);
        assert_eq!(app.offset, 0);
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(
            app.take_play_request().unwrap().url,
            "http://example.com/apple"
        );
    }

    #[test]
    fn home_unpins_the_selection_so_it_follows_the_top_again() {
        let mut app = App::new("test.m3u".into(), None);
        app.on_load_event(LoadEvent::Batch {
            channels: vec![channel("Mango", None), channel("Zebra", None)],
            new_groups: Vec::new(),
            skipped: 0,
            percent: Some(50),
        });
        app.handle_key(key(KeyCode::Down));
        app.handle_key(key(KeyCode::Home));
        app.on_load_event(LoadEvent::Batch {
            channels: vec![channel("apple", None)],
            new_groups: Vec::new(),
            skipped: 0,
            percent: Some(100),
        });
        assert_eq!(app.selected, 0);
        assert_eq!(filtered_names(&app)[0], "apple");
    }

    #[test]
    fn later_batches_clamp_a_stale_selection() {
        let mut app = loaded_app();
        app.selected = usize::MAX;
        app.offset = usize::MAX;

        app.on_load_event(LoadEvent::Batch {
            channels: vec![channel("Arte", None)],
            new_groups: Vec::new(),
            skipped: 0,
            percent: Some(100),
        });

        assert_eq!(app.selected, app.filtered.len() - 1);
        assert_eq!(app.offset, app.selected);
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
    fn favorites_view_absorbs_streamed_batches_incrementally() {
        let (mut store, dir) = temp_store("fav-stream");
        for name in ["Zebra", "apple", "Kiwi"] {
            store
                .toggle_favorite(&format!("http://example.com/{name}"))
                .unwrap();
        }
        let mut app = App::new("test.m3u".into(), Some(store));
        app.handle_key(key(KeyCode::Char('F')));
        app.handle_key(key(KeyCode::Char('/')));
        app.handle_key(key(KeyCode::Char('e'))); // "Zebra", "apple"
        app.handle_key(key(KeyCode::Enter));
        app.on_load_event(LoadEvent::Batch {
            channels: vec![channel("Zebra", None), channel("Mango", None)],
            new_groups: Vec::new(),
            skipped: 0,
            percent: Some(50),
        });
        app.on_load_event(LoadEvent::Batch {
            channels: vec![
                channel("apple", None),
                channel("Kiwi", None),
                channel("Melon", None),
            ],
            new_groups: Vec::new(),
            skipped: 0,
            percent: Some(100),
        });
        assert_eq!(filtered_names(&app), ["apple", "Zebra"]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn recents_view_picks_up_channels_from_later_batches() {
        let (mut store, dir) = temp_store("rec-stream");
        store.push_recent("http://example.com/Zebra").unwrap();
        store.push_recent("http://example.com/apple").unwrap();
        let mut app = App::new("test.m3u".into(), Some(store));
        app.handle_key(key(KeyCode::Char('R')));
        app.on_load_event(LoadEvent::Batch {
            channels: vec![channel("Zebra", None), channel("Mango", None)],
            new_groups: Vec::new(),
            skipped: 0,
            percent: Some(50),
        });
        assert_eq!(filtered_names(&app), ["Zebra"]);
        app.on_load_event(LoadEvent::Batch {
            channels: vec![channel("apple", None)],
            new_groups: Vec::new(),
            skipped: 0,
            percent: Some(100),
        });
        // Newest first, not alphabetical.
        assert_eq!(filtered_names(&app), ["apple", "Zebra"]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn group_popup_lists_groups_alphabetically() {
        let mut app = App::new("test.m3u".into(), None);
        app.on_load_event(LoadEvent::Batch {
            channels: vec![channel("A", Some(0)), channel("B", Some(1))],
            new_groups: vec!["Zeta".into(), "Alpha".into(), "beta".into()],
            skipped: 0,
            percent: Some(100),
        });
        let names: Vec<&str> = app
            .sorted_groups
            .iter()
            .map(|&id| app.groups[id].as_str())
            .collect();
        // Case-insensitive: "beta" sorts between "Alpha" and "Zeta".
        assert_eq!(names, ["Alpha", "beta", "Zeta"]);
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
    fn group_cursor_stays_on_its_group_when_a_batch_adds_groups() {
        // Regression: every batch with new groups reset the popup cursor
        // to "(all groups)", so Enter ignored what the user arrowed to.
        let mut app = App::new("test.m3u".into(), None);
        app.on_load_event(LoadEvent::Batch {
            channels: vec![channel("A", Some(0)), channel("B", Some(1))],
            new_groups: vec!["Movies".into(), "Sports".into()],
            skipped: 0,
            percent: Some(50),
        });
        app.handle_key(key(KeyCode::Char('g')));
        app.handle_key(key(KeyCode::Down));
        app.handle_key(key(KeyCode::Down)); // Sports, row 2
        // "Kids" sorts between them, pushing Sports down to row 3.
        app.on_load_event(LoadEvent::Batch {
            channels: vec![channel("C", Some(2))],
            new_groups: vec!["Kids".into()],
            skipped: 0,
            percent: Some(100),
        });
        assert_eq!(app.group_cursor, 3);
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.group_filter, Some(1));
    }

    #[test]
    fn group_search_cursor_stays_on_its_match_when_a_batch_adds_groups() {
        let mut app = App::new("test.m3u".into(), None);
        app.on_load_event(LoadEvent::Batch {
            channels: Vec::new(),
            new_groups: vec!["Sports HD".into(), "Sports SD".into()],
            skipped: 0,
            percent: Some(50),
        });
        app.handle_key(key(KeyCode::Char('g')));
        app.handle_key(key(KeyCode::Char('/')));
        for c in "sports".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        app.group_cursor = 1; // Sports SD
        app.on_load_event(LoadEvent::Batch {
            channels: Vec::new(),
            new_groups: vec!["Sports 4K".into()],
            skipped: 0,
            percent: Some(100),
        });
        assert_eq!(app.group_cursor, 2);
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.group_filter, Some(1));
    }

    #[test]
    fn group_search_filters_case_insensitively_and_selects_match() {
        let mut app = App::new("test.m3u".into(), None);
        app.on_load_event(LoadEvent::Batch {
            channels: vec![channel("A", Some(0)), channel("B", Some(1))],
            new_groups: vec!["News".into(), "Sports".into()],
            skipped: 0,
            percent: Some(100),
        });
        app.handle_key(key(KeyCode::Char('g')));
        app.handle_key(key(KeyCode::Char('/')));
        for c in "PORT".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        assert_eq!(app.mode, Mode::GroupSearch);
        assert_eq!(app.visible_groups, [1]);
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.mode, Mode::Normal);
        assert_eq!(app.group_filter, Some(1));
        assert_eq!(app.filtered, [1]);
    }

    #[test]
    fn group_search_escape_clears_search_without_closing_picker() {
        let mut app = loaded_app();
        app.handle_key(key(KeyCode::Char('g')));
        app.handle_key(key(KeyCode::Char('/')));
        app.handle_key(key(KeyCode::Char('x')));
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.mode, Mode::Groups);
        assert_eq!(app.group_search, "");
        assert_eq!(app.visible_groups, app.sorted_groups);
    }

    /// App with three groups, the popup open and "sports" typed into its
    /// search (matching "Sports HD" and "Sports SD", in that order).
    fn app_searching_sports_groups() -> App {
        let mut app = App::new("test.m3u".into(), None);
        app.on_load_event(LoadEvent::Batch {
            channels: Vec::new(),
            new_groups: vec!["News".into(), "Sports HD".into(), "Sports SD".into()],
            skipped: 0,
            percent: Some(100),
        });
        app.update_viewports(20);
        app.handle_key(key(KeyCode::Char('g')));
        app.handle_key(key(KeyCode::Char('/')));
        for c in "sports".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        app
    }

    #[test]
    fn group_search_can_move_between_matches() {
        // Regression: arrow keys were ignored while searching, so Enter
        // could only ever pick the first match.
        let mut app = app_searching_sports_groups();
        app.handle_key(key(KeyCode::Down));
        assert_eq!(app.mode, Mode::GroupSearch);
        assert_eq!(app.group_cursor, 1);
        app.handle_key(key(KeyCode::Down)); // clamps to the last match
        assert_eq!(app.group_cursor, 1);
        app.handle_key(key(KeyCode::Up));
        app.handle_key(key(KeyCode::PageDown));
        assert_eq!(app.group_cursor, 1);
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.group_filter, Some(2)); // Sports SD
    }

    #[test]
    fn group_search_escape_keeps_the_highlighted_group() {
        // Regression: Esc dropped the search and reset the cursor to
        // "(all groups)", losing the match the user had found.
        let mut app = app_searching_sports_groups();
        app.handle_key(key(KeyCode::Down)); // Sports SD
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.mode, Mode::Groups);
        assert_eq!(app.group_search, "");
        // Rows: (all groups), News, Sports HD, Sports SD.
        assert_eq!(app.group_cursor, 3);
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.group_filter, Some(2));
    }

    #[test]
    fn selecting_an_empty_group_search_reports_no_match() {
        let mut app = loaded_app();
        app.handle_key(key(KeyCode::Char('g')));
        app.handle_key(key(KeyCode::Char('/')));
        app.handle_key(key(KeyCode::Char('x')));
        app.handle_key(key(KeyCode::Enter));

        assert_eq!(app.mode, Mode::GroupSearch);
        assert!(
            app.message
                .as_deref()
                .unwrap()
                .contains("no matching groups")
        );
    }

    #[test]
    fn group_picker_supports_fast_scrolling() {
        let mut app = App::new("test.m3u".into(), None);
        let groups = (0..30).map(|i| format!("Group {i:02}")).collect();
        app.on_load_event(LoadEvent::Batch {
            channels: Vec::new(),
            new_groups: groups,
            skipped: 0,
            percent: Some(100),
        });
        app.handle_key(key(KeyCode::Char('g')));
        app.update_viewports(13);
        app.handle_key(key(KeyCode::PageDown));
        assert_eq!(app.group_cursor, 10);
        app.handle_key(key(KeyCode::End));
        assert_eq!(app.group_cursor, 30);
        app.handle_key(key(KeyCode::PageUp));
        assert_eq!(app.group_cursor, 20);
        app.handle_key(key(KeyCode::Home));
        assert_eq!(app.group_cursor, 0);
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
    fn text_inputs_ignore_control_and_alt_modified_characters() {
        let mut app = loaded_app();
        app.handle_key(key(KeyCode::Char('/')));
        app.handle_key(modified_key(KeyCode::Char('u'), KeyModifiers::CONTROL));
        app.handle_key(modified_key(KeyCode::Char('x'), KeyModifiers::ALT));
        app.handle_key(key(KeyCode::Char('\u{7f}')));
        assert_eq!(app.filter, "");
        app.handle_key(modified_key(
            KeyCode::Char('@'),
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        ));
        assert_eq!(app.filter, "@");

        app.handle_key(key(KeyCode::Esc));
        app.handle_key(key(KeyCode::Char('g')));
        app.handle_key(key(KeyCode::Char('/')));
        app.handle_key(modified_key(KeyCode::Char('w'), KeyModifiers::CONTROL));
        app.handle_key(modified_key(KeyCode::Char('x'), KeyModifiers::ALT));
        app.handle_key(key(KeyCode::Char('\u{7f}')));
        assert_eq!(app.group_search, "");
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
    fn filter_matches_name_and_group_separately() {
        // Regression: the filter ran over "name group" as one string, so
        // `$` anchored only at the group's end and patterns could match
        // across the gap between name and group.
        let mut app = App::new("test.m3u".into(), None);
        app.on_load_event(LoadEvent::Batch {
            channels: vec![
                channel("Sky HD", Some(0)),
                channel("BBC News", Some(1)),
                channel("Arte", Some(2)),
            ],
            new_groups: vec!["Movies".into(), "Sports".into(), "Culture HD".into()],
            skipped: 0,
            percent: Some(100),
        });
        app.on_load_event(LoadEvent::Finished);
        app.handle_key(key(KeyCode::Char('/')));
        for c in "hd$".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        // Name ending in HD, and group ending in HD.
        assert_eq!(filtered_names(&app), ["Arte", "Sky HD"]);

        app.handle_key(key(KeyCode::Esc));
        app.handle_key(key(KeyCode::Char('/')));
        for c in "news sports".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        assert_eq!(filtered_names(&app), Vec::<&str>::new());
    }

    #[test]
    fn substring_filter_also_matches_name_and_group_separately() {
        let mut app = loaded_app();
        app.set_regex_filter(false);
        app.handle_key(key(KeyCode::Char('/')));
        for c in "news news".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        // "BBC News" in group "News" used to match as "bbc news news".
        assert_eq!(app.filtered, Vec::<usize>::new());
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

        assert_eq!(app.channels, []);
        assert_eq!(app.groups, Vec::<String>::new());
        assert_eq!(app.filtered, Vec::<usize>::new());
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
        assert_eq!(app.channels, []);

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
    fn load_warning_is_kept_without_becoming_an_error() {
        let mut app = loaded_app_with(None);
        app.on_load_event(LoadEvent::Warning("refresh failed".into()));
        app.on_load_event(LoadEvent::Finished);
        assert!(app.error.is_none(), "a warning must not hide the list");
        app.handle_key(key(KeyCode::Down));
        assert_eq!(app.warning.as_deref(), Some("refresh failed"));
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
        assert_eq!(app.filtered, Vec::<usize>::new());
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
        assert_eq!(app.filter, "");
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
    fn playing_from_recents_keeps_the_selection_on_the_played_channel() {
        // Regression: the replayed channel moved to row 0 but the cursor
        // stayed on row 1, so a second Enter played a different stream.
        let (store, dir) = temp_store("rec-select");
        let mut app = loaded_app_with(Some(store));
        app.record_played("http://example.com/CNN");
        app.record_played("http://example.com/BBC News");
        app.handle_key(key(KeyCode::Char('R')));
        app.handle_key(key(KeyCode::Down)); // CNN, row 1
        app.handle_key(key(KeyCode::Enter));
        let request = app.take_play_request().unwrap();
        assert_eq!(request.name, "CNN");
        app.record_played(&request.url);

        assert_eq!(filtered_names(&app), ["CNN", "BBC News"]);
        assert_eq!(app.selected, 0);
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.take_play_request().unwrap().name, "CNN");
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
        assert!(matches!(&app.epg, EpgState::Failed(message) if message == "boom"));
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
