# m3u-viewer

A fast terminal viewer for large M3U/M3U8 playlists, written in Rust.
Browse, filter, and play IPTV channel lists that are far too big for a
text editor — smoothly, even at 100 MB.

## Usage

### Build & run

Requires a stable [Rust toolchain](https://rustup.rs) (pinned via
`rust-toolchain.toml`) and, for playback, [VLC](https://www.videolan.org).

```console
$ cargo build --release
$ ./target/release/m3u-viewer <playlist.m3u>
```

Or directly:

```console
$ cargo run --release -- <playlist.m3u>
```

The UI opens immediately; large files keep loading in the background
while you browse (progress shows in the status bar).

### Command line

```
m3u-viewer <playlist.m3u> [--epg <url-or-file>] [--vlc <path>] [--vlc-reuse-instance]
m3u-viewer --xtream <server> --username <user> --password <pass> [--epg <url-or-file>] [--user-agent <ua>] [--vlc <path>] [--vlc-reuse-instance] [--save-config]
m3u-viewer [--vlc <path>]   (with saved Xtream credentials)
```

- `<playlist.m3u>` — the playlist to open (`.m3u` or `.m3u8`, UTF-8).
- `--xtream <server>` — instead of a file, load the playlist of an
  Xtream Codes account. `<server>` is the provider's base URL (e.g.
  `http://provider.example:8080`; `http://` is assumed if omitted).
  Requires `--username` and `--password`. The playlist is downloaded via
  the account's `get.php` endpoint and streams into the viewer while it
  arrives. If the provider has disabled the M3U download (some panels
  block `get.php` entirely), the live channel list is fetched through
  the Xtream player API (`player_api.php`) instead, with categories as
  groups. Note that the credentials are visible in your shell history
  and process list.
