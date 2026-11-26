# Features

Detailed behaviour of the features Shingle implements, with their configuration. The Wayfire IPC surface is described separately in [ipc.md](ipc.md).

## Importing Wayfire configuration

A `wayfire.ini` is read line by line as Wayfire's wf-config reads it: every `#` not preceded by `\` starts a comment, `\#` is a literal `#` (WCM writes every hex colour as `\#RRGGBBAA`), a line ending in `\` continues on the next, as in Wayfire's own `plugins = \` list, and names and values are trimmed. A value Shingle cannot use keeps its default and is reported as a warning with its line, section and option, as Wayfire logs it, and the rest of the file still loads; so is each section of a plugin Shingle does not have (`[blur]`, `[cube]`, `[decoration]`, `[vswipe]`, `[wayfire-shell]`, `[wobbly]` and others), once. Activators keep their keys and buttons; a modifier alone (`toggle_menu = <super>`), touch gestures (`pinch in 4`, `swipe up 3`, `edge-swipe left 2`) and hotspots (`hotspot top-left 10x10 500`) are recognised, reported as not supported yet and never bound. Where Shingle's option takes one binding, the first key or button is used. The animations Shingle lacks fall back with a warning: `squeezimize` to `zoom`, and `fire`, `zap` and `spin` to `fade`. View matchers take Wayfire's `focusable equals true` form, so Wayfire's default `animate/enabled_for` works. Colours may be `#RRGGBBAA`, `#RGBA`, `#RRGGBB` or four numbers. i3 files warn the same way; YAML stays strict and rejects any bad value.

`shingle` prints the warnings once when it starts and again on a reload, as `shingle: PATH: line N: MESSAGE`. `shingle-wcm --config PATH --check` prints them and exits 0, or 1 when the file cannot be read.

A Wayfire import starts from Wayfire's own defaults, taken from its metadata (crates/shingle-config/data/wayfire-options.tsv, written by tools/wayfire_options/generate.py), so an empty `wayfire.ini` gives a 3×3 workspace grid, exits on `ctrl+alt+backspace` and leaves the wm-actions bindings disabled. Shingle's bindings that no Wayfire option sets are off: moving and resizing tiles, `super+1` to `super+0` with and without `shift`, `super+shift+r` reload and `super+f8` lock; `[simple-tile] key_move_*`, `key_resize_*` and `key_lock`, and `[idle] key_lock`, still set them. `[core] plugins`, or Wayfire's default list when the file has none, decides which plugins are on, as Wayfire loads only those: a plugin the list leaves out reads none of its options and its bindings, effects, animations, rules and autostart entries are off. Without `simple-tile` nothing tiles, without `place` new views open at the top left of the workarea, and the globals of `xdg-activation`, `foreign-toplevel`, `ext-toplevel`, `ext-workspace`, `security-context-v1`, `session-lock`, `shortcuts-inhibit` and `input-method-v1` are created only when they are listed. IPC is not a plugin here: `--ipc` turns it on, and the methods of an unlisted plugin still answer. YAML can set the same list as `plugins:`; without it every plugin is on.

`[vswitch] binding_N` switches to workspace N, counted row by row, `with_win_N` carries the focused window there and follows it, and `send_win_N` sends it there without switching; in YAML they are `vswitch.workspace_bindings`, `workspace_bindings_win` and `bindings_win`, lists of `{workspace: N, binding: KEY}`. With `[autostart] autostart_wf_shell` true, Wayfire's default, `wf-panel` and `wf-background` are added to the autostart commands, each unless an entry already names it.

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

