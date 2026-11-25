# Architecture

## Crates

| Crate | Role |
| --- | --- |
| `shingle-core` | Tiling trees, workspaces, geometry and the serializable session model. No Wayland dependency. |
| `shingle` | Wayland protocol handling, input routing, output state, IPC and the headless compositor event loop. Also the `shingle` binary. |
| `shingle-render` | wgpu textures, transforms, effects and render tiles. |
| `shingle-nested` | winit host window and the interactive frontend: overviews, transitions, gestures. |
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
| Keymap, focus, input routing | `crates/shingle/src/input.rs` |
| Seat devices, device gating, `input/*` IPC | `crates/shingle/src/input_device.rs` |
| XDG roles, map lifecycle, geometry, window rules | `crates/shingle/src/shell.rs` |
| IPC transport, dispatch, method list, events | `crates/shingle/src/ipc.rs` |
| Host input, visual state, frontend dispatch | `crates/shingle-nested/src/main.rs` |
| Native config model and XKB generation | `crates/shingle-config/src/lib.rs` |
| Wayfire INI import | `crates/shingle-config/src/wayfire.rs` |
| i3 import | `crates/shingle-config/src/i3.rs` |

## Design rules

- The tiling tree in `shingle-core` is the source of truth for layout and is what sessions persist. Temporary visual geometry (Scale thumbnails, Expo, vswitch slides, window rotation) must never be written back into it.
- Scale's snapshot selection lives in `shingle`, while its interactive state and wgpu thumbnail transforms live in `shingle-nested`.
- Wayfire-facing names stay as they are: `WAYFIRE_SOCKET`, IPC method and event names, and INI section names.
