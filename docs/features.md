# Features

Detailed behaviour of the features Shingle implements, with their configuration. The Wayfire IPC surface is described separately in [ipc.md](ipc.md).

## Importing Wayfire configuration

A `wayfire.ini` is read line by line as Wayfire's wf-config reads it: every `#` not preceded by `\` starts a comment, `\#` is a literal `#` (WCM writes every hex colour as `\#RRGGBBAA`), a line ending in `\` continues on the next, as in Wayfire's own `plugins = \` list, and names and values are trimmed. A value Shingle cannot use keeps its default and is reported as a warning with its line, section and option, as Wayfire logs it, and the rest of the file still loads; so is each section of a plugin Shingle does not have (`[blur]`, `[cube]`, `[wobbly]` and others), once. Activators keep their keys and buttons; a modifier alone (`<super>`) is bound for `[wayfire-shell] toggle_menu` and commands ([Modifier-alone bindings](#modifier-alone-bindings)) and elsewhere warns; touch gestures (`pinch in 4`, `swipe up 3`, `edge-swipe left 2`) and hotspots (`hotspot top-left 10x10 500`) are recognised, reported as not supported yet and never bound. Where Shingle's option takes one binding, the first key or button is used. The animations Shingle lacks fall back with a warning: `squeezimize` to `zoom`, and `fire`, `zap` and `spin` to `fade`. View matchers take Wayfire's `focusable equals true` form, so Wayfire's default `animate/enabled_for` works. Colours may be `#RRGGBBAA`, `#RGBA`, `#RRGGBB` or four numbers. i3 files warn the same way; YAML stays strict and rejects any bad value.

`shingle` prints the warnings once when it starts and again on a reload, as `shingle: PATH: line N: MESSAGE`. `shingle-wcm --config PATH --check` prints them and exits 0, or 1 when the file cannot be read.

A Wayfire import starts from Wayfire's own defaults, taken from its metadata (crates/shingle-config/data/wayfire-options.tsv, written by tools/wayfire_options/generate.py), so an empty `wayfire.ini` gives a 3×3 workspace grid, exits on `ctrl+alt+backspace` and leaves the wm-actions bindings disabled. Shingle's bindings that no Wayfire option sets are off: moving and resizing tiles, `super+1` to `super+0` with and without `shift`, `super+shift+r` reload and `super+f8` lock; `[simple-tile] key_move_*`, `key_resize_*` and `key_lock`, and `[idle] key_lock`, still set them. `[core] plugins`, or Wayfire's default list when the file has none, decides which plugins are on, as Wayfire loads only those: a plugin the list leaves out reads none of its options and its bindings, effects, animations, rules and autostart entries are off. Without `simple-tile` nothing tiles, without `place` new views open at the top left of the workarea, and the globals of `xdg-activation`, `foreign-toplevel`, `ext-toplevel`, `ext-workspace`, `security-context-v1`, `session-lock`, `shortcuts-inhibit` and `input-method-v1` are created only when they are listed. IPC is not a plugin here: `--ipc` turns it on, and the methods of an unlisted plugin still answer. YAML can set the same list as `plugins:`; without it every plugin is on. A bare `swaylock` in a `[command]` or `[autostart]` entry runs `shingle-lock` with the same arguments instead, with a warning ([Lock screen](#lock-screen)).

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

Expo redraws current client buffers whenever their contents or layout change. Each workspace is scaled into its cell as a whole, as Wayfire's expo scales its wall, so windows keep their places and sizes relative to their workspace. Entry and exit interpolate the workspace wall and its thumbnails. Changing the selection crossfades brightness without changing compositor opacity. Output resizes and window destruction invalidate thumbnail geometry, and configuration reloads dismiss Expo. A zero duration disables the corresponding transition.

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

### Alt+Tab switcher

Alt+Tab (`keybindings.focus_next`, Wayfire's `[switcher] next_view`) and Alt+Shift+Tab (`focus_previous`, `prev_view`) show the windows of the current workspace as Wayfire's switcher plugin does: the selected window in the middle of the output, scaled to fit 45 % of it, and the others a third of the output to its left and right, smaller, further back, dimmer and turned towards it, over black, with the wallpaper and bottom layers dimmed and panels on top. Each press moves the selection, by focus order, and the thumbnails slide between the slots; releasing Alt focuses and raises the selected window, restoring it if it was minimized, and the thumbnails slide back into place. Minimized windows are included. IPC sees the `switcher` plugin activate and deactivate (`plugin-activation-state-changed`).

```yaml
switcher:
  duration_ms: 500
  view_thumbnail_scale: 1.0
  view_thumbnail_rotation: 30
```

Wayfire's `[switcher] speed`, `view_thumbnail_scale` and `view_thumbnail_rotation` import directly; the easing curve of `speed` is ignored. The fast switcher (`[fast-switcher]`, Alt+Escape) still only dims the other windows.

### Vswitch workspace transitions

Normal workspace switches slide the old and new workspaces along the configured grid. The transition preserves window sizes and positions, keeps layer-shell panels stationary, and never writes temporary coordinates into the session. Opening Scale or Expo cancels the slide so overview transforms remain separate. Output changes and configuration reloads also discard stale transitions.

A floating window hanging over the edge of its workspace shows on the neighbouring workspace too, as in Wayfire: once that workspace is current, the part past the edge is drawn at its facing edge and takes clicks, which focus the window without switching. During a slide each workspace shows its part of the window, cut at the workspace's edge with the gap between the parts, and Expo shows each part in its workspace's cell. The window belongs to the workspace holding its centre, for IPC, the session and focus order; a floating window dropped after a move goes to that workspace and stays where it was dropped.

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

### Modifier-alone bindings

A binding of modifiers without a key, `super` or `ctrl+alt` in YAML and `<super>` in a `wayfire.ini`, triggers when those modifiers are pressed and released with nothing else in between, as in Wayfire (src/core/seat/keyboard.cpp): the last modifier must be pressed while only modifier keys are held and no pointer button or touch point is down, no other key, button, scroll or touch may come before its release, and the release must come within `input.modifier_binding_timeout` milliseconds (`[input] modifier_binding_timeout`, default 400; 0 never cancels). The keys themselves still reach bindings and clients, so `super+p` is unaffected. Two things act on one: `keybindings.toggle_menu` (`[wayfire-shell] toggle_menu`, default `super`), which tells the active output's shell clients to toggle their menu ([Shell clients](#shell-clients)), and commands, which run once on the release; a common Wayfire setup is `binding_launcher = <super>` with a launcher command. Other bindings do not take a modifier alone. While the session is locked only `always` commands run, and neither runs while a client inhibits shortcuts.

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

`text-input-v1` (`zwp_text_input_manager_v1`), which Chromium and Electron use for input methods by default, works as in Wayfire's input-method-v1 plugin: `activate` on the surface with its client's keyboard focus enables a text input and sends `enter`, its surrounding text, content type, cursor rectangle and `commit_state` serial reach the input method at once, `reset`, the preferred language and `invoke_action` reach input-method v1, and `deactivate` or a focus change sends `leave`. Both input-method versions serve it: input-method v1's events arrive unchanged with their serial, and input-method v2's commits as v1's deletion, commit string and preedit. `input.enable_text_input_v1` (Wayfire's `[input-method-v1] enable_text_input_v1`) defaults to true; the input methods exist while either text-input version is enabled.

