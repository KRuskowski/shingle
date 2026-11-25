# Wayfire IPC

The nested frontend serves Wayfire-compatible IPC: JSON messages with a native-endian `u32` length prefix over a Unix socket. Replies carry Wayfire's `result` or `error` fields.

```sh
cargo run -p shingle-nested -- --ipc /tmp/shingle-ipc.socket
```

The socket path is exported as `WAYFIRE_SOCKET` to every spawned process, so existing Wayfire scripts work unchanged. Library users can start the same server with `Compositor::enable_ipc(path)`.

`list-methods` returns a sorted inventory of the 75 supported methods. Numeric vswitch activators such as `binding_4` are matched dynamically and are not part of that count. The method table lives in `SUPPORTED_METHODS` in `crates/shingle/src/ipc.rs`.

## Methods

| Namespace | Methods |
| --- | --- |
| `window-rules` | `list-views`, `list-outputs`, `list-wsets`, `view-info`, `output-info`, `wset-info`, `get-focused-view`, `get-focused-output`, `get-cursor-position` (also `get_cursor_position`), `focus-view`, `close-view`, `configure-view`, `get-view-property`, `set-view-property`, `events/watch`, `unblock-map` |
| `wm-actions` | `minimize`, `set-minimized`, `set-fullscreen`, `set-sticky`, `set-always-on-top`, `set-always-on-bottom`, `bring-to-front`, `send-to-back`, `toggle_maximize`, `toggle_fullscreen`, `toggle_sticky`, `toggle_always_on_top`, `toggle_always_on_bottom`, `toggle_showdesktop` |
| `vswitch` | `set-workspace`, `send-view`, `binding_{left,right,up,down,last}`, `with_win_{…}`, `send_win_{…}`, plus numeric `binding_N` forms |
| `simple-tile` | `get-layout`, `set-layout`, `set-show-maximized` |
| `wayfire` | `get-config-option`, `list-config-options`, `set-config-options`, `get-keyboard-state`, `set-keyboard-state` |
| `wf/alpha` | `get-view-alpha`, `set-view-alpha` |
| `command` | `register-binding`, `unregister-binding`, `clear-bindings` |
| `scale`, `expo`, `invert` | `scale/toggle`, `scale/toggle_all`, `expo/toggle`, `invert/toggle` |
| `stipc` | `feed_key`, `feed_button`, `move_cursor`, `touch`, `touch_release`, `ping`, `run`, `get_display`, `layout_views` |
| (none) | `list-methods` |

## Views and outputs

- View IDs accept Wayfire's `id` and `view-id` spellings and the legacy `view_id`. `output_id` and `output-id` select the current output; unknown outputs are rejected.
- `configure-view` sets floating geometry, fullscreen and sticky state. Supplying geometry detaches a tiled view, updates its XDG role and emits the normal tiling event. The exact rectangle is preserved in saved sessions.
- `tiled-edges` accepts masks 0–15 for floating, half, quarter, combined and full-workarea tiling. The requested mask is also preserved in sessions.
- `wf/alpha/*` query and set per-window opacity, clamped to the inclusive 0–1 range.
- `get-view-property` and `set-view-property` store typed runtime metadata: booleans, signed and unsigned integers, strings and floats. Changing an existing property's type is rejected, and properties are removed with their view.
- Invalid outputs, coordinates, alpha values and unmapped views return errors.

## Window actions

- The `wm-actions` activators accept an explicit view ID, or act on the focused view when it is omitted.
- `toggle_showdesktop` preserves the set of windows that were already minimized and restores focus when leaving the desktop.
- `scale/*`, `expo/toggle` and `invert/toggle` dispatch frontend events to the nested renderer and also work as binding callbacks.

## Workspaces

- `vswitch/set-workspace` switches by grid coordinate, optionally taking a view along. `vswitch/send-view` moves a view without switching.
- `binding_*` switches, `with_win_*` carries the focused window and `send_win_*` sends it. Each supports left/right/up/down, last-workspace and one-based numeric targets. Directional actions honour configured wraparound. Invalid or out-of-grid numbers are rejected without changing state.

## Tiling layout

- `simple-tile/get-layout` exports split directions, child order, percentages, node geometry and view IDs from the persistent tiling tree. Floating views are omitted, and an unused workspace returns an empty layout.
- `simple-tile/set-layout` validates and imports split trees with positive `weight` or `percent` values. Included views move to the selected workspace, omitted views stay floating, mapped state is preserved, and invalid input is rejected without changing the session.
- `simple-tile/set-show-maximized` controls tiled-view maximization and rejects floating views.

## Configuration and keyboard

- `wayfire/get-config-option` returns a live `value` and its `default` as strings. `list-config-options` groups the 18 supported core, output, input, simple-tile and workaround options by section. Unknown options return errors.
- `wayfire/set-config-options` applies validated live updates with atomic rollback. Workspace-grid dimensions are writable; shrinking moves out-of-range windows to the nearest valid workspace, clamps the current workspace, and preserves geometry and tiling state. Live updates are not written back to the YAML file.
- `wayfire/get-keyboard-state` reports the layout names in the active XKB keymap. `set-keyboard-state` selects any valid layout index and sends the normal modifier notification. Comma-separated layouts and per-layout variants become distinct XKB groups.

## Runtime bindings

- `command/register-binding` supports normal, repeat and release modes, dispatched through nested keyboard and pointer input.
- A binding with a command runs it through the isolated launcher. Without one, the owning client receives a targeted `command-binding` event.
- `call-method` and `call-data` invoke an IPC method directly when the binding triggers, so a binding can perform compositor actions without launching a process.
- Notification bindings are removed when their client disconnects.

## Events

`window-rules/events/watch` subscribes a connection to all events or to a requested list. Duplicate watches and unknown event names return errors.

- Lifecycle, focus, title/app ID, geometry, workspace, minimized, fullscreen, sticky, tiling, stacking, workspace-set and output-layout events include current view or output metadata where available.
- Geometry events include `old-geometry`; tiling events include `old-edges` and `new-edges`; workspace events include `from` and `to`.
- Top/bottom stacking events distinguish state changes from ordinary z-ordering.
- Keyboard modifier events carry the current layout state.
- Scale, Expo and inversion emit Wayfire-compatible plugin activation events when their nested frontend state actually changes.

### Map blocking

Explicitly subscribing to `view-pre-map` delays each new view's first buffer map and sends its metadata. Every subscriber must release the view with `window-rules/unblock-map`; the last release emits `view-mapped`, shows the view and applies normal focus. A subscriber disconnecting releases its blocks.

While blocked, a view answers geometry and state methods but is not rendered, focused, shown in Scale or reported as mapped. A catch-all watch does not subscribe to `view-pre-map`, matching Wayfire.

## STIPC test input

- `feed_key` and `feed_button` accept common evdev key and button names, including `S-` prefixes and `press`, `release` or `full` click modes.
- `move_cursor` moves the pointer. `touch` sends down for a new finger and motion for an active one; `touch_release` lifts it.
- `run` launches a shell command with the compositor's `WAYLAND_DISPLAY`, returns its PID, and reaps exited children while polling.
- `get_display` reports the Wayland socket and an empty XWayland display, since XWayland is not implemented.
- `layout_views` validates and applies a batch of exact view rectangles, with optional output-name checks. Invalid input causes no partial update.
