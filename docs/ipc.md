# Wayfire IPC

Shingle serves Wayfire-compatible IPC with every backend: JSON messages with a native-endian `u32` length prefix over a Unix socket. Replies carry Wayfire's `result` or `error` fields.

```sh
cargo run --bin shingle -- --ipc /tmp/shingle-ipc.socket
```

The socket path is exported as `WAYFIRE_SOCKET` to every client Shingle starts, from restored and autostarted clients and `--spawn` programs to everything launched later, so existing Wayfire scripts work unchanged. Without `--ipc`, `WAYFIRE_SOCKET` is removed from their environment, so a nested Shingle never hands its children the host compositor's socket. Shingle's own environment is not changed; the children's comes from `Compositor::client_environment` (`shingle_session::ClientEnvironment`). Library users can start the same server with `Compositor::enable_ipc(path)`, before starting any client.

`list-methods` returns a sorted inventory of the 86 supported methods. Numeric vswitch activators such as `binding_4` are matched dynamically and are not part of that count. The method table lives in `SUPPORTED_METHODS` in `crates/shingle/src/ipc.rs`.

## Methods

| Namespace | Methods |
| --- | --- |
| `window-rules` | `list-views`, `list-outputs`, `list-wsets`, `view-info`, `output-info`, `wset-info`, `get-focused-view`, `get-focused-output`, `get-cursor-position` (also `get_cursor_position`), `focus-view`, `close-view`, `configure-view`, `get-view-property`, `set-view-property`, `events/watch`, `unblock-map` |
| `wm-actions` | `minimize`, `set-minimized`, `set-fullscreen`, `set-sticky`, `set-always-on-top`, `set-always-on-bottom`, `bring-to-front`, `send-to-back`, `toggle_maximize`, `toggle_fullscreen`, `toggle_sticky`, `toggle_always_on_top`, `toggle_always_on_bottom`, `toggle_showdesktop` |
| `vswitch` | `set-workspace`, `send-view`, `binding_{left,right,up,down,last}`, `with_win_{…}`, `send_win_{…}`, plus numeric `binding_N` forms |
| `simple-tile` | `get-layout`, `set-layout`, `set-show-maximized` |
| `wsets` | `set-output-wset`, `send-view-to-wset` |
| `wayfire` | `create-headless-output`, `destroy-headless-output`, `get-config-option`, `list-config-options`, `set-config-options`, `get-keyboard-state`, `set-keyboard-state` |
| `wf/alpha` | `get-view-alpha`, `set-view-alpha` |
| `command` | `register-binding`, `unregister-binding`, `clear-bindings` |
| `input` | `list-devices`, `configure-device` |
| `scale`, `expo`, `invert` | `scale/toggle`, `scale/toggle_all`, `expo/toggle`, `invert/toggle` |
| `wayfire-shell` | `toggle_menu`: sends `zwf_output_v2.toggle_menu` to the shell clients of the output `output_id` (or `output-id`) names, else of the active one, without focusing it; an unknown output or view id is an error, as in Wayfire |
| `stipc` | `feed_key`, `feed_button`, `move_cursor`, `touch`, `touch_release`, `ping`, `run`, `get_display`, `get_xwayland_display`, `get_xwayland_pid`, `layout_views`, `create_wayland_output`, `destroy_wayland_output` |
| (none) | `list-methods` |

## Views and outputs

