# Features

Detailed behaviour of the features Shingle implements, with their configuration. The Wayfire IPC surface is described separately in [ipc.md](ipc.md).

## Overviews and effects

### Scale overview

The example configuration opens the current-workspace overview with Super+P and the all-workspaces overview with Super+Shift+P. Arrow keys, Tab, Shift+Tab, scrolling, and pointer hover change the selection. Enter or a left click commits it; Escape cancels and restores the initially focused window and workspace. Workspace shortcuts navigate without moving windows between them.

Scale never writes thumbnail rectangles back into the tiling tree. Selecting a remote window switches to its workspace. Minimized windows can be included and are restored only when selected. Optional middle-click close and dismissal on new-window mapping match the corresponding Wayfire options.

Typing filters thumbnails by a substring of the title or app ID. Backspace removes one UTF-8 character, and clearing the filter restores all eligible windows. Matching is case-insensitive by default. The optional centered overlay shows the active query and resets when Scale exits.

`xdg_toplevel_icon_v1` accepts named icons and square SHM pixel buffers with commit-based assignment and immutable icon objects. Scale caches supplied pixel icons as 32-pixel GPU textures and requests 64-pixel sources from clients. Stock names use the configured GTK or `ICON_THEME` theme, XDG data roots, inherited themes, and hicolor fallback. PNG files use `image`; SVG files are rasterized with `resvg` at the requested size while preserving their aspect ratio.

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

Super+E opens every configured workspace in its exact grid position. Arrow, H/J/K/L, or keypad keys and pointer hover select a cell, including an empty workspace. Direct workspace shortcuts commit immediately, even when ordinary keyboard interaction is disabled. Enter, left-click, touch, or Super+E commits the selection. Pointer and touch presses begin a possible drag; release commits it. Dragging a thumbnail to another workspace moves that window there on release. Escape cancels without a drop. Other keys, buttons, and scrolling are captured while Expo is active, and keyboard navigation is suspended while a pointer drag is held. Selection alone never changes the active workspace or moves tiled windows.

Expo redraws current client buffers whenever their contents or layout change. Entry and exit interpolate the workspace wall and its thumbnails. Changing the selection crossfades brightness without changing compositor opacity. Output resizes and window destruction invalidate thumbnail geometry, and configuration reloads dismiss Expo. A zero duration disables the corresponding transition.

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

Wayfire's `select_workspace_2 = <super> KEY_2` and i3's `bindsym Mod4+2 expo select 2` import the same direct binding. WCM edits these as comma-separated `workspace=binding` pairs alongside the other Expo options.

### Vswitch workspace transitions

Normal workspace switches slide the old and new workspaces along the configured grid. The transition preserves window sizes and positions, keeps layer-shell panels stationary, and never writes temporary coordinates into the session. Opening Scale or Expo cancels the slide so overview transforms remain separate. Output changes and configuration reloads also discard stale transitions.

```yaml
vswitch:
  duration_ms: 300
  gap: 20
  background: [0.1, 0.1, 0.1, 1.0]
```

Wayfire's `[vswitch]` `duration`, `gap`, and `background` options are imported. The corresponding i3 variables are `$vswitch_duration_ms`, `$vswitch_gap`, and `$vswitch_background`. A zero duration switches immediately.

### Window rotation

The wrot 2D effect rotates a window around its center with Super+Ctrl and the right mouse button. Super+R resets the focused window; Super+Ctrl+R resets all windows. Moving within `effects.wrot_reset_radius` pixels of the center removes rotation. These visual transforms never change persistent tiling geometry.

```yaml
pointer:
  wrot_activate: super+ctrl+btn_right
keybindings:
  wrot_reset_one: super+r
  wrot_reset: super+ctrl+r
effects:
  wrot_reset_radius: 25.0
```

Wayfire's `[wrot]` `activate`, `reset`, `reset-one`, and `reset_radius` options import directly. i3 uses `wrot activate`, `wrot reset`, `wrot reset-one`, and `$wrot_reset_radius`. WCM edits all four settings. Wayfire's separate 3D mode is not implemented.

## Input