Shingle's own numbered bindings, `keybindings.workspace_switch` (`super+1` to `super+0` in data/shingle.yaml) and `keybindings.workspace_move` (`super+shift+1` to `super+shift+0`), switch to workspace N and send the focused window to workspace N. `workspace_move` does not follow the window: the current workspace stays, as with i3's `move container to workspace N`. The directional and last-workspace `workspace_move_*` bindings carry the window and follow it, as Wayfire's `with_win_*` do, and `workspace_send_*` send it without following; Wayfire's numbered `with_win_N` and `send_win_N` are described under [Importing Wayfire configuration](#importing-wayfire-configuration).

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

### Client environment

Every process Shingle starts (autostart, restored and `--spawn` clients, command bindings, `stipc/run`, the lock client and Xwayland) gets Shingle's `WAYLAND_DISPLAY`, its own `DISPLAY` or none, `WAYFIRE_SOCKET` with `--ipc` and `_JAVA_AWT_WM_NONREPARENTING=1`, and never the host session's `WAYLAND_SOCKET`, `DISPLAY` or `WAYFIRE_SOCKET` (crates/shingle-session/src/environment.rs). Since WG-3 it also gets what Wayfire exports:

- `XDG_CURRENT_DESKTOP=wayfire` when Shingle itself was started without the variable, as Wayfire does, so xdg-desktop-portal picks its backends from `wayfire-portals.conf` and scripts that test for Wayfire keep working. A value the login or the session file set, even an empty one, is kept. Shingle has no portals file of its own.
- `XCURSOR_THEME` from `input.cursor_theme` (`[input] cursor_theme`), unless it is `default`, which keeps the theme Shingle inherited, and `XCURSOR_SIZE` from `input.cursor_size`. At the default size 24 a numeric `XCURSOR_SIZE` Shingle inherited is kept, since Shingle then draws its own cursor at that size. Both follow a configuration reload and `wayfire/set-config-options` for every child started afterwards.
- `WAYFIRE_CONFIG_FILE`, the absolute path of the configuration file when it is a Wayfire file, as Wayfire exports the file it reads for WCM and scripts. With a YAML or i3 configuration the variable is removed, so a host Wayfire's file never reaches programs started inside Shingle and nothing edits a YAML file as `wayfire.ini`.

data/shingle.desktop is the session file for display managers; the [README](../README.md#starting-from-a-display-manager) says how to install it.

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

Each Wayfire `[output:NAME]` section, i3 `output NAME` line or YAML `outputs` entry configures the output with that connector name: `mode` (`auto`, `off`, `highres`, `highrr`, `WxH` or `WxH@R`, as Wayfire reads it), position (`auto`, Wayfire's default, or a fixed point), scale, transform and `vrr`; `output` is the default. The configuration for several outputs is read: what happens to an unplugged output's windows (`output_layout.unplug`, applied since MO-2), Wayfire's `[oswitch]` and `[wsets]` bindings, `[input-device:NAME] output` and `[workarounds] remove_output_limits`. The compositor keeps several outputs, each with its own `wl_output`, xdg-output geometry and output-management head, placed by the configured or automatic positions; headless Shingle starts with `--outputs N`, and IPC creates and destroys headless outputs with `wayfire/create-headless-output` and `wayfire/destroy-headless-output` on every backend. Client protocols are per output: surfaces enter the outputs they are shown on and get their scales, layer-shell panels and lock surfaces are on their own outputs, screencopy (`grim -o`, `grim -g`) captures each output in its physical pixels, power and gamma are per output, and every output has its own ext-workspace group. One output is active, as in Wayfire: a click or touch on another output, focusing a window on it, Wayfire's `oswitch` bindings (`super+o` for the next output, `super+shift+o` taking the focused window along, and the previous, left, right, above and below outputs) and IPC that names an output change it, never pointer motion alone, and the pointer moves freely across the layout without leaving it. Windows move between outputs with Wayfire's rules (tiled ones join the target tree, floating ones keep their relative place, fullscreen and snapped ones are applied again there): by oswitch, `configure-view` with `output_id`, `start_on_output NAME` window rules, `set_fullscreen` on an output, a drag onto another output, and Wayfire's `wsets` bindings and IPC, which show a workspace set on the active output or send a window to one. Dialogs open on their parent's output, other new windows on the active output. The frontend draws one scene per output: each output shows its own windows, clipped to it, with its own frames, zoom and workspace slides; Scale and Expo run on the active output; a window being dragged shows on every output it crosses; Wayfire's "Workspace set N" label appears on an output that selects a set; and `[workarounds] remove_output_limits` draws windows on every output they overlap. The offscreen backend starts with `--outputs N`, each output at its own refresh rate. Nested Shingle opens a window per output, as a nested Wayfire does: `--outputs N` (else `WLR_WL_OUTPUTS`) starts it with `HEADLESS-1` to `HEADLESS-N`, each window sized, drawn and presented as its own output with its pointer and touch input on that output, `stipc/create_wayland_output` and `stipc/destroy_wayland_output` open and close more windows, and closing a window unplugs its output under `output_layout.unplug`; a window opened again takes back its output's name and windows. The native backend lights every connected monitor it has a CRTC for, each flipping at its own refresh rate, adds and removes outputs as monitors are plugged in and out, lists monitors it cannot light as disabled heads that `wlr-randr --on` can turn on, turns single monitors off through wlr-output-power and maps absolute input devices to `[input-device:NAME] output` (MO-7, not yet run on hardware); `--outputs N` lights at most N. The native backend lights the configured mode and follows monitors being plugged in and out. wlr-output-management clients such as `wlr-randr` see every mode of every output and can change it, test a configuration without applying it, set position, scale and transform, and turn outputs off and on (not yet native ones); on a native output a mode change passes a KMS `TEST_ONLY` commit first. Outputs a client configured keep that placement across config reloads until Shingle restarts, as in Wayfire. Turning every output off is refused; wlr-output-power-management (DPMS) does that. When the output is replaced by another connector, clients see the old `wl_output` removed and a new one announced, and windows stay on their workspaces.

### Presentation timing

`wp_tearing_control_v1` applies asynchronous or vertical-sync hints on surface commit. When any visible surface requests asynchronous presentation, the nested wgpu frontend selects Immediate mode if the host supports it. Otherwise it keeps FIFO presentation. Destroying the control schedules a return to vertical sync.

`wp_fifo_v1` supports refresh barriers and wait-constrained commits. As in wlroots, an update that waits for a set barrier is queued with its own barrier requests, never merged with another; each presented frame (`Compositor::frame()` or `frame_presented()`) applies the queued updates up to the first that sets the barrier again, in order and before frame callbacks. A queued update keeps its buffer, so a client that commits faster than the output refreshes waits for buffers instead of running ahead. Synchronized subsurfaces, invisible surfaces, and tearing-enabled surfaces bypass the barrier as permitted by the protocol. Destroying a FIFO object does not cancel state already submitted. A buffer replaced while it waits is released to its client, as is one held by a destroyed surface (DB-3 in [design/dmabuf.md](design/dmabuf.md#db-3-as-built)).

`wp_presentation` reports `CLOCK_MONOTONIC`. Nested feedback carries the time the frame was drawn, the refresh interval from the output's refresh rate (60 Hz by default), no sequence number and no flags; offscreen feedback carries the time the GPU finished the frame, the refresh interval and a frame counter, without flags; a backend passes page-flip timestamps, sequence numbers and flags to `Compositor::frame_presented`. The native backend's feedback carries the flip's time and vblank sequence with `vsync`, `hw_clock` and `hw_completion`, and `zero_copy` for a monitor whose frames are drawn straight into the buffers it scans out; it uses such GBM buffers wherever the plane, the renderer and GBM share a modifier, and copies frames into dumb buffers otherwise or with `SHINGLE_SCANOUT=copy`. Not yet run on hardware. Frame callbacks carry the same time in milliseconds, and feedback for surfaces that were not visible is discarded.

`wp_commit_timing_v1` accepts monotonic target timestamps. As in wlroots, each timed update waits in order with its own deadline, one refresh and 500 µs before its timestamp, so that it is applied for the frame that shows it; a timestamp closer than that is not waited for. Waiting updates are never merged, keep their buffers, are checked at each presented frame and drive redraw scheduling. Invalid or duplicate timestamps and destroyed surfaces use the protocol's error codes.

### GPU client buffers

`zwp_linux_dmabuf_v1` version 5, as Wayfire's wlroots offers it, lets GPU clients hand Shingle their buffers without a copy. It is offered when the renderer can import DMA-BUFs and has a render node to name: nested and native on a GPU, and `--backend offscreen`; not headless, not on lavapipe, and not with `SHINGLE_DMABUF=0`. Clients get one feedback tranche on the render node with every format and modifier the renderer samples (8-bit ARGB, XRGB, ABGR and XBGR with their single-plane modifiers). Buffers are imported when the client creates them, so a buffer the renderer cannot use is refused with `failed`, and a commit waits until the client's GPU has finished writing the buffer (implicit sync). A buffer goes back to its client only after the GPU has finished every frame that showed it. `wl_drm` is not offered, as Wayfire does not by default. `SHINGLE_TRACE_BUFFERS=1` logs which path each client buffer takes.

`wp_linux_drm_syncobj_manager_v1` version 1 adds explicit sync, which Wayfire does not offer: it is offered with linux-dmabuf when the render node supports timeline syncobjs and their eventfds, and not with `SHINGLE_SYNCOBJ=0`. NVIDIA's Vulkan WSI, Xwayland's glamor and Chromium use it when it is there; NVIDIA's EGL should too, which has not been verified yet. A commit waits for its acquire point instead of the buffer's implicit fences, only that surface waits, and the release point is signalled together with `wl_buffer.release`, once the GPU has finished every frame that showed the buffer.

### Resource limits

Shared-memory buffers go to the GPU straight from the client's pool, in its own byte order, with only the damaged rectangles written, and are given back to the client in the turn they were committed, before any frame; a pool sealed against shrinking stays mapped. The headless backend, which draws nothing, copies them on the CPU instead.

`behavior.max_buffer_size` limits either dimension of a shared-memory client buffer and defaults to 16384 pixels, matching Wayfire. Oversized buffers are rejected before mapping or GPU upload. The limit is also editable in WCM.
