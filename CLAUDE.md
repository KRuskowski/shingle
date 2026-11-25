# Shingle

Rust/wgpu Wayland compositor built around Wayfire's tiling behaviour, compatible with Wayfire config and IPC. Formerly Tilefire.

## Read first

- docs/roadmap.md: what to work on next, in order. Finish and verify one item before starting another.
- docs/status.md: what works and how it was verified.
- docs/architecture.md: crate map and where each area lives.
- docs/development.md: build, test, formatting and real-client test rules.

## Pointers

- IPC method inventory: crates/shingle/src/ipc.rs:380 (`SUPPORTED_METHODS`); dispatch: crates/shingle/src/ipc.rs:2548 (`respond`).
- Map blocking (`view-pre-map`): crates/shingle/src/shell.rs:505 (`request_map`), crates/shingle/src/shell.rs:463 (`finish_map`); held views stay out of the layout via crates/shingle-core/src/core.rs:898 (`release_held`). Compiled client keymap: crates/shingle/src/input.rs:261 (`compile_keymap`).
- Seat input device registry and gating: crates/shingle/src/input_device.rs:1; registration API: crates/shingle/src/lib.rs:1020 (`add_input_device`); nested's devices: crates/shingle-nested/src/lib.rs:136 (`SeatDevices`); nested host-input gating: crates/shingle-nested/src/lib.rs:549 (`window_event_device`); nested IPC fixture tests: crates/shingle-nested/src/lib.rs:632 (`IpcFixture`).
- Shared keyboard (NB-3): crates/shingle-frontend/src/keyboard.rs:76 (`Keyboard`), evdev names crates/shingle-frontend/src/keyboard/names.rs:1, key repeat crates/shingle-frontend/src/keyboard/repeat.rs:13 (`KeyRepeat`); key dispatch: crates/shingle-frontend/src/frontend/bindings.rs:274 (`key_event`), crates/shingle-frontend/src/frontend/bindings.rs:341 (`keyboard_input`); nested's winit key and focus glue: crates/shingle-nested/src/lib.rs:287 (`keyboard_event`), crates/shingle-nested/src/host_keyboard.rs:1.
- Frontend (NB-4): crates/shingle-frontend/src/frontend.rs:70 (`Frontend`), input entry crates/shingle-frontend/src/frontend.rs:652 (`handle_input`), per-turn work crates/shingle-frontend/src/frontend.rs:693 (`process`), dispatch crates/shingle-frontend/src/frontend.rs:776; STIPC keys: crates/shingle/src/ipc.rs:1517 (`feed_key`) emits `Event::StipcKey`, handled at crates/shingle-frontend/src/frontend.rs:534; host seam crates/shingle-frontend/src/host.rs:19 (`Host`), crates/shingle-frontend/src/host.rs:51 (`InputEvent`); render seam crates/shingle-frontend/src/render_target.rs:14 (`RenderTarget`), scene crates/shingle-frontend/src/frontend/scene.rs:280 (`render`), one frame time for drawing and ending animations crates/shingle-frontend/src/frontend/scene.rs:295 (`render_frame`); FakeHost input-script tests crates/shingle-frontend/src/frontend/tests.rs:50; nested's `Host` crates/shingle-nested/src/lib.rs:175 (`WindowHost`).
- Backend host API (NB-2): `frame_presented` crates/shingle/src/lib.rs:1173, `shutdown_requested` lib.rs:1331, `set_session_active` lib.rs:1650, `set_input_time` lib.rs:1664, `set_output_info` lib.rs:2632, `impl AsFd` lib.rs:2793, `keyboard_layout_index` lib.rs:1677, `with_socket_path` lib.rs:905. In-process Wayland client for protocol tests: crates/shingle/src/test_client.rs:1.
- `shingle` binary (package `shingle-bin`): backend selection crates/shingle-bin/src/backend.rs:148 (`select`), options and `--help` crates/shingle-bin/src/cli.rs:112 (`parse`), crates/shingle-bin/src/cli.rs:8 (`HELP`), start-up decision and the SSH refusal crates/shingle-bin/src/main.rs:36 (`plan`), backend dispatch crates/shingle-bin/src/main.rs:83 (`start`), headless backend crates/shingle-bin/src/headless.rs:17, nested entry point crates/shingle-nested/src/lib.rs:573 (`run`). Design and reasons: docs/design/native-backend.md#the-shingle-binary.
- Native backend (NB-6, never run on this host): entry crates/shingle-native/src/lib.rs:66 (`run`); seat crates/shingle-native/src/session.rs:41 (`Session`), startup checks crates/shingle-native/src/session.rs:146 (`preflight`), crates/shingle-native/src/session.rs:163 (`wait_for_enable`), libseat crates/shingle-native/src/session.rs:256 (`LibSeatSession`); input crates/shingle-native/src/input.rs:326 (`InputSource`), translation crates/shingle-native/src/input.rs:366 (`Translator`), device options crates/shingle-native/src/input.rs:256 (`device_settings`), libinput crates/shingle-native/src/input/libinput.rs:76 (`LibinputSource`); hotplug registry crates/shingle-native/src/udev.rs:83 (`DeviceRegistry`); loop crates/shingle-native/src/host.rs:108 (`Backend`), pause and resume crates/shingle-native/src/host.rs:277, crates/shingle-native/src/host.rs:295; fake-driven tests crates/shingle-native/src/host/tests.rs:1; frontend pause crates/shingle-frontend/src/frontend.rs:774 (`set_session_active`). Test-machine plan: docs/native-testing.md.
- Shell-neutral toplevel role (XW-1): crates/shingle/src/shell.rs:116 (`ToplevelHandle`), crates/shingle/src/shell.rs:62 (`Role::toplevel`, `Role::toplevel_mut`), configure crates/shingle/src/shell.rs:940 (`configure_toplevel`, `ToplevelStates`), close crates/shingle/src/shell.rs:1156 (`close_window`); launch identity by PID crates/shingle/src/launch.rs:26 (`capture_pid`). Design: docs/design/xwayland.md#4-integration-with-the-window-model.
- XWayland server (XW-2): glue crates/shingle/src/xwayland/mod.rs:122 (`Xwayland`), per-turn crates/shingle/src/xwayland/mod.rs:210 (`poll`); crate crates/shingle-xwayland/src/lib.rs:51 (`XwaylandServer`), display crates/shingle-xwayland/src/sockets.rs:64 (`bind_in`), argv crates/shingle-xwayland/src/server.rs:99 (`arguments`), restart rule crates/shingle-xwayland/src/server.rs:61, XWM bring-up crates/shingle-xwayland/src/xwm/mod.rs:175 (`Xwm::start`); config crates/shingle-config/src/lib.rs:978 (`XwaylandMode`); namespace harness tools/xwayland_test/run.py (PID namespace; signal only through tools/xwayland_test/run.py:145 `signal_checked`).
- Managed X windows (XW-3): XWM events crates/shingle-xwayland/src/xwm/mod.rs:268 (`handle`), configure and close crates/shingle-xwayland/src/xwm/mod.rs:623, :688; classification crates/shingle-xwayland/src/xwm/window.rs:240 (`kind`), pairing crates/shingle-xwayland/src/xwm/pairing.rs:31, ConfigureRequest policy crates/shingle-xwayland/src/xwm/policy.rs:25; `xwayland_shell_v1` and `Role::Xwayland` crates/shingle/src/xwayland/shell.rs:26; glue crates/shingle/src/xwayland/windows.rs:79 (`apply`), :122 (`committed`), :200 (`try_map`), :242 (`new_toplevel`), :498 (X half of `configure_toplevel`); probe client crates/shingle-xwayland/examples/xw_probe.rs; harness groups tools/xwayland_test/run.py:779 (`windows_checks`), :917 (`probe_checks`).
- Selection bridge (XW-5): XWM side crates/shingle-xwayland/src/xwm/selection.rs:219 (`Selections`), entry crates/shingle-xwayland/src/xwm/mod.rs:1184 (`bridge_selections`), MIME mapping crates/shingle-xwayland/src/xwm/mime.rs:29; compositor side crates/shingle/src/xwayland/selection.rs:102 (`Bridge`), source keys crates/shingle/src/data_device.rs:37 (`SourceKey`), data-control's first announcements crates/shingle/src/data_control.rs:90 (`sync`). Fake-Xwayland test scripts: crates/shingle-xwayland/src/server.rs:384 (`write_script`, avoids `ETXTBSY`).
- Child process environment: crates/shingle-session/src/environment.rs:42 (`ClientEnvironment::new`), crates/shingle/src/lib.rs:1062 (`Compositor::client_environment`); launchers crates/shingle-session/src/lib.rs:298 (`launch_command`), crates/shingle-session/src/lib.rs:321 (`launch_program`).
- Null-buffer unmap and remap: crates/shingle/src/shell.rs:689 (`unmap_toplevel`); surface damage to buffer pixels: crates/shingle/src/state.rs:1445 (`surface_damage_to_buffer`); scroll source and stops: crates/shingle/src/input.rs:69 (`Axis`); keymap from XKB names: crates/shingle-config/src/lib.rs:2178 (`InputConfig::keymap`); corrupt sessions set aside: crates/shingle-session/src/lib.rs:184 (`SessionStore::load`); password zeroing: crates/shingle-auth/src/lib.rs:51 (`ZeroOnFree`).
- X focus, client requests, unmanaged windows (XW-4): crates/shingle-xwayland/src/xwm/mod.rs:998 (`Xwm::set_focus`), crates/shingle-xwayland/src/xwm/mod.rs:610 (`FocusIn` guard), crates/shingle-xwayland/src/xwm/mod.rs:491 (client messages), crates/shingle/src/xwayland/focus.rs:33 (`target`, `stacking`), crates/shingle/src/xwayland/requests.rs:257 (`focus_on_map`), crates/shingle/src/xwayland/unmanaged.rs:27 (`map`); the Unmanaged layer in crates/shingle/src/state.rs:633 (`visible_surfaces`). Design: docs/design/xwayland.md#xw-4-shingle-xwayland-behaviour-focus-unmanaged-windows-and-states.
- Wayland behaviour pin: crates/shingle/src/wayland_pins.rs:1, recorded transcript crates/shingle/testdata/wayland_pins.txt (rewrite with `SHINGLE_BLESS_PINS=1`).
- Tilefire migration fallbacks: crates/shingle-config/src/lib.rs:2202, crates/shingle-session/src/lib.rs:77, crates/shingle-auth/src/lib.rs:141, crates/shingle-frontend/src/frontend/session.rs:21.

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
- Native-backend code is built and unit-tested here but never run: this host's GPU, input devices and VTs belong to the operator's Wayfire session. Device tests run on the dedicated test machine (docs/native-testing.md). Without a display variable `shingle` now selects native and opens the seat, so every run here passes `--headless` or `--backend nested`.
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
