# Wayfire IPC

Shingle serves Wayfire-compatible IPC with every backend: JSON messages with a native-endian `u32` length prefix over a Unix socket. Replies carry Wayfire's `result` or `error` fields.

```sh
cargo run --bin shingle -- --ipc /tmp/shingle-ipc.socket
```

The socket path is exported as `WAYFIRE_SOCKET` to every client Shingle starts, from restored and autostarted clients and `--spawn` programs to everything launched later, so existing Wayfire scripts work unchanged. Without `--ipc`, `WAYFIRE_SOCKET` is removed from their environment, so a nested Shingle never hands its children the host compositor's socket. Shingle's own environment is not changed; the children's comes from `Compositor::client_environment` (`shingle_session::ClientEnvironment`). Library users can start the same server with `Compositor::enable_ipc(path)`, before starting any client.

`list-methods` returns a sorted inventory of the 79 supported methods. Numeric vswitch activators such as `binding_4` are matched dynamically and are not part of that count. The method table lives in `SUPPORTED_METHODS` in `crates/shingle/src/ipc.rs`.

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
| `input` | `list-devices`, `configure-device` |
| `scale`, `expo`, `invert` | `scale/toggle`, `scale/toggle_all`, `expo/toggle`, `invert/toggle` |
| `stipc` | `feed_key`, `feed_button`, `move_cursor`, `touch`, `touch_release`, `ping`, `run`, `get_display`, `get_xwayland_display`, `get_xwayland_pid`, `layout_views` |
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
- Clients receive the compiled keymap (`xkb_keymap_get_as_string`), as from wlroots compositors, so they need no XKB data of their own. This covers `wl_keyboard`, input-method keyboard grabs, keymaps uploaded by virtual keyboards and config reloads.
- `wayfire/get-keyboard-state` reports the layout names in the active XKB keymap. `set-keyboard-state` selects any valid layout index and sends the normal modifier notification. Comma-separated layouts and per-layout variants become distinct XKB groups.

## Runtime bindings

- `command/register-binding` supports normal, repeat and release modes, dispatched through nested keyboard and pointer input.
- STIPC `feed_key` keys take the path of a real key, as keys from Wayfire's headless STIPC keyboard do (`plugins/ipc/stipc.cpp`): in nested they go through the seat keyboard STIPC uses into the frontend's xkb state, so configured (YAML) key bindings, `xkb:` command bindings, runtime bindings, the overviews and the switchers all see them, modifiers come from the keys held, a matched key is not passed to clients and a held key repeats like a host key. The key is handled in the same turn as the request, just after the reply is sent. Without a frontend (the headless `shingle` binary) `feed_key` matches runtime bindings only, with the keyboard's depressed modifiers plus modifier keys held through `feed_key`.
- A binding with a command runs it through the isolated launcher. Without one, the owning client receives a targeted `command-binding` event.
- `call-method` and `call-data` invoke an IPC method directly when the binding triggers, so a binding can perform compositor actions without launching a process.
- Notification bindings are removed when their client disconnects.

## Input devices

The seat devices are a registry the backend fills (`Compositor::add_input_device` and `remove_input_device`, crates/shingle/src/input_device.rs). The nested frontend registers three: the keyboard, pointer and touch input of its host window, as IDs 1, 2 and 3. A native backend registers each libinput device capability as it appears. STIPC input goes through the registered device of each kind with the lowest ID, which in nested is one of those three. Client-created virtual keyboards and pointers are separate devices, as in Wayfire, and are neither listed nor gated.

- `input/list-devices` returns one object per registered device, in registration order, with Wayfire's fields: `id`, `name`, `vendor` and `product`, `type` (`keyboard`, `pointer` or `touch`) and `enabled`. IDs count up from 1 and are never reused while Shingle runs. `vendor` and `product` are numbers when the backend knows them, as Wayfire reports libinput's IDs, and `"unknown"` otherwise, which is always the case for nested's devices. A removed device disappears from the list.
- `input/configure-device` takes an integer `id` and a boolean `enabled`. Errors match Wayfire's text and order: `Missing "id"`, `Field "id" does not have the correct type, expected int64`, `Missing "enabled"`, `Field "enabled" does not have the correct type, expected bool`, then `Unknown input device!`, which also covers a removed device. A rejected request changes nothing; repeating the current state is a no-op.
- A disabled keyboard delivers no keys or modifier changes and fires no bindings. A disabled pointer delivers no motion, buttons, scroll or pointer gestures. A disabled touch device delivers no touch points. This covers host input, which the nested frontend also stops matching against its own bindings and actions, and STIPC input, whose methods still reply `ok`.

