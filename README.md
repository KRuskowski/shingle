# Shingle

Shingle is a Rust tiling Wayland compositor inspired by Wayfire. Its primary
model is a persistent tiling tree rather than a collection of independent
window positions. Rendering uses wgpu, and the nested frontend uses winit so
it can be tested without replacing the running compositor.

## Current status

The nested compositor supports XDG toplevels, layer shell, tiling and floating
windows, workspace navigation, snapping, focus and stacking rules, session
restore, idle/DPMS, session locking, screencopy, zoom, color inversion, window
opacity, opening/closing animations, Scale window overviews, and an Expo
workspace overview.

Shingle is not yet a drop-in replacement for Wayfire on physical hardware.
Direct DRM/libinput, DMA-BUF, XWayland, and complete multi-output support remain
unfinished. Several Wayfire visual effects and plugins are also missing. Scale
has entry, layout, and exit transitions plus configurable title overlays. Expo
has workspace-wall entry/exit zooms and selection brightness transitions.

## Build and test

The workspace requires Rust 1.92 or newer and the system libraries needed by
winit, Wayland, and PAM.

```sh
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

Rust formatting is fixed at 80 columns by `rustfmt.toml`:

```sh
cargo fmt --all
```

## Run without replacing Wayfire

Start a nested Shingle window inside the current graphical session:

```sh
cargo run -p shingle-nested -- --config data/shingle.yaml
```

The process prints its private `WAYLAND_DISPLAY` socket. Applications started
with `--spawn` receive that socket automatically:

```sh
cargo run -p shingle-nested -- \
  --config data/shingle.yaml --spawn foot
```

The host compositor remains running. Locking uses `shingle-lock` from `PATH`;
the `SHINGLE_LOCK` environment variable can select another executable.

## Configuration

Shingle reads YAML by default from
`$XDG_CONFIG_HOME/shingle/shingle.yaml`, or
`~/.config/shingle/shingle.yaml` when `XDG_CONFIG_HOME` is unset. The example
in `data/shingle.yaml` includes the available settings and default shortcuts.

Wayfire INI and i3-style configuration can be imported. Files ending in `.ini`,
and files beginning with an INI section, are treated as Wayfire configuration;
`.conf` and `.i3` files use the i3 parser. YAML is the native format and uses
`deny_unknown_fields` so misspelled options are rejected.

WCM provides a graphical configuration editor:

```sh
cargo run -p shingle-wcm -- --config data/shingle.yaml
```

It also validates and converts configuration without opening a window:

```sh
cargo run -p shingle-wcm -- --config data/shingle.yaml --check
cargo run -p shingle-wcm -- \
  --config wayfire.ini --output shingle.yaml --export-yaml
