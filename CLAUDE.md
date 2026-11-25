# Shingle

Rust/wgpu Wayland compositor built around Wayfire's tiling behaviour, compatible with Wayfire config and IPC. Formerly Tilefire.

## Read first

- docs/roadmap.md: what to work on next, in order. Finish and verify one item before starting another.
- docs/status.md: what works and how it was verified.
- docs/architecture.md: crate map and where each area lives.
- docs/development.md: build, test, formatting and real-client test rules.

## Pointers

- IPC method inventory: crates/shingle/src/ipc.rs:380 (`SUPPORTED_METHODS`); dispatch: crates/shingle/src/ipc.rs:2545 (`respond`).
- Map blocking (`view-pre-map`): crates/shingle/src/shell.rs:501 (`request_map`), crates/shingle/src/shell.rs:459 (`finish_map`); held views stay out of the layout via crates/shingle-core/src/core.rs:898 (`release_held`). Compiled client keymap: crates/shingle/src/input.rs:202 (`compile_keymap`).
- Seat input device registry and gating: crates/shingle/src/input_device.rs:1; registration API: crates/shingle/src/lib.rs:1018 (`add_input_device`); nested's devices: crates/shingle-nested/src/lib.rs:102 (`SeatDevices`); nested host-input gating: crates/shingle-nested/src/lib.rs:528 (`window_event_device`); nested IPC fixture tests: crates/shingle-nested/src/lib.rs:593 (`IpcFixture`).
- Shared keyboard (NB-3): crates/shingle-frontend/src/keyboard.rs:76 (`Keyboard`), evdev names crates/shingle-frontend/src/keyboard/names.rs:1, key repeat crates/shingle-frontend/src/keyboard/repeat.rs:13 (`KeyRepeat`); key dispatch: crates/shingle-frontend/src/frontend/bindings.rs:274 (`key_event`), crates/shingle-frontend/src/frontend/bindings.rs:341 (`keyboard_input`); nested's winit key and focus glue: crates/shingle-nested/src/lib.rs:253 (`keyboard_event`), crates/shingle-nested/src/host_keyboard.rs:1.
- Frontend (NB-4): crates/shingle-frontend/src/frontend.rs:70 (`Frontend`), input entry crates/shingle-frontend/src/frontend.rs:652 (`handle_input`), per-turn work crates/shingle-frontend/src/frontend.rs:693 (`process`), dispatch crates/shingle-frontend/src/frontend.rs:776; STIPC keys: crates/shingle/src/ipc.rs:1517 (`feed_key`) emits `Event::StipcKey`, handled at crates/shingle-frontend/src/frontend.rs:534; host seam crates/shingle-frontend/src/host.rs:18 (`Host`), crates/shingle-frontend/src/host.rs:50 (`InputEvent`); render seam crates/shingle-frontend/src/render_target.rs:14 (`RenderTarget`), scene crates/shingle-frontend/src/frontend/scene.rs:280 (`render`), one frame time for drawing and ending animations crates/shingle-frontend/src/frontend/scene.rs:295 (`render_frame`); FakeHost input-script tests crates/shingle-frontend/src/frontend/tests.rs:50; nested's `Host` crates/shingle-nested/src/lib.rs:141 (`WindowHost`).
- Backend host API (NB-2): `frame_presented` crates/shingle/src/lib.rs:1171, `shutdown_requested` lib.rs:1329, `set_session_active` lib.rs:1652, `set_input_time` lib.rs:1666, `set_output_info` lib.rs:2631, `impl AsFd` lib.rs:2792, `keyboard_layout_index` lib.rs:1679, `with_socket_path` lib.rs:903. In-process Wayland client for protocol tests: crates/shingle/src/test_client.rs:1.
- `shingle` binary (package `shingle-bin`): backend selection crates/shingle-bin/src/backend.rs:148 (`select`), options and `--help` crates/shingle-bin/src/cli.rs:106 (`parse`), crates/shingle-bin/src/cli.rs:8 (`HELP`), start-up decision crates/shingle-bin/src/main.rs:36 (`plan`), backend dispatch and the native stub crates/shingle-bin/src/main.rs:81 (`start`), headless backend crates/shingle-bin/src/headless.rs:17, nested entry point crates/shingle-nested/src/lib.rs:552 (`run`). Design and reasons: docs/design/native-backend.md#the-shingle-binary.
- Shell-neutral toplevel role (XW-1): crates/shingle/src/shell.rs:112 (`ToplevelHandle`), crates/shingle/src/shell.rs:60 (`Role::toplevel`, `Role::toplevel_mut`), configure crates/shingle/src/shell.rs:844 (`configure_toplevel`, `ToplevelStates`), close crates/shingle/src/shell.rs:1049 (`close_window`); launch identity by PID crates/shingle/src/launch.rs:26 (`capture_pid`). Design: docs/design/xwayland.md#4-integration-with-the-window-model.
- Child process environment: crates/shingle-session/src/environment.rs:42 (`ClientEnvironment::new`), crates/shingle/src/lib.rs:1060 (`Compositor::client_environment`); launchers crates/shingle-session/src/lib.rs:242 (`launch_command`), crates/shingle-session/src/lib.rs:265 (`launch_program`).
- Wayland behaviour pin: crates/shingle/src/wayland_pins.rs:1, recorded transcript crates/shingle/testdata/wayland_pins.txt (rewrite with `SHINGLE_BLESS_PINS=1`).
- Tilefire migration fallbacks: crates/shingle-config/src/lib.rs:2189, crates/shingle-session/src/lib.rs:77, crates/shingle-auth/src/lib.rs:79, crates/shingle-frontend/src/frontend/session.rs:21.

## Commands

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all
rg -n '.{81,}' crates --type rust
cargo run --bin shingle -- --config data/shingle.yaml --spawn foot
cargo run --bin shingle -- --headless --ipc "$XDG_RUNTIME_DIR/shingle-ipc"
cargo run --bin shingle -- --help
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
