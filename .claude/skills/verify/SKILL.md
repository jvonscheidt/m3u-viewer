---
name: verify
description: Drive the m3u-viewer TUI end-to-end on Windows and capture what it renders.
---

# Verifying m3u-viewer

Build with `cargo build`; the binary is `target/debug/m3u-viewer.exe`.
**Rebuild right before driving** — `cargo test`/`clippy` alone can leave a
stale exe behind.

## Drive the TUI headlessly (Git Bash + winpty)

winpty (bundled with Git for Windows) gives the app a console and
`-Xplain` strips escapes so the capture is greppable plain text:

```bash
(sleep 3; printf 'q') | winpty -Xallow-non-tty -Xplain \
    target/debug/m3u-viewer.exe sample.m3u > out.txt 2>&1
grep -a "Channel" out.txt
```

- Keys go through stdin: `printf 'e'` for plain keys, `printf '\x1b[B'`
  for Down arrow; sleep between sends so frames land.
- The capture concatenates *all* repaints — grep counts tell you whether
  something disappeared after a key press.
- winpty prints an `Assertion failed … winpty.cc` line at teardown;
  that's winpty noise, not the app.

## Fixtures

Generate playlists/XMLTV in the scratchpad (never check fixtures in).
For EPG, timestamps are `date -u -d '-1 hour' +%Y%m%d%H%M%S` etc. so a
programme brackets "now". A local file path works anywhere a guide URL
is accepted (`url-tvg="C:/…"` or `--epg guide.xml`).

## Where things land

- Log: `%APPDATA%/m3u-viewer/config/m3u-viewer.log` — truncated on every
  launch, so read it straight after the run you care about.
- Config: `%APPDATA%/m3u-viewer/config/config.toml`.
