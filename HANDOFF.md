# Tilefire / Shingle handoff

## Mission

Tilefire is a Rust/wgpu Wayland compositor built around Wayfire's tiling
behavior. The intended user-facing name is **Shingle**. The project should
become useful as a daily compositor, not merely a tiling-tree demonstration.
Compatibility with Wayfire configuration, IPC, input, and session behavior is a
core requirement.

The project root at the time of writing is `/workspace/tilefire`. The local
Wayfire reference checkout is `/workspace/wayfire`.

## Readiness summary

The nested compositor is useful for isolated testing and already has a broad
range of compositor features. It is not yet a drop-in hardware replacement for
Wayfire. The largest missing pieces are direct DRM/KMS and libinput, DMA-BUF,
XWayland, and complete multi-output support. A number of Wayfire plugins and
visual effects also remain absent.

Do not claim that Shingle is more stable or resource-efficient than Wayfire.
No comparative benchmark or long-running soak test has established that.
Nested wgpu rendering, CPU shared-memory copies, and per-frame work may make it
slower in some scenarios.

## Verification baseline

Before the unfinished input-device work described below, the tree passed:

```sh
cargo fmt --all
cargo clippy --workspace --all-targets --jobs 2 -- -D warnings
cargo test --workspace --jobs 2
```

There were 54 passing workspace tests. Real XDG clients and Unix-socket IPC
fixtures also exercised mapping, geometry, workspaces, XKB layout switching,
runtime bindings, and event delivery. The fixtures were removed after use.

The current tree still passes `cargo check -p tilefire --jobs 2`, but the
unfinished input-device slice introduces three dead-code warnings. Therefore
the current tree must not be described as strict-Clippy clean until that work
is finished and the full checks are rerun.

The imported Gloomy sources also emit existing warnings during workspace
builds. These are separate from the new input warnings; do not hide either
category in status reports.

## Current capabilities

### Compositor and renderer

- Rust compositor core with wgpu rendering and a winit nested frontend.
- XDG toplevels and popups, layer shell, dialogs, decorations, subsurfaces,
  viewports, fractional scaling, and shared-memory buffers.
- Tiling, floating windows, exact floating geometry, snapping, focus,
  stacking, minimize, fullscreen, sticky, and show-desktop behavior.
- Workspace navigation with directional, last, numbered, carry, and send
  actions, including configured wraparound behavior.
- Scale and Expo overviews, workspace transitions, zoom, inversion, opacity,
  window rotation, and opening/closing animations.
- Screencopy, gamma, tearing, FIFO, commit timing, presentation timing, and
  pointer-control protocol work.
- Text input v3, input-method v1/v2, input panels, foreign-toplevel protocols,
  ext-workspace, xdg-foreign, security-context filtering, and primary-selection
  controls.

### Sessions

The persistent tiling model records workspace, tree ordering, split weights,
window geometry, floating/tiled state, snapped state, and related flags.
Session writes use the session crate's atomic persistence path. The original
requirement is exact tile order and size after logout/login; retain that
requirement and test complete frontend/session cycles, not only serialization.

Pre-map deferred views use placeholder presence until released. They must not
be rendered, focused, included in Scale, or reported as mapped prematurely.

### Idle and locking

`tilefire-lock` is an `ext-session-lock-v1` client with swaylock-style options
and PAM authentication through `tilefire-auth`. Idle, DPMS, fullscreen
suppression, lock coordination, and lock-surface handling exist. Continue to
test real locking behavior rather than assuming option compatibility implies
complete swaylock parity.

### Configuration and WCM

- YAML is native and validated with unknown-field rejection.
- Wayfire INI and i3-style imports exist, including many key bindings.
- Physical-key and case-sensitive `xkb:` bindings are supported.
- Comma-separated XKB layouts and per-layout variants generate distinct XKB
  groups. This was fixed after a real `us,de` client test exposed the bug.
- WCM uses the imported Gloomy UI code and can edit, validate, and convert
  configuration.
- Live IPC configuration updates exist, but they are not written back to YAML.
- Not every Wayfire or i3 option is supported. Keep the README's supported
  option claims synchronized with the implementation.

### Wayfire IPC

The nested frontend exposes length-prefixed JSON over a Unix socket and exports
`WAYFIRE_SOCKET` to spawned processes. `list-methods` currently reports 75
methods before the unfinished input slice.

Implemented areas include:

- View, output, workspace-set, focus, cursor, geometry, and property queries.
- View configuration, tiling masks, close, focus, alpha, and window actions.
- Full simple-tile layout import/export and show-maximized control.
- Vswitch set/send and directional, last, and numeric activators.
- Runtime command registration, removal, normal/repeat/release modes, direct
  method callbacks, and notification cleanup on disconnect.