### XKB symbol commands

Ordinary bindings name physical keys the way Wayfire does, after the `KEY_*` constants of `linux/input-event-codes.h` in lower case without the prefix (`a`, `1`, `left`, `kp7`, `leftmeta`, `dot`, `leftbrace`), and the Wayfire and i3 importers turn `esc` and `return` into `escape` and `enter`. Command bindings may use `xkb:` followed by a case-sensitive XKB keysym name to match the logical key instead of its physical position. The keysym comes from Shingle's own keymap (`input.layout`, `variant`, `options` or `xkb_file`), not the host's, so with a `de` layout `xkb:z` is the key labelled Z on a German keyboard. Character keys and common editing/navigation keys are supported, and modifier, repeat, lock, and `always` policies are shared with ordinary commands.

```yaml
commands:
  - binding: super+xkb:A
    command: notify-send "Shifted A"
  - binding: super+xkb:XF86AudioPlay
    command: playerctl play-pause
```

The same syntax works in imported Wayfire and i3 command bindings. Physical-key bindings such as `super+a` are unchanged.

### Text input

`text-input-v3` follows keyboard focus with enter/leave events and implements committed enable/disable state, surrounding text, content type, cursor rectangles, and commit serials. `input.enable_text_input_v3` defaults to true and is imported from Wayfire or editable in WCM.

`zwp_input_method_v1` activates contexts for enabled text inputs and relays surrounding text, content type, commit strings, preedit text/cursors, and surrounding-text deletion. Hardware keys and modifiers go to the active keyboard grab. Input methods can forward them to the application, and forwarded held keys are released when the context is deactivated or destroyed.

`zwp_input_method_manager_v2` supports one input method per seat, activation and deactivation, surrounding text and change causes, content types, and atomic edit batches with done-count serial validation. Its keyboard grabs receive the current keymap, repeat settings, hardware keys, and modifiers. Releasing a grab restores normal application input. Version 2 popup surfaces receive text-input rectangle hints and become inert when their input method is destroyed.

`zwp_input_panel_v1` supports center-bottom panels and cursor-relative overlays. Version 2 popups use the same cursor-relative placement. Overlay placement follows committed cursor rectangles after the focused surface's next commit, flips above when necessary, and clamps to the output. Panels are visible only while their text input and input method are active and do not take keyboard focus away from the field.

### Pointer control

`wp_pointer_warp_v1` accepts focused-surface requests with a valid pointer-enter serial and an in-bounds local position. The nested frontend moves the host cursor with inverse output-transform and zoom mapping, while suppressing the resulting synthetic motion event. Pointer-lock and confinement behavior remains separate.

### Touch gestures

Wayfire's extra-gestures behavior is available in the nested frontend. Holding three fingers for 750 ms starts moving the window under their center. Tap close is disabled by default; lowering `close_fingers` enables it. Movement and timing limits cancel accidental gestures, and active drags finish on release.

```yaml
extra_gestures:
  move_fingers: 3
  move_delay: 750
  close_fingers: 20
```

Wayfire's `[extra-gestures]` options import directly. The i3 variables are `$gesture_move_fingers`, `$gesture_move_delay`, and `$gesture_close_fingers`. All three settings are editable in WCM and can be reloaded live.

## Clients and protocols

### Decorations

`behavior.preferred_decoration_mode` accepts `client` (the Wayfire default) or `server`. Explicit client requests are honored unless `behavior.force_preferred_decoration_mode` is enabled. Preference changes are sent to existing XDG decoration objects during live configuration reloads.

For clients with popup-focus bugs, `behavior.focus_main_surface_instead_of_popup` moves keyboard focus to the popup's main surface while leaving pointer input on the popup. It is disabled by default and can be changed through WCM or live configuration reloads.

### Dialogs

`xdg-dialog-v1` supports modal and nonmodal hints on parented XDG toplevels. Focusing an ancestor redirects to its topmost explicitly modal descendant even when `behavior.all_dialogs_modal` is disabled. Unsetting the hint or destroying the dialog object restores normal focus. The existing all-dialogs option remains available as a compatibility policy for every parented dialog.

