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
| 6 | Index cache | The index is cached as JSON in `$XDG_CACHE_HOME/shingle/launcher-index.json`. A start draws from the cache and rescans in a background thread after the first frame; a changed index replaces the shown one and the cache. Without a cache the first start scans before drawing. |
| 7 | Matching | A fuzzy subsequence match in the style of fzy, case-insensitive, scoring word starts, consecutive runs and prefixes, against the shown name, then the generic name, keywords and program name at lower weight. Ties and near ties are broken by frecency. |
| 8 | Ranking by use | Each launch records a count and a time per entry in `$XDG_STATE_HOME/shingle/launcher-history.json`. Frecency is `ln(1 + count)` scaled by `1 / (1 + age_days / 7)`. It adds to the match score, and with an empty query it orders the Recent view. |
| 9 | The user's layout | One human-readable YAML file, `$XDG_DATA_HOME/shingle/launcher.yaml`: favourites in order, categories in order with their entries in order, renames, hidden entries and custom shortcuts with a name and a command. It is written on every change, atomically (a temporary file renamed over it). |
| 10 | Keyboard | Typing filters at once, Enter launches the selection, Escape clears and then closes. Ctrl+D favourites, F2 renames, Ctrl+N adds a shortcut from the typed text, Ctrl+M moves to a category, Ctrl+H hides, Ctrl+Up and Ctrl+Down reorder, Delete removes a shortcut. Tab and Shift+Tab, or Left and Right with an empty query, change category. |
| 11 | Mouse | A click launches or selects a category; a right click opens a context menu with the same edits; dragging an entry reorders it in its list, onto a category moves it there, and onto Favourites favourites it. The wheel scrolls. |
| 12 | Theming | A `launcher:` section in Shingle's YAML config: panel size, font size, colours (backdrop, panel, text, dim text, accent, selection), terminal and renderer. Defaults are a dark slate panel with a blue accent, matching WCM. A Wayfire or i3 configuration gets the defaults. |
| 13 | Launching | Commands run through crates/shingle-session's `launch_command` (`/bin/sh -c`) with a `ClientEnvironment` built from the launcher's own environment, which Shingle (or Wayfire) gave it: its `WAYLAND_DISPLAY`, `DISPLAY` and `WAYFIRE_SOCKET` pass on and `WAYLAND_SOCKET` never does. Shingle learns each window's launch identity from `/proc` as for any client, so session restore relaunches it. |
| 14 | Binding | `data/shingle.yaml` binds `super+space` to `shingle-launcher`. The Wayfire import is unchanged: a `[command]` binding that runs rofi stays as it is, and no launcher binding is added. |
| 15 | Startup target | First frame under 100 ms after `exec` in a release build on this host, measured by the launcher itself and by the GPU harness. |

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

The panel is `launcher.width` × `launcher.height` logical pixels, centred, and shrinks to fit a smaller output. The buffer scale follows `wl_surface.preferred_buffer_scale` (version 6) when the compositor sends it, else 1; Gloomy draws at that scale factor.

### Rejected alternatives

- **A panel-sized surface.** Smaller buffers, but a click outside the panel would reach the window under it and the launcher could not close on it.
- **An `xdg_toplevel`.** The tiler would tile it, and it would not get the keyboard exclusively.

## 3. Toolkit and drawing

### Decision

