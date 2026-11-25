# Architecture

## Crates

| Crate | Role |
| --- | --- |
| `shingle-core` | Tiling trees, workspaces, geometry and the serializable session model. No Wayland dependency. |
| `shingle` | Wayland protocol handling, input routing, output state, IPC and the compositor event loop. Library only; no winit. |
| `shingle-render` | wgpu rendering without a windowing dependency: shared `GpuContext`, per-output `RenderState`, the `draw` pass, and window and offscreen targets. |
| `shingle-frontend` | The interactive frontend shared by the nested and (future) native hosts, without winit: `Frontend` with Scale, Expo, the switchers, drags, animations, effects, the scene build, binding dispatch, key repeat and session handling, behind the `Host` trait and `InputEvent`. |
| `shingle-bin` | The `shingle` binary: argument parsing, backend selection and the headless backend. Its own package because the `shingle` library must not depend on the backends that depend on it. |
| `shingle-nested` | Nested backend library, a thin winit host for `Frontend`: the window and renderer, winit-to-`InputEvent` translation, host focus handling and button emulation. Entry point `shingle_nested::run`. |
| `shingle-xwayland` | XWayland without `wayland-server`: X display sockets and lock, the supervised Xwayland process with its restart rule, and the X window manager (x11rb) behind an `XConnection` trait. |
| `shingle-config` | Validated YAML model, Wayfire INI and i3 imports, XKB keymap generation. |
| `shingle-session` | Atomic session persistence and relaunching saved applications. |
| `shingle-idle` | Idle, DPMS, fullscreen suppression and lock coordination. |
| `shingle-auth` | PAM authentication for the lock client. |
| `shingle-lock` | `ext-session-lock-v1` client with swaylock-style options. |
| `shingle-wcm` | Graphical configuration manager built on [Gloomy](https://github.com/KRuskowski/gloomy). |

## Where things live

| Area | File |
| --- | --- |
| Persistent windows and mapped presence | `crates/shingle-core/src/core.rs` |
| Tiling order, focus, masks and splits | `crates/shingle-core/src/tree.rs` |
| Session snapshot format | `crates/shingle-core/src/session.rs` |
| Environment of every child process (`ClientEnvironment`), launching clients | `crates/shingle-session/src/environment.rs`, `crates/shingle-session/src/lib.rs` |
| Public compositor API and renderer events | `crates/shingle/src/lib.rs` |
| Protocol timestamps (`CLOCK_MONOTONIC`) | `crates/shingle/src/clock.rs` |
| In-process Wayland test client | `crates/shingle/src/test_client.rs` |
| Shared state and workspace transitions | `crates/shingle/src/state.rs` |
| Keymap sent to clients, focus, input routing | `crates/shingle/src/input.rs` |
| Seat device registry, per-device holds and gating, `input/*` IPC | `crates/shingle/src/input_device.rs` |
| XDG roles, the shell-neutral toplevel role (`ToplevelRole`, `ToplevelHandle`), map lifecycle, geometry, window rules | `crates/shingle/src/shell.rs` |
| Launch identity from `/proc` (`capture`, `capture_pid`) | `crates/shingle/src/launch.rs` |
| Pinned outside view of Wayland toplevels | `crates/shingle/src/wayland_pins.rs`, `crates/shingle/testdata/wayland_pins.txt` |
| IPC transport, dispatch, method list, events | `crates/shingle/src/ipc.rs` |
| XWayland glue: display, Xwayland start and exit, XWM connection, `xwayland-ready` | `crates/shingle/src/xwayland.rs` |
| X display sockets, Xwayland process, XWM | `crates/shingle-xwayland/src/sockets.rs`, `server.rs`, `xwm/` |
| XWayland real-client harness (namespaces) | `tools/xwayland_test/run.py` |
| Host seam: `Host` trait, `InputEvent` | `crates/shingle-frontend/src/host.rs` |
| `Frontend` struct, public API, input dispatch | `crates/shingle-frontend/src/frontend.rs` |
| Frontend areas: bindings, Scale, Expo, switcher, drags, effects, animations, scene build, session | `crates/shingle-frontend/src/frontend/*.rs` |
| What the scene is drawn into (`RenderTarget`) | `crates/shingle-frontend/src/render_target.rs` |
| Frontend input-script tests (`FakeHost`) | `crates/shingle-frontend/src/frontend/tests.rs` |
| Command line, `--help` text | `crates/shingle-bin/src/cli.rs` |
| Backend selection from arguments and environment | `crates/shingle-bin/src/backend.rs` |
| What a start does (print, fail or run a backend) | `crates/shingle-bin/src/main.rs` |
| Headless backend | `crates/shingle-bin/src/headless.rs` |
| winit host: window, input translation, button emulation | `crates/shingle-nested/src/lib.rs` |
| Host keyboard focus changes | `crates/shingle-nested/src/host_keyboard.rs` |
| xkb keyboard state, key names, keysyms, modifier masks | `crates/shingle-frontend/src/keyboard.rs` |
| Key repeat timer | `crates/shingle-frontend/src/keyboard/repeat.rs` |
| Evdev code to Wayfire key name table | `crates/shingle-frontend/src/keyboard/names.rs` |
| Scale layout and title filter | `crates/shingle-frontend/src/scale.rs` |
| Expo, vswitch transitions, touch gestures | `crates/shingle-frontend/src/expo.rs`, `workspace_transition.rs`, `extra_gestures.rs` |
| GPU device, pipelines, textures, gamma and effects | `crates/shingle-render/src/gpu.rs` |
| Composite pass and tile geometry | `crates/shingle-render/src/draw.rs` |
| Window and offscreen render targets | `crates/shingle-render/src/target.rs` |
| Native config model and XKB generation | `crates/shingle-config/src/lib.rs` |
| Wayfire INI import | `crates/shingle-config/src/wayfire.rs` |
| i3 import | `crates/shingle-config/src/i3.rs` |

## Design rules

- The tiling tree in `shingle-core` is the source of truth for layout and is what sessions persist. Temporary visual geometry (Scale thumbnails, Expo, vswitch slides, window rotation) must never be written back into it.
- Scale's snapshot selection lives in `shingle`, while its interactive state and wgpu thumbnail transforms live in the frontend (`shingle-frontend`).
- `shingle` and `shingle-frontend` never depend on winit; only the backend crates and `shingle-bin` link it (`cargo tree -p shingle` lists no winit).
- `shingle-frontend` never depends on winit, DRM or libinput. A host reaches it only through `Frontend`'s public API, `InputEvent` and the `Host` trait, which the native backend will implement in NB-6.
- Wayfire-facing names stay as they are: `WAYFIRE_SOCKET`, IPC method and event names, and INI section names.
- Outside xdg request dispatch, code reaches a toplevel only through `Role::toplevel` and `Role::toplevel_mut`, and per-shell behaviour (configure, close, xdg-only protocols) only through `ToplevelHandle`, so X11 windows can share the role (docs/design/xwayland.md).
- Every process Shingle starts gets its environment from `Compositor::client_environment`. Shingle never changes its own environment for its children.