`zwp_input_method_v1` activates contexts for enabled text inputs and relays surrounding text, content type, commit strings, preedit text/cursors, and surrounding-text deletion. Hardware keys and modifiers go to the active keyboard grab. Input methods can forward them to the application, and forwarded held keys are released when the context is deactivated or destroyed.

`zwp_input_method_manager_v2` supports one input method per seat, activation and deactivation, surrounding text and change causes, content types, and atomic edit batches with done-count serial validation. Its keyboard grabs receive the current keymap, repeat settings, hardware keys, and modifiers. Releasing a grab restores normal application input. Version 2 popup surfaces receive text-input rectangle hints and become inert when their input method is destroyed.

`zwp_input_panel_v1` supports center-bottom panels and cursor-relative overlays. Version 2 popups use the same cursor-relative placement. Overlay placement follows committed cursor rectangles after the focused surface's next commit, flips above when necessary, and clamps to the output. Panels are visible only while their text input and input method are active and do not take keyboard focus away from the field.

### Per-device input options

A Wayfire `[input:NAME]` section, or YAML `input_devices.NAME`, gives one device its own libinput options in place of `[input]`'s: `left_handed_mode`, `middle_emulation`, the mouse and touchpad acceleration profiles and speeds, `mouse_natural_scroll`, `natural_scroll`, `tap_to_click`, `click_method`, `scroll_method`, `disable_touchpad_while_typing`, `disable_touchpad_while_mouse`, `tap_and_drag` and `drag_lock`; options it leaves out follow `[input]`. `[input-device:NAME]` (the same YAML entry) maps an absolute device to an output and gives a touch screen a libinput `calibration` matrix of six numbers. NAME is the device's udev `ID_PATH`, `ID_SERIAL`, `LIBINPUT_DEVICE_GROUP` or `DEVPATH`, tried in that order, or its name, as Wayfire looks sections up. The native backend applies them when a device is added, and `input/list-devices` reports the sections each device got as `input-section` and `input-device-section` ([ipc.md](ipc.md)); nested leaves input options to the host. Keyboard options per device are not supported and warn.

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

### Touchpad workspace swipes

Wayfire's `vswipe` plugin: a touchpad swipe with `fingers` fingers (four by default) drags the wall of workspaces under the fingers, and lifting them slides it to a workspace, as wayfire:plugins/single_plugins/vswipe.cpp does. The swipe's direction is decided once it has moved 0.05 of a workspace (`speed_factor` pixels make one workspace): horizontal or vertical when that axis is enabled and the grid has more than one workspace along it, or, with `enable_free_movement`, diagonal when it starts at a slope between 30° and 60° or moves 0.3 of a workspace along the other axis later. Each update moves the wall at most `speed_cap` workspaces and slows down like a rubber band past the neighbouring workspace (without free movement) or the grid's edge. When the fingers lift, the wall moves on to the next workspace if it went more than `threshold` of a workspace, or if the last two updates moved more than `delta_threshold` pixels (a flick); it never leaves the grid, vswitch's `wraparound` does not apply, and without free movement it moves one workspace at most. The workspace switches at once, as `wset-workspace-changed` reports, and the wall slides there over `duration` without vswitch's slide; with `enable_smooth_transition` the wall also eases to each new position while the fingers move. The wall is drawn with `gap` pixels between the workspaces and `background` behind them. Lifting the fingers after libinput cancelled the swipe is handled like any other end, as in Wayfire.

Clients get every swipe until vswipe takes it: a swipe with another finger count, one that begins with the pointer on no output, and one whose direction is never decided are theirs whole. When vswipe starts a swipe, the client it went to gets its end, cancelled, and nothing more of it, and the output under the pointer becomes the active one. vswipe does not start while Scale, Expo, a switcher, a drag, window rotation or vswitch's slide runs, while the session is locked or while a shell client inhibits the output, and a swipe that begins while vswipe's slide still runs goes to clients, as Wayfire's plugin activation refuses it then. A configuration reload, a disabled pointer, a paused session, or a resize or removal of the output drops a running swipe without switching.