```

### Resource limits

`behavior.max_buffer_size` limits either dimension of a shared-memory client
buffer and defaults to 16384 pixels, matching Wayfire. Oversized buffers are
rejected before mapping or GPU upload. The limit is also editable in WCM.

### Surface opacity

`wp_alpha_modifier_v1` applies client-requested alpha multipliers on surface
commit, including cached synchronized-subsurface commits. The multiplier ranges
from zero (transparent) to `UINT32_MAX` (opaque) and is combined with Shingle's
window-opacity setting. Destroying the modifier schedules a return to full alpha.

### Presentation timing

`wp_tearing_control_v1` applies asynchronous or vertical-sync hints on surface
commit. When any visible surface requests asynchronous presentation, the nested
wgpu frontend selects Immediate mode if the host supports it. Otherwise it keeps
FIFO presentation. Destroying the control schedules a return to vertical sync.

`wp_fifo_v1` supports refresh barriers and wait-constrained commits. Eligible
updates are coalesced in order and applied by `Compositor::frame()` before frame
callbacks. Synchronized subsurfaces, invisible surfaces, and tearing-enabled
surfaces bypass the barrier as permitted by the protocol. Destroying a FIFO object
does not cancel state already submitted.

`wp_commit_timing_v1` accepts monotonic target timestamps and applies updates as
closely as possible without presenting early. Pending updates preserve content
order, share FIFO frame processing, and drive nested redraw scheduling. Invalid
or duplicate timestamps and destroyed surfaces use the protocol's error codes.

### Pointer control

`wp_pointer_warp_v1` accepts focused-surface requests with a valid pointer-enter
serial and an in-bounds local position. The nested frontend moves the host cursor
with inverse output-transform and zoom mapping, while suppressing the resulting
synthetic motion event. Pointer-lock and confinement behavior remains separate.

### Decorations

`behavior.preferred_decoration_mode` accepts `client` (the Wayfire default) or
`server`. Explicit client requests are honored unless
`behavior.force_preferred_decoration_mode` is enabled. Preference changes are
sent to existing XDG decoration objects during live configuration reloads.

For clients with popup-focus bugs,
`behavior.focus_main_surface_instead_of_popup` moves keyboard focus to the
popup's main surface while leaving pointer input on the popup. It is disabled
by default and can be changed through WCM or live configuration reloads.

### Dialogs

`xdg-dialog-v1` supports modal and nonmodal hints on parented XDG toplevels.
Focusing an ancestor redirects to its topmost explicitly modal descendant even
when `behavior.all_dialogs_modal` is disabled. Unsetting the hint or destroying
the dialog object restores normal focus. The existing all-dialogs option remains
available as a compatibility policy for every parented dialog.

`xdg-foreign-unstable-v2` exports random toplevel handles and imports them in
other clients. Imported parents use the same stacking and session relationships
as ordinary XDG parents. Invalid handles emit `destroyed`; destroying an import,
export, or exported toplevel revokes only the relationships that depend on it.

### Primary selection

`behavior.disable_primary_selection` matches Wayfire's workaround for disabling
middle-click copy and paste. When enabled at startup, Shingle does not
advertise `zwp_primary_selection_device_manager_v1`. Changing the option requires
a compositor restart so existing clients and new clients see the same protocol
availability. The option can be imported and edited through WCM.

### Sandboxed clients

`wp_security_context_manager_v1` accepts listener sockets from sandbox engines.
Committed engine, app, and instance metadata is attached to every accepted
client. Sandboxed clients cannot nest security contexts. Every advertised
protocol global uses the same configurable visibility policy.

The `security_context.privileged_protocols` comma-separated list is imported
from Wayfire and editable in WCM. Its defaults cover foreign-toplevel management,
screencopy, data control, and keyboard-shortcut inhibition. Restrictions apply as
each client is inserted; already-connected clients retain the globals they
received.

### Text input

`text-input-v3` follows keyboard focus with enter/leave events and implements
committed enable/disable state, surrounding text, content type, cursor
rectangles, and commit serials. `input.enable_text_input_v3` defaults to true and
is imported from Wayfire or editable in WCM.

`zwp_input_method_v1` activates contexts for enabled text inputs and relays
surrounding text, content type, commit strings, preedit text/cursors, and
surrounding-text deletion. Hardware keys and modifiers go to the active keyboard
grab. Input methods can forward them to the application, and forwarded held keys
are released when the context is deactivated or destroyed.

`zwp_input_method_manager_v2` supports one input method per seat, activation
and deactivation, surrounding text and change causes, content types, and atomic
edit batches with done-count serial validation. Its keyboard grabs receive the
current keymap, repeat settings, hardware keys, and modifiers. Releasing a grab
restores normal application input. Version 2 popup surfaces receive text-input
rectangle hints and become inert when their input method is destroyed.

`zwp_input_panel_v1` supports center-bottom panels and cursor-relative overlays.
Version 2 popups use the same cursor-relative placement. Overlay placement
follows committed cursor rectangles after the focused surface's next commit,
flips above when necessary, and clamps to the output. Panels are visible only
while their text input and input method are active and do not take keyboard
focus away from the field.

### Scale overview

The example configuration opens the current-workspace overview with Super+P
and the all-workspaces overview with Super+Shift+P. Arrow keys, Tab,
Shift+Tab, scrolling, and pointer hover change the selection. Enter or a left
click commits it; Escape cancels and restores the initially focused window and
workspace. Workspace shortcuts navigate without moving windows between them.

Scale never writes thumbnail rectangles back into the tiling tree. Selecting a
remote window switches to its workspace. Minimized windows can be included and
are restored only when selected. Optional middle-click close and dismissal on
new-window mapping match the corresponding Wayfire options.

Typing filters thumbnails by a substring of the title or app ID. Backspace
removes one UTF-8 character, and clearing the filter restores all eligible
windows. Matching is case-insensitive by default. The optional centered overlay
shows the active query and resets when Scale exits.

`xdg_toplevel_icon_v1` accepts named icons and square SHM pixel buffers with
commit-based assignment and immutable icon objects. Scale caches supplied pixel
icons as 32-pixel GPU textures and requests 64-pixel sources from clients. Stock
names use the configured GTK or `ICON_THEME` theme, XDG data roots, inherited
themes, and hicolor fallback. PNG files use `image`; SVG files are rasterized
with `resvg` at the requested size while preserving their aspect ratio.

```yaml
scale:
  duration_ms: 750
  spacing: 50
  outer_margin: 0
  inactive_alpha: 0.75
  minimized_alpha: 0.45
  allow_zoom: false
  middle_click_close: false
  include_minimized: false
  close_on_new_view: false
  title_overlay: all
  title_font_size: 16
  title_position: center
  title_bg_color: [0.1, 0.1, 0.1, 0.9]
  title_text_color: [0.8, 0.8, 0.8, 1.0]

