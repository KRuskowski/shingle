# Shingle

Rust/wgpu Wayland compositor built around Wayfire's tiling behaviour, compatible with Wayfire config and IPC. Formerly Tilefire.

## Read first

- docs/roadmap.md: what to work on next, in order. Finish and verify one item before starting another.
- docs/status.md: what works and how it was verified.
- docs/architecture.md: crate map and where each area lives.
- docs/development.md: build, test, formatting and real-client test rules.

## Pointers

- IPC method inventory: crates/shingle/src/ipc.rs:380 (`SUPPORTED_METHODS`); dispatch: crates/shingle/src/ipc.rs:2541 (`respond`).
- Seat input device registry and gating: crates/shingle/src/input_device.rs:1; registration API: crates/shingle/src/lib.rs:644 (`add_input_device`); nested's devices: crates/shingle-nested/src/main.rs:177 (`SeatDevices`); nested host-input gating: crates/shingle-nested/src/main.rs:4751 (`window_event_device`); nested IPC fixture tests: crates/shingle-nested/src/main.rs:4879 (`IpcFixture`).
- Backend host API (NB-2): `frame_presented` crates/shingle/src/lib.rs:763, `shutdown_requested` lib.rs:903, `set_session_active` lib.rs:1172, `set_input_time` lib.rs:1186, `set_output_info` lib.rs:1937, `impl AsFd` lib.rs:2098, `with_socket_path` lib.rs:541. In-process Wayland client for protocol tests: crates/shingle/src/test_client.rs:1.
- Tilefire migration fallbacks: crates/shingle-config/src/lib.rs:1560, crates/shingle-session/src/lib.rs:34, crates/shingle-auth/src/lib.rs:46, crates/shingle-nested/src/main.rs:731.

## Commands

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all
rg -n '.{81,}' crates --type rust
```

- Gloomy is a pinned git dependency (Cargo.toml, `[workspace.dependencies]`); change it in its own repo, then bump `rev` (docs/development.md#gloomy).
- Native-backend code is built and unit-tested here but never run: this host's GPU, input devices and VTs belong to the operator's Wayfire session. Device tests run on the dedicated test machine (docs/design/native-backend.md#layer-2-dedicated-test-machine).
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