`xdg-foreign-unstable-v2` exports random toplevel handles and imports them in other clients. Imported parents use the same stacking and session relationships as ordinary XDG parents. Invalid handles emit `destroyed`; destroying an import, export, or exported toplevel revokes only the relationships that depend on it.

### Workspace tools

Shingle advertises `ext-workspace-v1` for workspace bars and switchers. Clients receive stable IDs, display names, grid coordinates, output membership, and active state. Activation requests use the protocol's atomic commit operation and share the normal workspace-switch path. Grid changes rebuild the client's workspace list; workspace creation and removal capabilities are not advertised.

`ext-foreign-toplevel-list-v1` provides read-only window handles with stable per-map identifiers, titles, app IDs, metadata updates, and close events. Each client receives its own handles for the same windows. Window control remains available through the existing WLR foreign-toplevel protocol.

### Primary selection

`behavior.disable_primary_selection` matches Wayfire's workaround for disabling middle-click copy and paste. When enabled at startup, Shingle does not advertise `zwp_primary_selection_device_manager_v1` and does not bridge `PRIMARY` with X clients; wlr-data-control clients keep their own primary selection, as under Wayfire. Changing the option requires a compositor restart so existing clients and new clients see the same protocol availability. The option can be imported and edited through WCM.

### XWayland

Shingle binds its own X display `:N` (the first free one, with `/tmp/.X<N>-lock`, `/tmp/.X11-unix/X<N>` and the abstract socket of that name) when it starts and runs a rootless `Xwayland` for it, with its own X window manager. `xwayland.mode` takes Wayfire's `[core] xwayland` values: `true` starts Xwayland with the compositor; `lazy`, Shingle's default, starts it on the first X11 connection and lets it exit 10 s after its last X client; `false` binds nothing and removes `DISPLAY` from children. The Wayfire importer keeps Wayfire's default, `true`, when the option is missing. A changed mode takes effect after a restart. Xwayland is restarted when it exits after running more than 5 s (at once when eager, on the next connection when lazy), and not otherwise. `xwayland.startup_script` (`[core] xwayland_startup_script`) runs in the client environment whenever Xwayland is ready. `SHINGLE_XWAYLAND` names another Xwayland binary. Both options can be imported and edited in WCM.