scale_title_filter:
  case_sensitive: false
  share_filter: false
  overlay: true
  font_size: 30
  bg_color: [0.0, 0.0, 0.0, 0.5]
  text_color: [0.8, 0.8, 0.8, 0.8]
```

### Expo workspace overview

Super+E opens every configured workspace in its exact grid position. Arrow,
H/J/K/L, or keypad keys and pointer hover select a cell, including an empty
workspace. Direct workspace shortcuts commit immediately, even when ordinary
keyboard interaction is disabled.
Enter, left-click, touch, or Super+E commits the selection. Pointer and touch
presses begin a possible drag; release commits it. Dragging a thumbnail to
another workspace moves that window there on release. Escape cancels without a
drop. Other keys, buttons, and scrolling are captured while Expo is active, and
keyboard navigation is suspended while a pointer drag is held. Selection alone
never changes the active workspace or moves tiled windows.

Expo redraws current client buffers whenever their contents or layout change.
Entry and exit interpolate the workspace wall and its thumbnails. Changing the
selection crossfades brightness without changing compositor opacity. Output
resizes and window destruction invalidate thumbnail geometry, and configuration
reloads dismiss Expo. A zero duration disables the corresponding transition.

```yaml
expo:
  background: [0.1, 0.1, 0.1, 1.0]
  duration_ms: 300
  offset: 10
  keyboard_interaction: true
  inactive_brightness: 0.7
  transition_length_ms: 200
  workspace_bindings:
    - workspace: 2
      binding: super+2
```

Wayfire's `select_workspace_2 = <super> KEY_2` and i3's
`bindsym Mod4+2 expo select 2` import the same direct binding. WCM edits these
as comma-separated `workspace=binding` pairs alongside the other Expo options.

### Vswitch workspace transitions

Normal workspace switches slide the old and new workspaces along the configured
grid. The transition preserves window sizes and positions, keeps layer-shell
panels stationary, and never writes temporary coordinates into the session.
Opening Scale or Expo cancels the slide so overview transforms remain separate.
Output changes and configuration reloads also discard stale transitions.

```yaml
vswitch:
  duration_ms: 300
  gap: 20
  background: [0.1, 0.1, 0.1, 1.0]