- `--epg <url-or-file>` — load an [XMLTV](https://wiki.xmltv.org)
  programme guide (plain or gzipped) and show what's airing now and
  next. Usually unnecessary: playlists that name a guide in their
  `#EXTM3U url-tvg="…"` header and Xtream accounts (via `xmltv.php`)
  get EPG automatically; this flag overrides both. Can also be set as
  `epg_url` in `config.toml`; the CLI value wins.
- `--user-agent <ua>` — send this `User-Agent` header when downloading
  the playlist from the Xtream server. Some providers only answer to
  known player user agents, e.g. `--user-agent "VLC/3.0.20 LibVLC/3.0.20"`.
  Can also be set as `user_agent` in `config.toml`; the CLI value wins.
- `--save-config` — write the Xtream credentials, the user agent, the
  EPG source, and the VLC path (if given) to `config.toml` in the
  config directory so you can omit them on future invocations. Run
  once; then `m3u-viewer` with no arguments picks up the saved
  credentials automatically. The file is created if it does not exist
  yet.
- `--vlc <path>` — use this VLC executable instead of auto-detection.
  Without it, `vlc` is looked up on `PATH`, then in the standard install
  locations (e.g. `C:\Program Files\VideoLAN\VLC` on Windows,
  `/Applications/VLC.app` on macOS).
- `--vlc-reuse-instance` — play channels in a single running VLC window
  (VLC's `--one-instance`) instead of opening a new window per channel.
  Can also be set as `vlc_reuse_instance = true` in `config.toml`.

### Programme guide (EPG)

When a guide is available — from `--epg`, the playlist's
`url-tvg` header, or the Xtream account itself — the channel list
gains a "now playing" column, and a line above the status bar shows
now/next with times for the selected channel:

```
▶ 20:15–21:45 Breaking Stories & More  ·  next 21:45 Late Review
```

Channels are matched by `tvg-id`, falling back to the channel name.
The guide loads in the background (`epg…` in the status bar; `epg ✗`
plus a log entry if it fails) and never blocks browsing. Only a
12-hour window around "now" is kept, so even multi-day guides for
huge playlists stay cheap. `e` hides/shows the EPG display.

### Filtering

`/` filters over channel name and group as you type. The pattern is a
case-insensitive [regular expression](https://docs.rs/regex/latest/regex/#syntax)
— `bbc|cnn` matches either channel, `^sky sports` anchors at the
name's start. Text that doesn't (yet) compile as a regex — usually a
pattern you're still typing — falls back to a plain substring match
instead of showing "no matches", and the status bar says so. Set
`regex_filter = false` in `config.toml` to always match literally
(then `ESPN+` finds only `ESPN+`).

### Keys at a glance

Press `?` inside the viewer for the full list. The essentials:

| Key | Action |
| --- | --- |
| `/` + text | filter channels as you type (regex) |
| `g` | restrict to one group |
| `Enter` | play the selected channel in VLC |
| `f` | mark/unmark as favorite (`★`) |
| `F` / `R` / `Tab` | favorites view / recents view / cycle views |
| `e` | toggle the EPG display |
| `Esc` | clear filter and group |
| `q` | quit |

### Where your data lives

All persistent data lives in the per-user config directory — on Windows
`%APPDATA%\m3u-viewer\config\`, on Linux `~/.config/m3u-viewer/`, on
macOS `~/Library/Application Support/m3u-viewer/`.

| File | Contents |
| --- | --- |
| `config.toml` | Xtream credentials, user agent, EPG source, and VLC path (written by `--save-config`); hand-edited toggles like `regex_filter` and `vlc_reuse_instance` |
| `favorites.json` | Favorited channel URLs |
| `recents.json` | Recently played channel URLs (newest first, capped at 50) |
| `cache/` | Last successfully downloaded Xtream playlist per account, shown instantly on the next launch while the live refresh runs |
| `m3u-viewer.log` | Diagnostic log (startup, loading, playback); overwritten each run |

Favorites and recents are keyed by stream URL, so they survive playlist
re-downloads and re-ordering. **`config.toml` stores Xtream credentials
in plaintext** — the file is private to your user account but is not
encrypted. Deleting the directory resets everything.

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
- Non-M3U playlist formats (XSPF, PLS).

### Functional requirements

#### Loading & parsing

- Invocation: `m3u-viewer <playlist.m3u>`; also accepts `.m3u8` (UTF-8).
- Alternative source (since 0.2.0): `--xtream <server> --username <u>
  --password <p>` downloads the account playlist over HTTP
  (`get.php?type=m3u_plus`) and streams it through the same parser;
  progress is indeterminate when the server does not announce a
  content length.
- Config file (since 0.3.0): `--save-config` persists Xtream credentials
  and the VLC path to `config.toml` in the platform config directory;
  subsequent invocations with no arguments use the saved values
  automatically. CLI arguments always take precedence over config. The
  file stores credentials in plaintext.
- Parses `#EXTINF` metadata: channel name, `tvg-id`, `tvg-logo` (ignored),
  `group-title`, and the stream URL on the following line.
- Malformed entries are skipped, counted, and reported in the status bar —
  a bad line must never abort loading.
- Parsing runs on a background thread; the UI appears immediately and fills
  in as entries stream in, with a progress indicator until the file is
  fully loaded.

#### Browsing & filtering

- Main view: a scrollable channel list (virtualized — only visible rows are
  rendered) showing channel name and group, sorted alphabetically
  (case-insensitive) rather than playlist order — including while a large
  playlist is still streaming in.
- `/` opens a filter prompt; matching is over channel name and group,
  updated on every keystroke (debounced ≤ 50 ms).
- Filter syntax (since 0.5.0): the text is a case-insensitive regular
  expression; input that fails to compile (typically a half-typed
  pattern) degrades to a case-insensitive substring match rather than
  an empty list, with an indicator in the status bar. `regex_filter =
  false` in `config.toml` forces literal substring matching.
- Group sidebar/selector: jump to or restrict the list to one
  `group-title`; groups are listed alphabetically too.
- Filter and group restriction combine (AND).

#### Programme guide (EPG, since 0.6.0)

- XMLTV source resolution, in order: `--epg <url-or-file>` (or
  `epg_url` in `config.toml`), the playlist's `#EXTM3U url-tvg` /
  `x-tvg-url` header, and — for Xtream accounts — the panel's
  `xmltv.php` endpoint. Gzipped feeds are detected by content, not
  file name.
- The guide loads and parses on a background thread; failures surface
  as a status-bar marker plus a log entry, never as an aborted start.
- Channels are matched by `tvg-id` first, then by display name
  (case-insensitive). The list shows the current programme per
  channel; a dedicated line shows now/next with times for the
  selection.
- Only programmes within a 12-hour window around load time are kept,
  bounding memory even for multi-day guides over very large playlists.
- `e` toggles the EPG display without discarding the loaded guide.

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
| `e` | Toggle EPG display |
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
