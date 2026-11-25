# Status

Last updated 2026-11-25, after the compositor gained the host API a native backend needs (NB-2).

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

### Backend host API

What a native backend needs from `Compositor` (NB-2 in [design/native-backend.md](design/native-backend.md#nb-2-shingle-compositor-host-api-what-a-real-backend-needs-from-compositor), crates/shingle/src/lib.rs). Unit-tested; nothing but nested uses it yet.

- `shutdown_requested()`: SIGINT, SIGTERM and `request_quit` end `shingle-nested` cleanly, saving the session.
- `impl AsFd`: the event-loop descriptor, readable whenever `dispatch_pending` has work, including a 50 ms timerfd tick for idle timeouts and IPC.
- `set_output_info(OutputInfo)`: connector name, make, model, serial, physical size, subpixel layout and refresh rate for `wl_output`, xdg-output, wlr-output-management, ext-workspace ids and IPC. The default is still `HEADLESS-1` at 60 Hz. There is no mode list yet, and the core output keeps its `HEADLESS-1` id (both NB-9).
- `frame_presented(PresentInfo)`: frame callbacks and `wp_presentation` feedback with the backend's timestamp, refresh interval, sequence number and flags. `frame()` reports "now" with the `OutputInfo` refresh rate.
- `set_input_time`: seat input carries the backend's `CLOCK_MONOTONIC` event time. Input and frame-callback timestamps now use `CLOCK_MONOTONIC` instead of the time since startup.
- `set_session_active(bool)`: pausing releases all held keys, buttons, touches, modifiers and gestures, then drops seat and STIPC input and withholds frame callbacks until resumed.
- `set_default_cursor(bool)`: draws the theme's default arrow when no client cursor applies. Off in nested.

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

`cargo test --workspace` runs 199 unit tests (2026-11-25), 88 of them in `shingle-core` and 51 in `shingle`. Coverage of the other crates is uneven.

`cargo clippy --workspace --all-targets -- -D warnings` passes (2026-11-25).

The input-device unit tests (`crates/shingle/src/input_device.rs`, `crates/shingle/src/ipc.rs`, `crates/shingle-nested/src/main.rs`) cover IPC dispatch, the 77-method inventory, field validation and error text, gating of keys, modifiers, motion, buttons, gestures and touch, release of held input on disable, STIPC binding dispatch and host-event classification. They run without a Wayland client, so they check compositor state, not delivered protocol events.

The `shingle-core` unit tests (`crates/shingle-core/src/tree.rs`, `core.rs`, `session.rs`, `geometry.rs`) pin exact tile order and slot geometry through insertion, drops, swaps and removal; split weights through resizing, minimum extents and floating or minimized windows; focus fallback, directional focus and stacking; floating, snapping, maximized and gap geometry; and placeholder reconciliation by launch identity. Session tests round-trip a two-output model with nested, unevenly weighted splits and every persisted flag through JSON and atomic file saves, and require tile order, sizes, state, focus and stacking to come back exactly, after repeated cycles and after reconciliation. They also cover version, unknown-field and malformed-snapshot errors. They exercise the model only; full frontend/session cycles with real clients are still needed.

The backend host API tests (`crates/shingle/src/lib.rs`, `input.rs`, `input_device.rs`, `presentation.rs`, `cursor_shape.rs`, `clock.rs`, `ipc.rs`) cover `shutdown_requested` after `request_quit` and after raising SIGTERM and SIGINT in the test thread; the event-loop descriptor becoming readable for a connecting client and for the tick, and not staying readable after dispatch; backend input timestamps and their precedence; release of all held input on session pause, dropped input and STIPC bindings while paused, and unchanged IPC device state; and the default cursor's visibility rules. Several use an in-process `wayland-client` test client (crates/shingle/src/test_client.rs) on a socket in the temporary directory, so they check what a client actually receives: `HEADLESS-1`, 310x175 mm and 60 Hz without output info; a described output's name, description, geometry, subpixel and refresh in `wl_output`, xdg-output and output-management heads and modes; updates to bound outputs without a second `name` event; `frame_presented` timestamps, refresh, sequence and flags in `wp_presentation_feedback.presented` and the frame callback time; and frames withheld while the session is inactive.

Writing these tests found three layout bugs, now fixed: splitting a tile across the axis reset its size to an even share; resizing treated floating and minimized siblings as occupying space, so the moved edge and amount were wrong; and directional focus ranked by centre distance, so focus-down from a top-right tile could jump to a full-height left tile. Directional focus now picks the tile across the crossed edge's midpoint, as Wayfire's tile plugin does.

### With real clients

Before the rename, real XDG clients and Unix-socket IPC fixtures exercised mapping, geometry, workspaces, XKB layout switching (`us,de`), runtime bindings and event delivery. Plugin activation events were checked over IPC, but no interactive visual test was done in a host session.

After the rename, 2026-11-25: the smoke test and IPC fixtures were repeated under the Shingle names. Python fixtures drove `shingle-nested` (debug build of `ec71234`) in one window on the host session, each run with its own `XDG_RUNTIME_DIR`, Wayland socket, IPC socket (`--ipc`, also exported as `WAYFIRE_SOCKET`), `XDG_CONFIG_HOME`, `XDG_STATE_HOME` and `HOME`; only the nested window itself connected to the host socket. Clients were a raw-protocol Python Wayland client (one `xdg_toplevel`, `wl_shm` buffers redrawn at every configured size, optional `wl_keyboard`, logging every configure, keymap, modifier, key and close event) and `wayland-info`; IPC used Wayfire's length-prefixed JSON. Instances were stopped with SIGKILL. The host output size, not the configured one, set the nested output (853×1440 in these runs). Results, all from observed protocol and IPC state:

| Area | Method | Result |
| --- | --- | --- |
| Nested smoke test | Start, `stipc/get_display`, `list-methods`, `wayland-info`, one client | Pass, 16/16. Private sockets in place, 77 sorted methods, 48 globals including `xdg_wm_base`, `wl_seat`, `wl_shm` and `zwlr_layer_shell_v1`; the client mapped, was focused and was configured to its tile size. |
| Mapping | Three clients, catch-all `events/watch`, `list-views`, `get-layout` | Pass, 8/8. `view-mapped` per client, titles and app IDs from the protocol, tile order equals mapping order, tiles cover the workarea exactly, and each client's last configure equals its view size. |
| Geometry | `configure-view`, `simple-tile/set-layout`, `set-fullscreen`, `set-minimized` | Pass, 19/19. Exact floating rectangle in `view-info` and in the client configure, `view-geometry-changed` with `old-geometry`, the floating view leaves the tree and the rest retile; 2:1 and 1:3 weights reported as 0.67 and 0.25 and configured; fullscreen fills the output with the fullscreen state and restores the tile; minimize and restore keep the order. |
| Workspaces | `vswitch/send-view`, `set-workspace` with and without `view-id`, `binding_right`, `binding_last`, `binding_left` | Pass, 15/15. `view-workspace-changed` with `from`/`to`, the sent view tiled alone on its workspace, `wset-workspace-changed`, carrying a view back, no wraparound at the edge, out-of-grid target rejected. |
| Wayfire IPC | View properties, alpha, `focus-view`, `view-pre-map` blocking, runtime bindings, `close-view` | Pass, 32/32. A blocked view is unmapped, unfocused and gets no `view-mapped` until `unblock-map`; with two subscribers one release keeps it blocked and disconnecting the other maps it. Notification, `command` and `call-method` bindings fire from STIPC `feed_key` with `<super>`; not without the modifier or after `unregister-binding`. `close-view` sends `xdg_toplevel.close` and emits `view-unmapped`. |
| XKB `us,de` | `get-keyboard-state`, `set-keyboard-state`, client keymap and modifiers | Pass, 11/11. Two layouts reported; the keymap the client received compiles with libxkbcommon to `English (US)` and `German`; switching to index 1 and back sends `wl_keyboard.modifiers` with group 1 and 0 and `keyboard-modifier-state-changed`; index 2 rejected. |
| Session round trip | Five clients in a nested, unevenly weighted split, one floating at an exact rectangle on workspace 2 and one floating on workspace 1; SIGKILL and restart twice | Pass, 17/17, after the fix below. Before the fix it failed: every restored client became a new tile and the five placeholders stayed, doubling the windows on each restart. After it, both restarts relaunched all five clients from their saved command lines and `get-layout` for all three workspaces and the `list-views` ID, geometry, tiled edges and state were identical to before, with each client configured to its saved size. |
| Tilefire migration | Session built with Shingle paths, then moved to `tilefire/tilefire.yaml` and `tilefire/session.json`; started without `--config` and with only `TILEFIRE_LOCK`; two windows turned into same-app-ID placeholders without a command, tagged with launch IDs | Pass, 23/23. The Tilefire config applied (3×1 grid, `us,de`); the session restored identically and was saved back to `tilefire/session.json`, with no `shingle/` config or session created; clients started by hand with `TILEFIRE_LAUNCH_ID` claimed their own placeholders in reverse order; a second restart was identical; the idle lock ran the `TILEFIRE_LOCK` program with the private `WAYLAND_DISPLAY`; with both set, `SHINGLE_LOCK` won. |

Bug found and fixed: a toplevel was created as soon as it sent `xdg_toplevel.set_title`, with the launch template's app ID (the program name), so a client that sends its title before its app ID never claimed its restored placeholder. The window is now created at the app ID or first buffer, and a claimed placeholder takes the new title. Regression test: `title_before_app_id_claims_the_restored_placeholder` in `crates/shingle/src/shell.rs`, which drives the protocol over a socket pair.

Observed, not fixed (see [roadmap.md](roadmap.md#7-remaining-wayfire-compatibility)): a view blocked in `view-pre-map` already holds its tile, so its neighbours shrink around an empty, unrendered slot until it is released; and clients receive the keymap as `include` statements rather than a compiled keymap. `shingle-nested` still kept running 3 s after SIGTERM.

Not covered: the PAM service fallback (needs `/etc/pam.d/tilefire`), and a clean quit and restart, because no IPC method quits and the quit binding needs the host keyboard; every restart above followed SIGKILL, so it tested the incremental saves. Host input and the clean quit are listed under [Needs operator: host input](#needs-operator-host-input).

Input devices, 2026-11-25 (before the run above): a Python IPC fixture and a minimal raw-protocol Wayland client (one `xdg_toplevel` with `wl_pointer`, `wl_keyboard` and `wl_touch`) ran against `shingle-nested` in a window on the host session, with a private `XDG_RUNTIME_DIR`, Wayland socket, IPC socket and XDG config/state directories. All 38 checks passed:

- `list-methods` reported 77 methods including both input methods; `input/list-devices` returned the keyboard, pointer and touch devices (IDs 1–3) with all six fields, all enabled.
- With a button, a key and a touch point held, disabling the pointer delivered a button release, disabling the keyboard delivered a key release, and disabling touch delivered `wl_touch.cancel`.
- With the pointer disabled, `stipc/move_cursor` produced no client event and left `get-cursor-position` unchanged; STIPC button clicks were also dropped.
- A binding registered with `command/register-binding` fired a `command-binding` event from STIPC `feed_key` while enabled, did not fire with the keyboard disabled, and keys produced no client events.
- Touch input was dropped while touch was disabled.
- Re-enabling all three restored client motion and cursor position, key delivery and binding firing.
- Unknown ID, missing `id` or `enabled`, non-boolean `enabled` and string `id` returned Wayfire's errors and changed nothing.

Not covered by that run: gating of real host keyboard, pointer and touch events and the nested frontend's switcher and drag clean-up, which need physical host input (see [Needs operator: host input](#needs-operator-host-input)); and modifier delivery on disable, which was only unit-tested. The fixture's `shingle-nested` did not exit on SIGTERM or SIGINT and had to be killed; that is fixed, see below.

Signals, 2026-11-25: a Python fixture started `shingle-nested --ipc` in a window on the host session with a private `XDG_RUNTIME_DIR`, Wayland socket (`wayland-0`), IPC socket and XDG config and state directories, waited until `list-methods` answered over IPC (77 methods), then sent one signal. With the NB-2 build, SIGTERM and SIGINT each made it exit with status 0 about 0.11 s later; it wrote `shingle/session.json` to the private state directory and removed its Wayland socket. The same fixture against a build of `master` at `cf7aca5` reproduced the bug: neither signal ended the process within 10 s, and it had to be killed.

#### Needs operator: host input

These checks need physical keyboard, pointer and touch input on the host, which an agent must not inject. Run them from a terminal on the host session, in the repository, after `cargo build -p shingle-nested`; `wev` is in Debian's `wev` package. Each step lists what to look for in `wev`'s output, which appears in the terminal that started Shingle.

1. Start an isolated instance with one `wev` window. The config moves the bindings used below to `ctrl+alt`, because Wayfire's defaults grab `super`+drag and `alt+escape` before they reach the nested window; pick other keys if the host still grabs these.

   ```sh
   export T=$(mktemp -d); mkdir -p $T/run $T/cfg $T/state
   chmod 700 $T/run
   cat > $T/test.yaml <<'EOF'
   pointer: {move_button: ctrl+alt+btn_left}
   keybindings:
     fast_switcher_next: ctrl+alt+j
     quit: ctrl+alt+shift+q
   EOF
   env XDG_RUNTIME_DIR=$T/run XDG_CONFIG_HOME=$T/cfg \
     XDG_STATE_HOME=$T/state \
     WAYLAND_DISPLAY=$XDG_RUNTIME_DIR/$WAYLAND_DISPLAY \
     target/debug/shingle-nested --config $T/test.yaml \
     --ipc $T/run/ipc.sock --spawn wev
   ```

2. In a second host terminal, define an IPC helper, open a second `wev` window and register a test binding on `F8`:

   ```sh
   export T=<the same directory>
   ipc() {
     python3 - "$T/run/ipc.sock" "$1" "$2" <<'PY'
   import json, socket, struct, sys
   s = socket.socket(socket.AF_UNIX)
   s.connect(sys.argv[1])
   body = json.dumps({'method': sys.argv[2],
                      'data': json.loads(sys.argv[3])}).encode()
   s.sendall(struct.pack('=I', len(body)) + body)
   n = struct.unpack('=I', s.recv(4))[0]
   print(s.recv(n).decode())
   PY
   }
   ipc stipc/run '{"cmd": "wev"}'
   ipc input/list-devices '{}'
   ipc command/register-binding \
     "{\"binding\": \"KEY_F8\", \"command\": \"touch $T/f8\"}"
   ```

3. Keyboard (ID 1). Run `sleep 4; ipc input/configure-device '{"id": 1, "enabled": false}'`, click into a `wev` window and hold `a` until the command returns. Expected: a `wl_keyboard` key event with state released at that moment, no repeats after it, and no further key or modifier events while typing. Press `F8` in the nested window: `$T/f8` must not appear. Re-enable with `'{"id": 1, "enabled": true}'` while holding `shift`: `wev` shows a modifiers event with shift depressed, and keys arrive again; `F8` now creates `$T/f8`.
4. Pointer (ID 2). Run `sleep 4; ipc input/configure-device '{"id": 2, "enabled": false}'` and hold the left button over a `wev` window until it returns. Expected: a `wl_pointer` button release at that moment; afterwards no motion, button or axis events, and `ipc window-rules/get-cursor-position '{}'` stays the same while the mouse moves. Re-enable: motion and clicks arrive again.
5. Drag clean-up. With the pointer enabled, run `sleep 4; ipc input/configure-device '{"id": 2, "enabled": false}'`, then start a `ctrl+alt`+left-button drag of one `wev` window onto the other and keep moving. Expected: when the pointer is disabled the drag ends with the windows as they were, without a swap, drop preview or snap; `ipc simple-tile/get-layout '{"wset-index": 1, "workspace": {"x": 0, "y": 0}}'` shows the original order. Re-enable the pointer afterwards.
6. Switcher. Run `sleep 4; ipc input/configure-device '{"id": 1, "enabled": false}'`, then in the nested window hold `ctrl+alt` and press `j` to open the fast switcher, and keep `ctrl+alt` held. Expected: when the keyboard is disabled the switcher closes as if `ctrl+alt` were released, with the selected window focused, and does not reopen or stay stuck. Re-enable the keyboard.
7. Touch (ID 3), only with a touchscreen. Run `sleep 4; ipc input/configure-device '{"id": 3, "enabled": false}'` and keep a finger on a `wev` window. Expected: `wl_touch` cancel, not up, at that moment, and no touch events afterwards. Re-enable: touches arrive again.
8. Clean quit and restore. Note `ipc simple-tile/get-layout '{"wset-index": 1, "workspace": {"x": 0, "y": 0}}'`, then press `ctrl+alt+shift+q` in the nested window. Expected: `shingle-nested` exits and `$T/state/shingle/session.json` exists. Start it again with step 1's command but without `--spawn wev`: both `wev` windows come back in the same tile order and sizes and the same `get-layout` output.
9. Stop the instance with `pkill -TERM -f "ipc $T/run/ipc.sock"` (it now exits cleanly on SIGTERM) and remove `$T`.

Record each result here, replacing this list.

When recording new verification, keep real-client results separate from compiled or unit-tested ones.
