//! Application state and key handling for the TUI.
//!
//! [`App`] owns the (growing) channel list, the active filter and group
//! restriction, and the selection. Rendering lives in [`crate::ui`]; the
//! binary's event loop feeds keys and [`LoadEvent`]s in here.

use std::collections::HashMap;

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::loader::LoadEvent;
use crate::playlist::{Channel, GroupId};
use crate::store::Store;

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

/// A channel the user asked to play, handed from [`App::handle_key`] to
/// the event loop (which owns the external player).
pub struct PlayRequest {
    /// Display name, for the status-bar confirmation.
    pub name: String,
    /// Stream URL to hand to the player.
    pub url: String,
}

/// Top-level TUI state.
pub struct App {
    pub(crate) channels: Vec<Channel>,
    /// Lowercase "name group" per channel, precomputed so a filter pass
    /// over a million entries stays within the latency budget.
    search_keys: Vec<String>,
    pub(crate) groups: Vec<String>,
    pub(crate) filter: String,
    filter_lower: String,
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
            groups: Vec::new(),
            filter: String::new(),
            filter_lower: String::new(),
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
                self.groups.extend(new_groups);
                self.skipped = skipped;
                self.percent = percent;
                let start = self.channels.len();
                for channel in &channels {
                    self.search_keys.push(self.search_key(channel));
                }
                self.channels.extend(channels);
                for index in start..self.channels.len() {
                    self.url_index
                        .entry(self.channels[index].url.clone())
                        .or_insert(index);
                }
                if self.view == View::All {
                    // Appending can't invalidate existing matches, so
                    // extend the filtered index list instead.
                    for index in start..self.channels.len() {
                        if self.matches(index) {
                            self.filtered.push(index);
                        }
                    }
                } else {
                    // Favorites/recents views need the store checks and
                    // (for recents) store-defined ordering.
                    self.recompute_filter();
                }
            }
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
                self.group_cursor = self.group_filter.map_or(0, |id| id + 1);
                self.mode = Mode::Groups;
            }
            KeyCode::Char('?') => self.mode = Mode::Help,
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
                self.group_filter = self.group_cursor.checked_sub(1);
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
        self.recompute_filter();
    }

    /// Rebuilds the filtered index list from scratch and clamps the
    /// selection.
    fn recompute_filter(&mut self) {
        self.filter_lower = self.filter.to_lowercase();
        self.filtered = match (self.view, &self.store) {
            // Recents ordering comes from the store (newest first), not
            // from playlist order.
            (View::Recents, Some(store)) => store
                .recents()
                .iter()
                .filter_map(|url| self.url_index.get(url).copied())
                .filter(|&index| self.matches(index))
                .collect(),
            (View::All, _) => (0..self.channels.len())
                .filter(|&index| self.matches(index))
                .collect(),
            (View::Favorites, Some(store)) => (0..self.channels.len())
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

    /// Group restriction and text filter (view membership is handled in
    /// [`Self::recompute_filter`]).
    fn matches(&self, index: usize) -> bool {
        let group_ok = self
            .group_filter
            .is_none_or(|id| self.channels[index].group == Some(id));
        group_ok
            && (self.filter_lower.is_empty()
                || self.search_keys[index].contains(&self.filter_lower))
    }

    /// Lowercase haystack for filtering: channel name plus group name.
    fn search_key(&self, channel: &Channel) -> String {
        let mut key = channel.name.to_lowercase();
        if let Some(name) = channel.group.and_then(|id| self.groups.get(id)) {
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
    }
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
}