```yaml
vswipe:
  enable_horizontal: true
  enable_vertical: true
  enable_free_movement: false
  enable_smooth_transition: false
  duration_ms: 180
  background: [0.1, 0.1, 0.1, 1.0]
  fingers: 4
  gap: 32.0
  threshold: 0.35
  delta_threshold: 24.0
  speed_factor: 256.0
  speed_cap: 0.05
```

Wayfire's twelve `[vswipe]` options import with these names (`duration` in milliseconds, its easing curve ignored, as for every Shingle animation), when `[core] plugins` lists `vswipe`; Wayfire's default list does not. Tests feed swipes with Shingle's `stipc/feed_gesture` ([ipc.md](ipc.md#stipc-test-input)).

## Application launcher

`shingle-launcher` (crates/shingle-launcher) is rofi's type-to-run and the KDE menu's in-place editing in one panel ([design/launcher.md](design/launcher.md)). `super+space` runs it in data/shingle.yaml, and running it again while it is open closes it. It is a wlr-layer-shell client on the overlay layer with the keyboard to itself, so it works under Wayfire too; it dims the output, and a click outside the panel closes it.

- **Finding and launching.** It lists the desktop entries of `$XDG_DATA_HOME/applications` and `$XDG_DATA_DIRS`, as the Desktop Entry Specification filters them, and the commands on `$PATH`. Typing filters everything at once with a fuzzy match on names, generic names, keywords and program names, ranked by how often and how recently each was launched; Enter launches the selection, and the last row, or Ctrl+Enter, runs the typed text as a command. It launches with the environment Shingle (or Wayfire) gave it and exits.
- **Categories and favourites.** The left column has Favourites, Recent, the categories (from the entries' freedesktop main categories, or the user's own) and All, and Hidden once something is hidden; Tab, or Left and Right with nothing typed, moves through them. The launcher opens on the favourites when there are any.
- **Editing in place.** Ctrl+D favourites or unfavourites, F2 renames, Ctrl+N makes a shortcut whose command is the typed text, Ctrl+E edits a shortcut's command, Ctrl+M moves to a category by name (a new name creates it), Ctrl+H hides or shows, Ctrl+Up and Ctrl+Down reorder, Delete removes a shortcut; Shift+F2 renames a category and Ctrl+Shift+Up and Down move it. A right click opens the same edits as a menu, and an entry dragged onto a row, a category, Favourites or Hidden goes there.
- **Storage.** Every change is written at once to `$XDG_DATA_HOME/shingle/launcher.yaml`, a YAML file of only what was changed, readable and safe to edit by hand; launch counts go to `$XDG_STATE_HOME/shingle/launcher-history.json` and the scanned index to `$XDG_CACHE_HOME/shingle/launcher-index.json`, rescanned in the background on every start.
- **Icons.** Each entry shows its icon from the icon theme (`launcher.icon_theme`, else KDE's or GTK's setting, falling back to hicolor and the pixmaps directories), PNG or SVG, decoded at the size drawn and cached in `$XDG_CACHE_HOME/shingle/launcher-icons`, so a warm start draws them in its first frame; an entry without one shows its first letter.
- **Desktop actions.** An application's actions, such as a browser's New Private Window, are found by typing their name and launched with Enter; the Menu key, Shift+F10 or a right click opens the entry's menu with its actions first.
- **Scaling.** On outputs with fractional scales (with `wp_fractional_scale_v1` and `wp_viewporter`) the panel is drawn in the output's device pixels, so text and icons stay sharp at 1.25, 1.5 or 2.
- **Look.** The `launcher:` section of Shingle's config sets the panel's size, font size, colours, icon theme, terminal for terminal entries, and renderer: by default lavapipe when installed, which starts the first frame in under 50 ms on the development host where the GPU's driver alone takes over 100 ms. WCM's Launcher page edits the section.

## Lock screen

`shingle-lock` (crates/shingle-lock) is the lock screen, an ext-session-lock-v1 client that takes swaylock's and swaylock-effects' options and unlocks through PAM ([design/lock-screen.md](design/lock-screen.md)). The lock binding (`super+f8`) and `idle.lock_seconds` run `lock.command`, `shingle-lock` by default (`SHINGLE_LOCK` overrides it).

