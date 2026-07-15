## Project

m3u-viewer — TUI viewer, big M3U playlists, Rust.

## Toolchain & commands

Stable Rust, pin via `rust-toolchain.toml`. Never call `rustc` direct:

- Build: `cargo build` (release: `cargo build --release`)
- Run: `cargo run -- <playlist.m3u>`
- Test: `cargo test`
- Format: `cargo fmt` (check: `cargo fmt --check`)
- Lint: `cargo clippy --all-targets --all-features -- -D warnings`
- Docs: `cargo doc --no-deps`

Before done, all three must pass: `cargo fmt --check`, clippy above, `cargo test`.

## Formatting (rustfmt)

- `cargo fmt` output law. No hand-format, no fight formatter.
- Project-wide style diffs go in `rustfmt.toml` root. No `#[rustfmt::skip]`
  except tabular test data, comment why.

## Linting (clippy)

- Warnings = errors. CI run `-D warnings`. Tree stay clippy-clean always.
- Lint levels set central in `Cargo.toml` `[lints.rust]` / `[lints.clippy]`,
  not scattered attributes.
  - `clippy::all` + `clippy::pedantic` on. Noisy pedantic lint → downgrade once
    in `[lints.clippy]`.
  - Local `#[allow(clippy::…)]` need one-line comment why.
- `unwrap()`/`expect()` banned in app code (`clippy::unwrap_used`); fine in
  tests. `expect()` with message OK only for provably-unreachable invariants.

## Code conventions

- Errors: `anyhow` at binary edge, `thiserror` typed errors in lib modules.
  Recoverable → `Result`, no `panic!`.
- No `clone()` just to please borrow checker; restructure first. Comment any
  clone that looks avoidable but isn't.
- Public items get `///` docs; each module starts `//!` overview.
- Unit tests next to code in `#[cfg(test)] mod tests`; integration tests in
  `tests/`. Every bug fix needs regression test.
- Large-playlist test fixtures generated in test, never checked in as
  multi-megabyte files.

## Dependencies

- Prefer stdlib + small well-maintained crates. New deps justified in PR desc.
- After touch `Cargo.toml`, run `cargo audit`, keep `Cargo.lock` committed.

## Git & GitHub

- Default branch `main`; minor changes commit direct. Major changes branch:
  `feat/<topic>`, `fix/<topic>`, `chore/<topic>`.
- Conventional Commits: `feat:`, `fix:`, `docs:`, `refactor:`, `test:`,
  `chore:`. Imperative subject ≤72 chars; body explain *why* not *what*.
- One logical change per commit; run fmt/clippy/test trio before each.
- Update branches `git pull --rebase`; never force-push shared branch.
- Use `gh` CLI for GitHub work (`gh pr create`, `gh issue view`, …). Keep PRs
  small, single-purpose; CI (fmt check, clippy, tests) green before merge.
- Update README before tag new version.