# Wayfire gap survey

What Shingle still lacks compared with Wayfire for daily use, for the roadmap's [Remaining Wayfire compatibility](../roadmap.md#6-remaining-wayfire-compatibility) item. Written 2026-11-26 against Shingle `ade17fd` (the `master` this branch started from). Nothing was built or run for it: every Shingle finding comes from reading the code, and every Wayfire finding from reading Wayfire's source.

## Sources and method

Wayfire's behaviour is taken from its GitHub mirror at pinned commits:

| Repository | Commit | Date |
| --- | --- | --- |
| WayfireWM/wayfire | `669bbe3647e798898982a2ff2cb172b6f626c12b` | 2026-11-15 |
| WayfireWM/wf-config (INI parser) | `1a48992a483d3ae3de800b806f87abdaba994148` | 2026-10-18 |
| WayfireWM/wf-shell (panel, dock, background) | `4789c5bc890b0b28bb4ffde4b97427882267cd48` | 2026-11-21 |
| WayfireWM/wayfire tag `v0.9.0`, for comparison | `15065ca1a2e3a596db3dbcba64823c5dee90f191` | 2024-08-24 |

This host runs Debian's Wayfire 0.9.0, so items new since `v0.9.0` are marked where it matters. In what follows `wayfire:PATH:LINE` is a file of the pinned Wayfire commit, `wf-config:` and `wf-shell:` likewise. Shingle paths are relative to the repository; `wayfire.rs` alone means crates/shingle-config/src/wayfire.rs (the INI importer) and `config/lib.rs` means crates/shingle-config/src/lib.rs.

