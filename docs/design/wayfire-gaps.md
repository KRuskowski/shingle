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
| wlroots, as the pinned Wayfire's `subprojects/wlroots` (WG-8 to WG-10) | `d783533489e1f75d6886c2ab5c5960090ef268f8` | 2026-07-07 |

This host runs Debian's Wayfire 0.9.0, so items new since `v0.9.0` are marked where it matters. In what follows `wayfire:PATH:LINE` is a file of the pinned Wayfire commit, `wf-config:` and `wf-shell:` likewise. Shingle paths are relative to the repository; `wayfire.rs` alone means crates/shingle-config/src/wayfire.rs (the INI importer) and `config/lib.rs` means crates/shingle-config/src/lib.rs.

The plugin and option list is every `<plugin>` and `<object>` in wayfire:metadata/*.xml (50 files, 47 plugins and 2 objects, 332 options; `vk-color-management` is defined twice), plus the plugins without metadata in wayfire:plugins/ (`xkb-bindings`, `ipc`, `ipc-rules`, `session-lock` and the protocol plugins). For each option the importer (wayfire.rs:32-733) was read for an arm that imports it, and the YAML model, compositor and frontend for what uses the value; an option is **implemented** when it is imported and has Wayfire's effect, **partly** when it is imported but something is missing, and **missing** when nothing reads it. Options that only tune Wayfire's own internals (plugin unloading, its repaint scheduler, transactions) are classified missing with "no counterpart needed".

As a realistic test corpus, the importer was also read against the operator's own `~/.config/wayfire.ini`, which WCM wrote for Wayfire 0.9.0 and which loads the tiling plugin (`simple-tile`). Only its structure is used here, not its contents. `wayfire-plugins-extra` (for example `fisheye`, which was a core plugin in 0.9.0 and is gone from the pinned commit) is out of scope.

docs/status.md records `wayland-info` runs only as a count (48 globals) and a few names, not the full list, so the protocol comparison in [Wayland protocols](#wayland-protocols) uses the globals Shingle creates in code (crates/shingle/src/state.rs:660 `create_globals` and the modules it calls) instead.

## Ranked gaps

Ranked by how much a daily Wayfire user with the tiling plugin would notice them on switching to Shingle. **Blocking**: Shingle does not start with their configuration, or does something destructive they did not configure. **Annoying**: noticed every day, with a workaround. **Rare**: noticed by few users or seldom. The native backend not having run on hardware is the biggest blocker of all, but it is roadmap item 2 and not counted here.

### Blocking

**B1. The INI reader is not wf-config's, and a WCM-written `wayfire.ini` does not load.** (Fixed by WG-1, see [WG-1 as built](#wg-1-as-built).) WCM writes every colour with an escaped sharp, `\#RRGGBBAA` (wf-config:src/file.cpp:84 `remove_escaped_sharps`, and :481 when writing). Shingle's `strip_comment` (wayfire.rs:776) leaves the backslash in place, and `parse_color` (config/lib.rs:670) only accepts a value starting with `#` or four numbers, so `[core] background_color = \#101010FF` is an error, and the importer returns it (wayfire.rs:529) and Shingle refuses to start (crates/shingle-bin/src/main.rs:117). The operator's file has nine such colours that the importer reads (`core`, `expo`, `scale`, `scale-title-filter`, `simple-tile`, `vswitch`). Two smaller parser differences go with it: wf-config joins lines ending in `\` (wf-config:src/file.cpp:134 `join_lines`), which Wayfire's own wayfire.ini uses for `plugins = \` (wayfire:wayfire.ini:49), and Shingle reads only the first line; and wf-config treats every unescaped `#` as a comment (wf-config:src/file.cpp:109), where Shingle needs whitespace before it and keeps hexadecimal colours.

**B2. One value Shingle does not support rejects the whole file.** (Fixed by WG-1, see [WG-1 as built](#wg-1-as-built).) wf-config logs an option it cannot parse and keeps the default (wf-config:src/file.cpp:339, `OPTION_PARSED_INVALID_CONTENTS`), and Wayfire's matcher falls back to the option's default (wayfire:src/core/matcher.cpp:36). Shingle's importer returns the first error and its `validate` (config/lib.rs:413) rejects whole configurations, so Shingle does not start. Values a real `wayfire.ini` holds that do this:

- Wayfire's own default `[animate] enabled_for`, `(type equals "toplevel" | (type equals "x-or" & focusable equals true))` (wayfire:metadata/animate.xml:87), which WCM wrote into the operator's file: Shingle's matcher accepts a predicate only bare or with `is true|false` (crates/shingle-config/src/view_matcher.rs:294), so `focusable equals true` is a parse error.
- The animation names Wayfire has and Shingle lacks: `fire`, `zap`, `spin` and `squeezimize`, which is Wayfire's default `minimize_animation` (wayfire:metadata/animate.xml), fail `validate` (config/lib.rs:559-582).
- Bindings that are not a key or button: a modifier alone such as `<super>` (wayfire.rs:831 `binding` returns `ConfigError::Binding`) in any imported option. Touch gestures and hotspots (`swipe up 3`, `pinch in 4`, `edge-swipe ...`, `hotspot top-left 10x10 500`) are worse: they parse without an error, as a key named after their last word (`3`, `4`, `500`), so an activator such as `toggle = <super> KEY_P | pinch in 4` silently binds the key `4`.

**B3. Options the file leaves out take Shingle's defaults, not Wayfire's, and Shingle's extra bindings are always on.** (Fixed by WG-2, see [WG-2 as built](#wg-2-as-built).) The importer starts from `ShingleConfig::default()` (wayfire.rs:6) and only changes `xwayland.mode` to Wayfire's default (wayfire.rs:9). A file that does not mention an option gets Shingle's value where Wayfire uses its metadata default: a 10×1 workspace grid instead of 3×3 (`core/vwidth`, `vheight`), `super+shift+q` quitting the compositor instead of `ctrl+alt+backspace` (`core/exit`), `super+alt+shift+down` for `vswitch/with_win_down` (Wayfire: none), `zoom` instead of `squeezimize` for minimize, and `toggle_fullscreen` on `super+f`, `toggle_sticky` on `super+s`, `minimize` on `super+n`, `bring_to_front` on `super+b`, `send_to_back` on `super+shift+b` and `toggle_showdesktop` on `super+d`, which are `disabled` in Wayfire (wayfire:metadata/wm-actions.xml). On top of these, Shingle's own actions with no Wayfire option are bound whatever the file says (data/shingle.yaml): moving tiles with `super+shift+h/j/k/l`, resizing with `super+ctrl+h/j/k/l`, workspaces 1 to 10 with `super+1`..`super+0` and `super+shift+1`..`0`, the last workspace with `super+grave`, reload with `super+shift+r` and lock with `super+f8`. These shadow the applications' own shortcuts, and an unexpected quit loses work. Shingle's commands are matched before its built-in bindings (crates/shingle-frontend/src/frontend/bindings.rs:810), so a key the user bound to a command still runs it.

**B4. `core/plugins` is ignored, so plugins the user did not load are active.** (Fixed by WG-2, see [WG-2 as built](#wg-2-as-built).) Wayfire loads only the listed plugins; the operator's list has 17 and leaves out, among others, `animate`, `scale`, `grid`, `zoom`, `alpha`, `invert`, `wrot`, `resize`, `idle` and `place`. Shingle reads the list only for `preserve-output` (wayfire.rs:645) and runs everything it has, so for that user `super+p` opens Scale, `super+left/right/up/down` snap windows with `grid`'s bindings (which WCM wrote although the plugin is off), `super+i` inverts the screen, `super`+scroll zooms, `super+alt`+scroll changes opacity, windows animate, and `[idle] screensaver_timeout` applies. Without `simple-tile` in the list Wayfire does not tile at all, while Shingle always does. This is ranked blocking with B3 because together they mean Shingle under a Wayfire configuration does not behave like that Wayfire.

### Annoying

**A1. No numeric workspace bindings from `[vswitch]`.** (Fixed by WG-2, see [WG-2 as built](#wg-2-as-built).) Wayfire's `workspace_bindings`, `workspace_bindings_win` and `bindings_win` dynamic lists (`binding_N`, `with_win_N`, `send_win_N`, wayfire:metadata/vswitch.xml:83-108) are how Wayfire users bind workspace numbers; the importer has only the directional and `last` names (wayfire.rs:558-606), so `binding_3 = <super> KEY_3` is dropped and Shingle's own `super+N` defaults remain (B3).

**A2. No touchpad workspace swipe (`vswipe`).** (Fixed by WG-5, see [WG-5 as built](#wg-5-as-built).) None of vswipe's 12 options is read and nothing turns pointer swipes into workspace motion; swipes only go to clients (crates/shingle-frontend/src/frontend.rs:1238). The operator loads `vswipe`; every laptop user of Wayfire's default setup notices.

**A3. No `wayfire-shell` protocol.** (Fixed by WG-4, see [WG-4 as built](#wg-4-as-built); `autostart_wf_shell` by WG-2.) wf-shell's panel and dock (Wayfire's default shell, started by `autostart_wf_shell`, default true) bind `zwf_shell_manager_v2` (wf-shell:src/util/wf-shell-app.cpp:168) for autohide hotspots, hiding while a view is fullscreen and the `toggle_menu` event, and say "Compositor does not support zwf_shell_manager_v2" without it (wf-shell:src/util/wf-autohide-window.cpp:81). Shingle has no such global (no `zwf_` in crates/), no `wayfire-shell/toggle_menu` activator or IPC method, no modifier-only bindings to trigger it (`toggle_menu = <super>`, with `input/modifier_binding_timeout`), and ignores `autostart_wf_shell` (wayfire.rs:609), so `wf-panel` and `wf-background` are not started either.

**A4. No server-side decorations.** (Fixed by WG-6, see [WG-6 as built](#wg-6-as-built).) Wayfire's `decoration` plugin draws a title bar with title, minimize, maximize and close buttons and a border for views that want server-side decorations, X11 windows without their own frame among them (wayfire:plugins/decor/decoration.cpp). Shingle can tell clients to use server-side decorations (`preferred_decoration_mode: server`, crates/shingle/src/decoration.rs:22) but draws nothing: there is no decoration code in shingle-frontend or shingle-render. Floating X11 windows and SSD-only Wayland clients have no title bar or border and can only be moved with `super`+drag. None of the 14 options is read.

**A5. The Alt+Tab switcher has no view.** (Fixed by WG-7, see [WG-7 as built](#wg-7-as-built).) Wayfire's `switcher` shows the views as rotated thumbnails while cycling (`speed`, `view_thumbnail_scale`, `view_thumbnail_rotation`, wayfire:plugins/single_plugins/switcher.cpp); Shingle's regular switcher only changes focus and opacity (crates/shingle-frontend/src/frontend/switcher.rs:12, `SwitcherKind::Regular`).

**A6. The client environment lacks Wayfire's variables.** (Fixed by WG-3, see [WG-3 as built](#wg-3-as-built).) Wayfire sets `XDG_CURRENT_DESKTOP=wayfire` when it is unset (wayfire:src/main.cpp:313), which xdg-desktop-portal uses to pick `wayfire-portals.conf` (wlr for screen casting, wayfire:wayfire-portals.conf), sets `XCURSOR_THEME` and `XCURSOR_SIZE` from `[input] cursor_theme` and `cursor_size` (wayfire:src/core/seat/cursor.cpp:116), and `WAYFIRE_CONFIG_FILE` (wayfire:src/default-config-backend.cpp:66), which WCM and scripts read. Shingle's client environment has none of them (crates/shingle-session/src/environment.rs:42): clients draw their own cursors in the toolkit default, and portals started from Shingle choose their backend from whatever the login set. There is no session `.desktop` file for display managers (Wayfire installs wayfire:wayfire.desktop).

### Rare

| Gap | Who notices | Evidence |
| --- | --- | --- |
| Animations: `fire`, `zap`, `spin`, `squeezimize`, per-type `fade_enabled_for`, `zoom_enabled_for`, `fire_enabled_for` and their durations, `startup_duration`, `spin_rotations`, `fire_*`; only `fade` and `zoom` exist (done since `shingle-animate`, see [animate as built](#animate-as-built)) | everyone, cosmetically | crates/shingle-frontend/src/animate.rs |
| `blur` (18 options) | users of translucent terminals | no `blur` in crates/ |
| `wobbly`, `cube` (with `idle`'s cube screensaver and `cube/*` IPC) and wrot's 3D mode | users of the effects | no `wobbly` or `cube` in crates/ except crates/shingle-idle/src/lib.rs:16 |
| `zwp_text_input_manager_v1` (`input-method-v1/enable_text_input_v1`), which Chromium and Electron use for IME by default (done since WG-8, see [WG-8 as built](#wg-8-as-built)) | IME users | wayfire:plugins/protocols/input-method-v1.cpp:526; crates/shingle/src/text_input/v1.rs |
| Touch-screen gesture and hotspot activators (`swipe`, `edge-swipe`, `pinch`, `hotspot`) | touch-screen and hot-corner users | B2; no gesture recogniser in crates/shingle-frontend |
| `zwp_tablet_manager_v2`; tablets are ignored at the libinput level | tablet users (for them, blocking) | crates/shingle-native/src/udev.rs:358 |
| Per-device input sections `[input:NAME]`, matched by udev `ID_PATH`, `ID_SERIAL`, `LIBINPUT_DEVICE_GROUP` or name, for pointer and keyboard options (libinput options and `[input-device:NAME] calibration` since WG-9, see [WG-9 as built](#wg-9-as-built); keyboard options per device are still missing) | users with several mice or keyboards | wayfire:src/core/plugin.cpp:47, :157 of src/core/seat/keyboard.cpp; crates/shingle-config/src/input_devices.rs |
| Input options `touchpad_scroll_speed`, `3fg_drag`, `gesture_sensitivity`, `gesture_finger_count`, `tablet_motion_mode`, `[input-device] calibration` | laptop and touch-screen users | no arm in wayfire.rs:194-261 |
| `ext_data_control_manager_v1`, `ext_image_copy_capture_manager_v1` and `ext_output_image_capture_source_manager_v1` (new since 0.9.0); clients fall back to the wlr protocols Shingle has | clipboard managers, xdg-desktop-portal-wlr, wf-shell's window-list previews | wayfire:src/core/core.cpp:149, :160, :161 |
| Screencopy has no DMA-BUF frames or damage (`copy_with_damage` copies everything) | screen recording and casting cost | crates/shingle/src/screencopy.rs:580 |
| `gtk_shell1` (`gtk-shell`, `global_menu_bar`) and `org_kde_kwin_appmenu_manager` (`kde-appmenu`) | GTK3 app menus, global menus | wayfire:plugins/protocols/gtk-shell.cpp:393, kde-appmenu.cpp:104 |
| `wp_drm_lease_device_v1` (VR), `zwlr_export_dmabuf_manager_v1` (wlrobs), `wp_color_manager_v1` and `wp_color_representation_manager_v1` (HDR, new since 0.9.0) | the clients named | [Wayland protocols](#wayland-protocols) |
| Output options `depth`, `hdr`, `icc_profile`, `vrr_idle_refresh_rate`, `min_render_budget`, and `mode = mirror ...` (Shingle turns the output off instead, wayfire.rs:750) | presenters (mirror), HDR and colour-managed setups | wayfire.rs:741-773 |
| IPC methods `wayfire/configuration`, `reload-config-metadata`, `reload-plugins`, `render-metrics`, `grid/slot_*` and `grid/restore`, `cube/*`, `stipc/tablet/*`, `stipc/delay_next_tx` | scripts and Wayfire's own tests | [IPC methods and events](#ipc-methods-and-events) |
| `[workarounds] use_external_output_configuration`, `app_id_mode`, `enable_input_method_v2` (Shingle always offers input-method v2 with text-input v3; Wayfire only with this option), `force_xwayland_scaling`, `use_native_buffer_size` (XW-7) | kanshi users, Steam and GTK app-id quirks, X11 HiDPI | no arm in wayfire.rs:481-527 |
| `xdg_toplevel` `suspended` state and `wm_capabilities` (Wayfire's xdg-shell is version 6 with `suspended`, wayfire:src/view/xdg-shell.cpp:487) | clients throttling when hidden | crates/shingle/src/shell.rs:158 has no `Suspended` |
| `scale-title-filter/share_filter` still has no effect, though several outputs exist now | multi-monitor Scale users | config/lib.rs:1238 |
| Cosmetic options of implemented plugins: `grid/duration` and `type` (`switcher/speed` since WG-7, `move/preview_*` since `shingle-workspace-followups`) | few | rows of [Plugins and options](#plugins-and-options) |
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
| `open_animation`, `close_animation`, `minimize_animation` | implemented since `shingle-animate`: `none`, `fade`, `zoom`, `fire`, `zap`, `spin` and `squeezimize` (WG-1 made the last four fall back to `fade` and `zoom`) | [animate as built](#animate-as-built) |
| `enabled_for` | implemented since WG-1, which parses Wayfire's default text (B2 before) | 388; crates/shingle-config/src/view_matcher.rs:294 |
| `duration` and every other `*_duration` | implemented with their easing curves since `shingle-animate` | crates/shingle-config/src/easing.rs |
| `startup_duration`, `fade_enabled_for`, `fade_duration`, `zoom_enabled_for`, `zoom_duration`, `fire_enabled_for`, `fire_duration`, `fire_particles`, `fire_particle_size`, `random_fire_color`, `fire_color`, `squeezimize_duration`, `zap_duration`, `spin_duration`, `spin_rotations` | implemented since `shingle-animate` | [animate as built](#animate-as-built) |

### autostart

| Option | Status | Evidence |
| --- | --- | --- |
| `autostart` (dynamic list) | implemented | 610 |
| `autostart_wf_shell` | implemented since WG-2: `wf-panel` and `wf-background` are added to the autostart commands unless an entry names them, as wayfire:plugins/single_plugins/autostart.cpp does | WG-2 as built |

### blur

| Option | Status | Evidence |
| --- | --- | --- |
| `blur_by_default`, `toggle`, `method`, `saturation`, `alpha_threshold`, `alpha_exponent`, `box_offset`, `box_degrade`, `box_iterations`, `gaussian_offset`, `gaussian_degrade`, `gaussian_iterations`, `kawase_offset`, `kawase_degrade`, `kawase_iterations`, `bokeh_offset`, `bokeh_degrade`, `bokeh_iterations` | missing | no `blur` in crates/ |

### command

| Option | Status | Evidence |
| --- | --- | --- |
| `bindings`, `repeatable_bindings`, `always_bindings`, `release_bindings` (`binding_N`/`repeatable_binding_N`/`always_binding_N`/`release_binding_N` with `command_N`) | implemented for keys, buttons and, since WG-4, a modifier alone; gestures and hotspots are recognised and warned about since WG-1, and never bound (B2 before) | 622-644, 722-734; crates/shingle-frontend/src/frontend/bindings.rs:252 |

### core

| Option | Status | Evidence |
| --- | --- | --- |
| `plugins` | implemented since WG-2 for every plugin Shingle has, except `ipc` and `ipc-rules`, which `--ipc` governs (B4 before) | WG-2 as built |
| `close_top_view`, `exit` | implemented | 538, 547 |
| `vwidth`, `vheight` | implemented; Wayfire's 3×3 default since WG-2 (B3 before) | 597, 600 |
| `background_color` | implemented; `\#` values since WG-1 (B1 before) | 528 |
| `preferred_decoration_mode` | implemented; since WG-6 a server-side window gets a frame (A4 before) | 508; crates/shingle/src/decoration.rs |
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
| `font_color`, `font_scale`, `title_halign`, `title_valign`, `title_height`, `border_size`, `button_order`, `button_scale`, `button_padding`, `active_color`, `inactive_color`, `ignore_views`, `forced_views` | implemented since WG-6 (A4 before) | `DecorationConfig`; crates/shingle/src/frame.rs, crates/shingle/src/decorator.rs, crates/shingle-frontend/src/frame_raster.rs |
| `font` | partly: imported and kept for IPC; every title is drawn in Shingle's bundled Inter | WG-6 as built |

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
| `cursor_theme`, `cursor_size` | implemented: Shingle's own cursor, and since WG-3 exported as `XCURSOR_THEME` and `XCURSOR_SIZE` to every child (A6 before) | 258-261; crates/shingle/src/cursor_shape.rs:49, crates/shingle-session/src/environment.rs (`with_desktop`) |
| `modifier_binding_timeout` | implemented since WG-4, for `wayfire-shell/toggle_menu` and commands | crates/shingle/src/modifier_binding.rs |
| `3fg_drag`, `touchpad_scroll_speed`, `gesture_sensitivity`, `gesture_finger_count`, `tablet_motion_mode` | missing | no arm |
| the libinput options above in `[input:NAME]` | implemented since WG-9 in the native backend, matched as Wayfire matches them ([WG-9 as built](#wg-9-as-built)); the keyboard, scroll speed and cursor options warn per device | crates/shingle-config/src/input_devices.rs, crates/shingle-native/src/input.rs (`DeviceOptions`) |

### input-device (object)

| Option | Status | Evidence |
| --- | --- | --- |
| `output` | implemented; since WG-9 also found by udev property | crates/shingle-config/src/input_devices.rs |
| `calibration` | implemented since WG-9 for touch devices in the native backend | crates/shingle-config/src/input_devices.rs (`parse_calibration`) |

### input-method-v1

| Option | Status | Evidence |
| --- | --- | --- |
| `enable_text_input_v3` | implemented; since WG-8 input-method v1 and v2 and the input panel exist while it or `enable_text_input_v1` is on | crates/shingle/src/state.rs (`create_globals`) |
| `enable_text_input_v1` | implemented since WG-8 ([WG-8 as built](#wg-8-as-built)) | crates/shingle/src/text_input/v1.rs |

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
| `preview_base_color`, `preview_base_border`, `preview_border_width` | implemented since `shingle-workspace-followups`: the snap preview has move's own colours, the drop preview simple-tile's (the snap preview used simple-tile's before) | `GridConfig`; crates/shingle-frontend/src/frontend/scene.rs (`append_drop_preview`) |

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
| `toggle`, `toggle_all`, `duration`, `allow_zoom`, `middle_click_close`, `include_minimized`, `close_on_new_view`, `spacing`, `outer_margin`, `inactive_alpha`, `minimized_alpha`, `title_overlay`, `title_font_size`, `title_position`, `bg_color`, `text_color` | implemented (escaped colours since WG-1, B1 before) | 324-387; crates/shingle-frontend/src/scale.rs:89 |

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
| `tile_by_default`, `keep_fullscreen_on_adjacent`, `button_move`, `button_resize`, `key_toggle`, `key_focus_left`, `key_focus_right`, `key_focus_above`, `key_focus_below`, `key_toggle_maximized`, `inner_gap_size`, `outer_horiz_gap_size`, `outer_vert_gap_size`, `animation_duration`, `preview_base_color`, `preview_base_border`, `preview_border_width` | implemented (escaped colours since WG-1, B1 before); `get-layout`, `set-layout` and `set-show-maximized` over IPC | 33-79, 113, 140-145; crates/shingle-core/src/tree.rs, crates/shingle/src/state.rs:153 |

Shingle also reads `key_move_*` and `key_resize_*` from `[simple-tile]` (116-137); they are not Wayfire options.

### switcher

| Option | Status | Evidence |
| --- | --- | --- |
| `next_view`, `prev_view` | implemented since WG-7, with Wayfire's thumbnails (A5 before) | 80-85; crates/shingle-frontend/src/switcher_view.rs, crates/shingle-frontend/src/frontend/switcher.rs |
| `speed`, `view_thumbnail_scale`, `view_thumbnail_rotation` | implemented since WG-7; `speed`'s easing curve is ignored | `SwitcherConfig` |

### vk-color-management (no options)

Missing, Vulkan renderer only and new since 0.9.0 (wayfire:plugins/single_plugins/vk-color-management.cpp); goes with `wp_color_manager_v1`.

### vswipe

| Option | Status | Evidence |
| --- | --- | --- |
| `enable_horizontal`, `enable_vertical`, `enable_free_movement`, `enable_smooth_transition`, `duration`, `background`, `fingers`, `gap`, `threshold`, `delta_threshold`, `speed_factor`, `speed_cap` | implemented since WG-5 (A2 before); `duration`'s easing curve is ignored | `VswipeConfig`; crates/shingle-frontend/src/vswipe.rs, crates/shingle-frontend/src/frontend/vswipe.rs |

### vswitch

| Option | Status | Evidence |
| --- | --- | --- |
| `binding_left`, `binding_right`, `binding_up`, `binding_down`, `binding_last`, `with_win_left`, `with_win_right`, `with_win_up`, `with_win_down`, `with_win_last`, `send_win_left`, `send_win_right`, `send_win_up`, `send_win_down`, `send_win_last`, `duration`, `gap`, `background`, `wraparound` | implemented | 548-606; crates/shingle-frontend/src/workspace_transition.rs:22 |
| `workspace_bindings`, `workspace_bindings_win`, `bindings_win` (`binding_N`, `with_win_N`, `send_win_N`) | implemented since WG-2 (A1 before) | `VswitchConfig`; crates/shingle-frontend/src/bindings.rs (`workspace_binding`) |

### wayfire-shell

| Option | Status | Evidence |
| --- | --- | --- |
| `toggle_menu` (and the `zwf_shell_manager_v2` global) | implemented since WG-4 (A3 before) | crates/shingle/src/wayfire_shell.rs |

### window-rules

| Option | Status | Evidence |
| --- | --- | --- |
| `rules` | implemented: every event (`created`, `maximized`, `unmaximized`, `minimized`, `fullscreened`) and action of wayfire:plugins/window-rules/view-action-interface.cpp | 619; crates/shingle-config/src/window_rule.rs:160, crates/shingle/src/window_rules.rs:6 |

### wm-actions

| Option | Status | Evidence |
| --- | --- | --- |
| `toggle_always_on_top`, `toggle_always_on_bottom`, `toggle_fullscreen`, `toggle_sticky`, `toggle_maximize`, `minimize`, `toggle_showdesktop`, `send_to_back`, `bring_to_front` | implemented; Wayfire's `disabled` defaults since WG-2 (B3 before) | 86-112 |

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
| `zwf_shell_manager_v2` (version 3) | wayfire:plugins/protocols/wayfire-shell.cpp:586 | version 3 since WG-4 (crates/shingle/src/wayfire_shell.rs) | annoying (A3) |
| `zwp_text_input_manager_v1` | wayfire:plugins/protocols/input-method-v1.cpp:528, with `enable_text_input_v1` | version 1 since WG-8 (crates/shingle/src/text_input/v1.rs) | rare |
| `zwp_tablet_manager_v2` | wayfire:src/core/core.cpp:155 | missing | rare |
| `ext_data_control_manager_v1` | wayfire:src/core/core.cpp:149 (new since 0.9.0) | missing; `zwlr_data_control_manager_v1` v2 (crates/shingle/src/data_control.rs:87) | rare |
| `ext_image_copy_capture_manager_v1`, `ext_output_image_capture_source_manager_v1` | wayfire:src/core/core.cpp:160-161 (new since 0.9.0) | missing; `zwlr_screencopy_manager_v1` v3, shm only (crates/shingle/src/screencopy.rs:250) | rare |
| `zwlr_export_dmabuf_manager_v1` | wayfire:src/core/core.cpp:163 | missing | rare |
| `wp_drm_lease_device_v1` | wayfire:src/core/core.cpp:166 | missing | rare |
| `org_kde_kwin_server_decoration_manager` | wayfire:src/core/core.cpp:197 | version 1 since the application sweep (crates/shingle/src/server_decoration.rs) | rare |
| `zxdg_exporter_v1`, `zxdg_importer_v1` | wayfire:src/core/core.cpp:250 | version 1 since the application sweep, sharing handles with v2 (crates/shingle/src/foreign.rs) | rare |
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
| `stipc/tablet/tool_proximity`, `tool_button`, `tool_axis`, `tool_tip`, `pad_button`, `stipc/delay_next_tx` | wayfire:plugins/ipc/stipc.cpp:302-307 | rare (test input) |

`wayfire-shell/toggle_menu` (wayfire:plugins/protocols/wayfire-shell.cpp:603) is there since WG-4. Partial: `list-views` leaves out override-redirect X windows. Since WG-2 `wayfire/get-config-option` and `list-config-options` answer every option of Wayfire's metadata, but `set-config-options` still changes only the 18 options Shingle keeps live. Every event of wayfire:plugins/ipc-rules/ipc-events.hpp:134-157, `view-pre-map` and `command-binding` are in `KNOWN_EVENTS` (crates/shingle/src/ipc.rs:470), and every field Wayfire's `view_to_json` writes (wayfire:plugins/ipc-rules/ipc-rules-common.hpp) appears in Shingle's view JSON. Shingle's `wm-actions/toggle_*`, `wm-actions/minimize`, the `vswitch/binding_*`, `with_win_*` and `send_win_*` activators and `invert/toggle` are Shingle's own; the pinned Wayfire registers only `wm-actions/toggle_showdesktop` among them.

## Implementation plan

The workspaces are ordered by rank and then by cost, and each can be finished and verified on its own. Every workspace also follows the repository rules: unit tests for new code, clippy clean, 80 columns, docs/status.md, docs/roadmap.md, docs/ipc.md and docs/features.md updated, real-client results recorded apart from unit tests, and signed commits. Real-client checks run in the existing harness namespaces, which never touch the operator's session: tools/output_test/run.py (headless), tools/gpu_test/run.py (offscreen and nested on the render node) and tools/xwayland_test/run.py (X11). Wayfire's behaviour is checked against the pinned commit above, which each workspace may move forward if it records the new one here.

### WG-1 `shingle-wayfire-ini-parser`: read what wf-config reads, and never refuse a Wayfire file

Make the INI reader follow wf-config:src/file.cpp: unescaped `#` starts a comment, `\#` is a literal `#`, a line ending in `\` continues on the next, whitespace is trimmed as wf-config trims it. Split the importer's errors into fatal ones (the file cannot be read) and per-option ones: an option whose value Shingle cannot use keeps its default and is reported with its line, as wf-config does, and the import succeeds. YAML stays strict. Teach the view matcher Wayfire's `PREDICATE equals true|false` (and `is`), and `type` values `x-or`, `unmanaged`, `background`, `panel`, `overlay`. Unknown animation names fall back as decided in [Questions](#questions-for-the-operator). Parse activators fully: keys and buttons as today, and a bare modifier, touch gestures and hotspots into binding kinds of their own that are recognised, reported as not yet supported and never matched as keys.

Acceptance:

- Unit tests: each wf-config rule above; Wayfire's stock wayfire:wayfire.ini at the pinned commit imports with no warning except unsupported plugins' options; a fixture written the way WCM writes (every section, `\#` colours, `400ms circle` durations, Wayfire's default `enabled_for`, `toggle_menu = <super>`, a `pinch in 4` activator) imports with the expected warnings and values; `squeezimize`, `fire`, `zap` and `spin` warn and fall back; the old silent `4` binding from `pinch in 4` is gone.
- `shingle-wcm --check` prints the warnings with line numbers and exits 0 for both files, and non-zero for an unreadable file.
- Real run: headless Shingle in tools/output_test/run.py's namespace starts with `--config` pointing at the WCM-style fixture, logs the warnings once, and `wayfire/get-config-option` returns the imported `core/background_color` and `simple-tile/inner_gap_size`.

#### WG-1 as built

The importer reads lines as wf-config does (crates/shingle-config/src/ini.rs): the first `#` not preceded by `\` ends the line, `\#` becomes `#`, trailing whitespace goes, a line ending in `\` is joined to the next (a trailing `\\`, which WCM writes for a value ending in a backslash, is one literal `\` and does not continue), and names and values are trimmed with wf-config's quirks. Each option is applied on its own (crates/shingle-config/src/import.rs): when its value does not parse, or the result fails a range check of `ShingleConfig::validate`, the option goes back to what it was and a `ConfigWarning` names its line, section and option; `ShingleConfig::load_with_warnings` and `parse_with_warnings` return the warnings with the configuration, and `load` and `parse` drop them. Lines before any section and lines without `=` warn as wf-config's do, a section of a plugin Shingle does not have warns once with its option count, and Expo workspace bindings and `assign_workspace` rules outside the workspace grid are dropped with a warning once the whole file is read, so the order of `[core]` and `[expo]` does not matter. The i3 importer applies lines the same way, as decision 1 says. `shingle` prints each warning once at start (and on a reload) as `shingle: PATH: line N: MESSAGE`; `shingle-wcm --check` prints them and exits 0, and 1 when the file cannot be read.

Activators are split into alternatives (crates/shingle-config/src/activator.rs) as wf-config:src/types.cpp splits them: keys and buttons, `none`/`disabled`, a modifier alone, touch gestures and hotspots, and anything else, which wf-config keeps as an extension. The view matcher takes `PREDICATE is|equals true|false`, and its `type` compares any of Wayfire's view types.

Where the code differs from the plan above, and why:

- The binding kinds are recognised inside the importer and are not part of `Binding` or the YAML model: nothing can act on them yet, a model field would have to be written back to YAML, and WG-4 adds modifier-only bindings to the model together with the code that matches them. A field that takes one binding uses the first key or button and warns about the others; an activator with no key or button disables the binding, since the user bound no key, and a command bound that way stays in the list with a disabled binding.
- Options of plugins Shingle has but that it does not read (`[animate] fire_duration`, `[autostart] autostart_wf_shell`) are still silent. Telling them from typing mistakes needs WG-2's table of Wayfire's options.
- wf-config's `#RGBA` colours are read too; Shingle's own `#RRGGBB` stays. An unescaped `#` after `=` is now a comment, as in wf-config, so a hand-written `background_color = #FF0000FF`, which Shingle used to read, now warns and keeps the default.
- The frontend still calls every layer-shell surface `overlay` when it evaluates `animate/enabled_for`; telling `background` and `panel` apart needs the surface's layer in its snapshot, left for later. `x-or` was already reported for override-redirect X windows.
- `wayfire/get-config-option` and `set-config-options` read and wrote the outer gaps for `simple-tile/inner_gap_size`; writing the real-run check found it, and they now use the inner gap.

Tested by the unit tests in crates/shingle-config (ini.rs for each line rule, activator.rs for each binding kind, import.rs, wayfire.rs and i3.rs for the warnings, view_matcher.rs), with Wayfire's stock wayfire.ini at the pinned commit (crates/shingle-config/testdata/wayfire-stock.ini, which warns only about `[cube]` and `[wayfire-shell]`) and a synthetic WCM-style file at Wayfire's metadata defaults (testdata/wayfire-wcm.ini). The real run is tools/output_test/run.py's `wcm_ini_checks`, recorded in docs/status.md.

### WG-2 `shingle-wayfire-defaults`: Wayfire's defaults and plugin list under a Wayfire file

Generate a table of every Wayfire option with its type and default from wayfire:metadata/*.xml at the pinned commit, with a Python tool under tools/ and the result checked in, and start the Wayfire import from those defaults rather than `ShingleConfig::default()`. Under a Wayfire file, Shingle-only bindings with no Wayfire option are off unless the operator decides otherwise. Honour `core/plugins` for every plugin Shingle implements: an unlisted plugin's bindings, effects and protocols are off, as when Wayfire does not load it; `simple-tile` missing means nothing tiles; IPC stays governed by `--ipc`. Import the numbered `[vswitch]` lists (`binding_N`, `with_win_N`, `send_win_N`, A1) and `autostart_wf_shell` with Wayfire's rule (start `wf-panel` and `wf-background` unless an autostart entry names them). `wayfire/get-config-option` and `list-config-options` answer every option of the table, with its live value where Shingle has one and its imported text otherwise.

Acceptance:

- Unit tests: the generated table matches the metadata (the tool's own Python tests read a sample of the XML); an empty `wayfire.ini` gives Wayfire's defaults, including a 3×3 grid, `ctrl+alt+backspace` to exit and no `super+shift+q`, `super+s`, `super+d`, `super+n` or `super+1` binding; a `plugins` line without `scale`, `zoom`, `grid` or `animate` turns those off and one without `simple-tile` stops tiling; `binding_3 = <super> KEY_3` switches to workspace 3; autostart's `wf-panel` rule in its three cases.
- Real run in tools/gpu_test/run.py's namespace with offscreen Shingle, input through STIPC: with `scale` unlisted, `feed_key` `super+p` reaches a `wl_probe` client and no `plugin-activation-state-changed` event follows; with it listed the event follows and the client gets no key. With `simple-tile` unlisted a second `wl_probe` is not tiled (`simple-tile/get-layout` is empty).
- Real run: stand-in `wf-panel` and `wf-background` scripts on the namespace's `PATH` are started once each with `autostart_wf_shell = true`, and not when an autostart entry runs them.
- `list-config-options` lists every option of the table; the method count in docs/ipc.md stays factual.

#### WG-2 as built

The table is crates/shingle-config/data/wayfire-options.tsv, one line per option with its section, name, type, default and, for a dynamic list, its entry prefixes, written by tools/wayfire_options/generate.py from wayfire:metadata/*.xml at the pinned commit (332 options in 50 files). crates/shingle-config/src/wayfire_options.rs reads it once (`wayfire_options`, `wayfire_option`).

A Wayfire import (crates/shingle-config/src/wayfire.rs, `parse`) first reads the file's last `[core] plugins` line, or Wayfire's default list without one, then starts from Shingle's defaults with the Shingle-only bindings off (moving and resizing tiles, the ten workspace bindings with and without `shift`, reload and lock) and applies the default of every option of the table whose section is core (`core`, `input`, `workarounds`) or a listed plugin, through the same arms as the file's own values. Only `animate/minimize_animation`'s `squeezimize` needs a fallback among the defaults, and it falls back silently; the file's own values still warn. The file is applied on top, skipping every option of a plugin the list leaves out. `ShingleConfig::plugins` keeps the list, and `ShingleConfig::turn_off_unlisted_plugins` (crates/shingle-config/src/plugins.rs), run on every parse, turns off what the model holds for an unlisted plugin of `PLUGINS`: its bindings, Expo's and vswitch's workspace bindings, animations (`none`), zoom's and alpha's modifiers, idle's timeouts, window rules, commands (and `xkb:` commands without `xkb-bindings`), autostart entries, oswitch and wsets bindings, text input (`input-method-v1`) and placement. The rest is checked where it is used through `ShingleConfig::plugin_enabled`: without `simple-tile` nothing tiles (`Config::tiling`, crates/shingle/src/shell.rs), without `extra-gestures` touch gestures are not recognised, without `scale-title-filter` typing does not filter Scale, and the globals of the protocol plugins (`ProtocolPlugins`: xdg-activation, foreign-toplevel, ext-toplevel, ext-workspace, security-context-v1, session-lock, shortcuts-inhibit) are only created when listed. The numbered `[vswitch]` options become `VswitchConfig::workspace_bindings`, `workspace_bindings_win` and `bindings_win`, and `autostart_wf_shell` appends `wf-panel` and `wf-background` to the autostart commands unless an entry's command contains the name. Every option the file sets is kept, as written, in `ShingleConfig::wayfire_values` (`WayfireValues`), which `wayfire/get-config-option` and `list-config-options` read: the live value for the 18 options Shingle keeps in the compositor, otherwise the file's text or the table default, Wayfire's metadata default as `default`, dynamic lists as Wayfire writes compound options, and the file's `output:NAME` and `input-device:NAME` sections as sections of their own.

Where the code differs from the plan above, and why:

- The table is a tab-separated data file read at run time, not generated Rust: generated source would have to pass rustfmt and the 80-column rule, and a data file keeps the generator simple and the table readable. A Python test checks the checked-in table against the pinned survey's count and defaults; a Rust test reads it.
- An unlisted plugin's IPC methods (`scale/toggle`, `expo/toggle`, `simple-tile/get-layout` and the others) stay available, as decision 4 leaves IPC to `--ipc`; `simple-tile/get-layout` reports an empty layout without simple-tile. `ipc-rules` is treated like `ipc`.
- The protocol plugins' globals are created once, at start; a reload that changes the list changes them only at the next start, like `enable_text_input_v3` before. A reload that unloads `simple-tile` floats every tiled view where its tile was, as Wayfire's simple-tile does when it is unloaded.
- Without `place`, a new view goes to the top left of the workarea: Wayfire clamps the view's own position, which starts at the origin, to the workarea (wayfire:src/view/view-impl.cpp, `adjust_view_pending_geometry_on_start_map`). This is a new placement mode, `none`. Without `grid`, move's snapping is off: Wayfire's move asks grid to snap the view on release. Without `input-method-v1`, text-input-v3 and the input-method protocols are off, as Shingle has one switch for all of them.
- A binding two plugins share stays while either is loaded: `toggle_maximized` (`simple-tile/key_toggle_maximized` and `wm-actions/toggle_maximize`), `move_button` (`move` and `simple-tile`) and `resize_button` (`resize` and `simple-tile`). With both loaded Shingle keeps one binding where Wayfire keeps both, and simple-tile's `super+m` default goes after wm-actions' `disabled`.
- The defaults of `[output]` and `[input-device]` are not applied: they describe each named section, so outputs without a section keep Shingle's own defaults.
- A numbered vswitch binding for a workspace outside the grid is dropped with a warning when the file has been read, as Expo's are since WG-1; Wayfire works the target out at each press.
- Options of plugins Shingle has but does not read are still silent. WG-1 left telling them from typing mistakes to this table; that warning is not added yet.
- Under Wayfire's default plugin list, which has neither `simple-tile` nor `scale` nor `wm-actions`, an empty `wayfire.ini` does not tile, as Wayfire does not.

Tested by the unit tests in crates/shingle-config (wayfire_options.rs, plugins.rs, wayfire.rs for the defaults, the plugin list, shared bindings, the vswitch lists, autostart and the kept values), crates/shingle (shell.rs for tiling without simple-tile, lib.rs for the gated globals, ipc.rs for the config options), crates/shingle-frontend (the numbered bindings and unloaded plugins through STIPC keys) and tools/wayfire_options/test_generate.py. The real runs are tools/gpu_test/run.py's groups `wayfire-plugins` and `wayfire-shell` and tools/output_test/run.py's `wcm_ini_checks`, recorded in docs/status.md.

### WG-3 `shingle-client-environment`: Wayfire's variables for children

Add `XDG_CURRENT_DESKTOP` when it is unset (the value is a question below), `XCURSOR_THEME` and `XCURSOR_SIZE` from `[input]`, and `WAYFIRE_CONFIG_FILE` naming the configuration in use, all through `ClientEnvironment::new` (crates/shingle-session/src/environment.rs:42); install a session file `data/shingle.desktop` for display managers and, if the operator wants one, a portals configuration as Wayfire's wayfire-portals.conf.

Acceptance:

- Unit tests for each variable, set and unset, and for a configuration reload changing the cursor variables for later children.
- Real run: headless Shingle in tools/output_test/run.py's namespace with `--ipc`; `stipc/run` of `env` records every variable as expected, and the host's `XDG_CURRENT_DESKTOP` is kept when set.

#### WG-3 as built

`ClientEnvironment::with_desktop` (crates/shingle-session/src/environment.rs) adds the variables to the socket variables `ClientEnvironment::new` sets, from `DesktopVariables` (cursor theme and size, Wayfire file) and a lookup of Shingle's own environment: `XDG_CURRENT_DESKTOP=wayfire` when Shingle has none (an empty value counts as set, as with Wayfire's `getenv`), `XCURSOR_THEME` unless the theme is `default`, `XCURSOR_SIZE`, and `WAYFIRE_CONFIG_FILE` or its removal. `CompositorState::client_environment` (crates/shingle/src/state.rs) builds it from the live `cursor_theme` and `cursor_size`, which `Compositor::apply_config` and `wayfire/set-config-options` change, so every child started after a reload or an IPC update gets the new values; Xwayland, started through the same environment, gets them too. `ShingleConfig::format` records the format a configuration was parsed from and `ShingleConfig::wayfire_config_file` turns the loaded path into the exported one; the frontend (nested, offscreen, native) passes it to `Compositor::set_wayfire_config_file` at start and after every reload, and the headless backend, which does not reload, at start. data/shingle.desktop is the session file; the README says how to install it, and nothing installs it. As decided (decision 6), Shingle has no portals file.

Where the code differs from the plan above, and why:

- The variables are added by `ClientEnvironment::with_desktop`, not by more arguments of `ClientEnvironment::new`: `new` keeps the three socket arguments its many callers and tests pass, and the lookup of Shingle's own environment is injected, so the tests never change the test process's environment.
- `WAYFIRE_CONFIG_FILE` names the file only when it is a Wayfire file, and is removed otherwise. Wayfire's file is always a `wayfire.ini`; WCM and scripts open the variable's file as one and WCM writes it back, which would destroy a YAML configuration, and a host Wayfire's variable passed through a nested Shingle would let WCM inside it edit the host's file. The path is made absolute, where Wayfire exports it as given, because restored clients start in their saved working directory.
- At the default `cursor_size` 24 a numeric `XCURSOR_SIZE` Shingle inherited is kept, where Wayfire always exports its configured size: Shingle draws its own cursor at the inherited size in that case (crates/shingle/src/cursor_shape.rs), and children should draw theirs at the same size. A configured size other than 24 is exported as Wayfire does, and `default` leaves the inherited `XCURSOR_THEME` as Wayfire's `init_xcursor` does.
- Wayfire sets `XDG_CURRENT_DESKTOP` in its own environment, which its children inherit; Shingle sets it only in its children's, as it never changes its own environment (the nested backend still talks to the host through it).
- The session file's `DesktopNames` is `Shingle;Wayfire;wlroots`, where Wayfire's is `Wayfire;wlroots`. Display managers set `XDG_CURRENT_DESKTOP` from it and Shingle keeps it; xdg-desktop-portal tries `shingle-portals.conf`, which does not exist, then `wayfire-portals.conf`, then the portals declared for `wlroots`, so portals behave as under Wayfire while scripts can still tell Shingle apart. `desktop-file-validate` reports `DesktopNames` as an extension key, as it does for Wayfire's file.

Tested by the unit tests in crates/shingle-session/src/environment.rs (each variable set and unset, kept and replaced, and a spawned shell seeing them), crates/shingle-config/src/lib.rs (the recorded format and the exported file), crates/shingle/src/lib.rs (the cursor variables after `apply_config` and `set-config-options`, the Wayfire file across a reload, an inherited `XDG_CURRENT_DESKTOP`), crates/shingle-frontend/src/frontend/tests.rs (a reload of the file changing the variables of later children) and crates/shingle-bin/src/main.rs (the session file's keys, and its `Exec` planning a native start in a display manager's environment). The real run is tools/output_test/run.py's `client_environment_checks`, recorded in docs/status.md.

### WG-4 `shingle-wayfire-shell`: `zwf_shell_manager_v2` and the menu key

Add `zwf_shell_manager_v2` version 3 from wayfire:proto/wayfire-shell-unstable-v2.xml: `zwf_output_v2` with `enter_fullscreen` and `leave_fullscreen` for the output's fullscreen views, `inhibit_output` and `inhibit_output_done` (an inhibited output draws nothing new), hotspots by edge and custom rectangle with Wayfire's threshold and timeout and `proximity_changed`, `toggle_menu`, and `zwf_surface_v2.interactive_move`, following wayfire:plugins/protocols/wayfire-shell.cpp. Add modifier-only bindings with `input/modifier_binding_timeout` (a bare modifier pressed and released alone within the timeout, wayfire:src/core/seat/keyboard.cpp:409), the `wayfire-shell/toggle_menu` activator and its IPC method. The global is privileged like foreign-toplevel only if the operator says so.

Acceptance:

- Unit tests through the in-process test client for every request and event, hotspot timing, fullscreen enter and leave across outputs, and the modifier timeout (pressed alone, with another key, held too long).
- Real run in tools/output_test/run.py's namespace with headless Shingle and `--outputs 2`, a raw-protocol Python client of the protocol: `stipc/move_cursor` to an edge fires `enter` after the timeout and `leave` when moved away; a `wl_probe` going fullscreen on one output sends `enter_fullscreen` to that output only; STIPC `feed_key` of `KEY_LEFTMETA` alone sends `toggle_menu`, and with another key in between does not; `interactive_move` starts a move that STIPC button and motion finish.
- `wayland-info` lists the global; `list-methods` counts the new method.

#### WG-4 as built

crates/shingle/protocols/wayfire-shell-unstable-v2.xml is wayfire:proto/wayfire-shell-unstable-v2.xml at the pinned commit; crates/shingle/src/wayfire_shell.rs generates its server code with `wayland-scanner`, and the in-process test clients (crates/shingle/src/test_client.rs, crates/shingle-frontend/src/test_client.rs) its client code. The global, version 3, exists when `wayfire-shell` is in `ProtocolPlugins` (`[core] plugins`, which loads it by default; always under YAML), and sandboxed clients may bind it (decision 8).

- `zwf_output_v2` follows its output. `wayfire_shell::sync`, at the end of every turn, sends `enter_fullscreen` and `leave_fullscreen` when `CompositorState::promoted_fullscreen` of its output changes, the fullscreen window drawn above that output's top layer. A new object on an output in that state gets `enter_fullscreen` at once. When the output goes or is renamed, the object does nothing more, sending `leave_fullscreen` first if it had sent `enter_fullscreen`.
- Hotspots compute their trigger and bounds as Wayfire's `calculate_trigger_geometry` and `calculate_hotspot_geometry` do (`edge_trigger`, `HotspotArea`): an edge is a line of no thickness on the output's last row or column, two edges the corner point, other masks the layout's origin, a custom rectangle is in output coordinates, and the bounds are the trigger grown by the threshold and cut to the output. Pointer motion and motion of touch point 0 record the position, which `sync` checks at the end of the turn, as Wayfire's idle call does: inside, an untriggered hotspot starts its timer and a triggered one sends `proximity_changed` (version 3 only); outside, the timer stops and a triggered hotspot sends `leave`. A one-shot timerfd of its own, armed for the earliest deadline, wakes the event loop for `enter`, which `proximity_changed` follows. An output that goes makes its hotspots leave, and they never trigger again.
- `toggle_menu` goes to every `zwf_output_v2` of version 2 or later of the output: of the active output for the binding, of the one IPC names for `wayfire-shell/toggle_menu` (`output_id` or `output-id`, `ok` without focusing it; `output id not found!` and `view id not found!` as Wayfire's `ipc_activator_t`). `list-methods` has 86 methods.
- `inhibit_output` and `inhibit_output_done` count per object, an output is inhibited while any of its objects has a count, the count goes with its object, and an extra `inhibit_output_done` is ignored. `Compositor::output_inhibited` and `Event::OutputInhibitChanged` tell the frontend, which then draws that output with no tiles over black.
- `zwf_surface_v2.interactive_move` on the surface of a toplevel emits `Event::BeginMove`, as `xdg_toplevel.move` does, which the frontend runs as a drag until the button is released; on any other surface it does nothing.
- Modifier-alone bindings are `Binding::modifiers_alone` in the YAML model (`super`, `ctrl+alt`, the text without a key), matched by `Binding::matches_modifiers_alone`, and the importer reads `<super>` into one for `[wayfire-shell] toggle_menu` (`keybindings.toggle_menu`, default `super`, Wayfire's) and command bindings; `[input] modifier_binding_timeout` is `input.modifier_binding_timeout`. `ModifierBindingTracker` (crates/shingle/src/modifier_binding.rs) is Wayfire's `mod_binding_key`: a modifier key pressed while only modifier keys are held and no button or touch is down starts a candidate, any other press ends it, so do buttons, scrolling and touches, and a release within the timeout (0 never cancels) returns the modifiers held with the released one. The frontend runs one over every key from the shared keyboard and, on a trigger, calls `Compositor::toggle_menu` for `toggle_menu` and runs the commands bound to those modifiers. Without a frontend, as in headless Shingle, the compositor runs one for STIPC keys and matches `toggle_menu` itself, as it matches oswitch's bindings. Neither acts while locked (commands with `always` excepted) or while a client inhibits shortcuts. The keys themselves go on to bindings and clients.

Where the code differs from the plan above, and why:

- The pinned Wayfire no longer acts on `inhibit_output`, `inhibit_output_done` or `interactive_move`: it logs that the first two are "left in for historical reasons" and that "Interactive move no longer supported!". The plan asked for both to work, so Shingle does what Wayfire 0.9.0, the Debian release on this host, did for inhibits (counts, released with their object, the output cleared to black), ignoring an extra `inhibit_output_done` where 0.9.0 posted `no_memory`, and moves the view for `interactive_move`.
- The fullscreen events follow Shingle's own rule for drawing a fullscreen window above the panels, the window focused on that output, rather than Wayfire's, the topmost view of its workspace (wayfire:src/output/promotion-manager.hpp), so that the events say what is drawn. Writing the tests found that the rule was not per output: a focused fullscreen window hid the top layer of every output and was drawn in each output's scene. Each output now takes the keyboard's window only if it is on that output, else the focused tile of its current workspace.
- A `zwf_output_v2` created while its output shows a fullscreen window gets `enter_fullscreen` at once, which Wayfire does not send, so that its later `leave_fullscreen` has the `enter_fullscreen` the protocol pairs it with.
- `proximity_changed` is the distance to the trigger on every side. Wayfire measures a zero-width or zero-height trigger only from one side (`(rect.width > 0) && ...` in `calculate_proximity`), so its top and left edges always report 0; the protocol says the value is the distance.
- `get_wf_output` with a `wl_output` whose output is gone, and `get_wf_surface` of a surface that is no view, create objects that do nothing, where Wayfire creates none and leaves the client's new id dangling.
- Hotspots follow the pointer and touch point 0; there are no tablets yet (`zwp_tablet_manager_v2` is missing), so tablet motion, which Wayfire also follows, does not reach them.
- Only `toggle_menu` and commands take a modifier alone; every other option still warns about one, and in YAML it never matches there. Modifier keys are recognised by their evdev codes (either logo, Control, Alt or Shift key), not through the keymap, so a key remapped to a modifier by an XKB option does not start a candidate. A command bound to modifiers alone runs once on the release, whether it is a normal, repeatable or release binding.
- The real run checks `interactive_move` against offscreen Shingle on lavapipe inside tools/output_test/run.py's namespace instead of headless Shingle: headless has no frontend, so neither `interactive_move` nor `xdg_toplevel.move` moves anything there. The rest runs against headless Shingle with `--outputs 2` as planned, and the offscreen run also checks the frontend's path to `toggle_menu`.
- The real run found that a drag the frontend runs took the button release from the compositor: after a move a client asked for, by `xdg_toplevel.move` or `interactive_move`, the client never got the release and the seat held the button for good, which also blocked every later modifier-alone binding. The frontend now gives the compositor the release of every press it gave it.

Tested by the unit tests in crates/shingle/src/wayfire_shell/tests.rs (every request and event through the in-process test client, hotspot timing with the timerfd, fullscreen across outputs, the menu key without a frontend: pressed alone, with another key, with a button and held too long), crates/shingle/src/modifier_binding.rs, crates/shingle/src/output/protocol_tests.rs (panels of other outputs stay), crates/shingle-config (the binding form and the import) and crates/shingle-frontend/src/frontend/tests.rs (the menu key through the frontend, the timeout, a modifier-alone command, an inhibited output drawn black, the release after a client's move). The real runs are tools/output_test/run.py's `wg4_headless_checks` and `wg4_offscreen_checks`, recorded in docs/status.md.

### WG-5 `shingle-vswipe`: touchpad workspace swipes

Port wayfire:plugins/single_plugins/vswipe.cpp and vswipe-processing.hpp: a swipe with `fingers` fingers drags the workspace wall horizontally, vertically or freely, with `threshold`, `delta_threshold`, `speed_factor`, `speed_cap`, `gap`, `background`, `duration` and smooth transition, reusing vswitch's transition (crates/shingle-frontend/src/workspace_transition.rs) and taking the swipe from clients only when it starts with the configured finger count.

Acceptance:

- Frontend input-script tests (crates/shingle-frontend/src/frontend/tests.rs, `FakeHost`) for each option: direction locking, thresholds, a cancelled swipe returning, smooth and immediate commits, the grid's edges with and without wraparound, and swipes with other finger counts reaching clients.
- Real run in tools/gpu_test/run.py's namespace with offscreen Shingle, swipes fed through a test-only method (a question below): `wset-workspace-changed` after a long swipe, none after a short one, and `grim` captures of the half-way wall.
- Host checks with a real touchpad are recorded under [Needs operator: host input](../status.md#needs-operator-host-input).

#### WG-5 as built

`VswipeConfig` (crates/shingle-config/src/lib.rs) holds the twelve options with Wayfire's defaults, YAML `vswipe:`; the Wayfire importer reads `[vswipe]` when `[core] plugins` lists it, and `vswipe` is a plugin of `PLUGINS`, checked where it is used (`ShingleConfig::plugin_enabled`). crates/shingle-frontend/src/vswipe.rs ports the arithmetic of wayfire:plugins/single_plugins/vswipe.cpp and vswipe-processing.hpp at the pinned commit: `Swipe::update` is `on_swipe_update` (direction from `calculate_direction` and `is_diagonal`, promotion to diagonal, `process_delta` per enabled axis, smooth or immediate), `Swipe::end` is `on_swipe_end` (`finish_target` per axis, the offsets restarted towards the target), and `Swipe::viewport` is `on_frame`'s interpolation between the start workspace and its neighbour along each axis of the direction. The frontend (crates/shingle-frontend/src/frontend/vswipe.rs) decides which swipes vswipe gets, as `on_swipe_begin` does (plugin loaded, an axis enabled, no vswipe active, `fingers`, the pointer on an output), starts one when its direction is decided, as `start_swipe` does (activation, then `focus_output`), and ends it after the last frame of the slide, as `post_frame` does. The wall is drawn by the scene as vswitch's slide is, with `workspace_transition::wall_geometry` placing each window of the output at the fractional viewport, `gap` between the workspaces and `background` as the clear colour; the windows of every workspace of the output are drawn while it runs, and frames are asked for until it ends. Lifting the fingers switches the workspace at once with `Compositor::switch_workspace`, as Wayfire's `set_workspace` does, which sends `wset-workspace-changed`, and vswitch's own slide is not started for that switch. `stipc/feed_gesture` (decision 7) feeds swipe, pinch and hold steps of the STIPC pointer; a frontend gets them as `Event::StipcGesture` and handles them as that pointer's gestures, and headless Shingle sends them to clients. The gesture step type, `shingle::Gesture`, moved from the frontend's host seam to the compositor, which the host re-exports.

Where the code differs from the plan and from Wayfire, and why:

- Clients get a swipe of the configured fingers until vswipe starts it, then its end with `cancelled` set and nothing more of it. Wayfire sends the client the begin and the updates before its input grab, and nothing after it, so the client never sees an end; Shingle closes the gesture as the protocol's cancellation describes. A swipe whose direction is never decided stays the client's, as in Wayfire.
- Wayfire's `activate_plugin` refuses vswipe while another plugin with conflicting capabilities runs, the output is inhibited or the session is locked. Shingle has no such registry, so `can_start_vswipe` checks what Shingle runs that Wayfire runs as such a plugin: Scale and Expo and their exits, the switchers, a drag, window rotation, vswitch's slide, the lock and a wayfire-shell inhibit. While vswipe runs, Wayfire's input grab also keeps pointer, keyboard and touch input from clients; Shingle takes only the swipe, and a swipe that begins during the slide goes to clients, where Wayfire's grab sends it nowhere.
- Wayfire runs one vswipe per output, each taking swipes that begin on its output; Shingle follows one swipe at a time for the whole seat. A touchpad has one swipe at a time anyway; only a swipe that begins on another output during a slide differs, and goes to clients.
- `duration` is linear: Shingle's animations ignore Wayfire's easing curves, so its default `circle` is not followed. The offsets are kept as Wayfire's `timed_transition_t` keep them, a start and an end with one clock restarted by every update and by the end.
- Shingle clears the whole output to `background` while the wall is up, as its vswitch slide clears it to vswitch's colour, so the outer gaps of every workspace show it too, and layer-shell surfaces stay in place as in that slide. Wayfire's workspace wall draws each workspace's own scene and its colour only between and around them.
- `fingers` must be 1 to 20, `speed_factor` above 0, `gap` and `speed_cap` at least 0 and the thresholds finite, or the value warns and keeps its default; Wayfire takes any value, and would divide by a zero `speed_factor`.
- The acceptance asks for the grid's edges with and without wraparound. vswipe has no wraparound in Wayfire (`vswipe_finish_target` clamps the target to the grid), and Shingle's does not follow `[vswitch] wraparound` either; the tests check that every wraparound mode stops at the edges.
- The design said to reuse vswitch's transition. The wall is not a `WorkspaceTransition`, which slides between two whole workspaces over a fixed time, because it follows the fingers and slides on from wherever they left it; it shares that module's wall layout through `wall_geometry`, and the scene treats both the same way.
- libinput's cancellation of a swipe changes nothing, as in Wayfire, whose `on_swipe_end` ignores it: the end picks the workspace as any end does.

Tested by the unit tests in crates/shingle-frontend/src/vswipe.rs (direction locking, disabled axes and single rows, diagonal and promoted swipes, the speed factor and cap, the rubber band, `finish_target`'s cases, long and short swipes, the slide over the duration, smooth and immediate updates, a zero duration and diagonal swipes across several workspaces), crates/shingle-frontend/src/workspace_transition.rs (the wall at a fractional viewport), crates/shingle-frontend/src/frontend/tests/vswipe.rs (input scripts through `FakeHost` for every option: long and short swipes without vswitch's slide, direction locking, disabled axes, the grid's edges in every wraparound mode, both thresholds, speed factor and cap, free movement, smooth and immediate commits over the duration and a swipe during the slide, swipes of other finger counts and undecided swipes reaching a client and a started swipe cancelled for it, the plugin unloaded, Scale and a disabled pointer, and the wall drawn with its gap and background on the GPU), crates/shingle-config/src/lib.rs (the import, defaults, plugin list and validation) and crates/shingle/src/ipc.rs (`stipc/feed_gesture` to clients and to a frontend, and its errors). The real run is tools/gpu_test/run.py's group `vswipe` (tools/gpu_test/vswipe_checks.py), recorded in docs/status.md; checks with a real touchpad are listed under [Needs operator: host input](../status.md#needs-operator-host-input).

### WG-6 `shingle-decoration`: server-side decorations

Draw Wayfire's decoration (wayfire:plugins/decor/) for xdg toplevels in server-side mode and for X11 windows that ask for a frame (`_MOTIF_WM_HINTS`): a title bar of `title_height` with the title in `font`, `font_color`, `font_scale` and alignment, `button_order` buttons with their scale and padding, a `border_size` border in `active_color` or `inactive_color`, and `ignore_views` and `forced_views`. Dragging the title bar moves the window, its buttons act, the border resizes. Geometry, IPC and sessions keep describing the client's window as Wayfire's do (the frame is outside the view's geometry). Text uses the rasterizer Scale's titles use (crates/shingle-frontend/src/scale_title.rs:48).

Acceptance:

- Unit tests for the layout (title and button rectangles, alignment, border), hit testing and the matcher lists, and pinned pixels in crates/shingle-render/tests/offscreen_pixels.rs.
- Real run in tools/gpu_test/run.py's namespace with offscreen Shingle: a `wl_probe` asking for server-side decorations is drawn with a frame in a `grim` capture, a client-side one without; STIPC clicks on close, maximize and minimize do what they say; a title-bar drag moves the window.
- Real run in tools/xwayland_test/run.py's namespace: `xterm` gets a frame, an X window with `_MOTIF_WM_HINTS` decorations off does not.

#### WG-6 as built

Behaviour was checked against wayfire:plugins/decor/ (decoration.cpp, deco-layout.cpp, deco-subsurface.cpp, deco-theme.cpp, deco-button.cpp) and wayfire:metadata/decoration.xml at the pinned commit, with wayfire:src/view/xdg-shell/xdg-toplevel-view.cpp and wayfire:src/view/xwayland/xwayland-toplevel-view.hpp for who wants a frame.

`DecorationConfig` (crates/shingle-config/src/lib.rs) holds the fourteen options with Wayfire's defaults, YAML `decoration:`; the Wayfire importer reads `[decoration]` when `[core] plugins` lists `decoration`, clamping numbers to the metadata's bounds as wf-config does (wf-config:include/wayfire/config/option.hpp), and `decoration` is a plugin of `PLUGINS`, checked where it is used.

- crates/shingle/src/frame.rs ports deco-layout.cpp: `FrameTheme` (sizes and the parsed `button_order`), `Margins` (`get_margins`: the border on three sides, title bar and border on top), `FrameLayout` (`decoration_layout_t::resize`: the buttons from right to left, the title bar around them, the title, then the four borders, inert when the window is tiled or maximized; a dialog, a toplevel with a parent, has no minimize button) and `FrameInput` (`handle_motion`, `handle_press_event`, `handle_focus_lost`: a left press on a border resizes its edges, a press on the title bar that moves more than five pixels within its area starts a move, two presses on it within 300 ms toggle maximize on the second release, and a release on the button it was pressed on acts).
- crates/shingle/src/decorator.rs decides which toplevels have a frame, as `should_decorate_view` does: the plugin is loaded, `ignore_views` does not match and `forced_views` matches or the client does not draw its own (`ToplevelRole::client_decorated`). The decision is made again whenever Shingle reconfigures its windows. A floating window that gains or loses its frame later keeps its client where it is, its rectangle grown by the margins and clamped into the workarea or shrunk, as `adjust_new_decorations` and `remove_decoration` do; a new X window's start rectangle, which is its client's, is grown at once, and so is every `ConfigureRequest` of a floating one before it is clamped, as Wayfire's `configure_request` expands the request by the margins.
- The frame is part of the window's geometry, as in Wayfire: `shell::window_geometry` is the tile, floating, maximized or fullscreen rectangle, which sessions keep and IPC reports as `geometry` and `bbox`, and the client is configured with it shrunk by the margins (`shell::configure_toplevel`), which is IPC's `base-geometry` and where the surface is drawn. A fullscreen window has no margins.
- Pointer input: `input::hit_at` finds a frame just below its toplevel's surface. The pointer on a frame leaves the client, `decorator::pointer_moved` and `pointer_button` work the frame, and its actions are those of `simple_decoration_node_t::handle_action`: a move or resize is `Event::BeginMove` or `BeginResize`, which the frontend runs as a drag, as for an `xdg_toplevel` request; close asks the client; maximize toggles; minimize minimizes. A click on a frame focuses its window first. Over a border the default cursor becomes the resize cursor Wayfire's `update_cursor` names.
- `Compositor::frames` gives the frontend a `FrameSnapshot` per framed window: rectangle, margins, layout, title, whether it is active, and the hovered and pressed buttons (`Event::DecorationChanged` asks for a redraw when those change). crates/shingle-frontend/src/frame_raster.rs draws what deco-theme.cpp draws with Cairo and Pango: four bands of background around the client, the title text at `title_height` times `font_scale` em with Pango's alignment, its two-pixel right margin and end ellipsis, and the buttons at the title bar's height, scaled down to the button: a disc, grey (60 %, 60 %, 63 % at 36 %) or in the button's colour when hovered (90 %) or pressed (44 %), a one-pixel ring and the icon. The scene (crates/shingle-frontend/src/frontend/frames.rs) draws a frame below its surface and maps it along wherever the surface tile goes.
- A view's size includes its frame elsewhere too, as Wayfire's: placement places a new floating window at its client's size grown by the frame, interactive resizes keep to the client's minimum and maximum sizes grown by the frame (`shell::size_constraints`), an X dialog's frame is centred on its parent's, and Scale and Expo lay out the window with its frame, putting the surface where the frame goes in the slot.
- xdg-decoration (crates/shingle/src/decoration.rs) now follows each mode it sends with an `xdg_surface.configure` and applies it when the client acknowledges that configure, as wlroots and Wayfire apply the acknowledged mode; a destroyed decoration object makes the toplevel draw its own decorations again. X windows read `_MOTIF_WM_HINTS` as wlroots does (`motif_decorations`): a frame unless the decorations field leaves out the title or the border, and none for splash screens.

Where the code differs from the plan above or from Wayfire, and why:

- The plan said that geometry, IPC and sessions keep describing the client's window, "the frame is outside the view's geometry". Wayfire does the opposite: `adjust_new_decorations` grows the toplevel's geometry by the margins and the client gets it shrunk again, so a tile includes the frame, and IPC's `geometry` (wayfire:plugins/ipc-rules/ipc-rules-common.hpp, `get_pending_geometry`) is the frame while `base-geometry` is the client. Shingle follows Wayfire, and this document is corrected here. Tiling needs no change, a tile's frame is inside the tile, and a floating rectangle stays one rectangle.
- Titles are drawn in Inter, the font Scale's titles use, whatever `font` names, since Shingle has no Pango or fontconfig; `font` is imported and kept for IPC and WCM. The line height is Inter's ascent and descent at the em size, as Pango's logical rectangle is.
- Wayfire draws its colours' bytes; Shingle's renderer takes solid colours as linear and encodes them to sRGB, so the frame's colours are decoded first and show their configured bytes. Blending happens in linear light, so a translucent frame (Wayfire's defaults are) over the background looks slightly different from Wayfire's, which blends the bytes. WG-6 decoded only the frame's colours; since the [colour and focus fixes](#colour-and-focus-fixes-as-built) every configured colour is decoded the same way, by `shingle_render::srgb_fill`.
- A button changes its look at once, where Wayfire animates the change over 100 ms.
- The middle and right buttons on the maximize button, Wayfire's vertical and horizontal maximize (`tile_request` with two edges), do nothing: Shingle has no half-maximized state outside the grid plugin's slots.
- Touch on a frame does nothing; Wayfire's `handle_touch_down` presses it. A touch there reaches no client either.
- Which windows have a frame is decided whenever Shingle reconfigures its windows, which it does on map, focus, tiling, maximize and fullscreen changes and reloads, where Wayfire decides on map, on a decoration-mode change and on `view-tiled`. A matcher on the title or app id therefore takes effect at the next reconfigure in Shingle, and at the next of those events in Wayfire.
- A window rotated by `wrot` is drawn without its frame, whose pieces would turn about their own centres; closing and minimizing animations show only the client.
- `org_kde_kwin_server_decoration_manager`, which Wayfire also offers, came with the application sweep (crates/shingle/src/server_decoration.rs).
- The pinned pixels in crates/shingle-render/tests/offscreen_pixels.rs (`a_decorated_window_matches_reference`, tests/golden/frame.rgba) are drawn from the frontend's `frame_pieces` and rasterizers, so shingle-render's integration tests have a dev-dependency on shingle-frontend; the scene's own drawing of a frame is checked by the frontend's GPU test.
- The X check uses `xw_probe --no-decorations` for a window without decorations, since no X client on the test host sets those hints by itself.
- Nested Shingle's winit window asks for server-side decorations, so a Shingle hosting it now frames it, as Wayfire does. The harnesses that run nested inside a headless or offscreen Shingle find nested's surface by the window's `base-geometry`, and the XWayland harness's host ignores the request, so that nested still covers its output there.
- IPC's `min-size` and `max-size` stay the client's, without the frame.

Found on the way: xdg-decoration's `configure` was sent without the `xdg_surface.configure` the protocol requires after it, and a mode was never tied to its toplevel, so nothing could act on it; Shingle's `_MOTIF_WM_HINTS` reading took any non-zero decorations field as decorated where wlroots requires the title and the border, and read hints shorter than five words.

Tested by the unit tests in crates/shingle/src/frame.rs (layout, margins, button order, dialogs, hit order and corner edges, and the press, drag, double-click and release rules), crates/shingle/src/decorator/tests.rs (through the in-process test client: configure sizes, IPC geometries, client-side, unloaded, preferred, forced and ignored views, the buttons, the title drag, a border resize and its cursor, a floating window keeping its client, fullscreen, a reload), crates/shingle/src/xwayland/windows.rs (X windows framed or not by their hints, and a configure request clamped with its frame), crates/shingle-xwayland/src/xwm/props.rs (the hints), crates/shingle-config/src/lib.rs (the import), crates/shingle-frontend/src/frame_raster.rs (pieces, title alignment, size and ellipsis, button pixels), crates/shingle-frontend/src/frontend/tests/frames.rs (the tiles and colours on the GPU, the drag and a button through the frontend's input) and the pinned pixels. The real runs are tools/gpu_test/run.py's group `decoration` (tools/gpu_test/decoration_checks.py) and tools/xwayland_test/run.py's group `decoration`, recorded in docs/status.md.

### WG-7 `shingle-switcher-view`: Wayfire's switcher

Port wayfire:plugins/single_plugins/switcher.cpp's view: thumbnails of the views around the selected one, scaled by `view_thumbnail_scale` and rotated by `view_thumbnail_rotation`, moving at `speed`, drawn by the frontend on the active output and never written to the tiling tree.

Acceptance:

- Frontend input-script tests for selection order, forward and backward, release on modifier up, a view closing while the switcher runs, and one view only.
- Real run in tools/gpu_test/run.py's namespace with offscreen Shingle: `grim` captures while STIPC holds `alt` and taps `tab` show the thumbnails in Wayfire's arrangement, and releasing `alt` focuses the selected view.

#### WG-7 as built

Behaviour was checked against wayfire:plugins/single_plugins/switcher.cpp and wayfire:metadata/switcher.xml at the pinned commit, with wayfire:src/view/view-3d.cpp for the 3D transform and wf-config:src/duration.cpp for its timers.

`SwitcherConfig` (crates/shingle-config/src/lib.rs) holds `speed` (`duration_ms`), `view_thumbnail_scale` and `view_thumbnail_rotation` with Wayfire's defaults (500 ms, 1.0, 30°), YAML `switcher:`; the Wayfire importer reads them when `[core] plugins` lists `switcher`.

- crates/shingle-frontend/src/switcher_view.rs ports the plugin's arithmetic: `SwitcherView` keeps Wayfire's list of thumbnails (`SwitcherView` in Wayfire, one timer and seven transitions each) with `arrange`, `move`, `next_view`, `fill_empty_slot`, `invalidate_last_in_slot`, `rebuild_view_list`, `dearrange`, `cleanup_expired`, `handle_view_removed` and the render order of `calculate_render_order`, including its bubble sort. Its timers are wf-config's `duration_t`: a transition never started is at its end, and every restart starts from the value at that moment. `projection` builds `view_3d_transformer_t`'s total transform for a thumbnail (`translation * perspective * view * depth_scale * rotation * scaling`, the eye at 1/tan(22.5°) and the depth scaled by the larger side of the view's box).
- The renderer draws a tile through a 4x4 matrix (`shingle_render::Projection`): its corners are transformed relative to an origin and the vertex keeps w, so that the GPU interpolates texture coordinates with perspective, as Wayfire's GLES renderer does.
- The frontend (crates/shingle-frontend/src/frontend/switcher.rs) starts the view on the active output for `next_view` and `prev_view` with the views of its current workspace, minimized ones included, by focus order (`Views`), each with its bounding box around its surfaces, subsurfaces and frame (popups too until [whole views](#whole-views-as-built) left them out, as Wayfire's transformed node has none). Each further press moves the selection, releasing any of the first press's modifiers lets go (`handle_done`), and the view stays until the thumbnails are back in place, as Wayfire's `pre_hook` ends the plugin; the `switcher` plugin's `plugin-activation-state-changed` events come at its start and end. While the view is up, the scene of that output (`SwitcherScene`) clears it to black and draws the background and bottom layers dimmed to 60 %, every thumbnail with its frame through its projection in render order, then the top and overlay layers and the cursor (`Compositor::layer_surfaces_on`, `stacking_order_on`), as `switcher_render_node_t` draws over the whole output. Letting go focuses and raises the selected view, restoring it if minimized, as `focus_raise_view` does; `next_view` raises the new selection as Wayfire's `view_bring_to_front` does.

Where the code differs from the plan and from Wayfire, and why:

- The first press of `prev_view` selects the next view, as the first `next_view` does: Wayfire's `arrange` takes no direction and ends with `handle_switch_request(-1)`, and Debian's Wayfire 0.9 does the same (checked by `run.py --checks rehearsal`, 2026-11-27). Only the later presses of `prev_view` go backwards. Shingle used to start `prev_view` from the least recent view, after a reading of the pinned commit that the reference compositor did not bear out; following the code, the design changed with it.
- Wayfire's `move` sets every moved thumbnail's opacity to 0.7, also the one that comes to the middle, because it tests the slot the thumbnail leaves, which is never expired; Shingle keeps that, so after the first press the selection is drawn at 0.7 too, as in Wayfire.
- `speed`'s easing curve is ignored, as for Shingle's other animations: the thumbnails move linearly over its duration.
- Wayfire's input grab keeps the pointer from clients too while the switcher is held; Shingle's switcher takes the keyboard only, as before.
- Scale and Expo, a configuration reload and the loss of the output end the view at once. In Wayfire Scale and Expo cannot start while the switcher plugin is active, including its exit animation.
- Thumbnails of a window rotated by `wrot` are drawn unrotated, and closing animations and the workspace-set label are not drawn on the switcher's output while it is up, as the switcher's node covers them in Wayfire.
- A window minimized while the switcher is held stays in it until the next press rearranges, as in Wayfire, which only rearranges on `view-disappeared`.
- Minimizing the focused view focuses the next one without making it the most recently focused, as in Wayfire; WG-7's test first assumed the opposite and focused that view by hand (see [colour and focus fixes](#colour-and-focus-fixes-as-built)).

Tested by the unit tests in crates/shingle-frontend/src/switcher_view.rs (selection order both ways and mixed, two views and one, every slot's offsets, depth, scale, turn and opacity, expiring copies, the render order, letting go, pressing again during the exit, removed and minimized views, the options and the projection against Wayfire's numbers), crates/shingle-frontend/src/frontend/tests/switcher.rs (input scripts through `FakeHost`: selection order forward and backward, the first press's modifiers and other keys, a view closing while switching, the last one closing, one view and none, a minimized view restored, Scale ending the view, and on the GPU the thumbnails over black with the background layer dimmed and the panel above them, then the usual scene after letting go), crates/shingle-render/src/draw.rs (the projection), crates/shingle/src/output/protocol_tests.rs (layer surfaces and stacking per output) and crates/shingle-config/src/lib.rs (the import). The real run is tools/gpu_test/run.py's group `switcher` (tools/gpu_test/switcher_checks.py), recorded in docs/status.md.

### Colour and focus fixes as built

Two differences WG-6 and WG-7 noted, fixed or settled on branch `shingle-colour-and-focus-fixes`.

**Configured colours.** Wayfire draws through wlroots' GLES2 renderer into an 8-bit framebuffer without sRGB encoding: `glClearColor` and its colour rectangles (wayfire:src/core/opengl.cpp, `clear` and `render_rectangle`; wayfire:src/view/compositor-view.cpp premultiplies a rectangle's colour) write a configured colour's components as they are, so `#336699FF` or `0.2 0.4 0.6 1.0` become the bytes 0x33, 0x66, 0x99. Headless Wayfire 0.9 on the render node with the GLES2 renderer writes exactly those bytes (tools/gpu_test/colour_checks.py). Shingle's targets are sRGB and its tile and clear colours linear, so every configured colour drawn as a solid tile or clear colour showed lighter, `#336699` as 124, 170, 203. crates/shingle-render/src/color.rs decodes a configured colour from sRGB first, `srgb_fill` premultiplied for tiles and `srgb_clear` for the clear colour, as wlroots' Vulkan renderer decodes its rectangles' colours (render/vulkan/pass.c, `color_to_linear_premult`), and the scene uses them for `[core] background_color`, `[expo] background` (the clear colour and the cells), `[vswitch] background`, `[vswipe] background` and `[simple-tile]`/`[move]` `preview_base_color` and `preview_base_border`; the frame's `active_color` and `inactive_color` go through `srgb_fill` instead of WG-6's own decoding. The drop preview's colours are premultiplied now, as Wayfire's are; they were drawn straight into a premultiplied pipeline. Colours CPU code writes into textures as bytes, Scale's `bg_color` and `text_color`, `scale-title-filter`'s and the frame's `font_color`, already showed their bytes, since the textures are sRGB and decode when sampled. Headless Shingle's CPU screencopy writes the background's bytes as they are.

Found on the way and fixed with it: the shader indexed its 256-entry gamma LUT by the tile colour's linear value times 255, truncated, which crushed every dark tone, client textures included (the byte 0x33 came out as 50, every byte below about 13 as 0), so even a decoded colour could not show its bytes. A wlr-gamma-control ramp maps the bytes sent to the display, so the LUT is now indexed by the colour's sRGB byte and its entry decoded again; with the identity ramp every byte comes out as it went in. The pinned pixels `plain`, `scaled_transformed_zoomed`, `frame` and `gamma_hue_inverted` in crates/shingle-render/tests/golden were re-blessed for this, and only for this: their dark pixels are now the bytes drawn, and `gamma_hue_inverted`'s ramp applies to bytes; `nearest_inverted` is unchanged.

Where Shingle still differs: translucent colours blend in linear light, where Wayfire blends bytes; the default `0.1 0.1 0.1` is 25.5 times 255, which Wayfire's GLES2 rounds to 26 and Shingle's GPU encoding may round either way; and inversion (`invert`) and the switcher's and Expo's dimming still work on linear values, where Wayfire's work on bytes, so an inverted 26 is 254 in Shingle and 229 in Wayfire. None of these is a colour option.

**Focus after a minimize.** WG-7's switcher test focused the next view by hand after minimizing the focused one, on the belief that Wayfire records the view it refocuses as recently focused, which would change the switchers' order. It does not: `seat_t::refocus` (wayfire:src/core/seat/seat.cpp) picks the visible view with the latest `last_focus_timestamp` and gives it the keyboard focus without stamping it; only `set_active_node`, through `focus_view`, stamps. So after minimizing the focused charlie of alpha, bravo and charlie, bravo has the focus, charlie is still the most recently focused, and one Alt+Tab selects bravo again. Headless Wayfire 0.9 does exactly that, and so does Shingle, whose `sync_focus` leaves `focus_recency` alone. The task's premise was wrong; following the code, nothing changed in Shingle's focus handling, WG-7's workaround was removed, its test now expects Wayfire's order (bravo, alpha, then the minimized charlie), a new test pins the order after a minimize, and tools/gpu_test/focus_checks.py compares Shingle with headless Wayfire.

### WG-8 to WG-10 `shingle-input-gaps`: text-input-v1, per-device input options and drag icons

Three daily-use gaps, one commit series each: `zwp_text_input_manager_v1` from the [Later](#later) list, per-device input options (the Rare row above), and a Wayland client's own drag icon, which XW-6 noted Shingle had never drawn ([docs/design/xwayland.md](xwayland.md#xw-6-shingle-xwayland-dnd-drag-and-drop)). Each was checked against the pinned Wayfire and the wlroots its `subprojects/wlroots` pins (`d783533`, [Sources](#sources-and-method)).

#### WG-8 as built

crates/shingle/src/text_input.rs keeps text-input v3 and v1 in one model, `TextInputs`, which input-method v1 (crates/shingle/src/input_method.rs) and v2 (input_method_v2.rs) read as before; crates/shingle/src/text_input/v1.rs is `zwp_text_input_manager_v1` version 1, as Wayfire's input-method-v1 plugin offers it (wayfire:plugins/protocols/input-method-v1.cpp, text-input-v1-v3.hpp).

- A v1 text input follows its client's keyboard focus without events. `activate` on the surface that has that focus enables it and sends `enter`; `activate` on any other surface is ignored, as Wayfire ignores it. Activating one disables the seat's other v1 text inputs with `leave`, since Wayfire serves one context at a time; between v1 and v3 the one enabled last is served. `deactivate` and a focus change send `leave`.
- Its state is not double-buffered: `set_surrounding_text`, `set_content_type` and `commit_state` reach input-method v1's context at the end of the turn as `surrounding_text`, `content_type` and `commit_state` with the client's serial; `reset`, `set_preferred_language` and `invoke_action` reach it as `reset`, `preferred_language` and `invoke_action`. `set_cursor_rectangle` takes effect with the next `commit_state` and places input panels, which Wayfire ignores. `show_input_panel` and `hide_input_panel` do nothing, as in Wayfire.
- Every event of an input-method-v1 context goes to a v1 text input as it is, with the serial the input method gave: `commit_string`, `preedit_string`, `preedit_styling`, `preedit_cursor`, `delete_surrounding_text`, `cursor_position`, `modifiers_map`, `keysym`, `language` and `text_direction`.
- `[input-method-v1] enable_text_input_v1` (YAML and i3 `input.enable_text_input_v1`, a WCM checkbox, default true) and `core/plugins` gate the global, which is read at start only, like v3's. Input-method v1 and v2 and the input panel exist while either text-input version is on.

Where the code differs from the task and from Wayfire, and why:

- The task asked for text-input v1 bridged to the input-method-v2 path "the way Wayfire's input-method-v1 plugin does". Wayfire bridges text-input v1 only to input-method v1; its input-method v2 relay (wayfire:src/core/seat/input-method-relay.cpp) serves text-input v3 alone. Shingle's input methods share one model, so v1 text inputs reach both: an input-method-v2 commit arrives as v1's `delete_surrounding_text` (index minus the bytes before the cursor, length both counts) followed by the `commit_string` it applies with, an empty one for a deletion alone, then `preedit_cursor` and `preedit_string`; a commit with neither a string nor a preedit clears a preedit shown. Wayfire's own input-method v1 path is the one Wayfire users run (Fcitx5), and it behaves as Wayfire's.
- Wayfire never sends `enter` or `leave` to a v1 text input (its `activate` and `deactivate` adapters are never called); Shingle sends them as the protocol describes, `enter` on an accepted `activate`.
- Input-method v1's `content_type` numbers purposes as text-input v1 does, and v3 has `pin` at 9, one before v1's `date` to `terminal`. The model keeps v3's numbers and hands input-method v1 v1's: `pin` becomes `digits`. Found on the way: Shingle, like Wayfire, used to pass v3's purpose from 9 on to input-method v1 one too high.

Tested by crates/shingle/src/text_input/tests.rs (through the in-process test client: the global and its option, `activate` needing the focused surface, every state request reaching an input-method-v1 context, every context event arriving unchanged, `deactivate` and focus loss, a second text input taking over, input-method-v2 edits in v1's terms), text_input/v1.rs (purpose mapping and deletion arguments) and crates/shingle-config (the option in Wayfire, i3 and YAML, and `core/plugins`). The real run is tools/output_test/run.py's `wg8_text_input_checks` (tools/output_test/text_input_checks.py), raw-protocol clients against headless Shingle, recorded in docs/status.md. Chromium was not run.

#### WG-9 as built

The task named `[input-device:NAME]` sections for libinput options. In Wayfire those hold only `output` and `calibration` (wayfire:metadata/input-device.xml, src/core/seat/input-manager.cpp, seat.cpp `touchscreen_device_t`); a device's libinput options come from `[input:NAME]`, which wf-config clones from `[input]` when the section starts (wf-config:src/file.cpp `check_section`), and which `pointing_device_t::load_options` reads in place of `[input]` (wayfire:src/core/seat/pointing-device.cpp, src/core/plugin.cpp `get_input_device_section`). Shingle follows Wayfire's code:

- `input_devices.NAME` (crates/shingle-config/src/input_devices.rs, YAML) holds both halves: the 15 libinput options of `[input]` that the native backend applies (`DEVICE_OPTIONS`), each unset unless the entry sets it, and `output` and `calibration` (six numbers, as Wayfire's `sscanf`). The importer reads `[input:NAME]` into the first and `[input-device:NAME]` into the second. An option of `[input:NAME]` Shingle cannot set per device (the XKB and repeat options, `mouse_scroll_speed`, the cursor and the missing ones) warns and follows `[input]`; the section no longer counts as a missing plugin's.
- A device takes the first entry named by its udev `ID_PATH`, `ID_SERIAL`, `LIBINPUT_DEVICE_GROUP` or `DEVPATH` and then by its name, as Wayfire looks sections up (`matching_device`); the two halves are looked up on their own, as Wayfire looks up `[input:...]` and `[input-device:...]` on their own. The native backend reads the four properties from libinput's udev device, gives a device `[input]` with its entry's options in their place, calibrates touch devices with their entry's matrix where libinput can, and maps an absolute device to the output its entry found that way names (crates/shingle-native/src/input.rs, `DeviceOptions`).
- `input/list-devices` reports the sections the native backend applied as two fields of Shingle's own, `input-section` (`input` or `input:NAME`) and `input-device-section` (`input-device:NAME` or null). Nested and headless, whose devices take no libinput options, leave them out, so that their replies stay exactly Wayfire's, as the pinned replies of crates/shingle-nested's tests require. Nested keeps its fixed devices.

Where it differs from Wayfire: wf-config copies `[input]`'s values into `[input:NAME]` only from the lines above the section, so an `[input]` written after it does not reach the device in Wayfire; in Shingle every `[input]` option applies wherever it is written. Options apply when a device is added, as before WG-9; a config reload does not reconfigure devices, where Wayfire's options reload.

Tested by crates/shingle-config/src/input_devices.rs (matching order, the halves on their own, overriding only what is set, every option from Wayfire text, calibration, YAML round trip and validation, a Wayfire file with both sections), crates/shingle/src/ipc.rs (the reported sections) and crates/shingle-native/src/host/tests.rs (through the fake libinput device: options by name and by `ID_PATH`, calibration of a touch device, the send-events mode per device, the sections in `input/list-devices`, an output mapping found by `ID_SERIAL`). Step 52 of [native-testing.md](../native-testing.md#wg-9-per-device-input-options) checks real devices on the test machine; it has not run.

#### WG-10 as built

A `wl_data_device.start_drag` icon surface takes `Role::DragIcon` (crates/shingle/src/shell.rs), as wlroots gives it the `wl_data_device-icon` role (wlroots:types/data_device/wlr_drag.c); a surface with another role is refused with the `role` error. `data_device::drag_icon` is the icon of the current drag while it is not dropped and has a buffer, and `visible_surfaces_on` draws it with its subsurfaces just below the cursor on the pointer's output, above the X drag icons, as Wayfire adds its `drag_icon_t` node at the front of the scene (wayfire:src/core/seat/drag-icon.cpp). Its snapshot has `SurfaceRole::DragIcon`; it and its subsurfaces take no input, so the drag sees the surface under them, it never animates, and it goes with the drop, when wlroots destroys the drag and its icon.

- It is drawn at the pointer moved by the sum of the offsets of its commits since the drag began (`wl_surface.attach`'s and `wl_surface.offset`), as wlroots' scene drag icon accumulates them (wlroots:types/scene/drag_icon.c); a surface reused for another drag starts at the pointer again. Wayfire adds only the last commit's offset (`drag_icon_t::get_position`), so its icon jumps back to the pointer after a commit without one; Shingle follows wlroots there.
- `data_device::sync_icon` pushes a commit event for the icon where it moved to and where it was, and when it goes, so that the frontend redraws both even when a cursor plane shows the cursor and pointer motion alone draws no frame.
- Touch drags, which Wayfire's icon also follows, still do not exist in Shingle, and neither do drags without a source.

Tested by crates/shingle/src/data_device/icon_tests.rs (through the in-process test client: the icon drawn at the pointer by its offsets, below the cursor and above the window, redraw events for both places, offsets adding up, no input, gone with the drop, reused from the pointer, the role error) and crates/shingle-frontend (no animation for a Wayland icon). The real run is tools/gpu_test/run.py's group `drag-icon` (tools/gpu_test/drag_icon_checks.py) with `wl_probe --drag --drag-icon`, recorded in docs/status.md.

### Workspace follow-ups as built

The `shingle-workspace-followups` branch closed the differences the IPC workspace coordinates left open ([ipc.md](../ipc.md#views-on-other-workspaces)) and went through this survey for anything partial or different in the plugins the operator loads (`autostart command expo fast-switcher move oswitch shortcuts-inhibit switcher vswitch wayfire-shell window-rules simple-tile wsets wm-actions vswipe ipc ipc-rules`), against the pinned Wayfire.

Fixed, each with unit tests:

- Unsticking a floating or snapped view leaves it on the current workspace, with only `view-sticky` (wayfire:src/view/toplevel-view.cpp:258-276, wayfire:src/output/workspace-impl.cpp:612); a tiled one keeps its tile (wayfire:plugins/tile/tree.cpp:433-437). crates/shingle/src/shell.rs (`set_sticky`).
- `configure-view` and `stipc/layout_views` that move a view to another workspace by its rectangle send no `view-workspace-changed` (wayfire:plugins/ipc-rules/ipc-rules.cpp:302-310); `Event::WindowMoved` says whether IPC reports a move.
- IPC rectangles are in each output's own plane (wayfire:src/output/workspace-impl.cpp:198-202), as are outputs' workareas (wayfire:src/output/workarea.cpp:20), and floating and snapped windows move with an output that moves in the layout. crates/shingle/src/ipc/workspace_coords.rs, crates/shingle/src/output/moving.rs (`carry_windows`).
- `[move] preview_base_color`, `preview_base_border` and `preview_border_width` colour move's snap preview (wayfire:plugins/single_plugins/move.cpp:494-495), apart from simple-tile's drop preview (wayfire:plugins/tile/tile-dragging.hpp:122-123).

Left, and why:

- A view straddling two workspaces was drawn on its own one only, where Wayfire draws it on both; `shingle-straddling-views` closed it ([straddling views as built](#straddling-views-as-built)).
- `list-views` still leaves out override-redirect X windows, which Wayfire lists as `x-or`: Shingle's unmanaged X windows have no core window and no view id, and every view method would need a second kind of id for them.
- `set-config-options` still changes only the options Shingle keeps live: the others are read once by the part that uses them, and making each one live is work per option, not a fix in IPC.
- The easing curves of animation durations (`vswitch/duration`, `vswipe/duration`, `switcher/speed`, `expo/duration`, `simple-tile/animation_duration`; wf-config:src/duration.cpp:28-35, :333-336) were still ignored; `shingle-animate` applies them ([animate as built](#animate-as-built)).
- Touch-screen gestures and hotspots in `command` and other activators, `stipc/tablet/*` and `stipc/delay_next_tx`, and the `wayfire/configuration`, `reload-config-metadata`, `reload-plugins` and `render-metrics` methods of ipc-rules stay as the tables above rank them: there is no touch gesture recogniser or tablet support to bind them to, `delay_next_tx` delays Wayfire's transactions, which Shingle does not have, and the utility methods describe Wayfire's own build and plugin loader.
- The differences recorded in the as-built sections above for vswipe (WG-5), wayfire-shell (WG-4) and the switcher (WG-7) are unchanged; each is there with its reason.

### Straddling views as built

Branch `shingle-straddling-views` closed the last difference of the workspace follow-ups: a floating window hanging over the edge of its workspace was drawn on that workspace only, clipped at the output's edge.

What Wayfire does, at the pinned commit:

- **The scene.** A workspace set's views share one plane in which workspace `(x, y)` lies `(x - current.x)` output widths and `(y - current.y)` heights from the current one, and a switch moves every view by the difference (wayfire:src/output/workspace-impl.cpp:501-516 `view_visible_on`, :596-617). The output renders whatever of the plane lies over it, so the part of a view past its workspace's edge shows at the facing edge of the neighbour once that is current. A view counts as on a workspace when its geometry, frame included, overlaps it (`view_visible_on`); its main workspace is the one holding its centre (:478-492).
- **Input.** Input goes to the view drawn under the pointer, and a click focuses it through `wayfire_focus::check_focus_surface` and `focus_raise_view(view)` with `allow_switch_ws` false (wayfire:src/core/wm.cpp:117-135, src/core/window-manager.cpp:130-166): the view is raised and focused, and the workspace stays.
- **Stacking.** A set's views of one layer are stacked together, and focusing a view brings it to the front (`view_bring_to_front`), so a view hanging over is above the neighbour's views focused before it and below those focused after.
- **Walls.** vswitch's and vswipe's slides and Expo draw a workspace wall: each workspace is rendered on its own into a buffer of the workspace's size (wayfire:plugins/common/workspace-wall.cpp, src/output/workspace-stream.cpp), so a view is cut at each workspace's edge and shown in every workspace it overlaps, with the wall's gap between the parts. Expo zooms the whole wall, so each workspace is scaled into its cell as a whole (wayfire:plugins/single_plugins/expo.cpp).
- **Screencopy** captures the output's scene, so it shows what is drawn.
- **Lists.** Scale's current workspace is the views whose centre is on the output (wayfire:plugins/scale/scale.cpp:733-749). The switcher, fast-switcher and wm-actions' show-desktop list the views `view_visible_on` the current workspace (`WSET_CURRENT_WORKSPACE`, wayfire:src/output/workspace-impl.cpp:418), so a view hanging over is in them on both workspaces.
- **Moves.** Wayfire has no home workspace: after a move drag the view is wherever its geometry is, and move sends `view-workspace-changed` to the current workspace at the drop (wayfire:plugins/single_plugins/move.cpp:128-133). The grid snaps a view into the current workspace's workarea.

Debian's headless Wayfire 0.9 behaves alike where IPC and `grim` can observe it: `tools/gpu_test/run.py --checks straddle` runs the same steps in it as in offscreen Shingle, a `wl_probe --pattern` 200 pixels into the first workspace's right edge and 100 past it shown to the edge, its other 100 pixels at the second workspace's left edge once that is current, and a click there focusing it without a switch. The slide and Expo are captured in Shingle only; the reference ran without them.

What Shingle does now:

- `visible_surfaces_on` (crates/shingle/src/state.rs) adds, after the current workspace's windows of each stacking layer, the windows of the set's other workspaces whose geometry with its frame, moved by the distance between the two workspaces, overlaps the output (crates/shingle/src/straddle.rs, `straddlers`). Their surfaces, popups and subsurfaces come with `geometry` moved by that distance and with it in the new `SurfaceSnapshot::shift`. Sticky, minimized and unmapped windows are left out, and everything while the desktop is shown.
- They are stacked among the current workspace's windows by focus: each goes directly below the lowest window of the workspace focused after it, or on top (`merge_by_recency`); the workspace's own order is kept.
- Input follows what is drawn: hit testing takes the snapshots, and the pointer's, touch's and drag-and-drop's surface coordinates, pointer constraints and warps, frames and the input panel take the surface's drawn rectangle (`drawn_rect`, `window_shift`). A click on the part shown focuses and raises the window without a switch.
- The CPU screencopy composites the same list; the frontend's captures are of its frames.
- The window keeps its own workspace, the one holding its centre, for IPC (which already reported its rectangle relative to the current workspace), sessions and focus order; Scale, the switchers and the other lists are unchanged.
- The frontend draws a window on vswitch's and vswipe's walls once for each workspace it overlaps, placed as on that workspace and cut to that workspace's cell (crates/shingle-frontend/src/straddle.rs, `wall_copies`, `apply_copies`), so the gap lies between the parts; a window inside its workspace is cut to its cell too, as Wayfire's streams cut it. Expo does the same in its cells (`expo_copies`) and takes a press on any part shown.
- A switch moves a window drawn moved by its shift without animating it as a geometry change, and its frame is drawn with it.
- At the end of a move drag a floating window goes to the workspace holding its centre, drawn where it was dropped, and one drawn here from another workspace is moved onto the current one before it snaps into a slot (`Compositor::settle_on_centre_workspace`, `Compositor::adopt_on_current_workspace`).

Decisions:

1. A window is drawn on another workspace by moving its snapshots (`SurfaceSnapshot::shift`), not by giving it a second place in the model: within one frame an output shows one workspace, so a window appears at most once in the normal scene, and only the walls draw it several times, which the frontend does from the window's own place. Everything that is not drawing or input (IPC, sessions, focus order, `wl_surface.enter`, window captures) keeps the window's one place.
2. A window counts as overlapping a workspace by its geometry with its frame, as `view_visible_on` decides. Its popups go with it; a popup of a window that does not itself overlap the workspace is not drawn there, where Wayfire's scene would draw it.
3. Shingle stacks each workspace on its own, so a window from another workspace is placed by focus order, which is Wayfire's raise order unless a view was raised or lowered without focus.
4. On the walls a tile is cut on the CPU by geometry and texture coordinates (`RenderTile::crop`), which needs no clip in the renderer. A rotated window (`wrot`) and one drawn by an effect (fire, squeeze) cannot be cut so and stay one uncut copy, as before.
5. Expo fitted each window into its workspace's cell on its own, keeping a small window at its size in the middle of the cell and piling a workspace's tiles on top of each other; nothing recorded that as intended. It now scales a workspace into its cell as a whole (`expo::workspace_scaled`), as Wayfire's zoomed wall does, without which a window could not be shown in part in two cells. Shingle's cells still follow its own grid of the output, not Wayfire's square wall.
6. Shingle keeps a home workspace for every window, which Wayfire does not have. Settling a floating window on the workspace holding its centre at the end of a drag keeps the home where Wayfire's main workspace is; adopting it before a snap snaps it into the workspace shown, as Wayfire's grid does. Neither sends `view-workspace-changed`, as the drop sent none before.
7. Performance: `straddlers` is one pass over the surfaces per output and frame that keeps only mapped windows of the output's set on other workspaces before it computes a geometry; with no window past its workspace's edge nothing more happens and the scene is as before. Each output culls on its own set and its own size. On the walls `overlapped_workspaces` looks only at the columns and rows the window spans, and copies are made only while a slide or Expo is drawn.

Where Shingle still differs, and why:

- The switcher, fast-switcher and show-desktop keep to a window's own workspace, as the task asked for focus order; in Wayfire a view hanging over is in them on both workspaces.
- An X window's position as X sees it is its place on its own workspace, so an override-redirect menu it opens while it is drawn from a neighbouring workspace appears where the window lies on its own; in Wayfire the view and its X position move with the switch.
- The workspaces of Shingle's slide show the wall's background colour and not the background layer, as before this branch.

Tested by crates/shingle/src/straddle/tests.rs (through the in-process test client: the shift, the merge by focus, a window drawn on the neighbour and on all four workspaces around a corner and not elsewhere, sticky, minimized and contained windows, a click on the part shown focusing it with the surface coordinates of where it is drawn, the stacking by focus and the click on the window on top, a screencopy of the neighbour, adopting and settling), crates/shingle/src/decorator/tests.rs (a frame hanging over taking hover and its close button where drawn), crates/shingle-frontend/src/straddle.rs (overlapped workspaces at edges, corners and the grid's edge, wall and Expo copies, cutting and the rotated tile left whole), crates/shingle-frontend/src/expo.rs (scaling a workspace into its cell, a press in either cell) and crates/shingle-frontend/src/frontend/tests/straddle.rs (on the GPU: the neighbour's scene without an animation, the click, the slide's two parts 20 pixels apart, vswipe's wall, Expo's two cells, the frame, and a drag by the part shown landing where it is dropped). The real run is `tools/gpu_test/run.py --checks straddle` (tools/gpu_test/straddle_checks.py), recorded in docs/status.md.

### animate as built

The `shingle-animate` branch completed Wayfire's animate plugin and the easing of every wf-config duration. Wayfire was read at the commits of [Sources and method](#sources-and-method): `wayfire:` is `669bbe36`, `wf-config:` is `1a48992a`.

Durations and easing (crates/shingle-config/src/easing.rs). `AnimationDuration::parse` reads the `animation` option type as wf-config's `from_string` does (wf-config:src/duration.cpp:354-420): an integer alone is milliseconds on `circle`; otherwise a number, `ms` or `s`, and an optional curve, `circle` without one; trailing words or an unknown curve are an error, which the importer turns into a warning that keeps the default. `Easing` has wf-config's five curves, `linear`, `circle` (`sqrt(2x - x²)`), `sigmoid` (`(1 + e⁻⁶)/(1 + e^(6 - 12x))`), `easeOutElastic` with its single-precision constants and `cubic-bezier X1 Y1 X2 Y2` solved by ten Newton steps (wf-config:src/duration.cpp:28-57, 302-336). Every config section with a wf-config duration keeps the curve next to its length: `animate` (all eight), `expo`, `scale`, `switcher`, `vswitch`, `vswipe`, `zoom` (`smoothing_easing`) and `simple-tile` (`animation_easing`); the YAML defaults are the metadata's. `crate::animate::Timing` (crates/shingle-frontend/src/animate.rs) pairs the two, and every frontend animation interpolates with `Timing::progress`, `easing(t)`, exactly 1 at the end as `duration_t::progress` (wf-config:src/duration.cpp:145-159), while whether an animation ended stays a question of the linear progress, since `easeOutElastic` passes 1 before the end. Expo's `transition_length`, an `int` option, runs on `circle` as wf-config's `duration_t` constructor gives integer lengths. `[wsets] label_duration` is only a timeout in Wayfire, so its curve is ignored.

Choosing an animation (crates/shingle-frontend/src/frontend/animation.rs, `choose_animation`) follows `get_animation_for_view` (wayfire:plugins/animate/animate.cpp:316-390): the option's own animation where `enabled_for` matches, then `fade_enabled_for`, `zoom_enabled_for` and `fire_enabled_for` in that order, each with its own duration, then the `open-`, `close-` and `minimize-animation-type` and `-animation-duration` view properties, which `window-rules/set-view-property` sets; without a chosen duration each effect takes the one Wayfire registered it with, `duration` for fade, zoom and fire and their own for zap, spin and squeezimize (:227-235). Minimizing runs `minimize_animation` itself with the chosen length, as Wayfire does (:477-480). A close or minimize of the same effect as a running open or restore turns that animation around from where it is, as `set_animation_type` reverses it (:144-161), and restoring a minimized window animates (`ANIMATION_TYPE_RESTORE`). A view minimized before it mapped gets no open animation (:444-452). The matchers are evaluated each frame for the surfaces drawn, since a surface that went cannot be matched any more; view properties are read when the animation is chosen, and the compositor keeps a destroyed window's properties until the next `drain_events` (`Compositor::view_string_property`) so that a close can still read them, as Wayfire reads them before the view unmaps. Layer surfaces now carry their layer (`SurfaceSnapshot::layer`), so the matchers see Wayfire's `background`, `panel` and `overlay` (wayfire:src/core/view-access-interface.cpp:131-141), which `fade_enabled_for`'s default, `type equals "overlay"`, needs.

The effects (crates/shingle-frontend/src/animate/effects.rs) are functions of a `Clock`, a port of `duration_t` that reverses half way (wf-config:src/duration.cpp:89-229):

- `fade` and `zoom` (wayfire:plugins/animate/basic_animations.hpp:8-156): transitions from transparent and a third of the size, flipped when hiding (`1 - easing(t)`); zoom's minimize and restore go to and from the minimize hint's centre and size.
- `zap` (wayfire:plugins/animate/zap.hpp:69-82): opacity in the first third, width in the second, height in the last, the clock reversed when hiding (`easing(1 - t)`).
- `spin` (wayfire:plugins/animate/spin.hpp:167-178): opacity and scale with progress, turned by `progress × 2π × spin_rotations`, counterclockwise as Wayfire's `view_2d_transformer_t` turns (wayfire:src/view/view-3d.cpp:211-223).
- `squeezimize` (wayfire:plugins/animate/squeezimize.hpp): the target is the minimize hint, else 100 by 50 pixels below the middle of the output's bottom edge, and the edge it is nearest picks the vertical or horizontal shader and its direction (:463-533); the area the warp covers and the boxes are Wayfire's (:342-397), and the warp is a WGSL port of both of Wayfire's GLSL shaders (crates/shingle-render/src/shader.wgsl, `fs_squeeze`), drawn by `TileEffect::Squeeze`.
- `fire` (wayfire:plugins/animate/fire/): the length is scaled by `min(height / 400, 3)` in whole milliseconds; each frame of the output moves the progress line, spawns a tenth of the system's particles there while the clock runs and updates every particle once, frame-rate dependent as Wayfire's, and the system holds `fire_particles × min(width / 400, 3.5)`. The view is drawn above the line and the particles over it; the animation lasts until the last particle died. The particle system runs on the CPU as Wayfire's does, with a seeded generator in place of `std::rand`, and the renderer draws the particles instanced in Wayfire's two passes, darkening with `ZERO, ONE_MINUS_SRC_ALPHA` and adding with `SRC_ALPHA, ONE` (`TileEffect::Particles`; wayfire:plugins/animate/fire/particle.cpp:290-338).

The startup fade (crates/shingle-frontend/src/frontend/animation.rs, `startup_frame`): each output scene starts black until a `background`-layer surface is drawn on it or 400 ms pass (wayfire:plugins/animate/animate.cpp:242-256, :429-442), which `Frontend::next_deadline` wakes the host for, then a black tile over everything fades out over `startup_duration` with its curve (wayfire:plugins/animate/system_fade.hpp); a length of 0 turns both off.

The minimize hint: `zwlr_foreign_toplevel_handle_v1.set_rectangle` is kept per window in layout coordinates, relative to the surface it names, as Wayfire's `handle_minimize_hint` does (wayfire:plugins/protocols/foreign-toplevel.cpp:209-222), and `Compositor::minimize_hint` reports it; an empty rectangle removes it, as the protocol says, where Wayfire keeps it and ignores it.

Cost: the squeeze and particle pipelines are built when a frame first has such a tile, and frames without one draw as before; fire's particles are stepped only while a fire animation runs; once an output's startup is over, nothing of animate runs per frame but the matchers' evaluation the frontend did before.

IPC: Wayfire's animate registers no IPC method and reports no plugin activation; its only IPC surface is the view properties above, which work.

Differences, and why:

- ~~Animations apply per surface, as Shingle's open animation did before: a view's subsurfaces and popups are not transformed with it, and a framed window's frame follows fade and zoom but is not drawn while it spins, squeezes or burns.~~ Closed by [whole views](#whole-views-as-built): every effect transforms a view's subsurfaces and frame with it; its popups are views of their own, as in Wayfire, and are not transformed with it.
- Particle colours are decoded from sRGB and blended in linear light, as every Shingle colour, where Wayfire blends the sRGB bytes; the fire looks somewhat brighter. The particles go through the gamma ramp and inversion of their output, which Wayfire applies to the whole output afterwards.
- Wayfire disables fire, spin and squeezimize on its Pixman renderer; Shingle has only the GPU renderer.
- Wayfire's horizontal squeeze shader compares the upward `uv.y` with the downward box edges (wayfire:plugins/animate/squeezimize.hpp:161-164), where the vertical one converts them (:92); the port keeps the formulas as they are, and only the vertical shader, which the bottom-edge fallback target uses, was checked against captures.

Tests: 5 in crates/shingle-config/src/easing.rs (each curve against wf-config's formula, `easeOutElastic`'s overshoot, the Bézier's Newton solve, parsing and printing), 11 in crates/shingle-frontend/src/animate/ (each effect's transform at chosen progress values against Wayfire's formulas, reversal half way, the squeeze target's edges and uniforms, fire's line, spawning and end, the particle update and spawn), 2 in crates/shingle-frontend/src/frontend/animation.rs (the transforms applied to tiles), 4 in crates/shingle-frontend/src/frontend/tests.rs (the choice order and properties, a minimize turned into a restore, the startup wait and fade, layer view types), 3 in crates/shingle-render (squeeze vertices, particle placement, `RenderTile::crop`) and 2 pixel tests in crates/shingle-render/tests/offscreen_pixels.rs (a squeeze at 0 is the window and at 1 empties its corner; particles darken then light), and 2 in crates/shingle/src/output/protocol_tests.rs (`set_rectangle`'s hint, a gone window's properties for one drain). The real run is tools/gpu_test/run.py's `animate` group (tools/gpu_test/animate_checks.py), recorded in [status.md](../status.md#animate).

### Whole views as built

Branch `shingle-animate-whole-view` closed the first difference of [animate as built](#animate-as-built): animate's effects were applied to each surface's tile on its own, so a view's subsurfaces stayed where they were or went at once, and a framed window's frame followed fade and zoom but was not drawn while it spun, burnt or was squeezed.

What Wayfire does, at the pinned commit:

- **The node an effect transforms.** Every effect adds a transformer to the view's transformed node (wayfire:plugins/animate/basic_animations.hpp:34-35 and :127-128, zap.hpp:64-66, spin.hpp:67-69, squeezimize.hpp:581-587, fire/fire.cpp:266). Below it is the view's surface root node: the main surface with its subsurfaces, and the decoration, which the decoration plugin adds at the back of that node (wayfire:plugins/decor/deco-subsurface.cpp:361).
- **Drawing it.** A transformer renders its children into an auxiliary buffer at the output's scale and draws that buffer with its effect (`transformer_render_instance_t::get_texture`, wayfire:src/api/wayfire/view-transform.hpp:129-151); when its one child can hand out a texture directly, a view of a single surface, it takes that texture instead (`zero_copy_texture`). `view_2d_transformer_t` scales and turns about the centre of the toplevel's geometry, which includes the decoration, and samples the buffer bilinearly (wayfire:src/view/view-3d.cpp:174-189, :260-300). Fire and squeezimize work in the children's bounding box (fire.cpp:179-192, squeezimize.hpp:344).
- **Closing.** At unmap animate puts an `unmapped_view_snapshot_node` in the view's place, which renders the surface root node into a buffer once (`view_interface_t::take_snapshot`, wayfire:src/view/view.cpp:94-116, plugins/animate/unmapped-view-node.hpp), and the closing effect transforms that.
- **Popups are views of their own.** An `xdg_popup` is a `wayfire_xdg_popup` view whose node goes into the output's unmanaged layer, or its parent's layer above the workspace one, and a nested popup's into its parent popup's root node (wayfire:src/view/xdg-shell.cpp:184-214); none is under its toplevel's transformed node, so no effect of the toplevel transforms it, and animate's default `enabled_for` does not match their type, `popup`. A dialog is under its parent's root node, above the transformed node (wayfire:src/view/toplevel-view.cpp:156), and animates on its own.
- **The overviews.** Scale gives each view a `view_2d_transformer_t` (wayfire:plugins/scale/scale.cpp:199-233), so the view's subsurfaces and frame scale and fade with it. Expo and vswitch's and vswipe's walls render the output's nodes per workspace (wayfire:src/output/workspace-stream.cpp:49-70), so subsurfaces move with their views. The switcher draws each thumbnail from the view's transformed node (wayfire:plugins/single_plugins/switcher.cpp:729-739) under a render node added on top of the whole scene (:371-372), so no popup shows while it is up.

What Shingle does now:

- A view is a root surface, a toplevel, layer surface, popup or X window, with the subsurfaces below it (`subsurface_roots`, `view_members`, crates/shingle-frontend/src/frontend/whole_view.rs). Subsurfaces are no views, so they get no animation of their own (`view_type`).
- An opening or restoring view with subsurfaces or a frame is drawn each frame into a texture of its own: its frame, then its surfaces, at the output's scale (`Frontend::draw_whole_views`), with `shingle_render::draw_view`, which composites without the output's gamma ramp and inversion so that they apply once, when the texture is drawn (crates/shingle-render/src/view.rs). The texture is kept per output and view while its size stays and goes once the animation ended (`ViewTextures`). Its one tile takes the effect: the 2D effects scale and turn it about the centre of the window's geometry with its frame, for which the texture's area is widened to be centred there (`whole_view::centred`), and fire and squeezimize work in the bounds of all the pieces (`AnimatedView`, `apply_animation`). The frame is not drawn apart, and the view's subsurfaces are not drawn apart either.
- A closing or minimizing view is drawn once, on its output's next frame, from its pieces as the last frame drew them (`ClosingImage`): its surfaces with their textures and buffers, and its frame, taken at the start of the batch of events that destroys it, before any of its surfaces is forgotten (`Frontend::view_pieces`); a gone window's frame title stays until then (`departed_titles`), and the frame keeps the pieces' DMA-BUF locks until the GPU is done.
- A view of one surface without a frame keeps drawing from its surface's texture, as Wayfire's `zero_copy_texture`, so that it costs no pass, and a view's popups are drawn as their own views, untransformed.
- The window geometry an animation starts from, which zoom's minimize hint and squeezimize measure from, is the window's with its frame (`Frontend::framed_geometry`), Wayfire's toplevel geometry.
- In Scale, Expo and on vswitch's and vswipe's walls each subsurface takes its view's window, role and geometry with its frame, so that it is placed, scaled, cut to the cells and faded as its window is; Expo and the walls list the subsurfaces of the windows they show (`Frontend::with_subsurfaces`), where they listed toplevels only.
- The switcher's thumbnails are the window's view without its popups (`whole_view::window_view`).

Decisions:

1. The design asked for the popups that Wayfire includes; Wayfire includes none, since its popups are views of their own outside the toplevel's transformed node. Shingle follows the code: popups are neither drawn into the view's texture nor transformed with it, in the animations and the switcher alike.
2. Only the animations draw a view into a texture. Scale, Expo and the walls only translate and scale a view, which carries each piece exactly where a texture would put it, so they place each piece's tile instead and need no pass per window and frame; they differ from Wayfire's buffers only where a view's pieces overlap at less than full opacity, where Wayfire fades the view as one.
3. An opening view's texture is drawn again every frame, where Wayfire redraws only its damage; the animations are short, and the cost below is measured with that.
4. A closing view is drawn from the last frame drawn before the client's batch of events that destroys it; Wayfire takes its snapshot at `view-pre-unmap`, which shows the same contents. A subsurface destroyed in an earlier batch than its view, which the frame between showed gone, is not in it, as it is not in Wayfire's.
5. Fire's length scales with the height of the window's geometry with its frame; Wayfire scales it with the transformed node's bounding box, which is larger only when a subsurface reaches out of the window.
6. A window rotated by `wrot` and the tiling's geometry crossfade still draw per surface, as before: a rotated framed window is drawn without its frame.

Cost: nothing without an animation, where no view is drawn apart and the scene only notes which subsurfaces belong to which view; a closing view costs one pass, in its first frame; an opening one a pass per frame while it opens. Eight views of a surface, a subsurface and a frame each, opening at once on a 3840x2160 output take 0.9 ms a frame (median, p95 1.6 to 1.9 ms) with zoom or spin against 0.4 to 0.5 ms for views of one surface, and fire 4.6 against 3.3 ms; closing they take what views of one surface take ([status.md](../status.md#animate), `--checks animate-cost`).

Tested by crates/shingle-render/src/view.rs (a view texture faded as one, so a covered piece does not show through, and graded once), crates/shingle-frontend/src/frontend/whole_view.rs (roots of nested subsurfaces and a cycle, bounds, the centred area and the texture's area per effect, texture sizes and the limit) and crates/shingle-frontend/src/frontend/tests/whole_view.rs (on the GPU: a zooming window's subsurface and frame drawn into one texture, its one tile zoomed about the frame's centre with the subsurface's green and the frame's blue where the zoom puts them and nothing where the subsurface was, and its popup in full at its own place; a closing view drawn once with its subsurface and frame, from a client that destroys its subsurface first and from one that disconnects; a minimizing view drawn whole; a spinning window's frame drawn with it; nothing drawn apart without an animation, for a view of one surface, or after the animation, and no texture kept; Scale and Expo placing a subsurface with its window; the switcher's view without the popup). The real run is `tools/gpu_test/run.py --checks animate`, which now also animates a probe with a subsurface, a popup and a server-side frame with every effect, and `--checks animate-cost`, which times the frames; both are recorded in [status.md](../status.md#animate).

### Initial size and placement as built

Two differences the config rehearsal left (`shingle-config-rehearsal`), closed on branch `shingle-initial-size`. What follows is read from the pinned Wayfire (669bbe36); Debian's Wayfire 0.9, run headless by `run.py --checks rehearsal`, behaves the same in every row the rehearsal compares: an untiled or floating `wl_probe` at its own 320x240 at (100, 100), and the autostart clients on the outputs the focus had when each was created.

**Initial size.** Wayfire's xdg toplevel answers its initial commit with the state committed before the view mapped (wayfire:src/view/xdg-shell/xdg-toplevel.cpp, `handle_surface_commit`), which is 0x0 with no states unless a fullscreen request came first, so the client picks its size. The view maps at that size at its own position, which a toplevel's state starts at 100, 100 in its output (wayfire:src/api/wayfire/toplevel.hpp), clamped to the workarea and placed by the place plugin in the map transaction if it is loaded (wayfire:src/view/view-impl.cpp, `adjust_view_pending_geometry_on_start_map`; plugins/single_plugins/place.cpp, `on_new_tx`). Only then does simple-tile tile it, on `view-mapped` (plugins/tile/tile-plugin.cpp, `on_view_mapped`), and the tile's size goes out as the second configure; tiling records the mapped geometry as the view's last windowed geometry (plugins/tile/tree.cpp, `view_node_t::set_geometry`, through `update_last_windowed_geometry`, which only records a view that is mapped and not tiled yet), and untiling with `key_toggle` gives it back (src/core/window-manager.cpp, `tile_request` with 0 edges). X views are the same: they map at the size and position they asked for and tile after.

Shingle created a new window in its tree as soon as the toplevel had an app ID, so its first configure was the tile's size, the neighbours shrank before it drew, and a window that floated from the start (outside `tile_by_default`, by a window rule) or was untiled before it ever floated kept that tile's size. Now a new window is held out of its tree until it maps (`held` in `materialize_toplevel`, crates/shingle/src/shell.rs, released by `finish_map`), every toplevel without a buffer that has not mapped is configured 0x0 (`chooses_own_size`), the tiling decision is taken when it maps, and a window that tiles then records its own size, placed as `place/mode` places a floating view, as the geometry untiling returns to (`windowed_on_map`, crates/shingle/src/placement.rs). Placement clamps the size to the workarea first, as Wayfire's map does, and with `place/mode = none` (no place plugin) a view starts at 100, 100 in its output, where Shingle had put it at the workarea's corner. X windows were already configured only once they mapped; they now also untile to the size they mapped with.

**Output of a new view.** Wayfire gives a view the focused output when it creates it, at `get_toplevel` for xdg (src/view/xdg-shell/xdg-toplevel-view.cpp, `xdg_toplevel_view_t::create`) and when a new X surface appears for X (src/view/xwayland/xwayland-toplevel-view.hpp, `create`), and `adjust_view_output_on_map` keeps it at map, except that a dialog goes to its parent's output; there is no launch-time output and nothing by PID. A `start_on_output` rule that moves an earlier view moves the focus with it, so a client whose toplevel is created after that opens on the new output, and one created before on the old. Shingle took the output active when the window was created in its tree, at the app ID or the first buffer. It now records the active output when the toplevel is created (`created_output`; for X windows when the window manager first reports the window mapped, which is the first Shingle hears of it) and opens the window there, on that output's current workspace at the time it is created, or on the active output if that one has gone. A window unmapped and mapped again opens where it was.

The rehearsal's autostart check compared where clients started at once landed, which depended on which of them mapped first, a matter of timing. Its recorder now starts each runnable entry only after the one before it mapped a window (or 45 s passed), in file order, under both compositors (`release_in_order`, tools/gpu_test/rehearsal_checks.py), so the comparison is of the rule.

Where Shingle still differs, deliberately:

- A floating window that chose its own size is configured with that size after it maps; Wayfire keeps sending 0x0 until something resizes it. The client draws the same either way.
- A window unmapped by a null buffer and mapped again is a new Shingle window, so its initial configure is 0x0 again; Wayfire's view keeps its committed state and sends its old size.
- A maximize request before the first map is still ignored for a window that will tile; Wayfire applies it to the unmapped view (workarea and the maximized state in the initial configure) and simple-tile then tiles it.
- An X window's output is the one active at its map request rather than at its creation, since the window manager reports nothing earlier.

### Later

Not planned in detail, in rank order (the animations and per-type animation options are done since `shingle-animate`): `blur`; keyboard options per device and the missing input options; `ext_data_control_manager_v1` and the ext image-capture protocols with DMA-BUF and damage in screencopy; `zwp_tablet_manager_v2` with libinput tablets in the native backend; `wobbly`; `cube` with the idle screensaver and wrot's 3D mode; the remaining IPC methods; the rare protocols. Each needs its own design when it comes up.

## Decisions

The operator asked for the work to continue without stopping for questions, so on 2026-11-26 the proposed defaults were adopted on the operator's behalf. Each can be revisited before the workspace that depends on it lands.

1. **Bad values in Wayfire or i3 files** warn and keep the default instead of refusing the file. YAML stays strict.
2. **A Wayfire import starts from Wayfire's defaults.**
3. **Shingle-only bindings are off under a Wayfire file**, unless set explicitly.
4. **`core/plugins` is honoured** for every plugin Shingle implements, including `simple-tile`, but not `ipc`, which `--ipc` governs.
5. **Unsupported animation names:** `squeezimize` becomes `zoom`; `fire`, `zap` and `spin` become `fade`; one warning each. (Superseded by `shingle-animate`, which implements all four.)
6. **`XDG_CURRENT_DESKTOP`** is set to `wayfire` when unset, as Wayfire does; no Shingle portals file.
7. **Touchpad gestures in tests:** add a Shingle-only `stipc/feed_gesture`.
8. **`zwf_shell_manager_v2`** is not restricted for sandboxed clients, as in Wayfire.
9. **`wayfire-plugins-extra`** (fisheye and others) stays out of scope.
10. **Initial size** (`shingle-initial-size`): follow Wayfire exactly, a 0x0 initial configure and tiling at map, although it adds a configure round trip before a tiled window's first tile-sized frame; measured with `run.py --checks first-frame`, it costs Firefox about 25 to 60 ms and kitty and foot nothing ([Initial size and placement as built](#initial-size-and-placement-as-built)). Deciding the tiling before the initial configure, which would avoid the round trip, was rejected because the tile's size would then differ from Wayfire's for every view a rule or `tile_by_default` matches only once its title is set.
11. **Output of a new view** (`shingle-initial-size`): the output focused when the toplevel is created, as Wayfire's; no launch-time output and no matching by PID, which Wayfire does not have.
12. **The rehearsal's autostart order** (`shingle-initial-size`): the harness starts the runnable autostart entries one after another in file order under both compositors, since the order they map in is timing; the output each opens on is still compared.

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