- **Nothing shows through.** From the moment a lock client asks for the lock until it unlocks, no output draws any client: each output is black until the lock's surface for it has committed a full opaque frame, and then shows only that surface. Overviews, switchers, slides, animations, the cursor and screencopy show nothing else either. A crashed lock client leaves every output black and the session locked; the lock binding then starts a new lock client. On native outputs, a DPMS wake, the return from another VT and a hotplug show a black buffer instead of a frame drawn before the lock. A lock the binding or the timeout starts lets the clients show, without input, until the lock client asks for the lock (at most 5 s), so that it can capture them.
- **Background.** Unless the config turns it off (`screenshot: false`), every output is captured before the lock is asked for and blurred once on the GPU (`effect-blur=RxT`, default `24x3`), then drawn under `effect-vignette=B:F` (default `0.5:0.5`), flowing like a slow liquid under frosted glass at the output's refresh rate, its colour masses drifting and changing shape and two broad lights wandering across (`lock.drift`: `intensity` 1, 0 for still, up to 3; `speed` 1, its waves coming round in 26 to 55 s, up to 4; `fps` steps it for battery). It is drawn in linear light at 16 bits and dithered, so gradients show no bands, and handed to the compositor as DMA-BUFs without a copy, or through `wl_shm` where that is unavailable. `shingle-lock` warns when an output was not captured and says why. Frames follow the compositor's frame callbacks, so nothing is drawn while an output is off. Without a screenshot the background is `color`.
- **Minimal style (`lock.style: minimal`, the default).** At rest the time (`%H:%M`, or `timestr`) is large and centred in a light face, with the date below only if `datestr` is set. The first key crossfades it gently up and away while a slim white rounded outline, transparent inside, rises into the centre with a white dot per character; dots fade and grow in and out. Caps Lock, the keyboard layout (with several layouts or `show-keyboard-layout`) and, with `show-failed-attempts`, the count are small white text below it. Enter breathes the outline's opacity while PAM checks; a wrong password shakes the input a few pixels, warms the outline faintly and fades the dots out; the right one fades the clock and input out over the blur and unlocks, and Shingle dissolves the blurred frame into the desktop (`lock.unlock_fade`, 0.35 s; never after a crashed lock client). `lock.input_timeout` (5 s) after the last key the time comes back the same way; `no-unlock-indicator` keeps the time alone. Transitions run at the output's refresh rate. Typography is Inter, Cantarell, Noto Sans or DejaVu Sans, whichever is installed first, unless `font` names one.
- **swaylock style (`lock.style: swaylock`).** The earlier look: the time and date above an outlined input in swaylock's ring, inside, line and text colours, with the key and backspace highlights, the per-state messages, Caps Lock and the layout as labels inside it; `indicator` keeps it shown.
- **swaylock options.** `shingle-lock` reads `$XDG_CONFIG_HOME/swaylock/config` (or `-C`), then `lock.options` of Shingle's config, then its command line, with swaylock's names: `daemonize`, `show-failed-attempts`, `clock`, `screenshot`, `effect-blur`, `effect-vignette`, `color`, `font`, `font-size`, `indicator`, `indicator-radius`, `indicator-thickness`, every `*-color` with its `-clear`, `-ver` and `-wrong` forms, `line-uses-inside`, `line-uses-ring`, `grace`, `grace-no-mouse`, `grace-no-touch`, `datestr`, `timestr`, `ignore-empty-password` and the Caps Lock and keyboard layout options. The colour, indicator size and `indicator` options apply to the swaylock style only, and the minimal style warns once when they are set. Options it does not draw (images, other effects) and unknown ones warn and are ignored. `daemonize` returns once the session is locked, so `swayidle -w … before-sleep 'shingle-lock -f'` holds a suspend until the lock is up.
- **Switching from swaylock.** A Wayfire import runs `shingle-lock` wherever a `[command]` or `[autostart]` entry runs a bare `swaylock`, with the same arguments, and warns; `/usr/bin/swaylock` keeps swaylock ([design/lock-screen.md](design/lock-screen.md#6-switching-from-swaylock)).

## Clients and protocols

### Client environment

Every process Shingle starts (autostart, restored and `--spawn` clients, command bindings, `stipc/run`, the lock client and Xwayland) gets Shingle's `WAYLAND_DISPLAY`, its own `DISPLAY` or none, `WAYFIRE_SOCKET` with `--ipc` and `_JAVA_AWT_WM_NONREPARENTING=1`, and never the host session's `WAYLAND_SOCKET`, `DISPLAY` or `WAYFIRE_SOCKET` (crates/shingle-session/src/environment.rs). Since WG-3 it also gets what Wayfire exports:

- `XDG_CURRENT_DESKTOP=wayfire` when Shingle itself was started without the variable, as Wayfire does, so xdg-desktop-portal picks its backends from `wayfire-portals.conf` and scripts that test for Wayfire keep working. A value the login or the session file set, even an empty one, is kept. Shingle has no portals file of its own.
- `XCURSOR_THEME` from `input.cursor_theme` (`[input] cursor_theme`), unless it is `default`, which keeps the theme Shingle inherited, and `XCURSOR_SIZE` from `input.cursor_size`. At the default size 24 a numeric `XCURSOR_SIZE` Shingle inherited is kept, since Shingle then draws its own cursor at that size. Both follow a configuration reload and `wayfire/set-config-options` for every child started afterwards.
- `WAYFIRE_CONFIG_FILE`, the absolute path of the configuration file when it is a Wayfire file, as Wayfire exports the file it reads for WCM and scripts. With a YAML or i3 configuration the variable is removed, so a host Wayfire's file never reaches programs started inside Shingle and nothing edits a YAML file as `wayfire.ini`.
- The lock client alone also gets `SHINGLE_CONFIG_FILE`, the absolute path of Shingle's configuration, whose `lock` section it reads.

data/shingle.desktop is the session file for display managers; the [README](../README.md#starting-from-a-display-manager) says how to install it.

### Shell clients

wf-shell's panel and dock, and other shell clients, bind `zwf_shell_manager_v2` (wayfire-shell-unstable-v2, version 3, as Wayfire; crates/shingle/src/wayfire_shell.rs), which the `wayfire-shell` plugin provides; under a Wayfire file it follows `[core] plugins`, and sandboxed clients may bind it, as in Wayfire.

- A `zwf_output_v2` gets `enter_fullscreen` when its output draws a fullscreen window above the top layer, where panels are, and `leave_fullscreen` when that ends: the window focused on that output, the keyboard's or else the focused tile of its current workspace, is fullscreen. An object created while that is so gets `enter_fullscreen` at once.
- `create_hotspot` makes an edge or corner hotspot, `create_custom_hotspot` one around a rectangle in output coordinates, each reaching `threshold` pixels out and cut to the output. Once the pointer or the first touch point has stayed inside for `timeout` milliseconds the hotspot gets `enter`, then `proximity_changed` (version 3) with its distance in pixels while it moves inside, and `leave` when it goes. A hotspot of an output that goes leaves.
- `toggle_menu` (version 2) comes from `keybindings.toggle_menu` and from IPC `wayfire-shell/toggle_menu`.
- `inhibit_output` makes the output draw black until as many `inhibit_output_done` came, or the client went, as Wayfire 0.9 did; Wayfire's current release ignores both.
- `zwf_surface_v2.interactive_move` starts a move of the surface's window, as `xdg_toplevel.move` does; Wayfire's current release only logs it.

### Decorations

`behavior.preferred_decoration_mode` accepts `client` (the Wayfire default) or `server`. Explicit client requests are honored unless `behavior.force_preferred_decoration_mode` is enabled. Preference changes are sent to existing XDG decoration objects during live configuration reloads. Every mode Shingle sends is followed by an `xdg_surface.configure` and takes effect when the client acknowledges it; destroying the decoration object goes back to client-side decorations.

Windows that want server-side decorations get Wayfire's frame, as its `decoration` plugin draws it (wayfire:plugins/decor/): an xdg toplevel in server-side mode, and an X window unless its `_MOTIF_WM_HINTS` leave out the title or the border or it is a splash screen. `decoration.forced_views` frames windows whatever their client asks, and `decoration.ignore_views` keeps windows unframed. The frame is a title bar `title_height` pixels high under a `border_size` border all round, in `active_color` for the focused window and `inactive_color` for the others; the title is drawn in Inter (Shingle draws every title in its bundled font, whatever `font` names) at `title_height` times `font_scale`, in `font_color`, aligned by `title_halign` and `title_valign` and ellipsized at its end; the buttons of `button_order` sit at the right, `title_height` times `button_scale` square with `button_padding` pixels on each side, grey, and red, green or yellow under the pointer. A dialog has no minimize button. A fullscreen window has no frame.

The frame is part of the window, as in Wayfire: a tile, a floating rectangle, a maximized window and IPC's `geometry` and `bbox` include it, and the client gets the rest, which is IPC's `base-geometry`. A floating window that gains or loses its frame keeps its client where it was. A click on the frame focuses the window; a drag of the title bar of five pixels or more moves it, a double click on it maximizes or restores it, the buttons minimize, maximize or restore and close it, and a press on a border resizes a floating window from that border or corner, with the resize cursor shown over it. The frame follows its window through animations, Scale, Expo and workspace slides; a window rotated by `wrot` is drawn without it.

```yaml
decoration:
  font: sans-serif
  font_color: [1.0, 1.0, 1.0, 1.0]
  font_scale: 0.8
  title_halign: left
  title_valign: top
  title_height: 30
  border_size: 4
  button_order: minimize maximize close
  button_scale: 0.7
  button_padding: 4
  active_color: [0.133, 0.133, 0.133, 0.667]
  inactive_color: [0.2, 0.2, 0.2, 0.867]
  ignore_views: none
  forced_views: none
```

Wayfire's fourteen `[decoration]` options import with these names when `[core] plugins` lists `decoration`, which Wayfire's default list does; numbers outside the bounds of Wayfire's metadata are clamped to them, as wf-config clamps them. Without a plugin list (a YAML file without `plugins`), the plugin is on.

For clients with popup-focus bugs, `behavior.focus_main_surface_instead_of_popup` moves keyboard focus to the popup's main surface while leaving pointer input on the popup. It is disabled by default and can be changed through WCM or live configuration reloads.

### Dialogs

An xdg toplevel with a parent, or with a fixed size, floats instead of tiling, as Wayfire's `simple-tile` refuses such views: a dialog is centred on its parent and kept inside the workarea, at the size it picks, and until it first draws its configures leave the size to it. A client's `xdg_surface.set_window_geometry` places its surface: the window geometry is the view, and client-side shadows outside it are drawn around the view and take no space in the layout. `org_kde_kwin_server_decoration_manager` lets Qt and KDE clients ask for a server-side frame or draw their own, with Wayfire's default from `preferred_decoration_mode`.

`xdg-dialog-v1` supports modal and nonmodal hints on parented XDG toplevels. Focusing an ancestor redirects to its topmost explicitly modal descendant even when `behavior.all_dialogs_modal` is disabled. Unsetting the hint or destroying the dialog object restores normal focus. The existing all-dialogs option remains available as a compatibility policy for every parented dialog.

`xdg-foreign-unstable-v1` and `-v2` export random toplevel handles and import them in other clients; a handle of either version imports with the other, as in wlroots, and GTK 3 and xdg-desktop-portal-gtk use version 1. Imported parents use the same stacking and session relationships as ordinary XDG parents. Invalid handles emit `destroyed`; destroying an import, export, or exported toplevel revokes only the relationships that depend on it.

### Workspace tools

Shingle advertises `ext-workspace-v1` for workspace bars and switchers. Clients receive stable IDs, display names, grid coordinates, output membership, and active state. Activation requests use the protocol's atomic commit operation and share the normal workspace-switch path. Grid changes rebuild the client's workspace list; workspace creation and removal capabilities are not advertised.

`ext-foreign-toplevel-list-v1` provides read-only window handles with stable per-map identifiers, titles, app IDs, metadata updates, and close events. Each client receives its own handles for the same windows. Window control remains available through the existing WLR foreign-toplevel protocol.

### Screen capture and sharing

Screenshot tools, screen recorders and the screen-sharing portal capture outputs through `zwlr_screencopy_manager_v1` (version 3), as `grim` and xdg-desktop-portal-wlr 0.7 do, or through `ext_image_copy_capture_manager_v1` with `ext_output_image_capture_source_manager_v1`, as newer xdg-desktop-portal-wlr, grim and OBS do; both are what Wayfire offers. A capture shows the output as it is drawn, with inversion, zoom, Scale, Expo and animations, and the cursor only when the client asks for it (`grim -c`, the portal's embedded cursor mode, `paint_cursors`). Streams are paced by what changes: a portal stream sends no frames while the screen stands still. Captures go into shared memory or straight into the client's DMA-BUFs on the GPU. As Shingle's own addition, `ext_foreign_toplevel_image_capture_source_manager_v1` captures one window alone, from its ext-foreign-toplevel-list handle, and cursor sessions report the pointer's position and image. The design and what is checked are in [design/screen-sharing.md](design/screen-sharing.md).

### Clipboard managers

Clipboard managers read and set the clipboard and the primary selection through `ext_data_control_manager_v1`, which newer ones prefer, or `zwlr_data_control_manager_v1`, as with Wayfire; a selection set through either is offered through both.

### Primary selection

`behavior.disable_primary_selection` matches Wayfire's workaround for disabling middle-click copy and paste. When enabled at startup, Shingle does not advertise `zwp_primary_selection_device_manager_v1` and does not bridge `PRIMARY` with X clients; wlr-data-control clients keep their own primary selection, as under Wayfire. Changing the option requires a compositor restart so existing clients and new clients see the same protocol availability. The option can be imported and edited through WCM.

Nested Shingle shares the clipboard and the primary selection with its host session, in both directions, so that text copied on the host desktop pastes into applications inside nested Shingle and the reverse, as Shingle's own addition: a nested Wayfire does not. Nested opens a second Wayland connection to the host and binds its `ext_data_control_manager_v1`, or else its `zwlr_data_control_manager_v1`, which need no keyboard focus or input serial. What the host copies becomes nested's selection, which nested's Wayland clients, wlr-data-control clients such as `wl-paste` and X clients read like any other, and what a nested client copies is offered to the host. A read hands the reader's own pipe to the writing client, so data of any size never passes through either compositor, and a slow reader or writer stalls only its own transfer. A selection mirrored one way is never mirrored back. When the nested client that owns a shared selection lets it go, the host's goes too, as with the host's own clients. The primary selection is shared only without `behavior.disable_primary_selection` and with ext-data-control or wlr-data-control version 2. A host without data-control, or an X11 host, gets one log line and nothing is shared. `behavior.host_clipboard: false` turns the bridge off; it is read at startup and defaults to true.

### XWayland

Shingle binds its own X display `:N` (the first free one, with `/tmp/.X<N>-lock`, `/tmp/.X11-unix/X<N>` and the abstract socket of that name) when it starts and runs a rootless `Xwayland` for it, with its own X window manager. `xwayland.mode` takes Wayfire's `[core] xwayland` values: `true` starts Xwayland with the compositor; `lazy`, Shingle's default, starts it on the first X11 connection and lets it exit 10 s after its last X client; `false` binds nothing and removes `DISPLAY` from children. The Wayfire importer keeps Wayfire's default, `true`, when the option is missing. A changed mode takes effect after a restart. Xwayland is restarted when it exits after running more than 5 s (at once when eager, on the next connection when lazy), and not otherwise. `xwayland.startup_script` (`[core] xwayland_startup_script`) runs in the client environment whenever Xwayland is ready. `SHINGLE_XWAYLAND` names another Xwayland binary. Both options can be imported and edited in WCM.

X11 toplevels are Shingle windows, like Wayland ones: they tile, float, go fullscreen and minimize, follow window rules and `tile_by_default` (as view type `toplevel`), can be held by `view-pre-map`, and appear in `list-views` with the `WM_CLASS` class as `app-id`, their title (`_NET_WM_NAME`, else `WM_NAME`) and their X client's PID. Shingle's tile, floating or fullscreen geometry is the X geometry, and `_NET_WM_STATE` reports fullscreen, maximized, minimized (`HIDDEN`) and focused. A tiled or fullscreen client asking for another geometry gets its own back; a floating one gets its request clamped to the workarea. `close-view` sends `WM_DELETE_WINDOW`, or disconnects a client that does not support it. A session placeholder is claimed by the class, with the launch identity read from `/proc` for the client's PID. Keyboard focus follows the ICCCM input models: passive and locally active windows are focused when they map, globally active `NORMAL` and `DIALOG` windows are offered the focus with `WM_TAKE_FOCUS`, no-input windows are not focused, and a click focuses any of them; `_NET_ACTIVE_WINDOW` names the focused X window, and an X client cannot take the focus from another. Clients' `_NET_WM_STATE` (fullscreen, maximized, hidden), `WM_CHANGE_STATE` (iconify), `_NET_ACTIVE_WINDOW`, `_NET_WM_MOVERESIZE` and `_NET_CLOSE_WINDOW` requests work, and a state set before mapping applies on map. Transient windows and fixed-size windows float, a transient centred on its parent, as Wayfire's tile plugin decides. X stacking follows Shingle's. Menus, tooltips and other override-redirect windows are drawn above the Top layer at their X position, and take keyboard focus as Wayfire's unmanaged views do. X windows without a cursor show the theme's arrow. Scale, Expo, the switchers and session restore include X windows (XW-4 in [design/xwayland.md](design/xwayland.md)). The clipboard and the primary selection are shared between X and Wayland clients in both directions, large selections included (X's `INCR`): what an X client copies is offered to focused Wayland clients and to wlr-data-control clients such as `wl-paste` and clipboard managers, and X clients can paste what Wayland clients copy at any time, as under wlroots. `text/plain;charset=utf-8` is X's `UTF8_STRING` and `text/plain` its `TEXT` and `STRING`; other types keep their names. `behavior.disable_primary_selection` also stops the primary selection crossing between X and Wayland (XW-5). Drag and drop works between X and Wayland clients in both directions over Xdnd: a Wayland drag over an X window is offered to it if it is `XdndAware`, and an X client's drag is offered to the Wayland windows under the pointer once it names its types; each side hears whether the other accepts, with which action, and when the drop is done. An X client's drag icon (a `_NET_WM_WINDOW_TYPE_DND` window) is drawn above everything and takes no input. Drags between X windows stay in X. Touch drags are not bridged (XW-6). A Wayland client's drag icon (`wl_data_device.start_drag`'s icon surface) is drawn just below the cursor at the pointer, moved by the icon's surface offsets, takes no input and goes with the drop (WG-10).

### Sandboxed clients

`wp_security_context_manager_v1` accepts listener sockets from sandbox engines. Committed engine, app, and instance metadata is attached to every accepted client. Sandboxed clients cannot nest security contexts. Every advertised protocol global uses the same configurable visibility policy.

The `security_context.privileged_protocols` comma-separated list is imported from Wayfire and editable in WCM. Its defaults cover foreign-toplevel management, screencopy, data control, and keyboard-shortcut inhibition. Restrictions apply as each client is inserted; already-connected clients retain the globals they received.

### Surface opacity

`wp_alpha_modifier_v1` applies client-requested alpha multipliers on surface commit, including cached synchronized-subsurface commits. The multiplier ranges from zero (transparent) to `UINT32_MAX` (opaque) and is combined with Shingle's window-opacity setting. Destroying the modifier schedules a return to full alpha.

### Output configuration

Each Wayfire `[output:NAME]` section, i3 `output NAME` line or YAML `outputs` entry configures the output with that connector name: `mode` (`auto`, `off`, `highres`, `highrr`, `WxH` or `WxH@R`, as Wayfire reads it), position (`auto`, Wayfire's default, or a fixed point), scale, transform and `vrr`; `output` is the default. The configuration for several outputs is read: what happens to an unplugged output's windows (`output_layout.unplug`, applied since MO-2), Wayfire's `[oswitch]` and `[wsets]` bindings, `[input-device:NAME] output` and `[workarounds] remove_output_limits`. The compositor keeps several outputs, each with its own `wl_output`, xdg-output geometry and output-management head, placed by the configured or automatic positions; headless Shingle starts with `--outputs N`, and IPC creates and destroys headless outputs with `wayfire/create-headless-output` and `wayfire/destroy-headless-output` on every backend. Client protocols are per output: surfaces enter the outputs they are shown on and get their scales, layer-shell panels and lock surfaces are on their own outputs, screencopy (`grim -o`, `grim -g`) captures each output in its physical pixels, power and gamma are per output, and every output has its own ext-workspace group. One output is active, as in Wayfire: a click or touch on another output, focusing a window on it, Wayfire's `oswitch` bindings (`super+o` for the next output, `super+shift+o` taking the focused window along, and the previous, left, right, above and below outputs) and IPC that names an output change it, never pointer motion alone, and the pointer moves freely across the layout without leaving it. Windows move between outputs with Wayfire's rules (tiled ones join the target tree, floating ones keep their relative place, fullscreen and snapped ones are applied again there): by oswitch, `configure-view` with `output_id`, `start_on_output NAME` window rules, `set_fullscreen` on an output, a drag onto another output, and Wayfire's `wsets` bindings and IPC, which show a workspace set on the active output or send a window to one. Dialogs open on their parent's output, other new windows on the active output. The frontend draws one scene per output: each output shows its own windows, clipped to it, with its own frames, zoom and workspace slides; Scale and Expo run on the active output; a window being dragged shows on every output it crosses; Wayfire's "Workspace set N" label appears on an output that selects a set; and `[workarounds] remove_output_limits` draws windows on every output they overlap. The offscreen backend starts with `--outputs N`, each output at its own refresh rate. Nested Shingle opens a window per output, as a nested Wayfire does: `--outputs N` (else `WLR_WL_OUTPUTS`) starts it with `HEADLESS-1` to `HEADLESS-N`, each window sized, drawn and presented as its own output with its pointer and touch input on that output, the output taking the window's scale factor (also after the window moves to a monitor of another scale), so that its logical size is the host's and Shingle's cursor sits under the host's, `stipc/create_wayland_output` and `stipc/destroy_wayland_output` open and close more windows, and closing a window unplugs its output under `output_layout.unplug`; a window opened again takes back its output's name and windows. The native backend lights every connected monitor it has a CRTC for, each flipping at its own refresh rate, adds and removes outputs as monitors are plugged in and out, lists monitors it cannot light as disabled heads that `wlr-randr --on` can turn on, turns single monitors off through wlr-output-power and maps absolute input devices to `[input-device:NAME] output` (MO-7, not yet run on hardware); `--outputs N` lights at most N. The native backend lights the configured mode and follows monitors being plugged in and out. wlr-output-management clients such as `wlr-randr` see every mode of every output and can change it, test a configuration without applying it, set position, scale and transform, and turn outputs off and on (not yet native ones); on a native output a mode change passes a KMS `TEST_ONLY` commit first. Outputs a client configured keep that placement across config reloads until Shingle restarts, as in Wayfire. Turning every output off is refused; wlr-output-power-management (DPMS) does that. When the output is replaced by another connector, clients see the old `wl_output` removed and a new one announced, and windows stay on their workspaces.

### Cursor

The cursor is the theme's arrow where the compositor draws one (native), the surface a client set with `wl_pointer.set_cursor`, or the shape it chose with `wp_cursor_shape_v1`, drawn on the output the pointer is on and redrawn wherever it moves. On the native backend each monitor shows it on its CRTC's hardware cursor plane where there is one, so moving the pointer draws no frame; it is drawn into the frame instead on a zoomed or rotated monitor, with inverted colours or a gamma ramp, for a cursor whose buffer has a viewport, a transform, another scale than the monitor or is a DMA-BUF, for one larger than the plane, after the card refused the plane, and with `SHINGLE_CURSOR_PLANE=0` or wlroots' `WLR_NO_HARDWARE_CURSORS=1` (NB-10, not yet run on hardware). As with a hardware cursor in Wayfire, screencopy captures do not show a cursor that is on the plane.

### Presentation timing

`wp_tearing_control_v1` applies asynchronous or vertical-sync hints on surface commit. When any visible surface requests asynchronous presentation, the nested wgpu frontend selects Immediate mode if the host supports it. Otherwise it keeps FIFO presentation. On the native backend, frames of a monitor with such a surface flip with `PAGE_FLIP_ASYNC` where the driver takes atomic async flips, unless the same flip moves the cursor plane, and their feedback has no `vsync` flag; `SHINGLE_ASYNC_FLIPS=0` keeps every flip synchronized (NB-10, not yet run on hardware). Destroying the control schedules a return to vertical sync.

`wp_fifo_v1` supports refresh barriers and wait-constrained commits. As in wlroots, an update that waits for a set barrier is queued with its own barrier requests, never merged with another; each presented frame (`Compositor::frame()` or `frame_presented()`) applies the queued updates up to the first that sets the barrier again, in order and before frame callbacks. A queued update keeps its buffer, so a client that commits faster than the output refreshes waits for buffers instead of running ahead. Synchronized subsurfaces, invisible surfaces, and tearing-enabled surfaces bypass the barrier as permitted by the protocol. Destroying a FIFO object does not cancel state already submitted. A buffer replaced while it waits is released to its client, as is one held by a destroyed surface (DB-3 in [design/dmabuf.md](design/dmabuf.md#db-3-as-built)).

`wp_presentation` reports `CLOCK_MONOTONIC`. Nested feedback carries the time the frame was drawn, the refresh interval from the output's refresh rate (60 Hz by default), no sequence number and no flags; offscreen feedback carries the time the GPU finished the frame, the refresh interval and a frame counter, without flags; a backend passes page-flip timestamps, sequence numbers and flags to `Compositor::frame_presented`. The native backend's feedback carries the flip's time and vblank sequence with `vsync`, `hw_clock` and `hw_completion`, and `zero_copy` for a monitor whose frames are drawn straight into the buffers it scans out; it uses such GBM buffers wherever the plane, the renderer and GBM share a modifier, and copies frames into dumb buffers otherwise or with `SHINGLE_SCANOUT=copy`. Frames drawn into GBM buffers hand the plane their render fence (`IN_FENCE_FD`) instead of waiting for the GPU, unless `SHINGLE_IN_FENCE=0`. A monitor with `vrr` set that is VRR-capable runs with `VRR_ENABLED`, its feedback has a refresh of 0 as the protocol asks for outputs without a constant rate, and its output-management head reports adaptive sync enabled. Not yet run on hardware. Frame callbacks carry the same time in milliseconds, and feedback for surfaces that were not visible is discarded.

`wp_commit_timing_v1` accepts monotonic target timestamps. As in wlroots, each timed update waits in order with its own deadline, one refresh and 500 µs before its timestamp, so that it is applied for the frame that shows it; a timestamp closer than that is not waited for. Waiting updates are never merged, keep their buffers, are checked at each presented frame and drive redraw scheduling. Invalid or duplicate timestamps and destroyed surfaces use the protocol's error codes.

### GPU client buffers

`zwp_linux_dmabuf_v1` version 5, as Wayfire's wlroots offers it, lets GPU clients hand Shingle their buffers without a copy. It is offered when the renderer can import DMA-BUFs and has a render node to name: nested and native on a GPU, and `--backend offscreen`; not headless, not on lavapipe, and not with `SHINGLE_DMABUF=0`. Clients get one feedback tranche on the render node with every format and modifier the renderer samples (8-bit ARGB, XRGB, ABGR and XBGR with their single-plane modifiers). Buffers are imported when the client creates them, so a buffer the renderer cannot use is refused with `failed`, and a commit waits until the client's GPU has finished writing the buffer (implicit sync). A buffer goes back to its client only after the GPU has finished every frame that showed it. `wl_drm` is not offered, as Wayfire does not by default. `SHINGLE_TRACE_BUFFERS=1` logs which path each client buffer takes.

`wp_linux_drm_syncobj_manager_v1` version 1 adds explicit sync, which Wayfire does not offer: it is offered with linux-dmabuf when the render node supports timeline syncobjs and their eventfds, and not with `SHINGLE_SYNCOBJ=0`. NVIDIA's Vulkan WSI, Xwayland's glamor and Chromium use it when it is there; NVIDIA's EGL should too, which has not been verified yet. A commit waits for its acquire point instead of the buffer's implicit fences, only that surface waits, and the release point is signalled together with `wl_buffer.release`, once the GPU has finished every frame that showed the buffer.

### Resource limits

Shared-memory buffers go to the GPU straight from the client's pool, in its own byte order, with only the damaged rectangles written, and are given back to the client in the turn they were committed, before any frame; a pool sealed against shrinking stays mapped. The headless backend, which draws nothing, copies them on the CPU instead.

`behavior.max_buffer_size` limits either dimension of a shared-memory client buffer and defaults to 16384 pixels, matching Wayfire. Oversized buffers are rejected before mapping or GPU upload. The limit is also editable in WCM.