The panel is built from Gloomy widgets (`Container`, `Label`, `Button` and Gloomy's flex layout), rebuilt from the launcher's model on each change, laid out with `compute_layout` and drawn with `GloomyRenderer` into a `Bgra8UnormSrgb` texture, whose bytes are already `wl_shm`'s little-endian `ARGB8888`. Hit testing uses Gloomy's `hit_test` on the same tree, with actions such as `entry:3` and `category:2`.

Input, the event loop and Wayland are the launcher's own: wayland-client with calloop, xkbcommon for the keymap, and calloop timers for key repeat, as in shingle-lock. Gloomy's own input handling and `TextInput` widget are not used, because its focus and event model are winit's; the search field is drawn as a styled label with a caret.

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

**Desktop entries.** The launcher reads `applications/` under `$XDG_DATA_HOME` (default `~/.local/share`) and then each directory of `$XDG_DATA_DIRS` (default `/usr/local/share:/usr/share`), recursively. An entry's desktop file ID is its path below `applications/` with `/` replaced by `-`; the first directory that has an ID wins, so a user's copy overrides the system's, and a user's `Hidden=true` copy removes it. It keeps `Type=Application` entries and drops `NoDisplay=true`, `Hidden=true`, entries whose `OnlyShowIn` does not name, or `NotShowIn` names, a desktop of `$XDG_CURRENT_DESKTOP`, and entries whose `TryExec` is not an executable. It reads `Name`, `GenericName`, `Comment`, `Keywords`, `Categories`, `Exec`, `Path`, `Terminal` and `Icon`, with the best locale variant for `LC_MESSAGES`/`LANG` (`lang_COUNTRY`, then `lang`, then none). Actions (`[Desktop Action …]`) are not shown in the first version.

**Commands on `$PATH`.** Every regular executable file in each `$PATH` directory, by name, deduplicated with the first directory winning, as rofi's run mode lists them. They have no category of their own (they show in All and in search results), and a desktop entry whose `Exec` program has the same name hides the plain command, so `firefox` appears once.

**Exec.** `Exec` is unquoted as the specification says, field codes `%f %F %u %U` are dropped (the launcher opens no files), `%i` becomes `--icon ICON`, `%c` the name and `%k` the file's path, and `%%` a `%`. The arguments are then quoted for `/bin/sh` and prefixed with `exec`; `Path` becomes `cd DIR && `; `Terminal=true` wraps the command in the terminal ([section 9](#9-launching)).

**Categories.** An entry's default category is the first freedesktop main category it lists, shown by a readable name: AudioVideo, Audio and Video as Multimedia, Development, Education, Game as Games, Graphics, Network as Internet, Office, Science, Settings, System, Utility as Accessories; an entry with none goes to Other.

**Cache.** The scanned index (entries, plus the directories scanned with their modification times) is JSON in `$XDG_CACHE_HOME/shingle/launcher-index.json`, versioned so that an incompatible cache is ignored. The start uses the cache; after the first frame a thread rescans everything and, when the result differs, sends it to the main loop, which shows it and writes the cache. With no cache the first start scans before drawing (about 20 ms for this host's applications and `$PATH`).

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
- **Categories** are listed in the order shown. An entry listed under a category is shown there, in that order, instead of in its default category; the default categories that are not listed follow, in name order, with their entries in name order. A category the user creates is listed with its entries; moving an entry out of the last listed entry of a category leaves an empty listed category, which the launcher still shows until the user removes it. Reordering inside a category writes the category's full order.
- **Renamed** maps an ID to the name shown and matched; renaming to an empty name removes the entry.
- **Hidden** entries are shown only in the Hidden category, from which they can be shown again.
- **Shortcuts** are the user's own entries with a name and a shell command, and an ID of `shortcut:` and a slug of the name, made unique.

IDs are desktop file IDs, `path:NAME` for commands on `$PATH`, and `shortcut:SLUG`. An ID the index no longer has (an uninstalled application) is kept in the file and ignored, so that reinstalling brings its place back.

The file is written after every change: serialised, written to `launcher.yaml.tmp` in the same directory, synced and renamed over the old one. A file that fails to parse is moved aside to `launcher.yaml.bad` with a message on stderr, as Shingle does with a corrupt session (crates/shingle-session/src/lib.rs:204), and the launcher starts unchanged.

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
  colors:
    backdrop: [0.0, 0.0, 0.0, 0.45]
    panel: [0.075, 0.085, 0.105, 1.0]
    field: [0.105, 0.12, 0.15, 1.0]
    text: [0.9, 0.92, 0.95, 1.0]
    dim_text: [0.55, 0.6, 0.68, 1.0]
    accent: [0.12, 0.42, 0.62, 1.0]
    selection: [0.16, 0.3, 0.45, 1.0]
```

Colours are `[r, g, b, a]` in 0 to 1, like the rest of Shingle's config. The defaults are WCM's palette, so the two look alike.

## 9. Launching

### Decision

The launcher was started by Shingle with Shingle's client environment (or by Wayfire with Wayfire's), so its own environment is the right one for applications. It builds `ClientEnvironment::new(WAYLAND_DISPLAY, DISPLAY, WAYFIRE_SOCKET)` from its own values, which removes `WAYLAND_SOCKET` (libwayland would otherwise hand the launcher's own connection to the child) and keeps the sockets, and runs the command with crates/shingle-session's `launch_command`, discarding nothing: output goes where the launcher's goes, which is Shingle's choice (`behavior.discard_command_output`). The launcher does not wait for the child; it exits, and the child is reparented.

A desktop entry with `Terminal=true`, and a shortcut with `terminal: true`, run as `TERMINAL -e sh -c COMMAND`, where `TERMINAL` is `launcher.terminal`, else `$TERMINAL`, else the first of foot, kitty, alacritty and xterm on `$PATH`.

Shingle's session restore needs nothing from the launcher: it reads each new client's command line from `/proc` (crates/shingle/src/launch.rs:26, `capture_pid`), and the `exec` in front of the command makes that the application's own.

### Rejected alternatives

- **IPC `stipc/run`.** Shingle would spawn the command itself, but STIPC is a test interface Wayfire only has with its test plugin, and the environment is the same either way.

## 10. Keyboard and mouse

### Decision

The panel has a search field on top, a category column on the left (Favourites, Recent, the categories, All, and Hidden when anything is hidden) and the entries of the selected category, or the search results, on the right. A status line at the bottom names the keys of the current mode.

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
| F2 | Renames the selected entry (or category, when the category column has the focus) in a prompt. |
| Ctrl+N | Adds a shortcut whose command is the typed text, then prompts for its name, in the selected category (or Accessories when searching). |
| Ctrl+E | Edits the selected shortcut's command. |
| Ctrl+M | Moves the selected entry to a category, by name; a new name creates the category. |
| Ctrl+H | Hides the selected entry, or shows it again in Hidden. |
| Ctrl+Up, Ctrl+Down | Moves the selected entry up or down in its category or in Favourites. |
| Delete | Removes the selected shortcut. |

A prompt replaces the search field with a labelled field (`Rename to:`, `Shortcut name:`, `Command:`, `Move to category:`); Enter accepts and Escape cancels. Ctrl+Backspace deletes a word, Ctrl+U the line.

Mouse:

- A click on an entry launches it; on a category, selects it; on the backdrop, closes the launcher.
- A right click on an entry opens a context menu at the pointer: Add to or Remove from Favourites, Rename, Move to category, Hide or Show, and for shortcuts Edit command and Remove. A right click on a category offers Rename and Remove (its entries go back to their default categories).
- A press on an entry and a move of more than 6 pixels starts a drag: the entry follows the pointer, and an insertion line shows where it lands. Dropping it on the entry list reorders it there; on a category, moves it to that category's end; on Favourites, favourites it. Escape cancels a drag.
- The wheel scrolls the list.

The keyboard path does not wait for the mouse path: the categories and favourites are shown from the first frame, and typing switches to results without any mode change.

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
13. **No desktop actions, icons or file arguments in the first version.** Icons need an icon theme lookup and image decoding on the start path; they are listed under open items.

## Open items

- Icons from the icon theme, loaded after the first frame.
- Desktop actions (`[Desktop Action …]`) as sub-entries.
- Fractional scale (`wp_fractional_scale_v1`); integer buffer scale only for now.
- An entry in WCM for the `launcher:` theme.

## As built

Filled in as the implementation lands.