- View IDs accept Wayfire's `id` and `view-id` spellings and the legacy `view_id`.
- Outputs have Wayfire's ids: counted from 1 and never reused, so an output that goes and comes back gets a new id. `list-outputs`, `output-info` (`id`) and `get-focused-output` describe every output with its id, name, layout geometry, workarea, `wset-index` and current workspace; `list-wsets` and `wset-info` (`id` is the index) list every workspace set, a parked one with `output-id` -1 and `output-name` "". Views report their set's output id and name (-1 and "null" while the set is parked) and `wset-index`. `get-focused-output` is the active output, which a click or touch on another output, focusing a view (`focus-view` too), oswitch and an activator naming an output change, as in Wayfire; each change sends `output-gain-focus`. Geometries are in layout coordinates, as Shingle has always reported them, where Wayfire's are relative to the view's output; for the first output the two are the same.
- `wayfire/create-headless-output` with `width` and `height` adds a virtual output named after the next unused `HEADLESS-N` and replies `ok` with its `output`; `wayfire/destroy-headless-output` with `output` (a name) or `output-id` removes one, moving its windows to the active output, and refuses outputs it did not create with Wayfire's errors (``Missing `output` or `output-id`!``, `Output not found!`, `Output is not a headless output created from an IPC command!`). They work on every backend. Clients on them get frame callbacks at the output's refresh rate from headless and at 60 Hz from the frontend of nested, offscreen and native (MO-5).
- `stipc/create_wayland_output` opens another window of nested Shingle, as it adds an output to a nested Wayfire: the output is named after the lowest `HEADLESS-N` no output has, so a window closed and opened again shows the same output and gets its windows back under `output_layout.unplug: return`, and the reply is `ok` with its `output` (Wayfire replies `ok` alone). `stipc/destroy_wayland_output` with `output` (a name) closes such a window, whose windows go where `output_layout.unplug` says. Both refuse with Wayfire's errors (`Wayfire is not running in nested wayland mode!` in every other backend, `Could not find output: "NAME"`, `Output is not a wayland output!` for an output created over IPC), and the last window cannot be destroyed (`Cannot destroy the last wayland output!`); closing it in the host quits nested Shingle.
- `configure-view` sets floating geometry, fullscreen and sticky state. Supplying geometry detaches a tiled view, updates its XDG role and emits the normal tiling event. The exact rectangle is preserved in saved sessions.
- `configure-view` with `output_id` (or `output-id`) first moves the view, with its dialogs, to that output, as Wayfire's `move_view_to_output`: a tiled view is appended to the target workspace's tree, a floating one keeps its place relative to the output scaled by the ratio of the outputs' sizes and clamped into the target workarea, and snapped and fullscreen views are applied again there. It lands on the workspace with the same offset from the target's current workspace as it had from its own. With a `geometry` the view is moved without rescaling and then given that rectangle. An unknown id is `output not found`.
- `tiled-edges` accepts masks 0–15 for floating, half, quarter, combined and full-workarea tiling. The requested mask is also preserved in sessions.
- `wf/alpha/*` query and set per-window opacity, clamped to the inclusive 0–1 range.
- `get-view-property` and `set-view-property` store typed runtime metadata: booleans, signed and unsigned integers, strings and floats. Changing an existing property's type is rejected, and properties are removed with their view.
- Invalid outputs, coordinates, alpha values and unmapped views return errors.
- X11 views are ordinary `toplevel` views: `app-id` is the class part of `WM_CLASS`, `title` is `_NET_WM_NAME` (else `WM_NAME`), `min-size` and `max-size` come from `WM_NORMAL_HINTS`, and `pid` is the X client's PID (from XRes, else `_NET_WM_PID`), as Wayfire reports it. Wayland views still report `pid` -1. `close-view` closes an X11 view with `WM_DELETE_WINDOW`, or disconnects its client (`KillClient`) when the window does not list that protocol. Override-redirect windows (menus, tooltips) are not listed, unlike Wayfire's `"type": "x-or"` views.

## Window actions

- The `wm-actions` activators accept an explicit view ID, or act on the focused view when it is omitted.
- `toggle_showdesktop` preserves the set of windows that were already minimized and restores focus when leaving the desktop.
- `scale/*`, `expo/toggle` and `invert/toggle` dispatch frontend events to the nested renderer and also work as binding callbacks.
- Activators take Wayfire's `output_id` or `output-id`: `scale/toggle`, `scale/toggle_all`, `expo/toggle`, `wm-actions/toggle_showdesktop` and the vswitch `binding_*`, `with_win_*` and `send_win_*` activators first make that output active, since Scale, Expo, show-desktop and the bindings act on the active output, and an unknown id is `output id not found!`. Without one they act on the active output.

## Workspaces

- `vswitch/set-workspace` switches the workspace of the output `output-id` names by grid coordinate, without making it active, optionally taking a view of that output along (`view belongs to a different output` otherwise). `vswitch/send-view` moves a view to another workspace of its own set, on whichever output shows it, without switching.
- `binding_*` switches, `with_win_*` carries the focused window and `send_win_*` sends it. Each supports left/right/up/down, last-workspace and one-based numeric targets. Directional actions honour configured wraparound. Invalid or out-of-grid numbers are rejected without changing state.

## Tiling layout

