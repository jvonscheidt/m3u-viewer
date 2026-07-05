# CLAUDE.md

## Project

m3u-viewer — a terminal (TUI) viewer for large M3U playlists, written in Rust.

## Toolchain & everyday commands

Stable Rust, pinned via `rust-toolchain.toml`. Use these commands (never invoke `rustc` directly):

- Build: `cargo build` (optimized: `cargo build --release`)
- Run: `cargo run -- <playlist.m3u>`
- Test: `cargo test`
- Format: `cargo fmt` (verify only: `cargo fmt --check`)
- Lint: `cargo clippy --all-targets --all-features -- -D warnings`
- Docs: `cargo doc --no-deps`

Before declaring any change done, all three must pass locally:
`cargo fmt --check`, `cargo clippy --all-targets --all-features -- -D warnings`, `cargo test`.

## Formatting (rustfmt)

- `cargo fmt` output is authoritative. Never hand-format or fight the formatter.
- Project-wide deviations from default style go in `rustfmt.toml` at the repo
  root — do not use `#[rustfmt::skip]` except for tabular test data, with a
  comment saying why.

## Linting (clippy)

- Warnings are errors: CI runs clippy with `-D warnings`; keep the tree
  clippy-clean at all times.
- Lint levels are configured centrally in `Cargo.toml` under `[lints.rust]` /
  `[lints.clippy]` — not through scattered attributes.
  - `clippy::all` and `clippy::pedantic` are enabled. If a pedantic lint is
    noisy across the codebase, downgrade it once in `[lints.clippy]`.
  - A local `#[allow(clippy::…)]` needs a one-line comment justifying it.
- `unwrap()`/`expect()` are banned in application code (`clippy::unwrap_used`);
  they are fine in tests. `expect()` with a message is acceptable only for
  invariants that are provably unreachable.

## Code conventions

- Error handling: `anyhow` at the binary edge, `thiserror` for typed errors in
  library modules. Recoverable conditions return `Result` — no `panic!`.
- Don't `clone()` just to satisfy the borrow checker; restructure first, and
  comment any clone that looks avoidable but isn't.
- Public items get `///` doc comments; each module starts with a `//!` overview.
- Unit tests live next to the code in `#[cfg(test)] mod tests`; integration
  tests go in `tests/`. Every bug fix comes with a regression test.
- Test fixtures for large-playlist behavior are generated in the test, never
  checked in as multi-megabyte files.

## Dependencies

- Prefer the standard library and small, well-maintained crates. New
  dependencies must be justified in the PR description.
- After touching `Cargo.toml`, run `cargo audit` (vulnerabilities) and keep
  `Cargo.lock` committed.

## Git & GitHub

- Default branch is `main`; minor changes commit to it directly. Major changes get branched:
  `feat/<topic>`, `fix/<topic>`, `chore/<topic>`.
- Conventional Commits: `feat:`, `fix:`, `docs:`, `refactor:`, `test:`,
  `chore:`. Imperative subject ≤ 72 chars; body explains *why*, not *what*.
- One logical change per commit; run the fmt/clippy/test trio before each one.
- Update branches with `git pull --rebase`; never force-push a shared branch.
- Use the `gh` CLI for GitHub work (`gh pr create`, `gh issue view`, …). Keep
  PRs small and single-purpose; CI (fmt check, clippy, tests) must be green
  before merge.
- Update the README before tagging a new version.
