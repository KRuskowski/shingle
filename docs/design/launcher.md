# Application launcher design

Status: accepted, 2026-11-27, with the defaults the lead proposed adopted on the operator's behalf ([Decisions](#decisions)); the operator reviews them later. Roadmap item 8 ([roadmap.md](../roadmap.md#8-application-launcher)). What was built is in [As built](#as-built), and the sections below are corrected where the code differs.

The operator starts applications with `rofi` and has always preferred the KDE start menu. From rofi they value its speed, type-to-search and that it works with the keyboard alone; from KDE, adding shortcuts in place, categories and marking favourites. Both must hold at once: the keyboard path stays as fast as rofi even with categories and favourites shown. This document decides how `shingle-launcher` is built, where its data lives, how it looks and how it is driven.

Sources: Shingle on branch `shingle-launcher`, cut from `master` at `a546046`; Gloomy at the pinned commit `42673c8` (the root `Cargo.toml`); the freedesktop Desktop Entry Specification 1.5 and Desktop Menu Specification 1.1 for `.desktop` files and their main categories; wlr-layer-shell-unstable-v1 version 4.

## Summary

| # | Topic | Decision |
| --- | --- | --- |
| 1 | Process model | A separate binary `shingle-launcher` (crate crates/shingle-launcher), started per invocation by a key binding and exiting after it launches something or is dismissed. A second invocation while one is open closes the first, so the binding toggles it. |
| 2 | Surface | A wlr-layer-shell client on the overlay layer, anchored to all four edges of the focused output with an exclusive zone of -1 and exclusive keyboard interactivity. The surface dims the whole output and draws the panel in its middle; a click outside the panel closes it. It therefore works under Wayfire and other wlroots compositors too. |
| 3 | Toolkit and drawing | Gloomy's widgets, layout and `GloomyRenderer` draw the panel into an offscreen wgpu texture, which is read back into a `wl_shm` buffer. The launcher speaks Wayland itself (wayland-client), as shingle-lock does, because Gloomy's windowing (gloomy-app on winit 0.29) cannot make layer surfaces. |
| 4 | GPU adapter | By default the panel is drawn on lavapipe, Mesa's CPU Vulkan driver, with only its ICD loaded; the hardware GPU is used only when lavapipe is missing or `launcher.renderer: hardware` asks for it. Measured below: NVIDIA's Vulkan instance and device take 115 to 140 ms to create, lavapipe's 15 to 25 ms. |
| 5 | Discovery | `.desktop` files from `$XDG_DATA_HOME/applications` and every `$XDG_DATA_DIRS/applications`, by desktop file ID with the first directory winning, filtered as the specification says; and every executable on `$PATH`, by name, as rofi's run mode lists them. |
| 6 | Index cache | The index is cached as JSON in `$XDG_CACHE_HOME/shingle/launcher-index.json`. A start draws from the cache and always rescans in a background thread after the first frame; a changed index replaces the shown one and the cache. Without a cache the first start scans before drawing. |
| 7 | Matching | A fuzzy subsequence match in the style of fzy, case-insensitive, scoring word starts, consecutive runs and prefixes, against the shown name, then the generic name, keywords and program name at lower weight. Ties and near ties are broken by frecency. |
| 8 | Ranking by use | Each launch records a count and a time per entry in `$XDG_STATE_HOME/shingle/launcher-history.json`. Frecency is `ln(1 + count)` scaled by `1 / (1 + age_days / 7)`. It adds to the match score, and with an empty query it orders the Recent view. |
| 9 | The user's layout | One human-readable YAML file, `$XDG_DATA_HOME/shingle/launcher.yaml`: favourites in order, categories in order with their entries in order, renames, hidden entries and custom shortcuts with a name and a command. It is written on every change, atomically (a temporary file renamed over it). |
| 10 | Keyboard | Typing filters at once, Enter launches the selection, Escape clears and then closes. Ctrl+D favourites, F2 renames, Ctrl+N adds a shortcut from the typed text, Ctrl+M moves to a category, Ctrl+H hides, Ctrl+Up and Ctrl+Down reorder, Delete removes a shortcut. Tab and Shift+Tab, or Left and Right with an empty query, change category; Shift+F2 renames it and Ctrl+Shift+Up and Down move it. |
| 11 | Mouse | A click launches or selects a category; a right click opens a context menu with the same edits; dragging an entry reorders it in its list, onto a category moves it there, and onto Favourites favourites it. The wheel scrolls. |
| 12 | Theming | A `launcher:` section in Shingle's YAML config: panel size, font size, colours (backdrop, panel, field, text, dim text, accent, selection), terminal and renderer. Colours are the bytes shown, as every colour of Shingle's config. Defaults are a dark slate panel with a blue accent. A Wayfire or i3 configuration gets the defaults. |
| 13 | Launching | Commands run through crates/shingle-session's `launch_command` (`/bin/sh -c`) with a `ClientEnvironment` built from the launcher's own environment, which Shingle (or Wayfire) gave it: its `WAYLAND_DISPLAY`, `DISPLAY` and `WAYFIRE_SOCKET` pass on and `WAYLAND_SOCKET` never does. Shingle learns each window's launch identity from `/proc` as for any client, so session restore relaunches it. |
| 14 | Binding | `data/shingle.yaml` binds `super+space` to `shingle-launcher`. A Wayfire import adds no launcher binding, but a `[command]` binding that runs a bare `rofi -show drun`, `run` or `combi` runs `shingle-launcher` instead, with a warning, as `swaylock` becomes `shingle-lock`; `/usr/bin/rofi`, `-dmenu`, other modes and commands with shell syntax keep rofi (revised 2026-11-28 at the operator's request after the first native session). |
| 15 | Startup target | First frame under 100 ms after `exec` in a release build on this host, measured by the launcher itself and by the GPU harness. Met: 44 to 53 ms median of warm starts ([As built](#as-built)), and still met with icons ([Icons as built](#icons-actions-scale-and-wcm-as-built)). |
| 16 | Icons | Each entry's `Icon` is looked up as the Icon Theme Specification says (the theme of `launcher.icon_theme`, else KDE's or GTK's setting, its parents, `hicolor`, the pixmaps directories), PNG and SVG are decoded at the drawn size in device pixels, and the result is cached in one file per theme and size. The first frame draws what the cache holds; the rest is decoded in a thread and drawn as it arrives ([section 12](#12-icons)). |
| 17 | Desktop actions | `[Desktop Action …]` groups are sub-entries: search lists an action when the query matches its name, and the entry's context menu, opened by a right click, the Menu key or Shift+F10, starts with its actions ([section 13](#13-desktop-actions)). |
| 18 | Fractional scale | With `wp_fractional_scale_v1` and `wp_viewporter` the buffer has the output's device pixels at the preferred scale and the viewport gives its logical size; Gloomy draws text at that scale and icons are decoded at it ([section 2](#2-surface)). |
| 19 | WCM | A Launcher page in shingle-wcm edits the `launcher:` section like the other pages ([section 8](#8-theming)). |

## What exists today

Shingle has a layer-shell implementation (crates/shingle/src/layer_shell.rs) that honours `keyboard_interactivity: exclusive`, a command binding type (crates/shingle-config/src/lib.rs, `CommandBinding`) and the client environment every child gets (crates/shingle-session/src/environment.rs:62, `ClientEnvironment::new`). Two clients of its own exist: WCM (crates/shingle-wcm) on Gloomy's winit window, and shingle-lock (crates/shingle-lock), which speaks wayland-client directly, draws with shingle-render's offscreen renderer and copies the pixels into `wl_shm` buffers. There is no launcher; the operator uses rofi, which the application sweep checks (tools/gpu_test/app_checks.py, `app-rofi`).

## 1. Process model

### Decision

`shingle-launcher` is its own binary and process. A binding runs it, it shows the panel, and it exits once it has launched something or been dismissed. Nothing stays resident.

A second invocation while one is open closes the first, so pressing the binding again toggles the launcher as the KDE menu key does. The first instance binds a Unix socket `$XDG_RUNTIME_DIR/shingle-launcher-<WAYLAND_DISPLAY>.sock`; a second instance that finds the socket alive writes `close` to it and exits with status 0. A socket nobody answers is stale and is replaced. The display is part of the name so that a nested Shingle's launcher and the host's do not close each other.

### Rejected alternatives

- **Drawn by the frontend.** Shingle's frontend could draw the launcher as it draws the switcher, which would start in no time. But it would not work under Wayfire, it would put text input, editing and a YAML store into the compositor, and a bug in it would take the session down. A client is what rofi and the KDE menu are.
- **A resident daemon** that shows a prepared surface on a signal would start fastest, but costs memory all day, needs a way to be told to show (IPC or a signal), and its stale state after an index change is a new class of bugs. Per-invocation is simpler and, with [section 4](#4-gpu-adapter-and-startup-time), fast enough.

## 2. Surface

### Decision

The launcher creates one `zwlr_layer_surface_v1` on the overlay layer with namespace `shingle-launcher`, on the compositor's choice of output (the focused one, as Shingle and Wayfire pick when the output is null). It anchors to all four edges with size 0×0, so the compositor configures it to the whole output, sets an exclusive zone of -1 so panels do not push it, and asks for exclusive keyboard interactivity, so every key goes to it while it is open.

The buffer covers the output. Outside the panel it is a translucent black backdrop (`launcher.colors.backdrop`, premultiplied `ARGB8888`), which is written once per buffer; each frame then draws only the panel and copies its rectangle into the free buffer, damaging that rectangle alone. A click on the backdrop closes the launcher, as clicking outside rofi does.

The panel is `launcher.width` × `launcher.height` logical pixels, centred, and shrinks to fit a smaller output. When the compositor offers both `wp_fractional_scale_v1` and `wp_viewporter`, the launcher asks for the surface's preferred fractional scale, makes its buffer the output's logical size times that scale in device pixels, sets the buffer scale to 1 and the viewport's destination to the logical size, and Gloomy draws at that scale factor, so that text is rasterised at its device size; the panel's device rectangle comes from its rounded edges, so it meets the backdrop without a gap. Without them the buffer scale follows `wl_surface.preferred_buffer_scale` (version 6) when the compositor sends it, else 1.

### Rejected alternatives

- **A panel-sized surface.** Smaller buffers, but a click outside the panel would reach the window under it and the launcher could not close on it.
- **An `xdg_toplevel`.** The tiler would tile it, and it would not get the keyboard exclusively.

## 3. Toolkit and drawing

### Decision

The panel is built from Gloomy widgets (`Container`, `Label` and `TextInput`, which draws the search field and its caret), rebuilt from the launcher's model on each change and drawn with `GloomyRenderer` into a `Bgra8Unorm` texture, whose bytes are already `wl_shm`'s little-endian `ARGB8888`. The format is not an sRGB one, so a configured colour is the bytes it shows as, as with every colour of Shingle's config. Every widget is placed absolutely (Gloomy's `Direction::None`), so the launcher knows each row's and category's rectangle and hit-tests the pointer against them itself, rather than through Gloomy's `hit_test`. Gloomy draws all text after all boxes, so the context menu and the dragged entry are a second tree drawn on Gloomy's overlay layers, above the rows' text.

Input, the event loop and Wayland are the launcher's own: wayland-client with calloop, xkbcommon for the keymap, and calloop timers for key repeat, as in shingle-lock. Gloomy's own input handling is not used, because its focus and event model are winit's.

Gloomy pins wgpu 0.20 and winit 0.29, while Shingle uses wgpu 30; the launcher depends on wgpu 0.20 directly for the device it hands Gloomy. Both versions are already in `Cargo.lock` through WCM.

### Rejected alternatives

- **gloomy-app's winit window.** winit 0.29 has no layer-shell support, so the launcher would be an ordinary toplevel.
- **A wgpu surface on the layer surface** (wgpu can create one from raw Wayland handles) would avoid the copy, but on NVIDIA it needs a swapchain, which needs the hardware device ([section 4](#4-gpu-adapter-and-startup-time)) and, in the test namespace, nvidia-modeset. The panel is small: copying its pixels takes well under a millisecond.
- **Changing Gloomy** to add a layer-shell host. Gloomy is the operator's own toolkit and is changed in its own repository; the launcher needs nothing from Gloomy that the pinned commit lacks.

## 4. GPU adapter and startup time

### What was measured

`crates/shingle-launcher/examples/gpu_start.rs` times wgpu 0.20's Vulkan instance, adapter, device and a `GloomyRenderer` for a 720×480 panel, in a release build, inside tools/gpu_test/run.py's namespace on this host (RTX 3060, driver's own ICD plus Mesa's), three runs each, the first with a cold shader cache:

| Adapter | Instance | Device | Renderer | Total |
| --- | --- | --- | --- | --- |
| Loader default (all ICDs, implicit layers), NVIDIA picked | 66 to 122 ms | +55 to 65 ms | +6 ms warm, +35 ms cold | 128 to 222 ms |
| NVIDIA's ICD alone | 60 to 80 ms | +55 to 60 ms | +6 ms | 119 to 143 ms |
| lavapipe's ICD alone, implicit layers off | 10 to 14 ms | +7 to 25 ms | +8 to 25 ms | 24 to 64 ms |

NVIDIA's driver alone takes longer than the whole budget, and the Vulkan loader creates an instance on every ICD it finds, so even choosing lavapipe after loading all of them costs NVIDIA's 60 ms.

### Decision

With `launcher.renderer: auto` (the default) the launcher looks for lavapipe's ICD manifest (`lvp_icd*.json` in the Vulkan loader's ICD directories) and, when it finds one, sets `VK_DRIVER_FILES` (and the older `VK_ICD_FILENAMES`) to it and `VK_LOADER_LAYERS_DISABLE=~implicit~` before creating the instance, restoring the variables as they were right after, so no launched application inherits them. Without lavapipe, or with `renderer: hardware`, the loader's default applies. `renderer: software` requires lavapipe and fails without it.

Drawing a frame of a few hundred rectangles and glyphs on lavapipe takes a few milliseconds, which keeps typing at frame rate; the GPU buys nothing for a panel copied into shared memory anyway.

The rest of the start is kept off the critical path: the Wayland connection and registry round trip, reading the config, the layout, the history and the cached index happen while nothing waits on the GPU, the background rescan starts after the first frame, and fonts are Gloomy's embedded Inter, so no font discovery runs.

The launcher prints `shingle-launcher: first frame after N ms` with `--timing`, measured from `main` to the commit of the first buffer, and the harness measures from `exec` to that line.

### Rejected alternatives

- **The hardware GPU with a CPU-drawn first frame.** The first frame would have to be drawn twice, by two code paths that must look the same.
- **Drawing on the CPU without wgpu** (tiny-skia, which Gloomy already depends on) would be faster still, but would not be Gloomy.

## 5. Discovery

### Decision

**Desktop entries.** The launcher reads `applications/` under `$XDG_DATA_HOME` (default `~/.local/share`) and then each directory of `$XDG_DATA_DIRS` (default `/usr/local/share:/usr/share`), recursively. An entry's desktop file ID is its path below `applications/` with `/` replaced by `-`; the first directory that has an ID wins, so a user's copy overrides the system's, and a user's `Hidden=true` copy removes it. It keeps `Type=Application` entries and drops `NoDisplay=true`, `Hidden=true`, entries whose `OnlyShowIn` does not name, or `NotShowIn` names, a desktop of `$XDG_CURRENT_DESKTOP`, and entries whose `TryExec` is not an executable. It reads `Name`, `GenericName`, `Comment`, `Keywords`, `Categories`, `Exec`, `Path`, `Terminal`, `Icon` and `Actions`, with the best locale variant for `LC_MESSAGES`/`LANG` (`lang_COUNTRY`, then `lang`, then none), and each action listed in `Actions` from its `[Desktop Action ID]` group ([section 13](#13-desktop-actions)).

**Commands on `$PATH`.** Every regular executable file in each `$PATH` directory, by name, deduplicated with the first directory winning, as rofi's run mode lists them. They have no category and show only in search results (a `/usr/bin` of thousands would bury All), and a desktop entry whose `Exec` program has the same name hides the plain command, so `firefox` appears once.

**Exec.** `Exec` is unquoted as the specification says, field codes `%f %F %u %U` are dropped (the launcher opens no files), `%i` becomes `--icon ICON`, `%c` the name and `%k` the file's path, and `%%` a `%`. The arguments are then quoted for `/bin/sh` and prefixed with `exec`; `Path` becomes `cd DIR && `; `Terminal=true` wraps the command in the terminal ([section 9](#9-launching)).

**Categories.** An entry's default category is the first freedesktop main category it lists, shown by a readable name: AudioVideo, Audio and Video as Multimedia, Development, Education, Game as Games, Graphics, Network as Internet, Office, Science, Settings, System, Utility as Accessories; an entry with none goes to Other.

**Cache.** The scanned index is JSON in `$XDG_CACHE_HOME/shingle/launcher-index.json`, versioned so that an incompatible cache is ignored (version 2 added icons and actions). The start uses the cache; after the first frame a thread always rescans everything and, when the result differs, writes the cache and sends it to the main loop, which shows it keeping the selection. With no cache the first start scans before drawing.

### Rejected alternatives

- **Watching the directories with inotify** while open: the launcher lives for seconds, and the background rescan already catches installs between starts.
- **The KDE menu files** (`*.menu`, Desktop Menu Specification's XML): heavy for what categories need, and the user's own layout replaces them.

## 6. Matching and ranking

### Decision

The query is matched against each entry's shown name (after a rename), and at lower weight against its generic name, keywords and program name; the best field counts. A match is a case-insensitive subsequence. The score follows fzy's model: a bonus for a match at the start of the string, after a separator (space, `-`, `_`, `.`, `/`) or at a lowercase-to-uppercase change, a larger one for consecutive matched characters, and a small penalty per skipped character; a contiguous prefix match scores highest. Hidden entries never match.

Frecency adds `2 × ln(1 + count) / (1 + age_days / 7)`, where `count` is how often the entry was launched and `age_days` how long ago the last launch was; so an application used daily beats an equally good match used once a month, but never a clearly better match. Desktop entries get a small bonus over plain commands, so that `fire` prefers Firefox to a `firecfg` binary.

Results are the 100 best, sorted by score, then by name. With a non-empty query the last row is always "Run `query`", which runs the typed text as a shell command, like rofi's run mode with no match; Ctrl+Enter runs the typed text directly.

History lives in `$XDG_STATE_HOME/shingle/launcher-history.json` (`{"version": 1, "entries": {"ID": {"count": N, "last": UNIX_SECONDS}}}`), written after each launch. It is not part of the layout file because it changes on every launch and is not something the user edits.

## 7. The user's layout

### Decision

`$XDG_DATA_HOME/shingle/launcher.yaml` holds everything the user changed and nothing else; a missing file is an unchanged launcher.

```yaml
# shingle-launcher layout; edited by the launcher, safe to edit by hand.
version: 1
favourites: [firefox.desktop, kitty.desktop, shortcut:backup]
categories:
  - name: Internet
    entries: [firefox.desktop, org.kde.konversation.desktop]
  - name: Work
    entries: [shortcut:backup]
renamed:
  firefox.desktop: Web
hidden: [vim.desktop]
shortcuts:
  - id: shortcut:backup
    name: Backup home
    command: rsync -a ~/ /mnt/backup/
    terminal: false
```

- **Favourites** are an ordered list of entry IDs, shown first and as their own category.
- **Categories** are listed in the order shown. An entry listed under a category is shown there, in that order, instead of in its default category; the default categories that are not listed follow, in name order, with their entries in name order. Whenever a category is placed into, created, renamed or moved, every category shown is listed by name in the order shown, so the order the user sees stays; a listed category the user emptied is still shown until it is removed. Placing an entry, or reordering inside a category, writes that category's full order. Renaming a category writes its members under the new name; an application installed later whose default is the old name shows under the old name again.
- **Renamed** maps an ID to the name shown and matched; renaming to an empty name removes the entry.
- **Hidden** entries are shown only in the Hidden category, from which they can be shown again.
- **Shortcuts** are the user's own entries with a name and a shell command, and an ID of `shortcut:` and a slug of the name, made unique.

IDs are desktop file IDs, `path:NAME` for commands on `$PATH`, and `shortcut:SLUG`. An ID the index no longer has (an uninstalled application) is kept in the file and ignored, so that reinstalling brings its place back.

The file is written after every change: serialised, with empty lists left out, written to `launcher.yaml.tmp` in the same directory, synced and renamed over the old one. A file that fails to parse is moved aside to `launcher.yaml.bad` with a message on stderr, as Shingle does with a corrupt session (crates/shingle-session/src/lib.rs:204), and the launcher starts unchanged.

## 8. Theming

### Decision

`ShingleConfig` gains a `launcher` section, read by the launcher from `--config PATH` or Shingle's default path (crates/shingle-config/src/lib.rs, `default_path`); without a file, or for a Wayfire or i3 file, the defaults apply.

```yaml
launcher:
  width: 760
  height: 480
  font_size: 16
  renderer: auto          # auto, software or hardware
  terminal: ""            # "" picks $TERMINAL, foot, kitty, alacritty, xterm
  icon_theme: ""          # "" follows KDE's or GTK's setting, then hicolor
  colors:
    backdrop: [0.0, 0.0, 0.0, 0.45]
    panel: [0.11, 0.12, 0.15, 1.0]
    field: [0.16, 0.18, 0.22, 1.0]
    text: [0.92, 0.93, 0.95, 1.0]
    dim_text: [0.58, 0.62, 0.68, 1.0]
    accent: [0.33, 0.58, 0.93, 1.0]
    selection: [0.2, 0.31, 0.48, 1.0]
```

Colours are `[r, g, b, a]` in 0 to 1, like the rest of Shingle's config, and show as those values times 255. The accent is lightened for text on the panel (the caret, special category titles, the Run row, favourite marks).

shingle-wcm has a Launcher page (`shingle-wcm --page launcher`) for the section: width, height, font size, icon theme, renderer and terminal as text fields, and each colour as `R G B A` (or `#RRGGBB`), checked by the config's own validation when the page is left or saved, as on every other page. The launcher reads the config at each start, so nothing has to be reloaded.

## 9. Launching

### Decision

The launcher was started by Shingle with Shingle's client environment (or by Wayfire with Wayfire's), so its own environment is the right one for applications. It builds `ClientEnvironment::new(WAYLAND_DISPLAY, DISPLAY, WAYFIRE_SOCKET)` from its own values, which removes `WAYLAND_SOCKET` (libwayland would otherwise hand the launcher's own connection to the child) and keeps the sockets, and runs the command with crates/shingle-session's `launch_command`, discarding nothing: output goes where the launcher's goes, which is Shingle's choice (`behavior.discard_command_output`). The launcher does not wait for the child; it exits, and the child is reparented.

A desktop entry with `Terminal=true`, and a shortcut with `terminal: true`, run as `TERMINAL -e sh -c COMMAND`, where `TERMINAL` is `launcher.terminal`, else `$TERMINAL`, else the first of foot, kitty, alacritty and xterm on `$PATH`.

Shingle's session restore needs nothing from the launcher: it reads each new client's command line from `/proc` (crates/shingle/src/launch.rs:26, `capture_pid`), and the `exec` in front of the command makes that the application's own.

### Rejected alternatives

- **IPC `stipc/run`.** Shingle would spawn the command itself, but STIPC is a test interface Wayfire only has with its test plugin, and the environment is the same either way.

## 10. Keyboard and mouse

### Decision

The panel has a search field on top, a category column on the left (Favourites, Recent, the categories, All, and Hidden when anything is hidden) and the entries of the selected category, or the search results, on the right. It opens on Favourites when there are any, else on All. A status line at the bottom names the keys of the current mode, or what just failed.

Keys, always available unless a prompt is open:

| Key | Action |
| --- | --- |
| Typing | Filters all entries; the first result is selected. |
| Enter | Launches the selected entry and exits; the Run row runs the query. |
| Ctrl+Enter | Runs the typed text as a command. |
| Up, Down, Page Up, Page Down, Home, End | Move the selection. |
| Tab, Shift+Tab; Left, Right with an empty query | Next and previous category. |
| Escape | Closes a prompt or menu, else clears the query, else exits. |
| Ctrl+D | Adds the selected entry to Favourites or removes it. |
| F2 | Renames the selected entry in a prompt. |
| Shift+F2 | Renames the selected category. |
| Ctrl+Shift+Up, Ctrl+Shift+Down | Move the selected category. |
| Ctrl+N | Adds a shortcut whose command is the typed text, then prompts for its name; with nothing typed it prompts for the command first. It goes into the selected category, or Shortcuts when searching or browsing Favourites, Recent or All. |
| Ctrl+E | Edits the selected shortcut's command. |
| Ctrl+M | Moves the selected entry to a category, by name; a new name creates the category. |
| Ctrl+H | Hides the selected entry, or shows it again in Hidden. |
| Ctrl+Up, Ctrl+Down | Moves the selected entry up or down in its category or in Favourites. |
| Delete | Removes the selected shortcut. |
| Menu, Shift+F10 | Opens the selected entry's context menu beside it, its desktop actions first; Up, Down and Enter choose. |

A prompt replaces the search field with a labelled field (`Rename to:`, `Shortcut name:`, `Command:`, `Move to category:`); Enter accepts and Escape cancels. Ctrl+Backspace deletes a word, Ctrl+U the line.

Mouse:

- A click on an entry launches it; on a category, selects it; on the backdrop, closes the launcher.
- A right click on an entry opens a context menu at the pointer: Add to or Remove from Favourites, Rename, Move to category, Hide or Show, and for shortcuts Edit command and Remove. A right click on a category offers Rename and Remove (its entries go back to their default categories).
- A press on an entry and a move of more than 6 pixels starts a drag: the entry follows the pointer, and an insertion line shows where it lands. Dropping it on the entry list reorders it there; on a category, moves it to that category's end; on Favourites, favourites it; on Hidden, hides it. Escape cancels a drag.
- The wheel scrolls the list.

The keyboard path does not wait for the mouse path: the categories and favourites are shown from the first frame, and typing switches to results without any mode change.

## 12. Icons

### Decision

Every row has an icon column of `round(1.75 × font_size)` logical pixels (28 at the default 16). The icon is the entry's `Icon`, or for an action its own `Icon` and else the entry's.

**Theme.** `launcher.icon_theme`, else the desktop's setting: under KDE (`KDE` in `$XDG_CURRENT_DESKTOP`) `[Icons] Theme` of `$XDG_CONFIG_HOME/kdeglobals` before GTK's, elsewhere GTK's `gtk-icon-theme-name` of `gtk-4.0/settings.ini`, then `gtk-3.0/settings.ini`, before KDE's; then GNOME's `org.gnome.desktop.interface icon-theme`, which comes first instead under GNOME (`GNOME` in `$XDG_CURRENT_DESKTOP`), where GTK follows it rather than `settings.ini`; else `hicolor`. GNOME's setting is read by running `gsettings get` with the session bus set to `disabled:` and `DISPLAY` removed, so that no D-Bus service is started (dconf reads its database file directly); the answer is cached in `launcher-icons/gnome-icon-theme` with the time of `$XDG_CONFIG_HOME/dconf/user`, so `gsettings` (3 ms here) runs again only after that database changed. Outside GNOME a missing database means GNOME's settings were never written, and the schema's default is not taken for a choice.

**Lookup.** As the Icon Theme Specification 0.13 says: base directories `$HOME/.icons`, `$XDG_DATA_HOME/icons` and each `$XDG_DATA_DIRS/icons`; in the theme's `index.theme` directories (and `ScaledDirectories`), one whose `Size`, `Scale`, `Type`, `MinSize`, `MaxSize` and `Threshold` match the icon size at the integer scale above the fractional one wins, else the closest by `DirectorySizeDistance`; then the theme's `Inherits`, depth first, then `hicolor`, then the unthemed pixmaps directories (each `$XDG_DATA_DIRS/pixmaps`, then `/usr/share/pixmaps`). An absolute `Icon` is used as it is, and a name with a `.png`, `.svg` or `.xpm` extension is looked up without it. Each directory is listed once and its file names kept, so a lookup costs no `stat` per candidate. PNG, SVG and XPM are searched, in that order, as the specification lists them.

**Decoding.** An SVG is drawn by resvg (Gloomy's version, 0.36) into a square of the icon's device pixels, centred with its aspect ratio; its text is converted to paths with the system's fonts, which fontdb finds through fontconfig's configuration and loads once, the first time an SVG containing `<text` is decoded. That load took 35 to 44 ms for this host's 1242 faces (the text itself 0.2 ms), so it happens only on the icon thread after the first frame and never for a warm start, whose icons come from the cache. An XPM (XPM 3, hexadecimal colours, `None` and X11 colour names from `rgb.txt`) is decoded by the launcher's own small decoder (crates/shingle-launcher/src/xpm.rs; no light pure-Rust crate decodes XPM, and `image` does not), and it and a PNG are scaled into that square with its alpha premultiplied, so transparent pixels do not darken the edges. Icons are straight RGBA, uploaded once per decoded icon as a non-sRGB texture, so an icon shows as its file's bytes as the panel's colours do, and drawn one texel per device pixel.

**Cache and start.** Decoded icons are kept in `$XDG_CACHE_HOME/shingle/launcher-icons/THEME-SIZE-PIXELS.bin`: for each name the file found (or none), its modification time and its pixels. The first draw reads the file of the size it draws at, so a warm start's first frame has every icon it had before. After the first frame a thread looks up what the file lacks, decodes what is new or whose file's time changed, hands each icon to the main loop as it is ready, where the next frame draws it, and merges what it decoded into the file at the end. When the background rescan finds entries the cached index lacked, as for an application installed since the last start, a second pass looks up the icons no pass knows yet, so they are drawn as soon as they are decoded; the passes merge into the file under a lock, so neither drops the other's icons. The file's format is version 2 (`SLICON02`), so that icons a version 1 file holds without their text, or without an XPM, are decoded again once. Icons the file already knows are only checked by their file's time, unless the file is older than a day, when every name is looked up again, which finds icons installed into a theme since. A row whose icon is missing, or not loaded yet, shows its name's first letter in a tile.

### Rejected alternatives

- **A generic theme icon (`application-x-executable`) for entries without one.** It needs a lookup of its own, differs between themes, and a letter tells entries apart.
- **Decoding before the first frame.** A cold start of a few hundred SVGs takes far longer than the whole budget; the cache makes the warm start as fast as without icons, and a cold one shows letters for a moment.
- **Gloomy's `Image` and `Icon` widgets.** At the pinned commit they place images in logical pixels on a target of device pixels, so at any scale but 1 they land in the wrong place, they sample an sRGB texture into the panel's linear target, which darkens them, and its image shader flips textures vertically. The launcher draws through Gloomy's `ImageRenderer` itself, in device pixels, with the rows uploaded bottom first; a fix belongs in Gloomy's own repository.

## 13. Desktop actions

### Decision

The actions an entry lists in `Actions`, in that order, are kept with it: their `Name` (localised), `Icon` and `Exec`, made into a command line as the entry's, with the entry's `Path` and `Terminal`. An action without `Exec`, which only D-Bus activation could run, is left out.

**Search.** An action is a result when the query matches its own name, or matches the entry's name and the action's together while the entry's name alone does not: `firefox` lists Firefox only, `private` and `fire priv` list "New Private Window" under it. Its score is the better of the two matches less 0.4, so an action ranks below an entry that matches as well, plus its own frecency, recorded under `ENTRY#ACTION`. The row shows the action's name with its entry's name on the right, and Enter launches it. Edits (Ctrl+D, F2 and the rest) apply to entries, not to an action's row.

**Menu.** An entry's context menu starts with its actions, then the edits. A right click opens it at the pointer, and the Menu key or Shift+F10 opens it below the selected row, so the actions are two keys away: Menu, Enter starts the first.

### Rejected alternatives

- **Actions expanded as rows under the entry by Right.** Left and Right already change category when nothing is typed, and an expanding list moves every row below it, which the pointer then misses.
- **Listing every action of every match.** Typing a browser's name would bury the next result under its actions.

## 11. Testing

Unit tests cover the pure parts: desktop entry parsing, Exec unquoting and field codes, discovery precedence and filters, `$PATH` listing, the cache round trip, matching and ranking, frecency, the layout store and every edit, history, the command and environment built for a launch, and the model's reaction to keys and pointer events without Wayland or a GPU.

The GPU harness's group `launcher` (tools/gpu_test/launcher_checks.py) runs offscreen Shingle with distinctive launcher colours, a private `XDG_DATA_HOME` and `XDG_DATA_DIRS` holding `.desktop` files for `wl_probe` clients, and the launcher in release with `--trace` (one JSON line per frame with the mode, query, selected category, rows and their rectangles) and `--timing`. It checks type-to-launch, favouriting, renaming, adding a shortcut from typed text, moving between categories by keyboard and by drag, the context menu, the layout file after each, persistence across restarts, the toggle, and the startup time.

## Decisions

The operator was not available; each open question got the default the lead proposed or the sensible one, recorded here with the reason, for the operator to review.

1. **A separate client binary, not drawn by the frontend.** It works under Wayfire too, keeps text input and file writing out of the compositor, and cannot crash the session. Cost: a process start per invocation, which section 4 keeps under 100 ms.
2. **Layer shell on the overlay layer, full output, exclusive keyboard.** Clicking outside closes it, keys cannot leak to the window below, and no panel covers it.
3. **Gloomy's widgets and renderer, with the launcher's own Wayland code.** Gloomy has no layer-shell host; adding one belongs in Gloomy's repository, and the launcher does not need it to.
4. **lavapipe by default.** NVIDIA's Vulkan start alone exceeds the 100 ms target on this host; lavapipe's is a fifth of it, and a small panel copied into shared memory gains nothing from the GPU. `launcher.renderer: hardware` restores the GPU. The loader variables are restored before anything is launched.
5. **Per invocation with a second invocation closing the first.** No resident memory, and the binding toggles.
6. **Index cached, rescanned in the background after the first frame.** The start never waits for a scan, except the very first.
7. **`$PATH` commands included, below desktop entries.** rofi's run mode is what the operator uses today.
8. **One YAML layout file in `$XDG_DATA_HOME/shingle/launcher.yaml`, written atomically on every change.** It is the user's data, not configuration, so it is not in `$XDG_CONFIG_HOME` next to the config Shingle reloads; YAML because the rest of Shingle's files are.
9. **Usage history in a separate state file.** It changes on every launch; keeping it out of the layout keeps that file stable and readable.
10. **Theme in Shingle's config, `launcher:`.** One place for the look of the desktop; WCM's palette as defaults.
11. **Launching with `launch_command` and the launcher's own client environment.** That environment is already the one Shingle or Wayfire gives its children; Shingle's launch identities need nothing more.
12. **`super+space` as the default binding.** `super+d`, rofi's usual key, is Shingle's show-desktop, and `super` alone toggles shell menus.
13. **No file arguments.** The launcher opens no files, so `%f`, `%F`, `%u` and `%U` expand to nothing. (Desktop actions and icons, left out of the first version, were added; Decisions 17 to 24.)
14. **Commands on `$PATH` only in search results.** They have no category, and thousands of them in All would bury the applications; typing finds them as rofi's run mode does.
15. **The category column is the keyboard's second axis, not a focus.** Tab and Left/Right change category; Shift+F2 and Ctrl+Shift+arrows edit the category, so no key moves a focus between two columns and typing always goes to the search.
16. **Colours are bytes, not sRGB-encoded.** The panel is drawn into a linear `Bgra8Unorm` texture, so a configured colour shows as its values times 255, as `background_color` does in Shingle.
17. **Icons by the Icon Theme Specification, PNG, SVG and XPM.** The theme is `launcher.icon_theme`, else KDE's under KDE, GNOME's under GNOME and GTK's elsewhere, falling back to the others, then `hicolor`, then pixmaps. (XPM and GNOME's setting were added by `shingle-small-followups`; see section 12.)
18. **One cache file per theme, icon size and device size, read at the first draw.** Icons the cache lacks are decoded in a thread after the first frame and drawn as they arrive, so the warm start does not change and a cold one shows letter tiles first. Known icons are checked by their file's time on each start and looked up again once the cache is a day old; an application that the background rescan finds gets its icon in the same run (since `shingle-small-followups`; before, at the next start).
19. **A letter tile for a missing icon**, not a theme's generic icon.
20. **Icons drawn beside Gloomy's widgets**, through its `ImageRenderer` in device pixels from a non-sRGB texture uploaded bottom row first, since its `Image` widget is misplaced at scales other than 1, darkened by its sRGB texture and flipped by its shader at the pinned commit (section 12). Gloomy is not changed from this workspace.
21. **Actions are search results when their own name matches, and the first items of the entry's context menu**, which the Menu key and Shift+F10 open by keyboard; no key expands them in the list, so no key collides with Left and Right changing category. Their launches count for frecency under `ENTRY#ACTION`.
22. **Fractional scale through `wp_fractional_scale_v1` with `wp_viewporter`**, the buffer at the output's device pixels; the integer buffer scale remains for compositors without them.
23. **The WCM page uses text fields like the other pages**, the renderer typed as `auto`, `software` or `hardware`, checked by the config's validation.
24. **STIPC knows `KEY_COMPOSE`** (the Menu key, as libevdev names it), so the harness can press it; Wayfire's stipc already takes every libevdev name.

## Open items

- An application installed after the background rescan ran, while the launcher stays open, is listed at the next start: the launcher lives for seconds, so inotify is still not worth it (section 6).
- Gloomy's `Image` widget at scales other than 1, its sRGB textures and its flipped image shader, to be fixed in Gloomy's repository; the launcher then need not draw icons itself.

## As built

Built on branch `shingle-launcher` in crates/shingle-launcher, with the `launcher:` section in crates/shingle-config/src/launcher.rs and the `super+space` binding in data/shingle.yaml. The sections above are corrected where the code differs; the differences from the first draft are Decisions 14 to 16, the Shift+F2 and Ctrl+Shift+arrow keys for categories, the search field drawn by Gloomy's `TextInput`, hit testing by the launcher's own rectangles, the overlay tree for the menu and the drag, and the cache without directory times.

- **Library** (unit-tested without Wayland or a GPU): crates/shingle-launcher/src/desktop.rs (desktop entries, locales, `Exec`, shell quoting), index.rs (discovery and the cache), matching.rs (the fzy score), search.rs (fields, bonuses and frecency), history.rs, layout.rs (`Layout`, `LayoutStore`), catalog.rs (the categories shown), model.rs (keys, pointer, prompts, menu, drags and every edit, with crates/shingle-launcher/src/model/tests.rs), view.rs (the Gloomy trees and pointer targets), theme.rs, launch.rs (terminal, environment, spawn), shm.rs, render.rs (Gloomy on wgpu 0.20, lavapipe selection and readback).
- **Binary**: crates/shingle-launcher/src/main.rs (options, the single-instance socket, loading, `--screenshot`) and client.rs (the layer surface, keyboard with repeat, pointer with cursor-shape, buffers, frame callbacks, the background rescan and `--trace`).
- **Startup**: 44 to 53 ms median from `main` to the first committed buffer over seven warm starts, in three runs (40.2 to 58.7 ms in all), 2 to 3 ms more from `exec` as the harness sees it, in the GPU harness's namespace on this host with the release build on lavapipe; the very first start, with no index cache and an empty Mesa shader cache, took 167 to 188 ms. The design's 100 ms target holds.
- **Verification**: 68 unit tests in the crate and 4 for the config section; the GPU harness's group `launcher` (tools/gpu_test/launcher_checks.py), 18 checks with the run under headless Wayfire, all passing ([status.md](../status.md#application-launcher)).
- **Wayfire**: run under the host's `wayfire` 0.9, headless inside the GPU harness's namespace, never under the operator's session. Wayfire configures the layer surface at 0x0 first and then at the output's size, which the launcher now waits through. It uses nothing Shingle-specific: wlr-layer-shell, `wl_shm`, `wl_seat` and, when offered, cursor-shape.

### Icons, actions, scale and WCM as built

Built on branch `shingle-launcher-icons` (2026-11-27), with sections 2, 5, 8 and 10 updated and sections 12 and 13 and Decisions 17 to 24 added.

- **Icons**: crates/shingle-launcher/src/icons.rs (`IconPaths`, `IconLookup`, `theme_name`), crates/shingle-launcher/src/icon_cache.rs (`IconStore`, `decode_svg`, `decode_png`, `resolve`, the cache file), the icon slots and letter tiles in view.rs (`IconSlot`), their drawing in render.rs (`Renderer::draw`, `icon_texture`), and the cache read at the first draw and the thread after it in client.rs (`load_icons`, `start_icons`). `launcher.icon_theme` is in crates/shingle-config/src/launcher.rs.
- **Actions**: `[Desktop Action]` groups in desktop.rs (`DesktopEntry::action`), `EntryAction` and `action_id` in index.rs (cache version 2), action candidates in search.rs (`action_score`) and catalog.rs (`Found`), `Row::Action`, `MenuAction::Action`, `Key::Menu` and `Key::F10` in model.rs.
- **Scale**: client.rs binds `wp_fractional_scale_v1` and `wp_viewporter` and draws at `State::scale`; `--screenshot` takes `--scale` and writes a PPM for a `.ppm` path, which the harness compares with its captures.
- **WCM**: the Launcher page in crates/shingle-wcm/src/ui.rs (`launcher_page`) and its fields in crates/shingle-wcm/src/app.rs (`sync_page`), with `LauncherRenderer::as_str` and `FromStr`.
- **Shingle** changed in three places the real runs found. A layer surface's geometry took its buffer's pixels as logical pixels, so a buffer at scale 2, or one of device pixels behind a viewport, covered twice or 1.5 times its size (crates/shingle/src/layer_shell.rs, `geometry`). An shm screencopy of a scaled output was drawn in logical pixels and repeated into the client's buffer, so `grim` at 1.25, 1.5 or 2 showed blocks rather than what the output shows; an untransformed scaled output is now drawn again in its device pixels and the buffer cut from that (crates/shingle-render/src/target.rs, `capture_frame_scaled`; crates/shingle-frontend/src/frontend/scene.rs, `answer_captures`; crates/shingle/src/capture.rs, `crop_device`). STIPC takes `KEY_COMPOSE` (crates/shingle/src/ipc.rs, `evdev_code`).
- **Startup**: 44.2 to 45.6 ms median from `main` to the first committed buffer over seven warm starts with icons, in four runs (40.5 to 58.5 ms in all), 2 to 3 ms more from `exec`, as before icons; the first start with neither cache took 169 to 193 ms and draws letter tiles, whose icons appear in the following frames. Warm starts draw every icon in their first frame from the cache.
- **Verification**: 20 new unit tests in the crate, 1 for the config section, 2 in WCM and 4 for the Shingle changes; the GPU harness's group `launcher`, 32 checks, all passing ([status.md](../status.md#application-launcher)).

### XPM, SVG text, GNOME's theme and late icons as built

Built on branch `shingle-small-followups` (2026-11-28), with section 12 and Decisions 17 and 18 updated and the open items closed but one.

- **XPM**: crates/shingle-launcher/src/xpm.rs (`decode`, `decode_with`), a decoder of XPM 3's C form: the header, colour definitions with the `c` visual first, then `g`, `g4` and `m`, hexadecimal colours of 1 to 4 digits a channel, `None`, and X11 colour names from `/usr/share/X11/rgb.txt` or `/etc/X11/rgb.txt`, matched as X does without case and spaces, with a few built-in names without one; sizes above 4096 or more than 8 characters a pixel are refused. XPM 2 (`! XPM2`) and HSV colours are not read; none of this host's 19 XPM files uses them. `xpm` is searched after `png` and `svg` (crates/shingle-launcher/src/icons.rs, `EXTENSIONS`) and scaled as a PNG is (crates/shingle-launcher/src/icon_cache.rs, `decode_xpm`, `fit`).
- **SVG text**: resvg's `text`, `system-fonts` and `memmap-fonts` features, which Gloomy already built, and `TreeTextToPath::convert_text` with a fontdb of the system's fonts loaded once on the icon thread when an SVG contains `<text` (`decode_svg_with`, `system_fonts`). Loading took 35 to 44 ms for 1242 faces, converting a two-letter text 0.2 ms; a warm start never loads fonts, since its icons come from the cache.
- **GNOME's theme**: `icons::theme_name` takes a fourth argument that reads GNOME's setting, `icons::gnome_theme` caches `gsettings`' answer by the dconf database's time, and `icons::run_gsettings` runs `gsettings get org.gnome.desktop.interface icon-theme` with `DBUS_SESSION_BUS_ADDRESS=disabled:`, without `DISPLAY` and with stderr dropped: under the Claude sandbox it printed `'breeze'` in 3 ms from `~/.config/dconf/user` with no bus at all. `Paths::dconf_database` is `$XDG_CONFIG_HOME/dconf/user`.
- **Late icons**: after the background rescan's index arrives, `State::start_new_icons` (crates/shingle-launcher/src/client.rs) starts a pass for the icon names no pass knows; every pass now merges what it decoded into the cache file under a lock (`IconStore::save_merged`). The cache format is version 2.
- **Startup**: the launcher group's warm-start median, before and after on the same day in alternating runs: before 45.7, 60.4, 52.8, 51.3, 51.6 and 49.2 ms (median 51.5), after 54.1, 66.5, 54.1, 48.5 and 45.1 ms (median 54.1), so within the 5 ms allowed; the host was shared with other builds, which the spread of each set shows.
- **Verification**: 10 new unit tests in the crate (xpm.rs 5, icon_cache.rs 3, icons.rs 2), and the theme order, the cache round trip and the paths tests extended and the launcher group's new step 13, which installs two entries after the index was cached, a red XPM in pixmaps and an SVG that is only green text in hicolor, and finds both icons drawn in `grim` in the same run (33 checks; the build before this branch fails only that one). The step starts a second launcher when the first only closed one the timing loop left open, which happened with both builds.

