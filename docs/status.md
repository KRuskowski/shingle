# Status

Last updated 2026-11-25, after replacing the separate binaries with one `shingle` binary that selects its backend.

## Readiness

The `shingle` binary runs nested inside an existing Wayland or X11 session, or headless, and is useful for isolated testing. It is **not** yet a drop-in hardware replacement for Wayfire. The largest gaps are:

- No direct DRM/KMS or libinput backend; only the nested winit backend and the headless one exist. `shingle` started outside a host session, or with `--backend native`, says that the native backend is not built yet and exits with status 1.
- No DMA-BUF; client buffers are shared memory and are copied by the CPU.
- No XWayland.
- Multi-output support is incomplete.
- Several Wayfire plugins and visual effects are missing, including wrot's 3D mode.

Do not claim that Shingle is more stable or uses fewer resources than Wayfire. No comparative benchmark or long-running soak test has been done, and nested wgpu rendering, CPU buffer copies and per-frame work may make it slower in some scenarios.

## Implemented

### Compositor and renderer

- wgpu rendering with a winit nested frontend. `shingle-render` itself has no winit dependency: a shared `GpuContext`, a per-output `RenderState`, one `draw` pass, and `WindowTarget` and `OffscreenTarget`, with `Renderer` and `OffscreenRenderer` as thin wrappers (native backend NB-1). The wgpu `drm` feature is no longer enabled.
- XDG toplevels and popups, layer shell, dialogs, decorations, subsurfaces, viewports, fractional scaling and shared-memory buffers.
- Tiling, floating windows, exact floating geometry, snapping, focus, stacking, minimize, fullscreen, sticky and show-desktop.
- Workspace navigation: directional, last, numbered, carry and send actions, with configurable wraparound.
- Scale and Expo overviews, vswitch transitions, zoom, inversion, opacity, window rotation, and opening/closing animations.
- Screencopy, gamma, tearing control, FIFO, commit timing, presentation timing and pointer warp. A client may destroy its FIFO or commit-timer object and create a new one for the same surface, as Mesa's Vulkan WSI does on swapchain recreation; before NB-3 that was a protocol error that disconnected every Mesa Vulkan client on its first resize.
- The interactive frontend is `shingle_frontend::Frontend` (NB-4): Scale, Expo, the switchers, drags, animations, effects, the scene build, binding dispatch, key repeat, config reload and session saving, behind a `Host` trait and a neutral `InputEvent` in output pixels and evdev codes, drawing into a `RenderTarget`. `shingle-nested` is a library and a thin winit host: the window and renderer, winit-to-`InputEvent` translation, host focus handling and button emulation. `cargo tree -p shingle-frontend` has no winit. Ctrl+Alt+F1..F12 (`XF86Switch_VT_n`) goes to `Host::switch_vt` first; nested cannot switch, so there the key behaves as before.
- Text input v3, input-method v1/v2, input panels, foreign-toplevel protocols, ext-workspace, xdg-foreign, security-context filtering and primary-selection controls.

Details are in [features.md](features.md).

### The `shingle` binary

