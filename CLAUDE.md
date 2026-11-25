# Shingle

Rust/wgpu Wayland compositor built around Wayfire's tiling behaviour, compatible with Wayfire config and IPC. Formerly Tilefire.

## Read first

- docs/roadmap.md: what to work on next, in order. Finish and verify one item before starting another.
- docs/status.md: what works and how it was verified.
- docs/architecture.md: crate map and where each area lives.
- docs/development.md: build, test, formatting and real-client test rules.

## Pointers

- IPC method inventory: crates/shingle/src/ipc.rs:379 (`SUPPORTED_METHODS`); dispatch: crates/shingle/src/ipc.rs:2431 (`respond`).
- Unfinished input-device slice (roadmap item 1): crates/shingle/src/input.rs:43, crates/shingle/src/input.rs:130, crates/shingle/src/input.rs:153.
- Tilefire migration fallbacks: crates/shingle-config/src/lib.rs:1560, crates/shingle-session/src/lib.rs:34, crates/shingle-auth/src/lib.rs:46, crates/shingle-nested/src/main.rs:622.

## Commands

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all
rg -n '.{81,}' crates --type rust
```

- Gloomy is a pinned git dependency (Cargo.toml, `[workspace.dependencies]`); change it in its own repo, then bump `rev` (docs/development.md#gloomy).
- This host lacks `libpam0g-dev`; linking fails with `-lpam` (docs/development.md#requirements has the `LIBRARY_PATH` workaround).
- Cargo writes to `~/.cargo`, which the Claude sandbox mounts read-only; cargo commands must run outside the sandbox.

## Conventions

- Rust: 2-space indent, 80 columns (rustfmt.toml). rustfmt silently skips expressions it cannot fit, so check for long lines after formatting.
- Doc comments on all public items; new code comes with unit tests.
- Keep Wayfire names (`WAYFIRE_SOCKET`, IPC methods/events, INI sections) unchanged.
- Tilefire fallbacks for old paths and env vars are intentional; see README.md#upgrading-from-tilefire.
- When work lands, update docs/status.md and docs/roadmap.md, and keep counts (IPC methods, tests) in docs/ipc.md and docs/status.md factual.
- Record real-client verification separately from unit tests.
- Never stop the host Wayfire session for a test.
- One logical change per commit, GPG-signed. Remote: github.com/KRuskowski/shingle.