X11 toplevels are Shingle windows, like Wayland ones: they tile, float, go fullscreen and minimize, follow window rules and `tile_by_default` (as view type `toplevel`), can be held by `view-pre-map`, and appear in `list-views` with the `WM_CLASS` class as `app-id`, their title (`_NET_WM_NAME`, else `WM_NAME`) and their X client's PID. Shingle's tile, floating or fullscreen geometry is the X geometry, and `_NET_WM_STATE` reports fullscreen, maximized, minimized (`HIDDEN`) and focused. A tiled or fullscreen client asking for another geometry gets its own back; a floating one gets its request clamped to the workarea. `close-view` sends `WM_DELETE_WINDOW`, or disconnects a client that does not support it. A session placeholder is claimed by the class, with the launch identity read from `/proc` for the client's PID. Keyboard focus follows the ICCCM input models: passive and locally active windows are focused when they map, globally active `NORMAL` and `DIALOG` windows are offered the focus with `WM_TAKE_FOCUS`, no-input windows are not focused, and a click focuses any of them; `_NET_ACTIVE_WINDOW` names the focused X window, and an X client cannot take the focus from another. Clients' `_NET_WM_STATE` (fullscreen, maximized, hidden), `WM_CHANGE_STATE` (iconify), `_NET_ACTIVE_WINDOW`, `_NET_WM_MOVERESIZE` and `_NET_CLOSE_WINDOW` requests work, and a state set before mapping applies on map. Transient windows and fixed-size windows float, a transient centred on its parent, as Wayfire's tile plugin decides. X stacking follows Shingle's. Menus, tooltips and other override-redirect windows are drawn above the Top layer at their X position, and take keyboard focus as Wayfire's unmanaged views do. X windows without a cursor show the theme's arrow. Scale, Expo, the switchers and session restore include X windows (XW-4 in [design/xwayland.md](design/xwayland.md)). The clipboard and the primary selection are shared between X and Wayland clients in both directions, large selections included (X's `INCR`): what an X client copies is offered to focused Wayland clients and to wlr-data-control clients such as `wl-paste` and clipboard managers, and X clients can paste what Wayland clients copy at any time, as under wlroots. `text/plain;charset=utf-8` is X's `UTF8_STRING` and `text/plain` its `TEXT` and `STRING`; other types keep their names. `behavior.disable_primary_selection` also stops the primary selection crossing between X and Wayland (XW-5). Not yet: drag and drop between X and Wayland (XW-6).

### Sandboxed clients

`wp_security_context_manager_v1` accepts listener sockets from sandbox engines. Committed engine, app, and instance metadata is attached to every accepted client. Sandboxed clients cannot nest security contexts. Every advertised protocol global uses the same configurable visibility policy.

The `security_context.privileged_protocols` comma-separated list is imported from Wayfire and editable in WCM. Its defaults cover foreign-toplevel management, screencopy, data control, and keyboard-shortcut inhibition. Restrictions apply as each client is inserted; already-connected clients retain the globals they received.

### Surface opacity

`wp_alpha_modifier_v1` applies client-requested alpha multipliers on surface commit, including cached synchronized-subsurface commits. The multiplier ranges from zero (transparent) to `UINT32_MAX` (opaque) and is combined with Shingle's window-opacity setting. Destroying the modifier schedules a return to full alpha.

### Output configuration

Each Wayfire `[output:NAME]` section, i3 `output NAME` line or YAML `outputs` entry configures the output with that connector name: `mode` (`auto`, `off`, `highres`, `highrr`, `WxH` or `WxH@R`, as Wayfire reads it), position, scale, transform and `vrr`; `output` is the default. The native backend lights the configured mode and follows monitors being plugged in and out. wlr-output-management clients such as `wlr-randr` see every mode of the output and can change it, test a configuration without applying it, and set position, scale and transform; on a native output a mode change passes a KMS `TEST_ONLY` commit first. Turning the only output off is refused; wlr-output-power-management (DPMS) does that. When the output is replaced by another connector, clients see the old `wl_output` removed and a new one announced, and windows stay on their workspaces.

### Presentation timing

`wp_tearing_control_v1` applies asynchronous or vertical-sync hints on surface commit. When any visible surface requests asynchronous presentation, the nested wgpu frontend selects Immediate mode if the host supports it. Otherwise it keeps FIFO presentation. Destroying the control schedules a return to vertical sync.

`wp_fifo_v1` supports refresh barriers and wait-constrained commits. Eligible updates are coalesced in order and applied by `Compositor::frame()` or `frame_presented()` before frame callbacks. Synchronized subsurfaces, invisible surfaces, and tearing-enabled surfaces bypass the barrier as permitted by the protocol. Destroying a FIFO object does not cancel state already submitted. A buffer replaced while it waits is released to its client, as is one held by a destroyed surface (DB-3 in [design/dmabuf.md](design/dmabuf.md#db-3-as-built)).

`wp_presentation` reports `CLOCK_MONOTONIC`. Nested feedback carries the time the frame was drawn, the refresh interval from the output's refresh rate (60 Hz by default), no sequence number and no flags; offscreen feedback carries the time the GPU finished the frame, the refresh interval and a frame counter, without flags; a backend passes page-flip timestamps, sequence numbers and flags to `Compositor::frame_presented`. Frame callbacks carry the same time in milliseconds, and feedback for surfaces that were not visible is discarded.

`wp_commit_timing_v1` accepts monotonic target timestamps and applies updates as closely as possible without presenting early. Pending updates preserve content order, share FIFO frame processing, and drive nested redraw scheduling. Invalid or duplicate timestamps and destroyed surfaces use the protocol's error codes.

### Resource limits

`behavior.max_buffer_size` limits either dimension of a shared-memory client buffer and defaults to 16384 pixels, matching Wayfire. Oversized buffers are rejected before mapping or GPU upload. The limit is also editable in WCM.