```

Wayfire's `[vswitch]` `duration`, `gap`, and `background` options are imported.
The corresponding i3 variables are `$vswitch_duration_ms`, `$vswitch_gap`, and
`$vswitch_background`. A zero duration switches immediately.

### Workspace tools

Shingle advertises `ext-workspace-v1` for workspace bars and switchers. Clients
receive stable IDs, display names, grid coordinates, output membership, and
active state. Activation requests use the protocol's atomic commit operation and
share the normal workspace-switch path. Grid changes rebuild the client's
workspace list; workspace creation and removal capabilities are not advertised.

`ext-foreign-toplevel-list-v1` provides read-only window handles with stable
per-map identifiers, titles, app IDs, metadata updates, and close events. Each
client receives its own handles for the same windows. Window control remains
available through the existing WLR foreign-toplevel protocol.

### IPC for scripts

The nested frontend can expose Wayfire-style length-prefixed JSON over a local
Unix socket:

```sh
cargo run -p shingle-nested -- --ipc /tmp/shingle-ipc.socket
```

It exports `WAYFIRE_SOCKET` for spawned processes. Supported `window-rules`
methods list views, outputs, and workspace sets; query focused objects and cursor
position; focus or close views; and configure floating geometry, fullscreen, and
sticky state. Supplying geometry detaches a tiled view, updates its XDG role,
and emits the normal tiling event; the exact rectangle is preserved in saved
sessions. `tiled-edges` accepts masks 0–15 for floating, half, quarter, combined,
and full-workarea tiling; the requested mask is also preserved in sessions.
`output_id` and `output-id` select the current output or reject unknown outputs.
View IDs accept Wayfire's `id`, `view-id`, and legacy `view_id`
spellings. `vswitch/set-workspace` switches by grid coordinate, optionally taking
a view along, and `vswitch/send-view` moves a view without switching. Wayfire's
`binding_*`, `with_win_*`, and `send_win_*` activators support left/right/up/down,
last-workspace, and one-based numeric workspace actions such as `binding_4`.
Invalid or out-of-grid numbers are rejected without changing state. Directional
actions honor configured wraparound; carry and send actions move the focused
window. Window actions cover minimize, fullscreen, sticky,
always-on-top/bottom, and raising or
lowering. `wf/alpha/get-view-alpha` and `wf/alpha/set-view-alpha` query and
adjust per-window opacity, clamped to the inclusive 0–1 range. Invalid outputs,
coordinates, alpha values, and unmapped views return errors.
Messages use a native `u32` length prefix and Wayfire's `result` or `error`
fields. `window-rules/events/watch` subscribes a connection to all events
or a requested list. Lifecycle, focus, title/app ID, geometry, workspace,
minimized, fullscreen, sticky, tiling, stacking, workspace-set, and output-layout
events include current view or output metadata where available. Geometry events
include `old-geometry`; tiling events include `old-edges` and `new-edges`;
workspace events include `from` and `to` points. Keyboard modifier events carry
the current layout state. Scale, Expo, and inversion report Wayfire-compatible
plugin activation state when their nested frontend state actually changes.
Top/bottom stacking events distinguish state changes from ordinary z-ordering.
Duplicate watches and unknown event names return errors. Explicitly subscribing
to `view-pre-map` delays each new view's first buffer map and sends its current
metadata. Each subscriber releases the delay with `window-rules/unblock-map`;
the last release emits `view-mapped`, shows the view, and applies normal focus.
Deferred views remain available to geometry and state methods but are excluded
from rendering, focus, and Scale. Disconnecting a subscriber releases its block.
A catch-all watch does not subscribe to `view-pre-map`, matching Wayfire.
`wayfire/get-keyboard-state` reports the names in the active XKB keymap, and
`wayfire/set-keyboard-state` selects any valid layout index and sends the normal
modifier notification. Comma-separated XKB layouts and per-layout variants are
assigned to distinct groups. Custom keymaps are compiled for validation before
being installed; reloads reset an invalid layout selection and update clients.
`wayfire/get-config-option` returns a live `value` and its `default` as strings.
`wayfire/list-config-options` groups 18 supported core, output, input,
simple-tile, and workaround options by section. Unknown options return errors.
`wayfire/set-config-options` applies validated live updates with atomic rollback.
Workspace-grid dimensions are also writable. Shrinking relocates out-of-range
windows to the nearest valid workspace, clamps the current workspace, and
preserves their geometry and tiling state.
`simple-tile/set-show-maximized` controls tiled-view maximization and rejects
floating views. `simple-tile/get-layout` exports split directions, child order,
percentages, node geometry, and view IDs from the persistent tiling tree. It
omits floating views and returns an empty layout for an unused workspace.
`simple-tile/set-layout` validates and imports split trees with positive
`weight` or `percent` values. It moves included views to the selected workspace,
keeps omitted views floating, preserves mapped state, and rejects invalid input
without changing the session. `list-methods` returns a sorted inventory of the
75 currently supported methods so scripts can discover functionality explicitly.
Runtime command bindings use `command/register-binding`, `unregister-binding`,
and `clear-bindings`. Normal, repeat, and release modes are dispatched through
nested keyboard and pointer input. A command runs through the isolated launcher;
without one, the owning client receives a targeted `command-binding` event.
Notification bindings are removed on disconnect. `call-method` and `call-data`
invoke the selected IPC method directly when the binding is triggered, allowing
real compositor actions without launching a process.
`wm-actions/toggle_showdesktop` is available directly and through callbacks;
it preserves the minimized-window set and restores focus when leaving the
desktop. Selected-view activators also cover minimize, maximize, fullscreen,
sticky, always-on-top/bottom, and raise/lower. They accept an explicit view ID
or use the focused view when the ID is omitted. `scale/toggle`,
`scale/toggle_all`, `expo/toggle`, and `invert/toggle` dispatch frontend events
to the existing nested renderer paths and also work through callbacks.
`window-rules/get-view-property` and
`set-view-property` provide typed runtime metadata for scripts. Boolean, signed
and unsigned integer, string, and floating-point values are supported; changing
an existing property's type is rejected, and properties are removed when the
view is destroyed. STIPC supports `feed_key`, `feed_button`, `move_cursor`,
`touch`, `touch_release`, and `ping` for automated input. Touch calls dispatch
down for a new finger and motion for an active finger. Common evdev key and
button names are supported, including `S-` plus `press`, `release`, or `full`
pointer-click modes. `run` launches a shell command with the compositor's own
`WAYLAND_DISPLAY`, returns its PID, and reaps exited children during polling.
`get_display` reports the Wayland socket and an empty XWayland display until
XWayland is implemented. `layout_views` validates and applies a batch of exact
view rectangles, with optional output-name checks and no partial updates on
invalid input. Library users can enable the same server with
`Compositor::enable_ipc(path)`.

### Touch gestures

Wayfire's extra-gestures behavior is available in the nested frontend. Holding
three fingers for 750 ms starts moving the window under their center. Tap close
is disabled by default; lowering `close_fingers` enables it. Movement and timing
limits cancel accidental gestures, and active drags finish on release.

```yaml
extra_gestures:
  move_fingers: 3
  move_delay: 750
  close_fingers: 20