The plugin and option list is every `<plugin>` and `<object>` in wayfire:metadata/*.xml (50 files, 47 plugins and 2 objects, 332 options; `vk-color-management` is defined twice), plus the plugins without metadata in wayfire:plugins/ (`xkb-bindings`, `ipc`, `ipc-rules`, `session-lock` and the protocol plugins). For each option the importer (wayfire.rs:32-733) was read for an arm that imports it, and the YAML model, compositor and frontend for what uses the value; an option is **implemented** when it is imported and has Wayfire's effect, **partly** when it is imported but something is missing, and **missing** when nothing reads it. Options that only tune Wayfire's own internals (plugin unloading, its repaint scheduler, transactions) are classified missing with "no counterpart needed".

As a realistic test corpus, the importer was also read against the operator's own `~/.config/wayfire.ini`, which WCM wrote for Wayfire 0.9.0 and which loads the tiling plugin (`simple-tile`). Only its structure is used here, not its contents. `wayfire-plugins-extra` (for example `fisheye`, which was a core plugin in 0.9.0 and is gone from the pinned commit) is out of scope.

docs/status.md records `wayland-info` runs only as a count (48 globals) and a few names, not the full list, so the protocol comparison in [Wayland protocols](#wayland-protocols) uses the globals Shingle creates in code (crates/shingle/src/state.rs:660 `create_globals` and the modules it calls) instead.

## Ranked gaps

Ranked by how much a daily Wayfire user with the tiling plugin would notice them on switching to Shingle. **Blocking**: Shingle does not start with their configuration, or does something destructive they did not configure. **Annoying**: noticed every day, with a workaround. **Rare**: noticed by few users or seldom. The native backend not having run on hardware is the biggest blocker of all, but it is roadmap item 2 and not counted here.

### Blocking

**B1. The INI reader is not wf-config's, and a WCM-written `wayfire.ini` does not load.** WCM writes every colour with an escaped sharp, `\#RRGGBBAA` (wf-config:src/file.cpp:84 `remove_escaped_sharps`, and :481 when writing). Shingle's `strip_comment` (wayfire.rs:776) leaves the backslash in place, and `parse_color` (config/lib.rs:670) only accepts a value starting with `#` or four numbers, so `[core] background_color = \#101010FF` is an error, and the importer returns it (wayfire.rs:529) and Shingle refuses to start (crates/shingle-bin/src/main.rs:117). The operator's file has nine such colours that the importer reads (`core`, `expo`, `scale`, `scale-title-filter`, `simple-tile`, `vswitch`). Two smaller parser differences go with it: wf-config joins lines ending in `\` (wf-config:src/file.cpp:134 `join_lines`), which Wayfire's own wayfire.ini uses for `plugins = \` (wayfire:wayfire.ini:49), and Shingle reads only the first line; and wf-config treats every unescaped `#` as a comment (wf-config:src/file.cpp:109), where Shingle needs whitespace before it and keeps hexadecimal colours.

**B2. One value Shingle does not support rejects the whole file.** wf-config logs an option it cannot parse and keeps the default (wf-config:src/file.cpp:339, `OPTION_PARSED_INVALID_CONTENTS`), and Wayfire's matcher falls back to the option's default (wayfire:src/core/matcher.cpp:36). Shingle's importer returns the first error and its `validate` (config/lib.rs:413) rejects whole configurations, so Shingle does not start. Values a real `wayfire.ini` holds that do this:

- Wayfire's own default `[animate] enabled_for`, `(type equals "toplevel" | (type equals "x-or" & focusable equals true))` (wayfire:metadata/animate.xml:87), which WCM wrote into the operator's file: Shingle's matcher accepts a predicate only bare or with `is true|false` (crates/shingle-config/src/view_matcher.rs:294), so `focusable equals true` is a parse error.
- The animation names Wayfire has and Shingle lacks: `fire`, `zap`, `spin` and `squeezimize`, which is Wayfire's default `minimize_animation` (wayfire:metadata/animate.xml), fail `validate` (config/lib.rs:559-582).
- Bindings that are not a key or button: a modifier alone such as `<super>` (wayfire.rs:831 `binding` returns `ConfigError::Binding`) in any imported option. Touch gestures and hotspots (`swipe up 3`, `pinch in 4`, `edge-swipe ...`, `hotspot top-left 10x10 500`) are worse: they parse without an error, as a key named after their last word (`3`, `4`, `500`), so an activator such as `toggle = <super> KEY_P | pinch in 4` silently binds the key `4`.

**B3. Options the file leaves out take Shingle's defaults, not Wayfire's, and Shingle's extra bindings are always on.** The importer starts from `ShingleConfig::default()` (wayfire.rs:6) and only changes `xwayland.mode` to Wayfire's default (wayfire.rs:9). A file that does not mention an option gets Shingle's value where Wayfire uses its metadata default: a 10×1 workspace grid instead of 3×3 (`core/vwidth`, `vheight`), `super+shift+q` quitting the compositor instead of `ctrl+alt+backspace` (`core/exit`), `super+alt+shift+down` for `vswitch/with_win_down` (Wayfire: none), `zoom` instead of `squeezimize` for minimize, and `toggle_fullscreen` on `super+f`, `toggle_sticky` on `super+s`, `minimize` on `super+n`, `bring_to_front` on `super+b`, `send_to_back` on `super+shift+b` and `toggle_showdesktop` on `super+d`, which are `disabled` in Wayfire (wayfire:metadata/wm-actions.xml). On top of these, Shingle's own actions with no Wayfire option are bound whatever the file says (data/shingle.yaml): moving tiles with `super+shift+h/j/k/l`, resizing with `super+ctrl+h/j/k/l`, workspaces 1 to 10 with `super+1`..`super+0` and `super+shift+1`..`0`, the last workspace with `super+grave`, reload with `super+shift+r` and lock with `super+f8`. These shadow the applications' own shortcuts, and an unexpected quit loses work. Shingle's commands are matched before its built-in bindings (crates/shingle-frontend/src/frontend/bindings.rs:810), so a key the user bound to a command still runs it.

**B4. `core/plugins` is ignored, so plugins the user did not load are active.** Wayfire loads only the listed plugins; the operator's list has 17 and leaves out, among others, `animate`, `scale`, `grid`, `zoom`, `alpha`, `invert`, `wrot`, `resize`, `idle` and `place`. Shingle reads the list only for `preserve-output` (wayfire.rs:645) and runs everything it has, so for that user `super+p` opens Scale, `super+left/right/up/down` snap windows with `grid`'s bindings (which WCM wrote although the plugin is off), `super+i` inverts the screen, `super`+scroll zooms, `super+alt`+scroll changes opacity, windows animate, and `[idle] screensaver_timeout` applies. Without `simple-tile` in the list Wayfire does not tile at all, while Shingle always does. This is ranked blocking with B3 because together they mean Shingle under a Wayfire configuration does not behave like that Wayfire.

### Annoying

**A1. No numeric workspace bindings from `[vswitch]`.** Wayfire's `workspace_bindings`, `workspace_bindings_win` and `bindings_win` dynamic lists (`binding_N`, `with_win_N`, `send_win_N`, wayfire:metadata/vswitch.xml:83-108) are how Wayfire users bind workspace numbers; the importer has only the directional and `last` names (wayfire.rs:558-606), so `binding_3 = <super> KEY_3` is dropped and Shingle's own `super+N` defaults remain (B3).

**A2. No touchpad workspace swipe (`vswipe`).** None of vswipe's 12 options is read and nothing turns pointer swipes into workspace motion; swipes only go to clients (crates/shingle-frontend/src/frontend.rs:1238). The operator loads `vswipe`; every laptop user of Wayfire's default setup notices.

**A3. No `wayfire-shell` protocol.** wf-shell's panel and dock (Wayfire's default shell, started by `autostart_wf_shell`, default true) bind `zwf_shell_manager_v2` (wf-shell:src/util/wf-shell-app.cpp:168) for autohide hotspots, hiding while a view is fullscreen and the `toggle_menu` event, and say "Compositor does not support zwf_shell_manager_v2" without it (wf-shell:src/util/wf-autohide-window.cpp:81). Shingle has no such global (no `zwf_` in crates/), no `wayfire-shell/toggle_menu` activator or IPC method, no modifier-only bindings to trigger it (`toggle_menu = <super>`, with `input/modifier_binding_timeout`), and ignores `autostart_wf_shell` (wayfire.rs:609), so `wf-panel` and `wf-background` are not started either.

**A4. No server-side decorations.** Wayfire's `decoration` plugin draws a title bar with title, minimize, maximize and close buttons and a border for views that want server-side decorations, X11 windows without their own frame among them (wayfire:plugins/decor/decoration.cpp). Shingle can tell clients to use server-side decorations (`preferred_decoration_mode: server`, crates/shingle/src/decoration.rs:22) but draws nothing: there is no decoration code in shingle-frontend or shingle-render. Floating X11 windows and SSD-only Wayland clients have no title bar or border and can only be moved with `super`+drag. None of the 14 options is read.

**A5. The Alt+Tab switcher has no view.** Wayfire's `switcher` shows the views as rotated thumbnails while cycling (`speed`, `view_thumbnail_scale`, `view_thumbnail_rotation`, wayfire:plugins/single_plugins/switcher.cpp); Shingle's regular switcher only changes focus and opacity (crates/shingle-frontend/src/frontend/switcher.rs:12, `SwitcherKind::Regular`).

**A6. The client environment lacks Wayfire's variables.** Wayfire sets `XDG_CURRENT_DESKTOP=wayfire` when it is unset (wayfire:src/main.cpp:313), which xdg-desktop-portal uses to pick `wayfire-portals.conf` (wlr for screen casting, wayfire:wayfire-portals.conf), sets `XCURSOR_THEME` and `XCURSOR_SIZE` from `[input] cursor_theme` and `cursor_size` (wayfire:src/core/seat/cursor.cpp:116), and `WAYFIRE_CONFIG_FILE` (wayfire:src/default-config-backend.cpp:66), which WCM and scripts read. Shingle's client environment has none of them (crates/shingle-session/src/environment.rs:42): clients draw their own cursors in the toolkit default, and portals started from Shingle choose their backend from whatever the login set. There is no session `.desktop` file for display managers (Wayfire installs wayfire:wayfire.desktop).

### Rare

| Gap | Who notices | Evidence |
| --- | --- | --- |
| Animations: `fire`, `zap`, `spin`, `squeezimize`, per-type `fade_enabled_for`, `zoom_enabled_for`, `fire_enabled_for` and their durations, `startup_duration`, `spin_rotations`, `fire_*`; only `fade` and `zoom` exist | everyone, cosmetically | config/lib.rs:1267, crates/shingle-frontend/src/frontend/animation.rs:154 |
| `blur` (18 options) | users of translucent terminals | no `blur` in crates/ |
| `wobbly`, `cube` (with `idle`'s cube screensaver and `cube/*` IPC) and wrot's 3D mode | users of the effects | no `wobbly` or `cube` in crates/ except crates/shingle-idle/src/lib.rs:16 |
| `zwp_text_input_manager_v1` (`input-method-v1/enable_text_input_v1`), which Chromium and Electron use for IME by default | IME users | wayfire:plugins/protocols/input-method-v1.cpp:526; crates/shingle/src/text_input.rs:44 is v3 only |
| Touch-screen gesture and hotspot activators (`swipe`, `edge-swipe`, `pinch`, `hotspot`) | touch-screen and hot-corner users | B2; no gesture recogniser in crates/shingle-frontend |
| `zwp_tablet_manager_v2`; tablets are ignored at the libinput level | tablet users (for them, blocking) | crates/shingle-native/src/udev.rs:358 |
| Per-device input sections `[input:NAME]`, matched by udev `ID_PATH`, `ID_SERIAL`, `LIBINPUT_DEVICE_GROUP` or name, for pointer and keyboard options | users with several mice or keyboards | wayfire:src/core/plugin.cpp:47, :157 of src/core/seat/keyboard.cpp; only `[input-device:NAME] output` is read (wayfire.rs:702) |
| Input options `modifier_binding_timeout`, `touchpad_scroll_speed`, `3fg_drag`, `gesture_sensitivity`, `gesture_finger_count`, `tablet_motion_mode`, `[input-device] calibration` | laptop and touch-screen users | no arm in wayfire.rs:194-261 |
| `ext_data_control_manager_v1`, `ext_image_copy_capture_manager_v1` and `ext_output_image_capture_source_manager_v1` (new since 0.9.0); clients fall back to the wlr protocols Shingle has | clipboard managers, xdg-desktop-portal-wlr, wf-shell's window-list previews | wayfire:src/core/core.cpp:149, :160, :161 |
| Screencopy has no DMA-BUF frames or damage (`copy_with_damage` copies everything) | screen recording and casting cost | crates/shingle/src/screencopy.rs:580 |
| `gtk_shell1` (`gtk-shell`, `global_menu_bar`) and `org_kde_kwin_appmenu_manager` (`kde-appmenu`) | GTK3 app menus, global menus | wayfire:plugins/protocols/gtk-shell.cpp:393, kde-appmenu.cpp:104 |
| `org_kde_kwin_server_decoration_manager`, `zxdg_exporter_v1`/`zxdg_importer_v1` (GTK3's xdg-foreign), `wp_drm_lease_device_v1` (VR), `zwlr_export_dmabuf_manager_v1` (wlrobs), `wp_color_manager_v1` and `wp_color_representation_manager_v1` (HDR, new since 0.9.0) | the clients named | [Wayland protocols](#wayland-protocols) |
| Output options `depth`, `hdr`, `icc_profile`, `vrr_idle_refresh_rate`, `min_render_budget`, and `mode = mirror ...` (Shingle turns the output off instead, wayfire.rs:750) | presenters (mirror), HDR and colour-managed setups | wayfire.rs:741-773 |
| `wayfire/get-config-option` and `list-config-options` know 18 options (crates/shingle/src/ipc.rs:2614); Wayfire answers every option | scripts and WCM reading options | wayfire:plugins/ipc-rules/ipc-utility-methods.hpp:37 |
| IPC methods `wayfire/configuration`, `reload-config-metadata`, `reload-plugins`, `render-metrics`, `grid/slot_*` and `grid/restore`, `cube/*`, `wayfire-shell/toggle_menu`, `stipc/tablet/*`, `stipc/delay_next_tx` | scripts and Wayfire's own tests | [IPC methods and events](#ipc-methods-and-events) |
| `[workarounds] use_external_output_configuration`, `app_id_mode`, `enable_input_method_v2` (Shingle always offers input-method v2 with text-input v3; Wayfire only with this option), `force_xwayland_scaling`, `use_native_buffer_size` (XW-7) | kanshi users, Steam and GTK app-id quirks, X11 HiDPI | no arm in wayfire.rs:481-527 |
| `xdg_toplevel` `suspended` state and `wm_capabilities` (Wayfire's xdg-shell is version 6 with `suspended`, wayfire:src/view/xdg-shell.cpp:487) | clients throttling when hidden | crates/shingle/src/shell.rs:158 has no `Suspended` |
| `scale-title-filter/share_filter` still has no effect, though several outputs exist now | multi-monitor Scale users | config/lib.rs:1238 |
| Cosmetic options of implemented plugins: `move/preview_*`, `grid/duration` and `type`, `switcher/speed` | few | rows of [Plugins and options](#plugins-and-options) |
| Override-redirect X windows are not in `list-views` (Wayfire lists them as `x-or`) | X11 scripting | docs/ipc.md, views and outputs |

## Plugins and options

Every plugin and object in wayfire:metadata/ and every plugin in wayfire:plugins/ without metadata, alphabetically. Line numbers in the evidence column are wayfire.rs unless a path is given.

### alpha

| Option | Status | Evidence |
| --- | --- | --- |
| `modifier`, `min_value` | implemented | wayfire.rs:412-415; crates/shingle-frontend/src/frontend/effects.rs:70; IPC `wf/alpha/*` |

### animate

| Option | Status | Evidence |
| --- | --- | --- |
| `open_animation`, `close_animation`, `minimize_animation` | partly: only `none`, `fade` and `zoom`; `fire`, `zap`, `spin` and `squeezimize` (the minimize default) are rejected (B2) | 391-399; config/lib.rs:559-582 |
| `enabled_for` | partly: imported, but Wayfire's default text does not parse (B2) | 388; crates/shingle-config/src/view_matcher.rs:294 |
| `duration` | implemented (easing curve ignored) | 400; crates/shingle-frontend/src/frontend/animation.rs:154 |
| `startup_duration`, `fade_enabled_for`, `fade_duration`, `zoom_enabled_for`, `zoom_duration`, `fire_enabled_for`, `fire_duration`, `fire_particles`, `fire_particle_size`, `random_fire_color`, `fire_color`, `squeezimize_duration`, `zap_duration`, `spin_duration`, `spin_rotations` | missing | no arm; config/lib.rs:1272 `AnimateConfig` has none of them |

### autostart

| Option | Status | Evidence |
| --- | --- | --- |
| `autostart` (dynamic list) | implemented | 610 |
| `autostart_wf_shell` | missing: Wayfire starts `wf-panel` and `wf-background` unless an entry already does (wayfire:plugins/single_plugins/autostart.cpp) | 609 ignores it |

### blur

| Option | Status | Evidence |
| --- | --- | --- |
| `blur_by_default`, `toggle`, `method`, `saturation`, `alpha_threshold`, `alpha_exponent`, `box_offset`, `box_degrade`, `box_iterations`, `gaussian_offset`, `gaussian_degrade`, `gaussian_iterations`, `kawase_offset`, `kawase_degrade`, `kawase_iterations`, `bokeh_offset`, `bokeh_degrade`, `bokeh_iterations` | missing | no `blur` in crates/ |

### command

| Option | Status | Evidence |
| --- | --- | --- |
| `bindings`, `repeatable_bindings`, `always_bindings`, `release_bindings` (`binding_N`/`repeatable_binding_N`/`always_binding_N`/`release_binding_N` with `command_N`) | implemented for keys and buttons; partly for activators that are gestures, hotspots or a bare modifier (B2) | 622-644, 722-734; crates/shingle-frontend/src/frontend/bindings.rs:252 |

### core

| Option | Status | Evidence |
| --- | --- | --- |
| `plugins` | partly: only `preserve-output` is honoured (B4) | 645 |
| `close_top_view`, `exit` | implemented | 538, 547 |
| `vwidth`, `vheight` | implemented (default differs, B3) | 597, 600 |
| `background_color` | implemented, but `\#` values fail (B1) | 528 |
| `preferred_decoration_mode` | partly: `server` is announced but nothing is drawn (A4) | 508; crates/shingle/src/decoration.rs:22 |
| `xwayland`, `xwayland_startup_script` | implemented | 511, 519 |
| `focus_button_with_modifiers`, `focus_buttons`, `focus_buttons_passthrough` | implemented | 531-537; crates/shingle/src/input.rs:1080 |
| `max_render_time`, `transaction_timeout` | missing, no counterpart needed: Wayfire's repaint delay and transaction timeout; Shingle has neither mechanism | no arm |

### cube

| Option | Status | Evidence |
| --- | --- | --- |
| `activate`, `rotate_left`, `rotate_right`, `deform`, `light`, `zoom`, `initial_animation`, `speed_zoom`, `speed_spin_horiz`, `speed_spin_vert`, `background_mode`, `background`, `skydome_mirror`, `skydome_texture`, `cubemap_image` | missing | no `cube` in crates/ |

### decoration

| Option | Status | Evidence |
| --- | --- | --- |
| `font`, `font_color`, `font_scale`, `title_halign`, `title_valign`, `title_height`, `border_size`, `button_order`, `button_scale`, `button_padding`, `active_color`, `inactive_color`, `ignore_views`, `forced_views` | missing (A4) | no arm; no decoration drawing in crates/shingle-frontend or crates/shingle-render |

### expo

| Option | Status | Evidence |
| --- | --- | --- |
| `toggle`, `background`, `duration`, `offset`, `keyboard_interaction`, `inactive_brightness`, `transition_length`, `workspace_bindings` (`select_workspace_N`) | implemented | 290-320; crates/shingle-frontend/src/frontend/expo.rs:358, crates/shingle-frontend/src/frontend/scene.rs:725 |

### ext-toplevel, ext-workspace, foreign-toplevel, session-lock (no options)

| Plugin | Status | Evidence |
| --- | --- | --- |
| `ext-toplevel` | implemented | crates/shingle/src/ext_toplevel.rs:33 |
| `ext-workspace` | implemented (no create or remove, as Wayfire) | crates/shingle/src/ext_workspace.rs:63 |
| `foreign-toplevel` | implemented | crates/shingle/src/foreign_toplevel.rs:53 |
| `session-lock` | implemented | crates/shingle/src/shell.rs:221 |

### extra-gestures

| Option | Status | Evidence |
| --- | --- | --- |
| `move_fingers`, `move_delay`, `close_fingers` | implemented | 185-193; crates/shingle-frontend/src/extra_gestures.rs:89 |

### fast-switcher

| Option | Status | Evidence |
| --- | --- | --- |
| `activate`, `activate_backward`, `inactive_alpha` | implemented | 403-411; crates/shingle-frontend/src/frontend/switcher.rs:12 |

### grid

| Option | Status | Evidence |
| --- | --- | --- |
| `slot_bl`, `slot_b`, `slot_br`, `slot_l`, `slot_c`, `slot_r`, `slot_tl`, `slot_t`, `slot_tr`, `restore` | implemented as bindings; partly: no `grid/slot_*` or `grid/restore` IPC methods, which Wayfire registers (wayfire:plugins/grid/grid.cpp:58, :105) | 159-184; crates/shingle-frontend/src/frontend/bindings.rs:586 |
| `duration`, `type` | missing: snaps use the tiling animation duration, no crossfade or wobbly choice | no arm |

### gtk-shell

| Option | Status | Evidence |
| --- | --- | --- |
| `global_menu_bar` (and the `gtk_shell1` global) | missing | no `gtk_shell` in crates/ |

### idle

| Option | Status | Evidence |
| --- | --- | --- |
| `toggle`, `dpms_timeout`, `disable_on_fullscreen`, `disable_initially` | implemented | 265, 275-287; crates/shingle-idle/src/lib.rs:9 |
| `screensaver_timeout` | partly: drives Shingle's idle state; there is no cube screensaver | 281; crates/shingle-idle/src/lib.rs:16 |
| `cube_zoom_speed`, `cube_rotate_speed`, `cube_max_zoom` | missing | no arm |

### input

| Option | Status | Evidence |
| --- | --- | --- |
| `xkb_layout`, `xkb_file`, `xkb_model`, `xkb_options`, `xkb_rules`, `xkb_variant`, `kb_repeat_delay`, `kb_repeat_rate`, `kb_numlock_default_state`, `kb_capslock_default_state` | implemented | 194-211; crates/shingle/src/lib.rs:433 |
| `left_handed_mode`, `middle_emulation`, `mouse_accel_profile`, `mouse_cursor_speed`, `mouse_natural_scroll`, `touchpad_accel_profile`, `touchpad_cursor_speed`, `tap_to_click`, `click_method`, `scroll_method`, `disable_touchpad_while_typing`, `disable_touchpad_while_mouse`, `natural_scroll`, `tap_and_drag`, `drag_lock` | implemented in the native backend (not run on hardware); nested leaves them to the host | 215-257; crates/shingle-native/src/input.rs:257, :315 |
| `mouse_scroll_speed` | partly: applied to every scroll; Wayfire uses `touchpad_scroll_speed` for finger scrolling | 212; crates/shingle/src/lib.rs:1801 |
| `cursor_theme`, `cursor_size` | partly: Shingle's own cursor only; not exported as `XCURSOR_*` (A6) | 258-261; crates/shingle/src/cursor_shape.rs:49 |
| `modifier_binding_timeout`, `3fg_drag`, `touchpad_scroll_speed`, `gesture_sensitivity`, `gesture_finger_count`, `tablet_motion_mode` | missing | no arm |

### input-device (object)

| Option | Status | Evidence |
| --- | --- | --- |
| `output` | implemented | 702-711; crates/shingle-config/src/multi_output.rs:156 |
| `calibration` | missing | no arm |

### input-method-v1

| Option | Status | Evidence |
| --- | --- | --- |
| `enable_text_input_v3` | implemented (also gates input-method v1 and v2 and the input panel) | 321; crates/shingle/src/state.rs:687 |
| `enable_text_input_v1` | missing: no `zwp_text_input_manager_v1` | no arm |

### invert

| Option | Status | Evidence |
| --- | --- | --- |
| `toggle`, `preserve_hue` | implemented | 416-421; crates/shingle-frontend/src/frontend/session.rs:112 |

### ipc, ipc-rules (no options)

| Plugin | Status | Evidence |
| --- | --- | --- |
| `ipc` | implemented, enabled by `--ipc` rather than the plugin list | crates/shingle/src/ipc.rs:382 |
| `ipc-rules` | partly: see [IPC methods and events](#ipc-methods-and-events) | crates/shingle/src/ipc.rs:2998 |

### kde-appmenu (no options)

Missing: no `org_kde_kwin_appmenu_manager` (wayfire:plugins/protocols/kde-appmenu.cpp:104).

### move

| Option | Status | Evidence |
| --- | --- | --- |
| `activate`, `enable_snap`, `snap_threshold`, `quarter_snap_threshold`, `enable_snap_off`, `snap_off_threshold`, `join_views`, `workspace_switch_after` | implemented | 146, 460-480; crates/shingle/src/shell.rs:3038, crates/shingle/src/lib.rs:2798, :2889 |
| `preview_base_color`, `preview_base_border`, `preview_border_width` | missing: the snap preview uses `simple-tile`'s colours | no arm |

### oswitch

| Option | Status | Evidence |
| --- | --- | --- |
| `next_output`, `next_output_with_win`, `prev_output`, `prev_output_with_win`, `left_output`, `right_output`, `above_output`, `below_output` | implemented | 660-683; crates/shingle/src/output/bindings.rs:28 |

### output (object)

| Option | Status | Evidence |
| --- | --- | --- |
| `mode`, `position`, `scale`, `transform`, `vrr` | implemented, except `mode = mirror ...`, which turns the output off (MO-8 is later) | 712-718, 741-773 |
| `min_render_budget`, `vrr_idle_refresh_rate`, `hdr`, `depth`, `icc_profile` | missing | 771 ignores them |

### place

| Option | Status | Evidence |
| --- | --- | --- |
| `mode` | implemented (`center`, `cascade`, `maximize`, `random`) | 450; crates/shingle/src/placement.rs:74 |

### preserve-output

| Option | Status | Evidence |
| --- | --- | --- |
| `last_output_focus_timeout` | implemented | 656; the plugin itself via `plugins`, 645 |

### resize

| Option | Status | Evidence |
| --- | --- | --- |
| `activate`, `min_width`, `min_height`, `corner_threshold`, `activate_preserve_aspect` | implemented | 143-158; crates/shingle/src/input.rs:802, crates/shingle-frontend/src/frontend/bindings.rs:118, :149 |

### scale

| Option | Status | Evidence |
| --- | --- | --- |
| `toggle`, `toggle_all`, `duration`, `allow_zoom`, `middle_click_close`, `include_minimized`, `close_on_new_view`, `spacing`, `outer_margin`, `inactive_alpha`, `minimized_alpha`, `title_overlay`, `title_font_size`, `title_position`, `bg_color`, `text_color` | implemented (colours fail when escaped, B1) | 324-387; crates/shingle-frontend/src/scale.rs:89 |

### scale-title-filter

| Option | Status | Evidence |
| --- | --- | --- |
| `case_sensitive`, `overlay`, `font_size`, `bg_color`, `text_color` | implemented | 346-363 |
| `share_filter` | partly: imported, no effect | 349; config/lib.rs:1238 |

### security-context-v1

| Option | Status | Evidence |
| --- | --- | --- |
| `privileged_protocols` | implemented | 498; crates/shingle/src/security_context.rs:109 |

### shortcuts-inhibit

| Option | Status | Evidence |
| --- | --- | --- |
| `inhibit_by_default`, `ignore_views`, `break_grab` | implemented | 266-274; crates/shingle/src/shortcuts.rs:74, :105 |

### simple-tile

| Option | Status | Evidence |
| --- | --- | --- |
| `tile_by_default`, `keep_fullscreen_on_adjacent`, `button_move`, `button_resize`, `key_toggle`, `key_focus_left`, `key_focus_right`, `key_focus_above`, `key_focus_below`, `key_toggle_maximized`, `inner_gap_size`, `outer_horiz_gap_size`, `outer_vert_gap_size`, `animation_duration`, `preview_base_color`, `preview_base_border`, `preview_border_width` | implemented (colours fail when escaped, B1); `get-layout`, `set-layout` and `set-show-maximized` over IPC | 33-79, 113, 140-145; crates/shingle-core/src/tree.rs, crates/shingle/src/state.rs:153 |

Shingle also reads `key_move_*` and `key_resize_*` from `[simple-tile]` (116-137); they are not Wayfire options.

### switcher

| Option | Status | Evidence |
| --- | --- | --- |
| `next_view`, `prev_view` | partly: focus cycles without the thumbnail view (A5) | 80-85; crates/shingle-frontend/src/frontend/switcher.rs:12 |
| `speed`, `view_thumbnail_scale`, `view_thumbnail_rotation` | missing | no arm |

### vk-color-management (no options)

Missing, Vulkan renderer only and new since 0.9.0 (wayfire:plugins/single_plugins/vk-color-management.cpp); goes with `wp_color_manager_v1`.

### vswipe

| Option | Status | Evidence |
| --- | --- | --- |
| `enable_horizontal`, `enable_vertical`, `enable_free_movement`, `enable_smooth_transition`, `duration`, `background`, `fingers`, `gap`, `threshold`, `delta_threshold`, `speed_factor`, `speed_cap` | missing (A2) | no `vswipe` in crates/ |

### vswitch

| Option | Status | Evidence |
| --- | --- | --- |
| `binding_left`, `binding_right`, `binding_up`, `binding_down`, `binding_last`, `with_win_left`, `with_win_right`, `with_win_up`, `with_win_down`, `with_win_last`, `send_win_left`, `send_win_right`, `send_win_up`, `send_win_down`, `send_win_last`, `duration`, `gap`, `background`, `wraparound` | implemented | 548-606; crates/shingle-frontend/src/workspace_transition.rs:22 |
| `workspace_bindings`, `workspace_bindings_win`, `bindings_win` (`binding_N`, `with_win_N`, `send_win_N`) | missing (A1) | no arm for the numbered keys |

### wayfire-shell

| Option | Status | Evidence |
| --- | --- | --- |
| `toggle_menu` (and the `zwf_shell_manager_v2` global) | missing (A3) | no `zwf_` or `toggle_menu` in crates/ |

### window-rules

| Option | Status | Evidence |
| --- | --- | --- |
| `rules` | implemented: every event (`created`, `maximized`, `unmaximized`, `minimized`, `fullscreened`) and action of wayfire:plugins/window-rules/view-action-interface.cpp | 619; crates/shingle-config/src/window_rule.rs:160, crates/shingle/src/window_rules.rs:6 |

### wm-actions

| Option | Status | Evidence |
| --- | --- | --- |
| `toggle_always_on_top`, `toggle_always_on_bottom`, `toggle_fullscreen`, `toggle_sticky`, `toggle_maximize`, `minimize`, `toggle_showdesktop`, `send_to_back`, `bring_to_front` | implemented (defaults differ, B3) | 86-112 |

### wobbly

| Option | Status | Evidence |
| --- | --- | --- |
| `friction`, `spring_k`, `grid_resolution` | missing | no `wobbly` in crates/ |

### workarounds

| Option | Status | Evidence |
| --- | --- | --- |
| `all_dialogs_modal`, `keep_last_toplevel_activated`, `discard_command_output`, `max_buffer_size`, `force_preferred_decoration_mode`, `disable_primary_selection`, `focus_main_surface_instead_of_popup`, `auto_reload_config`, `config_reload_delay`, `remove_output_limits` | implemented | 481-507, 522-527, 699 |
| `app_id_mode`, `use_external_output_configuration`, `enable_input_method_v2`, `force_xwayland_scaling`, `use_native_buffer_size` | missing | no arm |
| `dynamic_repaint_delay`, `enable_so_unloading`, `enable_opaque_region_damage_optimizations`, `force_frame_sync` | missing, no counterpart needed (Wayfire's scheduler, plugin loading and damage tracking) | no arm |

### wrot

| Option | Status | Evidence |
| --- | --- | --- |
| `activate`, `reset`, `reset-one`, `reset_radius` | implemented | 422-433; crates/shingle-frontend/src/frontend.rs:132 |
| `activate-3d`, `sensitivity`, `invert` | missing: no 3D mode | no arm |

### wsets

| Option | Status | Evidence |
| --- | --- | --- |
| `wsets_bindings`, `send_window_bindings`, `label_duration` | implemented | 684-698; crates/shingle/src/output/wsets.rs:16 |

### xdg-activation

| Option | Status | Evidence |
| --- | --- | --- |
| `check_surface`, `only_last_request`, `timeout` | implemented | 451-459; crates/shingle/src/activation.rs:108 |

### xkb-bindings (no metadata)

Implemented as `xkb:` command bindings, matched against Shingle's own keymap (wayfire.rs:853, docs/features.md#xkb-symbol-commands).

### zoom

| Option | Status | Evidence |
| --- | --- | --- |
| `modifier`, `speed`, `smoothing_duration`, `interpolation_method` | implemented | 434-449; crates/shingle-frontend/src/frontend/effects.rs:85 |

## Wayland protocols

What the pinned Wayfire advertises and Shingle does not. Globals that only wlroots provides and Wayfire does not create (content type, system bell, toplevel tags, transient seats) are not counted.

| Global | Wayfire | Shingle | Rank |
| --- | --- | --- | --- |
| `zwf_shell_manager_v2` (version 3) | wayfire:plugins/protocols/wayfire-shell.cpp:586 | missing | annoying (A3) |
| `zwp_text_input_manager_v1` | wayfire:plugins/protocols/input-method-v1.cpp:528, with `enable_text_input_v1` | missing | rare |
| `zwp_tablet_manager_v2` | wayfire:src/core/core.cpp:155 | missing | rare |
| `ext_data_control_manager_v1` | wayfire:src/core/core.cpp:149 (new since 0.9.0) | missing; `zwlr_data_control_manager_v1` v2 (crates/shingle/src/data_control.rs:87) | rare |
| `ext_image_copy_capture_manager_v1`, `ext_output_image_capture_source_manager_v1` | wayfire:src/core/core.cpp:160-161 (new since 0.9.0) | missing; `zwlr_screencopy_manager_v1` v3, shm only (crates/shingle/src/screencopy.rs:250) | rare |
| `zwlr_export_dmabuf_manager_v1` | wayfire:src/core/core.cpp:163 | missing | rare |
| `wp_drm_lease_device_v1` | wayfire:src/core/core.cpp:166 | missing | rare |
| `org_kde_kwin_server_decoration_manager` | wayfire:src/core/core.cpp:197 | missing; `zxdg_decoration_manager_v1` (crates/shingle/src/decoration.rs:49) | rare |
| `zxdg_exporter_v1`, `zxdg_importer_v1` | wayfire:src/core/core.cpp:250 | missing; v2 only (crates/shingle/src/foreign.rs:35) | rare |
| `wp_color_representation_manager_v1` | wayfire:src/core/core.cpp:258 (new since 0.9.0) | missing | rare |
| `wp_color_manager_v1` | wayfire:src/core/core.cpp:284, when the renderer can (new since 0.9.0) | missing | rare |
| `gtk_shell1` | wayfire:plugins/protocols/gtk-shell.cpp:393 | missing | rare |
| `org_kde_kwin_appmenu_manager` | wayfire:plugins/protocols/kde-appmenu.cpp:104 (new since 0.9.0) | missing | rare |
| `wl_drm` | only with `--legacy-wl-drm` (wayfire:src/core/core.cpp:131) | missing on purpose (docs/design/dmabuf.md) | none |

Every other global Wayfire advertises Shingle has too, at the same or a later version: compositor, subcompositor, shm, linux-dmabuf, linux-drm-syncobj, data device, primary selection, wlr data control, xdg-shell, layer shell (Wayfire 4, Shingle 5), xdg-output, wlr output management and power, gamma, screencopy, idle notify and inhibit, xdg-decoration, virtual keyboard and pointer, pointer gestures, relative pointer and constraints, input-method v2 and text-input v3, input-method v1 and input panel, presentation, viewporter, xdg-foreign v2, fractional scale, single-pixel buffer, cursor shape (Wayfire 1, Shingle 2), keyboard-shortcuts inhibit, xdg-activation, session lock, security context, wlr foreign toplevel, ext-foreign-toplevel-list, ext-workspace and xwayland-shell. Shingle also has globals Wayfire lacks: alpha modifier, tearing control, FIFO, commit timing, pointer warp, xdg-dialog and xdg-toplevel-icon.

## IPC methods and events

Wayfire's methods are those its plugins register (`register_method` and `ipc_activator_t`) at the pinned commit. Shingle's are `SUPPORTED_METHODS` (crates/shingle/src/ipc.rs:382).

| Methods Wayfire has and Shingle lacks | Wayfire | Rank |
| --- | --- | --- |
| `wayfire/configuration`, `wayfire/reload-config-metadata`, `wayfire/reload-plugins`, `wayfire/render-metrics` | wayfire:plugins/ipc-rules/ipc-utility-methods.hpp:34-42 | rare |
| `grid/slot_bl`, `slot_b`, `slot_br`, `slot_l`, `slot_c`, `slot_r`, `slot_tl`, `slot_t`, `slot_tr`, `grid/restore` | wayfire:plugins/grid/grid.cpp:58, :105 | rare |
| `cube/activate`, `cube/rotate_left`, `cube/rotate_right` | wayfire:plugins/cube/cube.cpp:809-811 | rare |
| `wayfire-shell/toggle_menu` | wayfire:plugins/protocols/wayfire-shell.cpp:603 | annoying, with A3 |
| `stipc/tablet/tool_proximity`, `tool_button`, `tool_axis`, `tool_tip`, `pad_button`, `stipc/delay_next_tx` | wayfire:plugins/ipc/stipc.cpp:302-307 | rare (test input) |

Partial: `wayfire/get-config-option` and `list-config-options` cover 18 options (crates/shingle/src/ipc.rs:2614), and `list-views` leaves out override-redirect X windows. Every event of wayfire:plugins/ipc-rules/ipc-events.hpp:134-157, `view-pre-map` and `command-binding` are in `KNOWN_EVENTS` (crates/shingle/src/ipc.rs:470), and every field Wayfire's `view_to_json` writes (wayfire:plugins/ipc-rules/ipc-rules-common.hpp) appears in Shingle's view JSON. Shingle's `wm-actions/toggle_*`, `wm-actions/minimize`, the `vswitch/binding_*`, `with_win_*` and `send_win_*` activators and `invert/toggle` are Shingle's own; the pinned Wayfire registers only `wm-actions/toggle_showdesktop` among them.

## Implementation plan

The workspaces are ordered by rank and then by cost, and each can be finished and verified on its own. Every workspace also follows the repository rules: unit tests for new code, clippy clean, 80 columns, docs/status.md, docs/roadmap.md, docs/ipc.md and docs/features.md updated, real-client results recorded apart from unit tests, and signed commits. Real-client checks run in the existing harness namespaces, which never touch the operator's session: tools/output_test/run.py (headless), tools/gpu_test/run.py (offscreen and nested on the render node) and tools/xwayland_test/run.py (X11). Wayfire's behaviour is checked against the pinned commit above, which each workspace may move forward if it records the new one here.

### WG-1 `shingle-wayfire-ini-parser`: read what wf-config reads, and never refuse a Wayfire file

Make the INI reader follow wf-config:src/file.cpp: unescaped `#` starts a comment, `\#` is a literal `#`, a line ending in `\` continues on the next, whitespace is trimmed as wf-config trims it. Split the importer's errors into fatal ones (the file cannot be read) and per-option ones: an option whose value Shingle cannot use keeps its default and is reported with its line, as wf-config does, and the import succeeds. YAML stays strict. Teach the view matcher Wayfire's `PREDICATE equals true|false` (and `is`), and `type` values `x-or`, `unmanaged`, `background`, `panel`, `overlay`. Unknown animation names fall back as decided in [Questions](#questions-for-the-operator). Parse activators fully: keys and buttons as today, and a bare modifier, touch gestures and hotspots into binding kinds of their own that are recognised, reported as not yet supported and never matched as keys.

Acceptance:

- Unit tests: each wf-config rule above; Wayfire's stock wayfire:wayfire.ini at the pinned commit imports with no warning except unsupported plugins' options; a fixture written the way WCM writes (every section, `\#` colours, `400ms circle` durations, Wayfire's default `enabled_for`, `toggle_menu = <super>`, a `pinch in 4` activator) imports with the expected warnings and values; `squeezimize`, `fire`, `zap` and `spin` warn and fall back; the old silent `4` binding from `pinch in 4` is gone.
- `shingle-wcm --check` prints the warnings with line numbers and exits 0 for both files, and non-zero for an unreadable file.
- Real run: headless Shingle in tools/output_test/run.py's namespace starts with `--config` pointing at the WCM-style fixture, logs the warnings once, and `wayfire/get-config-option` returns the imported `core/background_color` and `simple-tile/inner_gap_size`.

### WG-2 `shingle-wayfire-defaults`: Wayfire's defaults and plugin list under a Wayfire file

Generate a table of every Wayfire option with its type and default from wayfire:metadata/*.xml at the pinned commit, with a Python tool under tools/ and the result checked in, and start the Wayfire import from those defaults rather than `ShingleConfig::default()`. Under a Wayfire file, Shingle-only bindings with no Wayfire option are off unless the operator decides otherwise. Honour `core/plugins` for every plugin Shingle implements: an unlisted plugin's bindings, effects and protocols are off, as when Wayfire does not load it; `simple-tile` missing means nothing tiles; IPC stays governed by `--ipc`. Import the numbered `[vswitch]` lists (`binding_N`, `with_win_N`, `send_win_N`, A1) and `autostart_wf_shell` with Wayfire's rule (start `wf-panel` and `wf-background` unless an autostart entry names them). `wayfire/get-config-option` and `list-config-options` answer every option of the table, with its live value where Shingle has one and its imported text otherwise.

Acceptance:

- Unit tests: the generated table matches the metadata (the tool's own Python tests read a sample of the XML); an empty `wayfire.ini` gives Wayfire's defaults, including a 3×3 grid, `ctrl+alt+backspace` to exit and no `super+shift+q`, `super+s`, `super+d`, `super+n` or `super+1` binding; a `plugins` line without `scale`, `zoom`, `grid` or `animate` turns those off and one without `simple-tile` stops tiling; `binding_3 = <super> KEY_3` switches to workspace 3; autostart's `wf-panel` rule in its three cases.
- Real run in tools/gpu_test/run.py's namespace with offscreen Shingle, input through STIPC: with `scale` unlisted, `feed_key` `super+p` reaches a `wl_probe` client and no `plugin-activation-state-changed` event follows; with it listed the event follows and the client gets no key. With `simple-tile` unlisted a second `wl_probe` is not tiled (`simple-tile/get-layout` is empty).
- Real run: stand-in `wf-panel` and `wf-background` scripts on the namespace's `PATH` are started once each with `autostart_wf_shell = true`, and not when an autostart entry runs them.
- `list-config-options` lists every option of the table; the method count in docs/ipc.md stays factual.

### WG-3 `shingle-client-environment`: Wayfire's variables for children

Add `XDG_CURRENT_DESKTOP` when it is unset (the value is a question below), `XCURSOR_THEME` and `XCURSOR_SIZE` from `[input]`, and `WAYFIRE_CONFIG_FILE` naming the configuration in use, all through `ClientEnvironment::new` (crates/shingle-session/src/environment.rs:42); install a session file `data/shingle.desktop` for display managers and, if the operator wants one, a portals configuration as Wayfire's wayfire-portals.conf.

Acceptance:

- Unit tests for each variable, set and unset, and for a configuration reload changing the cursor variables for later children.
- Real run: headless Shingle in tools/output_test/run.py's namespace with `--ipc`; `stipc/run` of `env` records every variable as expected, and the host's `XDG_CURRENT_DESKTOP` is kept when set.

### WG-4 `shingle-wayfire-shell`: `zwf_shell_manager_v2` and the menu key

Add `zwf_shell_manager_v2` version 3 from wayfire:proto/wayfire-shell-unstable-v2.xml: `zwf_output_v2` with `enter_fullscreen` and `leave_fullscreen` for the output's fullscreen views, `inhibit_output` and `inhibit_output_done` (an inhibited output draws nothing new), hotspots by edge and custom rectangle with Wayfire's threshold and timeout and `proximity_changed`, `toggle_menu`, and `zwf_surface_v2.interactive_move`, following wayfire:plugins/protocols/wayfire-shell.cpp. Add modifier-only bindings with `input/modifier_binding_timeout` (a bare modifier pressed and released alone within the timeout, wayfire:src/core/seat/keyboard.cpp:409), the `wayfire-shell/toggle_menu` activator and its IPC method. The global is privileged like foreign-toplevel only if the operator says so.

Acceptance:

- Unit tests through the in-process test client for every request and event, hotspot timing, fullscreen enter and leave across outputs, and the modifier timeout (pressed alone, with another key, held too long).
- Real run in tools/output_test/run.py's namespace with headless Shingle and `--outputs 2`, a raw-protocol Python client of the protocol: `stipc/move_cursor` to an edge fires `enter` after the timeout and `leave` when moved away; a `wl_probe` going fullscreen on one output sends `enter_fullscreen` to that output only; STIPC `feed_key` of `KEY_LEFTMETA` alone sends `toggle_menu`, and with another key in between does not; `interactive_move` starts a move that STIPC button and motion finish.
- `wayland-info` lists the global; `list-methods` counts the new method.

### WG-5 `shingle-vswipe`: touchpad workspace swipes

Port wayfire:plugins/single_plugins/vswipe.cpp and vswipe-processing.hpp: a swipe with `fingers` fingers drags the workspace wall horizontally, vertically or freely, with `threshold`, `delta_threshold`, `speed_factor`, `speed_cap`, `gap`, `background`, `duration` and smooth transition, reusing vswitch's transition (crates/shingle-frontend/src/workspace_transition.rs) and taking the swipe from clients only when it starts with the configured finger count.

Acceptance:

- Frontend input-script tests (crates/shingle-frontend/src/frontend/tests.rs, `FakeHost`) for each option: direction locking, thresholds, a cancelled swipe returning, smooth and immediate commits, the grid's edges with and without wraparound, and swipes with other finger counts reaching clients.
- Real run in tools/gpu_test/run.py's namespace with offscreen Shingle, swipes fed through a test-only method (a question below): `wset-workspace-changed` after a long swipe, none after a short one, and `grim` captures of the half-way wall.
- Host checks with a real touchpad are recorded under [Needs operator: host input](../status.md#needs-operator-host-input).

### WG-6 `shingle-decoration`: server-side decorations

Draw Wayfire's decoration (wayfire:plugins/decor/) for xdg toplevels in server-side mode and for X11 windows that ask for a frame (`_MOTIF_WM_HINTS`): a title bar of `title_height` with the title in `font`, `font_color`, `font_scale` and alignment, `button_order` buttons with their scale and padding, a `border_size` border in `active_color` or `inactive_color`, and `ignore_views` and `forced_views`. Dragging the title bar moves the window, its buttons act, the border resizes. Geometry, IPC and sessions keep describing the client's window as Wayfire's do (the frame is outside the view's geometry). Text uses the rasterizer Scale's titles use (crates/shingle-frontend/src/scale_title.rs:48).

Acceptance:

- Unit tests for the layout (title and button rectangles, alignment, border), hit testing and the matcher lists, and pinned pixels in crates/shingle-render/tests/offscreen_pixels.rs.
- Real run in tools/gpu_test/run.py's namespace with offscreen Shingle: a `wl_probe` asking for server-side decorations is drawn with a frame in a `grim` capture, a client-side one without; STIPC clicks on close, maximize and minimize do what they say; a title-bar drag moves the window.
- Real run in tools/xwayland_test/run.py's namespace: `xterm` gets a frame, an X window with `_MOTIF_WM_HINTS` decorations off does not.

### WG-7 `shingle-switcher-view`: Wayfire's switcher

Port wayfire:plugins/single_plugins/switcher.cpp's view: thumbnails of the views around the selected one, scaled by `view_thumbnail_scale` and rotated by `view_thumbnail_rotation`, moving at `speed`, drawn by the frontend on the active output and never written to the tiling tree.

Acceptance:

- Frontend input-script tests for selection order, forward and backward, release on modifier up, a view closing while the switcher runs, and one view only.
- Real run in tools/gpu_test/run.py's namespace with offscreen Shingle: `grim` captures while STIPC holds `alt` and taps `tab` show the thumbnails in Wayfire's arrangement, and releasing `alt` focuses the selected view.

### Later

Not planned in detail, in rank order: the remaining animations and per-type animation options; `zwp_text_input_manager_v1`; `blur`; per-device `[input:NAME]` sections and the missing input options; `ext_data_control_manager_v1` and the ext image-capture protocols with DMA-BUF and damage in screencopy; `zwp_tablet_manager_v2` with libinput tablets in the native backend; `wobbly`; `cube` with the idle screensaver and wrot's 3D mode; the remaining IPC methods; the rare protocols. Each needs its own design when it comes up.

## Questions for the operator

Each has a proposed default, which the workspaces above assume.

1. **Invalid values in a Wayfire file.** Keep the default and warn, as wf-config does, instead of refusing to start? Proposed: yes, for Wayfire and i3 imports only; YAML stays strict.
2. **Defaults under a Wayfire file.** Start from Wayfire's metadata defaults rather than Shingle's? Proposed: yes.
3. **Shingle-only bindings under a Wayfire file** (move and resize tiles with `super+shift`/`super+ctrl` and `h/j/k/l`, `super+1`..`0`, `super+grave`, reload, lock). Proposed: off unless set in YAML or as `[simple-tile] key_move_*` and the other Shingle names the importer already reads.
4. **`core/plugins`.** Honour it for every plugin Shingle implements, including `simple-tile` (nothing tiles without it), but not for `ipc`, which `--ipc` governs? Proposed: yes.
5. **Unsupported animation names.** Proposed: `squeezimize` falls back to `zoom` (both shrink the view), `fire`, `zap` and `spin` to `fade`, each with one warning.
6. **`XDG_CURRENT_DESKTOP`.** Proposed: set it to `wayfire` when unset, exactly as Wayfire does, so `wayfire-portals.conf` and scripts testing for Wayfire keep working; Shingle installs no portals file of its own.
7. **A test-only input method for touchpad gestures.** Wayfire's STIPC has none. Proposed: a Shingle-only `stipc/feed_gesture` (swipe and pinch with fingers, deltas and end or cancel), listed in `list-methods` and documented as Shingle's, like the `xwayland-ready` event.
8. **`zwf_shell_manager_v2` and security contexts.** Proposed: not privileged, as in Wayfire, whose default `privileged_protocols` does not name it.
9. **Scope.** `wayfire-plugins-extra` (fisheye and the rest) stays out of this survey. Proposed: yes; a separate survey if wanted.