- `simple-tile/get-layout` exports split directions, child order, percentages, node geometry and view IDs from the persistent tiling tree of the set `wset-index` names, shown or parked. Floating views are omitted, and an unused workspace returns an empty layout.
- `simple-tile/set-layout` validates and imports split trees with positive `weight` or `percent` values into the set `wset-index` names. Included views move to the selected workspace, views of other sets first join this one as Wayfire's set-layout moves them, omitted views stay floating, mapped state is preserved, and invalid input is rejected without changing the session.

## Workspace sets

Wayfire's `wsets` plugin, by IPC and by the `wsets` bindings (`wset_N`, `send_to_wset_N`):

- `wsets/set-output-wset` (`output-id`, `wset-index`) shows that set on that output, creating it if no set has the index. An output that showed the set gets a new, empty set with the lowest free index, and the set the output showed until then is parked, or destroyed if nothing is left in it and no window is away from it. Two calls therefore swap two outputs' sets. `output-wset-changed` is sent for each output whose set changed, and `view-set-output` for each window whose output changed with its set.
- `wsets/send-view-to-wset` (`view-id`, `wset-index`) sends the view with its dialogs to that set, creating it if needed; the view keeps its place relative to the output, and is out of sight while the set is parked. Errors are Wayfire's `output not found` and `view not found`.
- `simple-tile/set-show-maximized` controls tiled-view maximization and rejects floating views.

## Configuration and keyboard

- `wayfire/get-config-option` answers every option of Wayfire's metadata at the commit docs/design/wayfire-gaps.md pins (crates/shingle-config/data/wayfire-options.tsv, 332 options), as Wayfire's IPC writes them: `value` and `default` as strings for a plain option, and only `value`, a list of `[name, values...]` entries, for a dynamic list such as `command/bindings`. The 18 options `set-config-options` changes (core grid, background and decoration mode, output, cursor, scroll, simple-tile gaps and fullscreen, two workarounds) report their live value; every other option reports what the Wayfire file set, or Wayfire's default when the file did not set it, the value was rejected, or the configuration is not a Wayfire file. `default` is always Wayfire's metadata default. `list-config-options` groups every option by section, with a section of its own for each `[output:NAME]` and `[input-device:NAME]` of the file. Unknown options return errors.
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
- `output-gain-focus` carries the `output` that became active. `view-set-output` carries the view's new `output` (null while its set is parked) and the `view`, and `view-wset-changed` the `old-wset`, the `new-wset` and the `view`, whenever a window moves between outputs or sets: by `configure-view`, oswitch, the wsets methods and bindings, `start_on_output`, `set_fullscreen` with an output, a drag onto another output, or an output going and coming back.
- `output-added` and `output-removed` carry the `output` as `list-outputs` describes it, and `output-wset-changed` `new-wset`, `output`, `new-wset-data` and `output-data` when an output takes a parked set. `output-layout-changed` carries Wayfire's `configuration`, every output with `name`, `output-id`, `source` (`self`, or `none` for a dark output), `depth`, `scale`, `vrr`, `transform`, `mirror-from`, `position` and `mode`, once per batch of changes; `wset-workspace-changed` carries `previous-workspace`, `new-workspace`, `output`, `wset`, `output-data` and `wset-data`. Before MO-2 the two carried `[output]` and `wset` only.
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
- In nested, offscreen and native Shingle, `feed_key`, `feed_button` and `move_cursor` go through the frontend as input of the STIPC devices, as Wayfire's STIPC devices go through its plugins: key and pointer bindings fire, and `S-BTN_LEFT` with `move_cursor` drags a window, also onto another output. Headless Shingle, which has no frontend, handles them in the compositor.
- `run` launches a shell command in the client environment (the compositor's `WAYLAND_DISPLAY`, `WAYFIRE_SOCKET` and its own `DISPLAY`, never the host's, and Wayfire's `XDG_CURRENT_DESKTOP`, `XCURSOR_THEME`, `XCURSOR_SIZE` and `WAYFIRE_CONFIG_FILE`, [features.md](features.md#client-environment)), returns its PID, and reaps exited children while polling.
- `get_display` reports the Wayland socket and Shingle's X display (`"xwayland": ":N"`), which is bound at startup, so it is known before a lazy Xwayland starts; it is `""` with `xwayland.mode: false`. `get_xwayland_display` returns `{"result": "ok", "display": ":N"}` and `get_xwayland_pid` `{"result": "ok", "pid": P}`, with -1 while no Xwayland runs, as in Wayfire.
- `layout_views` validates and applies a batch of exact view rectangles, with optional output-name checks. Invalid input causes no partial update.
