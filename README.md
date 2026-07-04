# m3u-viewer

A fast terminal viewer for large M3U/M3U8 playlists, written in Rust.
Browse, filter, and play IPTV channel lists that are far too big for a
text editor — smoothly, even at 100 MB.

## Specification

### Goals

- Open M3U files **up to 100 MB** (roughly 500k–1M entries) and stay
  responsive throughout.
- Instant, as-you-type filtering over the full playlist.
- One-keystroke playback of the selected channel in **VLC**.
- Persistent **favorites** and **recently played** lists across sessions.

### Non-goals

- Editing or saving playlists (read-only viewer).
- Built-in media playback — VLC is the player.
- Non-M3U formats (XSPF, PLS) and EPG/XMLTV data.

### Functional requirements

#### Loading & parsing

- Invocation: `m3u-viewer <playlist.m3u>`; also accepts `.m3u8` (UTF-8).
- Parses `#EXTINF` metadata: channel name, `tvg-id`, `tvg-logo` (ignored),
  `group-title`, and the stream URL on the following line.
- Malformed entries are skipped, counted, and reported in the status bar —
  a bad line must never abort loading.
- Parsing runs on a background thread; the UI appears immediately and fills
  in as entries stream in, with a progress indicator until the file is
  fully loaded.

#### Browsing & filtering

- Main view: a scrollable channel list (virtualized — only visible rows are
  rendered) showing channel name and group.
- `/` opens a filter prompt; matching is case-insensitive substring over
  channel name and group, updated on every keystroke (debounced ≤ 50 ms).
- Group sidebar/selector: jump to or restrict the list to one
  `group-title`.
- Filter and group restriction combine (AND).

#### Playback (VLC)

- `Enter` launches the selected channel's URL in VLC as a detached
  process; the viewer stays open.
- VLC discovery: `vlc` on `PATH`, standard install locations per OS
  (e.g. `C:\Program Files\VideoLAN\VLC\vlc.exe` on Windows), overridable
  via config file or `--vlc <path>`.
- If VLC cannot be found or spawned, show a non-blocking error in the
  status bar.

#### Favorites

- `f` toggles favorite on the selected channel; favorites are marked
  (e.g. `★`) in the list.
- A favorites view (`F`) lists only favorites.
- Persisted by channel URL (survives playlist re-ordering) in the platform
  config directory (e.g. `%APPDATA%\m3u-viewer\favorites.json`).

#### Recent channels

- Every successful playback prepends the channel to a recents list
  (deduplicated, capped at 50).
- A recents view (`R`) lists them newest-first; persisted alongside
  favorites.

### Key bindings

| Key | Action |
| --- | --- |
| `↑`/`↓`, `PgUp`/`PgDn`, `Home`/`End` | Navigate list |
| `Enter` | Play in VLC |
| `/` | Filter (type to narrow, `Esc` clears) |
| `g` | Group selector |
| `f` | Toggle favorite |
| `F` | Favorites view |
| `R` | Recent channels view |
| `Tab` | Cycle views: all / favorites / recents |
| `?` | Help overlay |
| `q` | Quit |

### Performance targets (100 MB playlist)

| Metric | Target |
| --- | --- |
| Time to interactive UI | < 200 ms |
| Full background parse | < 3 s |
| Keystroke-to-filter-result | < 100 ms |
| Scrolling | no perceptible lag (60 fps redraw budget) |
| Memory | < 4× file size resident |

### Architecture

- **Language/UI:** Rust with [ratatui](https://ratatui.rs) +
  crossterm (cross-platform terminal backend).
- **Parser:** single-pass streaming parser over a buffered reader;
  entries stored in a flat `Vec<Channel>` with interned group names.
- **Filtering:** background search over the in-memory vector; results are
  index lists into the main vector, so no entry data is copied.
- **State:** favorites/recents as small JSON files in the platform config
  dir (`directories` crate); written atomically on change.
- **Playback:** `std::process::Command` spawn, detached, stdout/stderr
  discarded.

### Future ideas (out of scope for v1)

- Stream health check (probe URLs, flag dead streams).
- Export marked channels as a new, smaller M3U.
- mpv as an alternative player.
