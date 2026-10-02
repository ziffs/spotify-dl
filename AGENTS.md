# AGENTS.md

Guidance for coding agents working in this repository.

## Project overview

`spotify-dl` is a Rust CLI (edition 2024) that downloads tracks, podcasts, playlists and albums from Spotify. It talks to Spotify through `librespot` 0.8 (crates.io, `native-tls`), encodes to MP3 (default, behind the `mp3` feature) or FLAC, tags files with `audiotags`/`id3`, and renders a full-screen `ratatui` download view plus an interactive playlist-folder tree picker. State (picker selection, cached metadata, unavailable tracks) lives in `~/.spotify-dl/`.

A Spotify **Premium** account is required for anything that touches the network. Never hardcode credentials; tests must not require them.

## Code layout

- `src/main.rs` — CLI entry point (`structopt`).
- `src/lib.rs` — wires everything together.
- `src/track.rs` — track/album/playlist parsing and metadata fetching.
- `src/download.rs` — download orchestration, rate limiting, playlist sync.
- `src/download_ui.rs` — ratatui download view.
- `src/folder_picker.rs` — interactive TUI playlist-folder picker.
- `src/account.rs`, `src/account_state.rs` — account API access and persisted picker state.
- `src/stream/` — audio streaming via librespot's player.
- `src/encoder/` — FLAC/MP3 encoding and tagging.
- `src/capture.rs` — raw API response capture for offline tests.
- `src/unavailable.rs` — tracking of tracks Spotify reports as unavailable.
- `tests/account_mock.rs` + `tests/fixtures/account/` — integration test against anonymized captures.

## Build

```
cargo build            # debug
cargo build --release  # what CI builds on linux/macos/windows
```

- The release profile optimizes for size (`opt-level = "z"`, LTO, strip).
- `build-dependencies` pin `vergen =9.0.6` on purpose (see comment in `Cargo.toml`); do not bump it casually — `cargo install` ignores `Cargo.lock` and would otherwise pick an incompatible 9.1.x.
- There is a `librespot` git submodule in `.gitmodules`, but the build uses librespot from crates.io; the submodule is vestigial and not needed to build.

## Test

```
cargo test
```

- Unit tests are inline (`#[cfg(test)]`) in most modules (`account.rs`, `download.rs`, `folder_picker.rs`, `rate_limit.rs`, `capture.rs`, `unavailable.rs`, `lock.rs`, `log.rs`, `download_ui.rs`, `account_state.rs`).
- The integration test `tests/account_mock.rs` runs against fixtures in `tests/fixtures/account/` and needs no network or credentials.

### Regenerating test fixtures (captures)

Live runs can record raw API responses, which are then anonymized into fixtures:

```
SPOTIFY_DL_CAPTURE_DIR=/tmp/captures spotify-dl --from-account
cargo run --example anonymize-captures -- /tmp/captures tests/fixtures/account
cargo test --test account_mock
```

The picker can also be exercised offline against fixtures (confirming only reports what would be downloaded):

```
SPOTIFY_DL_MOCK_DIR=tests/fixtures/account spotify-dl --from-account
```

## Lint & format

CI (`rust-clippy.yml`) runs:

```
cargo clippy --all-features
```

Run it before committing; keep the code `cargo fmt`-clean.

## Running locally

```
cargo run -- <spotify uri or url>...
cargo run -- --from-account   # interactive picker (needs Premium login)
```

Logs for every run go to `~/.spotify-dl/spotify-dl.log` (rotates at 5 MB, one backup) — useful when debugging a failed run.

## Workflow rules

- **Commit after each task.** When a task is complete (code compiles, tests you ran pass), create a git commit with a concise, imperative message describing the change. Do not commit unrelated changes together.
- Do not push or create branches unless asked.
- Keep changes minimal and consistent with existing style; prefer existing dependencies over adding new ones.
