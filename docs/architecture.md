# Architecture

## Crates

| Crate | Role |
| --- | --- |
| `shingle-core` | Tiling trees, workspaces, geometry and the serializable session model. No Wayland dependency. |
| `shingle` | Wayland protocol handling, input routing, output state, IPC and the headless compositor event loop. Also the `shingle` binary. |
| `shingle-render` | wgpu rendering without a windowing dependency: shared `GpuContext`, per-output `RenderState`, the `draw` pass, and window and offscreen targets. |
| `shingle-frontend` | The interactive frontend shared by the nested and (future) native hosts, without winit: `Frontend` with Scale, Expo, the switchers, drags, animations, effects, the scene build, binding dispatch, key repeat and session handling, behind the `Host` trait and `InputEvent`. |
| `shingle-nested` | Thin winit host for `Frontend`: the window and renderer, winit-to-`InputEvent` translation, host focus handling, button emulation and argument parsing. |
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
| Public compositor API and renderer events | `crates/shingle/src/lib.rs` |
| Protocol timestamps (`CLOCK_MONOTONIC`) | `crates/shingle/src/clock.rs` |
| In-process Wayland test client | `crates/shingle/src/test_client.rs` |
| Shared state and workspace transitions | `crates/shingle/src/state.rs` |
| Keymap sent to clients, focus, input routing | `crates/shingle/src/input.rs` |
| Seat device registry, per-device holds and gating, `input/*` IPC | `crates/shingle/src/input_device.rs` |
| XDG roles, map lifecycle, geometry, window rules | `crates/shingle/src/shell.rs` |
| IPC transport, dispatch, method list, events | `crates/shingle/src/ipc.rs` |
| Host seam: `Host` trait, `InputEvent` | `crates/shingle-frontend/src/host.rs` |
| `Frontend` struct, public API, input dispatch | `crates/shingle-frontend/src/frontend.rs` |
| Frontend areas: bindings, Scale, Expo, switcher, drags, effects, animations, scene build, session | `crates/shingle-frontend/src/frontend/*.rs` |
| What the scene is drawn into (`RenderTarget`) | `crates/shingle-frontend/src/render_target.rs` |
| Frontend input-script tests (`FakeHost`) | `crates/shingle-frontend/src/frontend/tests.rs` |
| winit host: window, input translation, button emulation | `crates/shingle-nested/src/main.rs` |
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
- `shingle-frontend` never depends on winit, DRM or libinput. A host reaches it only through `Frontend`'s public API, `InputEvent` and the `Host` trait, which the native backend will implement in NB-6.
- Wayfire-facing names stay as they are: `WAYFIRE_SOCKET`, IPC method and event names, and INI section names.