```

Wayfire's `[extra-gestures]` options import directly. The i3 variables are
`$gesture_move_fingers`, `$gesture_move_delay`, and `$gesture_close_fingers`.
All three settings are editable in WCM and can be reloaded live.

### XKB symbol commands

Command bindings may use `xkb:` followed by a case-sensitive XKB keysym name to
match the logical key instead of its physical position. Character keys and common
editing/navigation keys are supported, and modifier, repeat, lock, and
`always` policies are shared with ordinary commands.

```yaml
commands:
  - binding: super+xkb:A
    command: notify-send "Shifted A"
  - binding: super+xkb:XF86AudioPlay
    command: playerctl play-pause
```

The same syntax works in imported Wayfire and i3 command bindings. Physical-key
bindings such as `super+a` are unchanged.

### Window rotation

The wrot 2D effect rotates a window around its center with Super+Ctrl and the
right mouse button. Super+R resets the focused window; Super+Ctrl+R resets all
windows. Moving within `effects.wrot_reset_radius` pixels of the center removes
rotation. These visual transforms never change persistent tiling geometry.

```yaml
pointer:
  wrot_activate: super+ctrl+btn_right
keybindings:
  wrot_reset_one: super+r
  wrot_reset: super+ctrl+r
effects:
  wrot_reset_radius: 25.0
```

Wayfire's `[wrot]` `activate`, `reset`, `reset-one`, and `reset_radius` options
import directly. i3 uses `wrot activate`, `wrot reset`, `wrot reset-one`, and
`$wrot_reset_radius`. WCM edits all four settings. Wayfire's separate 3D mode is
not implemented.

## Code organization

- `shingle-core`: tiling trees, workspaces, geometry, and serializable state.
- `shingle`: Wayland protocols, input routing, output state, and the headless
  compositor event loop.
- `shingle-render`: wgpu textures, transforms, effects, and render tiles.
- `shingle-nested`: winit host window and interactive compositor frontend.
- `shingle-config`: validated YAML plus Wayfire INI and i3 imports.
- `shingle-session`: atomic session persistence and application launching.
- `shingle-idle`: idle, DPMS, fullscreen suppression, and lock coordination.
- `shingle-auth`: PAM authentication used by the lock client.
- `shingle-lock`: `ext-session-lock-v1` client with swaylock-style options.
- `shingle-wcm`: Gloomy-based graphical configuration manager.

Scale-specific snapshot selection lives in `shingle`; interactive state and
wgpu thumbnail transforms live in `shingle-nested`. This separation keeps the
persistent tiling model independent of temporary overview geometry.
