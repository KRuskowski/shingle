# Status

Last updated 2026-11-25, after `shingle-core` gained unit tests and API documentation.

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
- WCM, built on the Gloomy UI library, edits, validates and converts configuration.
- Live IPC configuration updates are not written back to YAML.

### Wayfire IPC

77 methods and the Wayfire event set, including `input/list-devices` and `input/configure-device` for the three nested seat devices. Disabling a device releases its held input. See [ipc.md](ipc.md).

## Verification

### Automated

`cargo test --workspace` runs 168 unit tests (2026-11-25), 88 of them in `shingle-core`. Coverage of the other crates is uneven.

`cargo clippy --workspace --all-targets -- -D warnings` passes (2026-11-25).

The input-device unit tests (`crates/shingle/src/input_device.rs`, `crates/shingle/src/ipc.rs`, `crates/shingle-nested/src/main.rs`) cover IPC dispatch, the 77-method inventory, field validation and error text, gating of keys, modifiers, motion, buttons, gestures and touch, release of held input on disable, STIPC binding dispatch and host-event classification. They run without a Wayland client, so they check compositor state, not delivered protocol events.

The `shingle-core` unit tests (`crates/shingle-core/src/tree.rs`, `core.rs`, `session.rs`, `geometry.rs`) pin exact tile order and slot geometry through insertion, drops, swaps and removal; split weights through resizing, minimum extents and floating or minimized windows; focus fallback, directional focus and stacking; floating, snapping, maximized and gap geometry; and placeholder reconciliation by launch identity. Session tests round-trip a two-output model with nested, unevenly weighted splits and every persisted flag through JSON and atomic file saves, and require tile order, sizes, state, focus and stacking to come back exactly, after repeated cycles and after reconciliation. They also cover version, unknown-field and malformed-snapshot errors. They exercise the model only; full frontend/session cycles with real clients are still needed.

Writing these tests found three layout bugs, now fixed: splitting a tile across the axis reset its size to an even share; resizing treated floating and minimized siblings as occupying space, so the moved edge and amount were wrong; and directional focus ranked by centre distance, so focus-down from a top-right tile could jump to a full-height left tile. Directional focus now picks the tile across the crossed edge's midpoint, as Wayfire's tile plugin does.

### With real clients

Before the rename, real XDG clients and Unix-socket IPC fixtures exercised mapping, geometry, workspaces, XKB layout switching (`us,de`), runtime bindings and event delivery. Plugin activation events were checked over IPC, but no interactive visual test was done in a host session.

After the rename, the older smoke test and IPC fixtures have not been repeated under the new names.

Input devices, 2026-11-25: a Python IPC fixture and a minimal raw-protocol Wayland client (one `xdg_toplevel` with `wl_pointer`, `wl_keyboard` and `wl_touch`) ran against `shingle-nested` in a window on the host session, with a private `XDG_RUNTIME_DIR`, Wayland socket, IPC socket and XDG config/state directories. All 38 checks passed:

- `list-methods` reported 77 methods including both input methods; `input/list-devices` returned the keyboard, pointer and touch devices (IDs 1–3) with all six fields, all enabled.
- With a button, a key and a touch point held, disabling the pointer delivered a button release, disabling the keyboard delivered a key release, and disabling touch delivered `wl_touch.cancel`.
- With the pointer disabled, `stipc/move_cursor` produced no client event and left `get-cursor-position` unchanged; STIPC button clicks were also dropped.
- A binding registered with `command/register-binding` fired a `command-binding` event from STIPC `feed_key` while enabled, did not fire with the keyboard disabled, and keys produced no client events.
- Touch input was dropped while touch was disabled.
- Re-enabling all three restored client motion and cursor position, key delivery and binding firing.
- Unknown ID, missing `id` or `enabled`, non-boolean `enabled` and string `id` returned Wayfire's errors and changed nothing.

Not covered by that run: gating of real host keyboard, pointer and touch events and the nested frontend's switcher and drag clean-up, which need host input injection; and modifier delivery on disable, which was only unit-tested. The fixture's `shingle-nested` did not exit on SIGTERM or SIGINT and had to be killed.

When recording new verification, keep real-client results separate from compiled or unit-tested ones.