- Scale, Expo, and inversion activation dispatch.
- STIPC keys, buttons, cursor, touch, command execution, display query, and
  atomic batch view layout.
- Live config query/list/update, including safe workspace-grid shrinking.

Events include lifecycle, focus, metadata, geometry with `old-geometry`,
workspace, minimize, fullscreen, sticky, tiling, stacking, workspace-set,
output-layout, keyboard modifier state, and plugin activation state.

Explicit `view-pre-map` subscriptions delay the first map until every watcher
calls `window-rules/unblock-map`. A disconnect releases that watcher's block.
Catch-all watches deliberately do not subscribe to pre-map, matching Wayfire.

Plugin activation events are sent by actual nested Scale/Expo/invert state
transitions. Their IPC payload delivery was tested, but an interactive visual
host-session test was not performed.

## Important source locations

- `crates/tilefire-core/src/core.rs`: persistent windows and mapped presence.
- `crates/tilefire-core/src/tree.rs`: tiling order, focus, masks, and splits.
- `crates/tilefire-core/src/session.rs`: serializable session representation.
- `crates/tilefire/src/lib.rs`: public compositor API and renderer events.
- `crates/tilefire/src/state.rs`: shared state and workspace transitions.
- `crates/tilefire/src/input.rs`: keymap, focus, input routing, and STIPC input.
- `crates/tilefire/src/shell.rs`: XDG roles, map lifecycle, geometry, and rules.
- `crates/tilefire/src/ipc.rs`: IPC transport, dispatch, discovery, and events.
- `crates/tilefire-nested/src/main.rs`: host input, visual state, and frontend
  dispatch.
- `crates/tilefire-config/src/lib.rs`: native model and XKB generation.
- `crates/tilefire-config/src/wayfire.rs`: Wayfire INI import.
- `crates/tilefire-config/src/i3.rs`: i3 import.
- `crates/tilefire-session`: atomic session persistence and process launching.
- `crates/tilefire-idle`: idle, DPMS, and lock coordination.
- `crates/tilefire-lock`: lock client and swaylock-style option handling.
- `crates/tilefire-wcm`: Gloomy-based configuration UI.
- `README.md`: user-facing capabilities and limitations.

## Unfinished work currently in the tree

An `input/list-devices` / `input/configure-device` slice was interrupted before
completion. It is not exposed through IPC and has not passed the full checks.

Current partial changes in `crates/tilefire/src/input.rs` include:

- `InputDeviceKind::{Keyboard, Pointer, Touch}`.
- Stable nested device metadata with IDs 1, 2, and 3 in `INPUT_DEVICES`.
- A three-entry `devices_enabled` array in `InputState`.
- `input_device_enabled` and `set_input_device_enabled` helpers.
- Pointer motion and pointer-button gating.

Still required if this slice is continued:

1. Add both methods to IPC dispatch and `SUPPORTED_METHODS`; the count becomes
   77.
2. Return Wayfire-compatible fields: `id`, `name`, `vendor`, `product`, `type`,
   and `enabled`. Unknown vendor/product strings are appropriate for nested
   virtual devices.
3. Validate device IDs and require a boolean `enabled` field. Unknown IDs must
   return an error.
4. Complete keyboard, modifier, scroll, touch, gesture, and nested host-input
   gating. Decide how disable-during-hold releases pressed keys, buttons, and
   touches so clients are not left stuck.
5. Test with real IPC calls: list three devices, disable pointer and verify
   STIPC cursor movement is suppressed, disable keyboard and verify a
   registered action is not triggered, re-enable both, and test bad requests.
6. Update README and this handoff, remove fixtures, then rerun all checks.

The current three warnings are expected from this partial state: unused
Keyboard/Touch variants, `INPUT_DEVICES`, and `set_input_device_enabled`.
Finishing the IPC and routing work should remove them naturally; do not silence
them with allowances.

## Recommended priorities

1. Finish and verify the partial input-device slice before starting another
   compatibility feature.
2. Add native DRM/KMS and libinput with careful failure handling.
3. Add DMA-BUF and reduce unnecessary CPU texture copies.
4. Add XWayland support.
5. Complete multi-output lifecycle, focus, workspace, session, and rendering
   behavior, including headless-output IPC where appropriate.
6. Continue missing Wayfire plugin/config compatibility using the local
   Wayfire source as the behavioral reference.
7. Add long-running nested and native soak tests, then measure CPU, RSS, frame
   latency, and GPU behavior before making resource or stability claims.

## Shingle rename, formatting, and documentation phase

A substantial amount of formatting and documentation work remains. It should
be done **alongside** the Tilefire-to-Shingle rename as one coordinated phase,
not scattered through unrelated behavioral fixes. Do not rename pieces
opportunistically while implementing features.