### Releasing held input

Each device tracks the keys, buttons and touch points it holds. Disabling or removing a device first returns it to a neutral state, the way libinput does when Wayfire suspends a device, so no client is left with stuck input, and leaves every other device alone:

- Held keys and pointer buttons get ordinary release events. The protocol has no cancel for them, and a release is what the client would see if the user let go; a held button therefore also completes a drag-and-drop in progress.
- A key, button or touch point that another device also holds stays down for clients until that device lets go too; the same rule applies to ordinary releases.
- Depressed and latched modifiers are cleared and sent once no keyboard holds any key; locked modifiers are kept. While another keyboard still holds keys, the modifiers are left to the frontend, which owns the xkb state.
- Active touch points are cancelled with `wl_touch.cancel` rather than lifted, so a touch interrupted by disabling never registers as a tap.
- An active swipe, pinch or hold gesture ends with `cancelled` set if this device began it. Only the device that began a gesture updates or ends it.
- The nested frontend finishes an open window switcher as if its modifiers were released, and abandons a move or resize drag where the window is, without the drop, swap or snap a release would perform. When the keyboard is re-enabled, the host's current modifiers are sent again.

## Events

`window-rules/events/watch` subscribes a connection to all events or to a requested list. Duplicate watches and unknown event names return errors.

- Lifecycle, focus, title/app ID, geometry, workspace, minimized, fullscreen, sticky, tiling, stacking, workspace-set and output-layout events include current view or output metadata where available.
- Geometry events include `old-geometry`; tiling events include `old-edges` and `new-edges`; workspace events include `from` and `to`.
- Top/bottom stacking events distinguish state changes from ordinary z-ordering.
- Keyboard modifier events carry the current layout state.
- Scale, Expo and inversion emit Wayfire-compatible plugin activation events when their nested frontend state actually changes.
- `xwayland-ready` is Shingle's own (Wayfire has no such event): `{"event": "xwayland-ready", "display": ":N", "pid": P}`, sent each time Xwayland has started and Shingle's X window manager is connected, after `xwayland.startup_script` was launched. A catch-all watch includes it.

### Map blocking

Explicitly subscribing to `view-pre-map` delays each new view's first buffer map and sends its metadata. Every subscriber must release the view with `window-rules/unblock-map`; the last release emits `view-mapped`, shows the view and applies normal focus. A subscriber disconnecting releases its blocks.

While blocked, a view answers geometry and state methods but is not rendered, focused, shown in Scale or reported as mapped. A catch-all watch does not subscribe to `view-pre-map`, matching Wayfire.

A blocked new view takes no tile until it is released, as Wayfire's tile plugin only attaches a view on `view-mapped`: `simple-tile/get-layout` leaves it out, its neighbours keep their size, and `set-layout` leaves it alone. On release it joins the end of its workspace's root split, after any views that mapped meanwhile. Until then it is configured to the output size, or floats if `tile_by_default` does not match it. A client claiming a restored session placeholder is different: the placeholder keeps its saved slot while blocked.

## STIPC test input

- `feed_key` and `feed_button` accept common evdev key and button names, including `S-` prefixes and `press`, `release` or `full` click modes.
- STIPC input goes through the first registered seat device of each kind, so it is dropped while that device is disabled, and also when no device of that kind is registered (see [Input devices](#input-devices)). The methods still reply `ok`.
- `move_cursor` moves the pointer. `touch` sends down for a new finger and motion for an active one; `touch_release` lifts it.
- `run` launches a shell command in the client environment (the compositor's `WAYLAND_DISPLAY`, `WAYFIRE_SOCKET` and its own `DISPLAY`, never the host's), returns its PID, and reaps exited children while polling.
- `get_display` reports the Wayland socket and Shingle's X display (`"xwayland": ":N"`), which is bound at startup, so it is known before a lazy Xwayland starts; it is `""` with `xwayland.mode: false`. `get_xwayland_display` returns `{"result": "ok", "display": ":N"}` and `get_xwayland_pid` `{"result": "ok", "pid": P}`, with -1 while no Xwayland runs, as in Wayfire.
- `layout_views` validates and applies a batch of exact view rectangles, with optional output-name checks. Invalid input causes no partial update.
