# Status

Last updated 2026-11-25, after the Tilefire-to-Shingle rename.

## Readiness

The nested compositor runs inside an existing Wayland session and is useful for isolated testing. It is **not** yet a drop-in hardware replacement for Wayfire. The largest gaps are:

- No direct DRM/KMS or libinput backend; only the nested winit frontend exists.
- No DMA-BUF; client buffers are shared memory and are copied by the CPU.
- No XWayland.
- Multi-output support is incomplete.
- Several Wayfire plugins and visual effects are missing, including wrot's 3D mode.

Do not claim that Shingle is more stable or uses fewer resources than Wayfire. No comparative benchmark or long-running soak test has been done, and nested wgpu rendering, CPU buffer copies and per-frame work may make it slower in some scenarios.

## Implemented

### Compositor and renderer

- wgpu rendering with a winit nested frontend.
- XDG toplevels and popups, layer shell, dialogs, decorations, subsurfaces, viewports, fractional scaling and shared-memory buffers.
- Tiling, floating windows, exact floating geometry, snapping, focus, stacking, minimize, fullscreen, sticky and show-desktop.
- Workspace navigation: directional, last, numbered, carry and send actions, with configurable wraparound.
- Scale and Expo overviews, vswitch transitions, zoom, inversion, opacity, window rotation, and opening/closing animations.
- Screencopy, gamma, tearing control, FIFO, commit timing, presentation timing and pointer warp.
- Text input v3, input-method v1/v2, input panels, foreign-toplevel protocols, ext-workspace, xdg-foreign, security-context filtering and primary-selection controls.

Details are in [features.md](features.md).

### Sessions

The persistent tiling model records workspace, tree order, split weights, window geometry, floating/tiled and snapped state and related flags. Sessions are written atomically.

The requirement is exact tile order and size after logout and login. Test full frontend/session cycles, not only serialization.

Views blocked before their first map (see [ipc.md](ipc.md#map-blocking)) are placeholders until released. They must not be rendered, focused, included in Scale or reported as mapped.

### Idle and locking

`shingle-lock` is an `ext-session-lock-v1` client with swaylock-style options and PAM authentication through `shingle-auth`. Idle, DPMS, fullscreen suppression and lock coordination exist. Option compatibility does not imply full swaylock parity; keep testing real locking.

### Configuration and WCM

- YAML is native and rejects unknown fields.
- Wayfire INI and i3-style imports cover many options and key bindings, but not all of them.
- Physical-key and case-sensitive `xkb:` bindings are supported.
- Comma-separated XKB layouts and per-layout variants become distinct XKB groups.
- WCM, built on the vendored Gloomy UI, edits, validates and converts configuration.
- Live IPC configuration updates are not written back to YAML.

### Wayfire IPC

75 methods and the Wayfire event set. See [ipc.md](ipc.md).

## Verification

### Automated

`cargo test --workspace` runs 60 unit tests (2026-11-25). Coverage is uneven: `shingle-core`, which holds the tiling tree and session model, has **no** unit tests.

`cargo clippy --workspace --all-targets -- -D warnings` currently **fails**. The unfinished input-device slice (see [roadmap.md](roadmap.md)) leaves three dead-code warnings in `crates/shingle/src/input.rs`, and the vendored Gloomy crates emit their own warnings. Do not report the tree as Clippy-clean until both are resolved.

### With real clients

Before the rename, real XDG clients and Unix-socket IPC fixtures exercised mapping, geometry, workspaces, XKB layout switching (`us,de`), runtime bindings and event delivery. Plugin activation events were checked over IPC, but no interactive visual test was done in a host session.

After the rename, only the unit tests have been run. The nested smoke test and real-client IPC checks still need repeating under the new names.

When recording new verification, keep real-client results separate from compiled or unit-tested ones.