One binary, `shingle` (package `shingle-bin`, crates/shingle-bin), replaces `shingle-nested` and the old headless-only `shingle`; `shingle-nested` is now a library and its binary is gone. It picks its backend the way Wayfire (through wlroots' `wlr_backend_autocreate`) does: `--backend nested|native|headless` or `--headless` first, then `SHINGLE_BACKEND`, then nested when `WAYLAND_DISPLAY`, `WAYLAND_SOCKET` or `DISPLAY` is set (empty values count as unset), and native otherwise. Native is not built yet: `shingle` explains why it was selected and exits with status 1 before starting anything. Unknown backends, unknown or repeated options and conflicting backends exit with status 2 and point to `--help`. The options are the union of the old binaries' (`--config`, `--ipc`, `--spawn`, `--headless`) plus `--backend`, `--help` and `--version`; headless now accepts `--ipc` and `--spawn` too. `WAYFIRE_SOCKET` is exported before any client starts, so restored and autostarted clients and `--spawn` children see it, which they did not in nested before. `cargo tree -p shingle` has no winit. Details and the reasons for the layout are in the design's [The `shingle` binary](design/native-backend.md#the-shingle-binary).

### Backend host API

What a native backend needs from `Compositor` (NB-2 in [design/native-backend.md](design/native-backend.md#nb-2-shingle-compositor-host-api-what-a-real-backend-needs-from-compositor), crates/shingle/src/lib.rs). Unit-tested; nothing but nested uses it yet.

- `shutdown_requested()`: SIGINT, SIGTERM and `request_quit` end the nested backend cleanly, saving the session.
- `impl AsFd`: the event-loop descriptor, readable whenever `dispatch_pending` has work, including a 50 ms timerfd tick for idle timeouts and IPC.
- `set_output_info(OutputInfo)`: connector name, make, model, serial, physical size, subpixel layout and refresh rate for `wl_output`, xdg-output, wlr-output-management, ext-workspace ids and IPC. The default is still `HEADLESS-1` at 60 Hz. There is no mode list yet, and the core output keeps its `HEADLESS-1` id (both NB-9).
- `frame_presented(PresentInfo)`: frame callbacks and `wp_presentation` feedback with the backend's timestamp, refresh interval, sequence number and flags. `frame()` reports "now" with the `OutputInfo` refresh rate.
- `set_input_time`: seat input carries the backend's `CLOCK_MONOTONIC` event time. Input and frame-callback timestamps now use `CLOCK_MONOTONIC` instead of the time since startup.
- `set_session_active(bool)`: pausing releases all held keys, buttons, touches, modifiers and gestures, then drops seat and STIPC input and withholds frame callbacks until resumed.
- `set_default_cursor(bool)`: draws the theme's default arrow when no client cursor applies. Off in nested.
- Input-device registry (NB-5): `add_input_device(InputDeviceInfo)` returns an `InputDeviceId` that every keyboard, pointer, gesture and touch entry point takes; `remove_input_device` releases what the device holds and unregisters it. Keys, buttons and touch points are held per device, so disabling one keyboard does not release keys held on another, and `Event::InputDeviceChanged` carries the ID. Nested registers its three devices as IDs 1 to 3, and its `input/*` IPC output is unchanged.

### Sessions

The persistent tiling model records workspace, tree order, split weights, window geometry, floating/tiled and snapped state and related flags. Sessions are written atomically.

The requirement is exact tile order and size after logout and login. Test full frontend/session cycles, not only serialization.

Views blocked before their first map (see [ipc.md](ipc.md#map-blocking)) are placeholders until released. They must not be rendered, focused, included in Scale or reported as mapped.

### Idle and locking

`shingle-lock` is an `ext-session-lock-v1` client with swaylock-style options and PAM authentication through `shingle-auth`. Idle, DPMS, fullscreen suppression and lock coordination exist. Option compatibility does not imply full swaylock parity; keep testing real locking.

### Configuration and WCM

- YAML is native and rejects unknown fields.
- Wayfire INI and i3-style imports cover many options and key bindings, but not all of them.
- Physical-key and case-sensitive `xkb:` bindings are supported. They also fire from STIPC `feed_key`, which nested feeds through its keyboard like a host key, as Wayfire's headless STIPC keyboard does ([ipc.md](ipc.md#runtime-bindings)).
- Nested interprets keys with Shingle's own keymap (`shingle_frontend::keyboard`, NB-3), not the host's: ordinary bindings match Wayfire's `KEY_*` names from the evdev code, `xkb:` bindings match the keysym Shingle's layout produces, binding modifiers follow Shingle's keymap options, Scale's filter gets the text of Shingle's layout, and clients get depressed, latched, locked and group masks from that state, so Caps Lock and layout-switching keys now work in nested. Names that only winit used no longer match: `digit1` is `1` (so the default `super+1`..`super+0` work), `period` is `dot`, `bracketleft` is `leftbrace`, `shiftleft` is `leftshift`. Key repeat for bindings, the overviews and the switchers is the frontend's own timer (NB-4): the most recently pressed repeating key repeats at Shingle's `repeat_rate` and `repeat_delay`, the values clients get, and nested ignores the host's repeats.
- Comma-separated XKB layouts and per-layout variants become distinct XKB groups.
- WCM, built on the Gloomy UI library, edits, validates and converts configuration.
- Live IPC configuration updates are not written back to YAML.

### Wayfire IPC

77 methods and the Wayfire event set, including `input/list-devices` and `input/configure-device` for the registered seat devices (in nested, its keyboard, pointer and touch input). Disabling a device releases the input it holds. See [ipc.md](ipc.md).

## Verification

### Automated

`cargo test --workspace` runs 333 tests (2026-11-25). Coverage of the other crates is uneven.

The `shingle-bin` unit tests (crates/shingle-bin/src/backend.rs, cli.rs, main.rs, headless.rs; 38 tests) run backend selection and the start-up decision with an injected environment: `WAYLAND_DISPLAY`, `WAYLAND_SOCKET` and `DISPLAY` each select nested, none (or only empty ones) selects native, `--backend`/`--headless` win over `SHINGLE_BACKEND`, which wins over the display variables, and an unknown `SHINGLE_BACKEND` is an error even inside a session. They cover every option, values after `=`, `--spawn` taking the rest of the line (including a `--help` meant for the program), non-UTF-8 paths, missing values, repeated paths, conflicting backends, unknown backends, options and arguments, their messages and exit statuses, the native-not-built message with its reason, `--help` fitting 80 columns and `--version`, and that a headless `--spawn` child gets the socket and its arguments.

The `shingle-frontend` input-script tests (`crates/shingle-frontend/src/frontend/tests.rs`) run a `Frontend` around a compositor on a socket in the temporary directory, with an in-process Wayland client mapping `wl_shm` toplevels and a `FakeHost` that records redraw requests, warps, VT switches, device changes and exit. They feed input through `handle_input` and `process` as a host does and cover Scale (keys, pointer hover and click, touch, cancel restoring focus), Expo (keys and a pointer click), both switchers (dimming while held, focus and opacity on release), a move drag that drops one tile onto another, a workspace switch with its transition, key repeat at the configured rate (and no repeat for modifiers or after release), `Reset`, the Ctrl+Alt+F2 VT switch, the zoom wheel, disabling the pointer over IPC and the quit binding. STIPC tests send `stipc/feed_key` over the real IPC socket and check that a YAML key binding opens Expo, keys then reach Expo, no modifier stays held, a `super+xkb:z` command runs from physical Y with a `de` layout, an IPC binding fires exactly once, and keys are dropped while the session is inactive; all but the IPC binding failed on `master` at `9a2663c`. A scene test records the tiles drawn into an `OffscreenRenderer` and checks the tiled windows, then the Scale thumbnails with a title inside each. Two more draw through a target that takes 400 ms per frame, longer than the 300 ms animation, and require the last frame drawn to show the settled state: the window at full size and alpha after the zoom open animation, and the old workspace fully off screen after a workspace slide. Both failed on `master` at `9a2663c`, which ended each animation on its first, intermediate frame. The scene tests are skipped without a GPU adapter. `KeyRepeat` has its own unit tests.

`shingle-render` has 15 unit tests and 8 offscreen pixel tests (`crates/shingle-render/tests/offscreen_pixels.rs`). The pixel tests render fixed scenes (solid, translucent, textured, UV-cropped, transformed and rotated tiles; scale, output transform and zoom; gamma ramp and both inversion modes; nearest filtering) and compare the readback with reference images in `crates/shingle-render/tests/golden`, which were rendered by the renderer before the NB-1 split. They need a wgpu adapter and are skipped without one; set `SHINGLE_RENDER_BLESS=1` to regenerate the references.

`cargo clippy --workspace --all-targets -- -D warnings` passes (2026-11-25).

The input-device unit tests (`crates/shingle/src/input_device.rs`, `crates/shingle/src/ipc.rs`, `crates/shingle-nested/src/lib.rs`) cover IPC dispatch, the 77-method inventory, field validation and error text, gating of keys, modifiers, motion, buttons, gestures and touch, release of held input on disable, STIPC binding dispatch and host-event classification. Registry tests cover ID allocation without reuse, vendor and product IDs in `input/list-devices`, two keyboards with independent holds (including a key both hold and modifiers kept while the other keyboard holds keys), removing devices while keys and buttons are held, configure-device on unknown and removed IDs, input from unknown or wrongly kinded devices, gestures owned by one of two pointers, and STIPC's choice of device. These run without a Wayland client, so they check compositor state, not delivered protocol events. Two nested-backend fixture tests run a compositor whose sockets are in the temporary directory (`Compositor::with_socket_path`), register nested's devices as `App::new` does and send `input/list-devices` and `input/configure-device` over the real IPC socket; the `list-devices` reply must match, byte for byte, the reply recorded from `master` at `7189b50`.

The `shingle-core` unit tests (`crates/shingle-core/src/tree.rs`, `core.rs`, `session.rs`, `geometry.rs`) pin exact tile order and slot geometry through insertion, drops, swaps and removal; split weights through resizing, minimum extents and floating or minimized windows; focus fallback, directional focus and stacking; floating, snapping, maximized and gap geometry; and placeholder reconciliation by launch identity. Session tests round-trip a two-output model with nested, unevenly weighted splits and every persisted flag through JSON and atomic file saves, and require tile order, sizes, state, focus and stacking to come back exactly, after repeated cycles and after reconciliation. They also cover version, unknown-field and malformed-snapshot errors. They exercise the model only; full frontend/session cycles with real clients are still needed.

The backend host API tests (`crates/shingle/src/lib.rs`, `input.rs`, `input_device.rs`, `presentation.rs`, `cursor_shape.rs`, `clock.rs`, `ipc.rs`) cover `shutdown_requested` after `request_quit` and after raising SIGTERM and SIGINT in the test thread; the event-loop descriptor becoming readable for a connecting client and for the tick, and not staying readable after dispatch; backend input timestamps and their precedence; release of all held input on session pause, dropped input and STIPC bindings while paused, and unchanged IPC device state; and the default cursor's visibility rules. Several use an in-process `wayland-client` test client (crates/shingle/src/test_client.rs) on a socket in the temporary directory, so they check what a client actually receives: `HEADLESS-1`, 310x175 mm and 60 Hz without output info; a described output's name, description, geometry, subpixel and refresh in `wl_output`, xdg-output and output-management heads and modes; updates to bound outputs without a second `name` event; `frame_presented` timestamps, refresh, sequence and flags in `wp_presentation_feedback.presented` and the frame callback time; and frames withheld while the session is inactive.

The map-blocking and keymap tests (`crates/shingle/src/shell.rs`, `input.rs`, `crates/shingle-core/src/tree.rs`, `core.rs`, `session.rs`) drive the in-process test client with xdg toplevels, shm buffers and a `view-pre-map` subscriber. A blocked new view is absent from `simple-tile/get-layout` from creation until `unblock-map`, also when the subscriber arrives after its app ID; its neighbour keeps its configured size; it still answers `view-info`; and on release it joins the layout last. A second client with the blocked view's app ID gets its own window. A blocked view outside `tile_by_default` floats at its buffer size. A blocked client claiming a restored placeholder keeps the placeholder's slot throughout. In the model, a held leaf takes no space or focus, `release_held` appends it, `reconcile_mapped` skips it and sessions do not persist the flag. The keymaps a client receives on `wl_keyboard`, after a virtual-keyboard upload, on an input-method v2 grab and after `set_keymap` with `us,de` contain no `include` and compile with libxkbcommon without include paths. The five protocol tests failed on `master` at `efc0846`.

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

Observed in that run and fixed since (see the Wayfire compatibility run below): a view blocked in `view-pre-map` already held its tile, so its neighbours shrank around an empty, unrendered slot until it was released; and clients received the keymap as `include` statements rather than a compiled keymap. `shingle-nested` still kept running 3 s after SIGTERM in that run.

Not covered: the PAM service fallback (needs `/etc/pam.d/tilefire`), and a clean quit and restart, because no IPC method quits and the quit binding needs the host keyboard; every restart above followed SIGKILL, so it tested the incremental saves. Host input and the clean quit are listed under [Needs operator: host input](#needs-operator-host-input).

Renderer split (NB-1), 2026-11-25: the same isolated setup (private `XDG_RUNTIME_DIR`, config, state, `HOME`, IPC and Wayland sockets, `WAYFIRE_SOCKET` overridden) ran `shingle-nested` from `ca03d41` and from the NB-1 branch, one after the other, each with one raw-protocol `wl_shm` client drawing a fixed pattern at its configured size. `grim` took two wlr-screencopy captures of each instance through its private socket. With `data/shingle.yaml` (853×1440 output) and with a copy using output scale 1.5 and transform 90 (1440×853 captures, background visible), the captures before and after were byte-identical (0 differing bytes of 3,684,960 and 4,913,280). Instances were stopped with SIGKILL. The host tiling changed the nested window width once between runs (854 instead of 853), so only captures of the same size were compared.

Wayfire compatibility, 2026-11-25: a Python fixture started `shingle-nested` (debug build of the `shingle-wayfire-compat` branch) in a window on the host session with a private `XDG_RUNTIME_DIR`, Wayland socket, IPC socket (`--ipc`, also `WAYFIRE_SOCKET`), `XDG_CONFIG_HOME`, `XDG_STATE_HOME` and `HOME`, and a copy of `data/shingle.yaml` with `layout: us,de`. Raw-protocol Wayland clients (one `xdg_toplevel` each, `wl_shm` buffers redrawn at every configured size, one with `wl_keyboard`) and two IPC connections drove it; no host input was sent. All 22 checks passed (output 640×1440 in the recorded run):

- One view tiled alone at 640 px. A second IPC connection watched `view-pre-map`, and a second client's view raised it with its metadata. For the following second, `get-layout` listed only the first view at 640 px and every configure the first client received stayed 640 px wide; the blocked view answered `view-info` and was unmapped in `list-views`.
- After `unblock-map`, `get-layout` listed both views at 320 px, the released view last; both clients were configured to 315 px (the tile inset by its gaps), and the view reported `mapped`.
- The keymap the client received (70,372 bytes) contained no `include` and compiled with libxkbcommon without include paths or environment names to `English (US)` and `German`; as a control, an `include`-form keymap failed to compile that way.
- No protocol errors; SIGTERM ended the compositor with status 0.

Behaviour that changed with the fix: a blocked view is configured to the output size until it is released, and then to its tile.

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

Input-device registry, 2026-11-25: two Python fixtures ran against `shingle-nested` built from `master` at `7189b50` and then from the NB-5 branch, each in a window on the host session with a private `XDG_RUNTIME_DIR`, Wayland socket, IPC socket and XDG config and state directories, ended with SIGTERM (exit 0 every time). No host input was sent.

- IPC transcript, 38 requests: `input/list-devices` before and after toggling devices, every `input/configure-device` error case, `stipc/move_cursor` and `get-cursor-position` with the pointer enabled and disabled, STIPC buttons and touch while disabled, and a `command/register-binding` binding fired by STIPC `feed_key` with the keyboard enabled, disabled and re-enabled. All 38 raw replies and the binding events were byte-identical between the two builds.
- Held-input release, 18 steps, with a raw-protocol client (one `xdg_toplevel` with `wl_pointer`, `wl_keyboard` and `wl_touch`): a STIPC button held and the pointer disabled gave `wl_pointer.button` released; a key held and the keyboard disabled gave `wl_keyboard.key` released; a touch point held and touch disabled gave `wl_touch.cancel`; input while disabled gave no events; re-enabling the keyboard resent the modifiers, and keys, buttons and touch were delivered again afterwards. The client received the same event sequence from both builds.

Not covered with real clients: more than one device of a kind, and device removal, since nested registers exactly three devices and never removes them; those are unit-tested only. Real libinput devices are for NB-6 on the test machine.

Shared keyboard (NB-3), 2026-11-25. STIPC `feed_key` then fired only IPC-registered bindings (fixed since, see the frontend fixes below), so configured bindings were driven through a second, isolated Shingle as the host: an outer `shingle-nested` (default `us` keymap) ran in a window on the host session with a private `XDG_RUNTIME_DIR`, Wayland socket, IPC socket, `XDG_CONFIG_HOME`, `XDG_STATE_HOME` and `HOME`, and the `shingle-nested` under test ran as its Wayland client with its own private directories, a `de` keymap with `ctrl:nocaps`, and lavapipe (`VK_ICD_FILENAMES` set to `lvp_icd.json`) so that it could present through `wl_shm`. Keys were fed to the outer instance with STIPC `feed_key`, which delivers them to its focused client, the inner window, as real `wl_keyboard` events that winit reports to the inner instance. No input reached the host session. Command bindings touched marker files, workspaces were read with `window-rules/list-wsets`, and both instances were stopped with SIGTERM (exit 0).

| Check (inner keymap `de`, `ctrl:nocaps`) | NB-3 branch | `master` at `f35ff89` |
| --- | --- | --- |
| `xkb:z` fires on physical `KEY_Y` | Pass | Fail: `xkb:y` fired instead, from the host's `us` layout |
| `xkb:y` does not fire on `KEY_Y` | Pass | Fail |
| `xkb:y` fires on `KEY_Z` | Pass | Pass, by the host layout |
| `super+y` fires on `KEY_Y` | Pass | Fail |
| `super+z` does not fire on `KEY_Y` | Pass | Pass |
| `super+xkb:odiaeresis` fires on `KEY_SEMICOLON` | Pass | Fail |
| `ctrl+k` fires with Caps Lock held (`ctrl:nocaps`) | Pass | Fail |
| Built-in `super+2` switches to workspace 2 | Pass | Fail |
| Built-in `super+1` switches back | Pass | Pass, trivially: it never left workspace 1 |

10 of 10 checks passed on the branch (the table plus an IPC reply check) and 4 of 10 on `master`. The outer instance sends no `wl_keyboard.modifiers` for STIPC keys, because STIPC does not run through XKB, so `master`, which took modifiers from the host, saw none at all; the branch derives them from the keys. The layout result does not depend on that: `master` matched `xkb:` bindings against the host's layout. The first attempts also found the FIFO and commit-timing protocol errors above, which disconnected the lavapipe client after its first swapchain recreation; the runs above used the fixed outer build for both columns. Not covered: holding modifiers across host focus changes (`crates/shingle-nested/src/host_keyboard.rs`), which needs host input; see step 9 below.

Frontend split (NB-4), 2026-11-25. Before moving any code, a scripted session pinned nested's behaviour. To be independent of the host session, the outer compositor was a small headless Shingle compositor with IPC, STIPC seat devices and a frame every 16 ms (no window on the host at all); the `shingle-nested` under test ran as its Wayland client on lavapipe with a private `XDG_RUNTIME_DIR`, Wayland socket, IPC socket, `XDG_CONFIG_HOME`, `XDG_STATE_HOME` and `HOME`, with three raw-protocol `wl_shm` clients inside it. Keys, buttons, motion and touch were fed to the outer compositor over STIPC, which delivered them to the nested window as real `wl_keyboard`, `wl_pointer` and `wl_touch` events. The script has 38 steps: both switchers (also while Alt is held), Scale with keys, pointer hover and click, touch and the title filter, Expo with keys and a pointer click, sending a window to workspace 3 and following it, relative workspace switches, directional focus and move, a move drag and a resize drag with the pointer, grid snap and restore, click-to-focus, inversion on and off, a command binding and a held repeatable one, closing the focused window and the quit binding. After every step it recorded four IPC replies (`list-views` without PIDs, `get-focused-view`, `list-wsets`, `get-cursor-position`), a wlr-screencopy capture of nested's own output and one of the outer output, which shows the frame nested actually drew, Scale, Expo, the drop preview and inversion included. Because nested then decided after drawing a frame whether an animation needed another, a slow lavapipe frame could end an animation on an intermediate frame; the script therefore changes one view's alpha and sets it back before each capture, which makes nested draw the settled state again. That is fixed since: a frame draws and ends animations at one time, so the final state is always drawn (see the frontend fixes below).

| Build | IPC replies | Captures | Held binding runs | Quit binding |
| --- | --- | --- | --- | --- |
| `master` at `fcefeb9`, run twice | 152/152 identical | 74/74 identical | 46 and 46 | exit 0 |
| Key repeat moved (second commit) | 152/152 identical to `fcefeb9` | 74/74 identical | 46 | exit 0 |
| `App` moved into `Frontend` (third commit) | 152/152 identical | 74/74 identical | 46 | exit 0 |
| Final branch | 152/152 identical | 74/74 identical | 46 | exit 0 |

The held-binding count matches because the outer compositor repeats at Shingle's defaults too. With the nested instance configured for 10 repeats a second after 200 ms, holding the binding for 1.5 s ran it 45 times on `fcefeb9` (the host's 40 a second after 400 ms) and 15 times on the branch, which is the intended change. The nested smoke test on the same setup passed 14/14 on both `fcefeb9` and the branch: `WAYLAND_DISPLAY` printed and served from the private runtime directory, `stipc/get_display`, 77 sorted methods in `list-methods`, 48 globals in `wayland-info` including `xdg_wm_base`, `wl_seat`, `wl_shm` and `zwlr_layer_shell_v1`, one client mapped, focused and filling the output, and SIGTERM ending nested with status 0 after saving the session.

Not covered: real host keyboard focus changes, the left-handed and middle-emulation options (whose button translation changed, see the NB-4 entry in the design), the mouse wheel and relative pointer motion, which STIPC cannot produce; the wheel and zoom are covered by unit tests. Key repeat with physical keys is step 10 below.

Frontend fixes, 2026-11-25. STIPC keys sent to nested's own IPC socket, with the headless outer compositor of the NB-4 run as the only host (no window on the host session, no host input) and nested on lavapipe with a private `XDG_RUNTIME_DIR`, Wayland socket, IPC socket, `XDG_CONFIG_HOME`, `XDG_STATE_HOME` and `HOME`. Nested's config had a `de` layout and two command bindings writing marker files; an IPC binding was registered with `command/register-binding`. Both builds were stopped with SIGTERM (exit 0).

| Check, keys fed with `stipc/feed_key` to nested | Branch | `master` at `9a2663c` |
| --- | --- | --- |
| Default `super+2` switches to workspace 2 | Pass, (0, 0) to (1, 0) | Fail, stays at (0, 0) |
| YAML command `super+o` runs once | Pass, 1 run | Fail, 0 runs |
| YAML `super+xkb:z`, tapped on `KEY_Z` and then `KEY_Y` | Pass, 1 run | Fail, 0 runs |
| IPC binding on `KEY_F9` runs once | Pass, 1 run | Pass, 1 run |

The animation fix was checked with unit tests only (see [Automated](#automated)); no real-client capture was repeated for it.

Single binary, 2026-11-25: a Python fixture started the debug build of `target/debug/shingle` from the `shingle-single-binary` branch twice, each time with a private `XDG_RUNTIME_DIR`, IPC socket (`--ipc`), `XDG_CONFIG_HOME`, `XDG_STATE_HOME` and `HOME`, a config with `session: {restore: false}`, `--spawn sh -c ...` recording the child's `WAYFIRE_SOCKET` and `WAYLAND_DISPLAY`, and lavapipe with implicit Vulkan layers disabled. The nested run had no backend option and only `WAYLAND_DISPLAY` set, to the host socket's absolute path (`DISPLAY` and `WAYLAND_SOCKET` unset), so it opened one window on the host session; no host input was sent. The headless run had `--headless` and no display variable at all. Both passed 17/17: the startup line named the backend and its reason (`using the nested backend (WAYLAND_DISPLAY is set)`, `using the headless backend (requested on the command line)`); `WAYLAND_DISPLAY=wayland-0` was printed and served from the private runtime directory and matched `stipc/get_display`; `list-methods` returned 77 sorted methods; the `--spawn` child saw the IPC path in `WAYFIRE_SOCKET` and the private socket in `WAYLAND_DISPLAY`; `wayland-info` listed 48 globals including `xdg_wm_base`, `wl_seat`, `wl_shm` and `zwlr_layer_shell_v1`; one raw-protocol `wl_shm` client mapped, was focused and filled the output (853×1440 nested, 1280×720 headless); `/proc/<pid>/fd` showed no DRM, input or TTY device open; and SIGTERM ended `shingle` with status 0 after it wrote `shingle/session.json` to the private state directory. Outside a session, `shingle` and `shingle --backend native` printed the native-not-built message and exited 1; `--backend drm`, `--bogus` and `SHINGLE_BACKEND=x` exited 2 with the error.

#### Needs operator: host input

These checks need physical keyboard, pointer and touch input on the host, which an agent must not inject. Run them from a terminal on the host session, in the repository, after `cargo build --bin shingle`; `wev` is in Debian's `wev` package. Each step lists what to look for in `wev`'s output, which appears in the terminal that started Shingle.

1. Start an isolated instance with one `wev` window. The config moves the bindings used below to `ctrl+alt`, because Wayfire's defaults grab `super`+drag and `alt+escape` before they reach the nested window; pick other keys if the host still grabs these.

   ```sh
   export T=$(mktemp -d); mkdir -p $T/run $T/cfg $T/state
   chmod 700 $T/run
   cat > $T/test.yaml <<'EOF'
   pointer: {move_button: ctrl+alt+btn_left}
   keybindings:
     fast_switcher_next: ctrl+alt+j
     expo_toggle: ctrl+alt+e
     quit: ctrl+alt+shift+q
   EOF
   env XDG_RUNTIME_DIR=$T/run XDG_CONFIG_HOME=$T/cfg \
     XDG_STATE_HOME=$T/state \
     WAYLAND_DISPLAY=$XDG_RUNTIME_DIR/$WAYLAND_DISPLAY \
     target/debug/shingle --config $T/test.yaml \
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
8. Clean quit and restore. Note `ipc simple-tile/get-layout '{"wset-index": 1, "workspace": {"x": 0, "y": 0}}'`, then press `ctrl+alt+shift+q` in the nested window. Expected: `shingle` exits and `$T/state/shingle/session.json` exists. Start it again with step 1's command but without `--spawn wev`: both `wev` windows come back in the same tile order and sizes and the same `get-layout` output.
9. Keyboard focus (NB-3). Hold `ctrl+alt` on the host keyboard, click into a `wev` window inside the nested window, and press `e` while still holding them. Expected: Expo opens, because modifiers held on focus gain are taken over. Close it with `escape`. Then hold `ctrl+alt` in the nested window, click a host window outside it, release them there, come back into the nested window and press `e`: `wev` receives the `e` key with no modifiers held and Expo does not open.
10. Key repeat (NB-4). Press `ctrl+alt+e` to open Expo and hold `right` (or `l`). Expected: the selection moves once, then after 400 ms about 40 times a second (Shingle's defaults), whatever the host's own repeat settings are. Close Expo with `escape`.
11. Stop the instance with `pkill -TERM -f "ipc $T/run/ipc.sock"` (it now exits cleanly on SIGTERM) and remove `$T`.

Record each result here, replacing this list.

When recording new verification, keep real-client results separate from compiled or unit-tested ones.