### Rename scope

- Rename the workspace, crates, package names, library crate identifiers, and
  binaries consistently. Expected names include `shingle`, `shingle-core`,
  `shingle-nested`, `shingle-lock`, `shingle-wcm`, and related crates.
- Update `Cargo.toml`, `Cargo.lock`, module imports, build scripts, CMake or
  helper scripts, assets, example commands, and documentation together.
- Rename user-visible text, logs, error prefixes, application IDs, and default
  configuration/session locations deliberately.
- Choose migration behavior for old configuration, sessions, and environment
  variables. Do not silently strand existing sessions. A transitional
  `TILEFIRE_LOCK` fallback may be useful when introducing `SHINGLE_LOCK`.
- Preserve Wayfire compatibility names such as `WAYFIRE_SOCKET`, IPC method
  names, event names, and Wayfire INI section names.
- Do not rename upstream Wayland protocols or third-party Gloomy crates as if
  they belonged to Shingle.
- Update absolute paths in documentation and tooling from `/workspace/tilefire`
  only when the actual directory is renamed.

### Formatting scope

- Keep Rust at the configured 80-column limit in `rustfmt.toml`.
- Run `cargo fmt --all` after edits and manually review long matches, protocol
  guards, JSON literals, and nested expressions that rustfmt cannot make clear.
- Keep Markdown and code examples near 80 columns where practical.
- Avoid a blind reformat of imported third-party Gloomy sources. Separate any
  upstream cleanup from Shingle-owned changes.
- Check generated files and lockfiles after the rename rather than editing
  generated output inconsistently.

### Documentation scope

- Document public APIs, state-machine transitions, session invariants, map
  blocking, focus rules, output transforms, and non-obvious protocol behavior.
- Update comments and docstrings whenever behavior changes; remove descriptions
  of the old Tilefire names or old behavior.
- Keep method counts, supported configuration lists, test counts, and known
  limitations factual and current.
- Record what was tested with real clients separately from what was only
  compiled or unit-tested.
- Update this handoff at the end of the rename and cleanup phase.

## Build environment

The working toolchain is bundled under `/workspace/.tools`. Use these exports
in this environment:

```sh
export CARGO_HOME=/workspace/.tools/cargo
export RUSTUP_HOME=/workspace/.tools/rustup
export PATH=/workspace/.tools/bin:/workspace/.tools/cargo/bin:$PATH
export PATH=/workspace/.tools/sysroot/usr/bin:$PATH
export PATH=/workspace/.tools/sysroot/bin:$PATH
export CARGO_BUILD_TARGET=x86_64-unknown-linux-gnu
export CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER=/workspace/.tools/bin/cc
export PKG_CONFIG=/workspace/.tools/sysroot/usr/bin/pkg-config
export PKG_CONFIG_PATH="/workspace/.tools/sysroot/usr/lib/\
x86_64-linux-gnu/pkgconfig"
export PKG_CONFIG_PATH="$PKG_CONFIG_PATH:\
/workspace/.tools/sysroot/usr/share/pkgconfig"
export LD_LIBRARY_PATH=/workspace/.tools/sysroot/usr/lib/x86_64-linux-gnu
export XKB_CONFIG_ROOT=/workspace/.tools/sysroot/usr/share/X11/xkb
```

`XKB_CONFIG_ROOT` is required here because the normal host XKB data path is
absent. External fixture builds should also use:

```sh
export CARGO_TARGET_DIR=/workspace/tilefire/target
```

Change that target path when the project directory is renamed.

## Test and operational rules

- Check `/proc/loadavg` before builds and keep Cargo to at most `--jobs 2`.
- Run builds and test suites sequentially rather than creating load spikes.
- Never kill or replace the user's running Wayfire session for a test.
- Use isolated XDG_RUNTIME_DIR, Wayland, and IPC sockets for real-client tests.
- Exercise actual protocol calls and state changes, not just imports or builds.
- Remove temporary fixture sources, binaries outside `target`, sockets, and logs
  after each run. Leave the normal `target` directory intact.
- The Tilefire project directory is not itself a Git repository at the time of
  writing. Do not assume `git status` can provide a change inventory.
- No background tasks were running and `/workspace/.tmp` was empty when this
  handoff was prepared.

## Safe nested smoke test

From the project root:

```sh
cargo run -p tilefire-nested -- \
  --config data/tilefire.yaml --spawn foot
```

This should open a nested compositor without stopping the host Wayfire. The
nested process provides its own Wayland socket to the spawned application.
Locking uses `tilefire-lock` from `PATH`, with `TILEFIRE_LOCK` available as an
override until the rename migration is implemented.
