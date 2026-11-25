# XWayland design

Status: accepted, 2026-11-25; the operator's answers to the former open questions are under [Decisions](#decisions). Roadmap item "XWayland" ([roadmap.md](../roadmap.md#4-xwayland)). XW-1 to XW-4 are done (2026-11-25); their deviations from this design are listed in their entries ([XW-1](#xw-1-shingle-toplevel-handle-shell-neutral-toplevel-and-client-environment), [XW-2](#xw-2-shingle-xwayland-server-sockets-process-configuration-and-xwm-bring-up), [XW-3](#xw-3-shingle-xwayland-windows-association-and-managed-toplevels), [XW-4](#xw-4-shingle-xwayland-behaviour-focus-unmanaged-windows-and-states)).

Shingle has no XWayland, so X11 applications cannot run under it. `stipc/get_display` returns an empty `xwayland` value (crates/shingle/src/ipc.rs:2676). This document decides how Shingle starts and supervises `Xwayland`, how its own X window manager (XWM) turns X11 windows into Shingle windows, and how input, selections and scaling work for them. It is the brief for the implementation workspaces in [section 8](#8-implementation-plan). Sections 1 to 4, and section 5 apart from selections and drag and drop, are implemented (XW-2 to XW-4); selections (XW-5), drag and drop (XW-6) and HiDPI (XW-7) are not yet.

Line numbers refer to branch `shingle-xwayland-design`, cut from `master` at `8df73d6`.

Sources and dates:

- **Wayfire** was read at `/home/karl/dev/wayfire`, commit `b36615a7` (2026-02-22).
- **wlroots** was read on master `56f15f8` (2026-11-21, `0.21.0-dev`), cloned from gitlab.freedesktop.org. Its GitHub mirror (`swaywm/wlroots`) was last pushed in 2022. The wlroots tags are 0.20.2 (2026-07-07) and 0.19.3 (2026-03-19). Wayfire builds on wlroots and uses its XWM, so wlroots' `xwayland/` directory is the behaviour Shingle has to match.
- **smithay** was read on master `54cf9c1` (2026-11-24).
- **x11rb** was read on master `b0d68dc` (2026-11-11), and its 0.13.2 release from `~/.cargo/registry`.
- **wayland-protocols 0.32.13** was read from `~/.cargo/registry`.
- **Crate versions** come from the crates.io API and **Debian versions** from the ftp-master madison API, both on 2026-11-25.

## Summary of decisions

| # | Topic | Decision |
| --- | --- | --- |
| 1 | Server management | Shingle binds the X display itself when it starts: it chooses a free `:N`, writes the lock file and opens a filesystem and an abstract socket. It execs `Xwayland -rootless` with `-listenfd`, `-wm` and `-displayfd`, and gives it a Wayland connection through `WAYLAND_SOCKET`. The new option `xwayland.mode` takes Wayfire's values `true`, `false` and `lazy`; the importer maps `[core] xwayland` and `xwayland_startup_script` onto it. `DISPLAY` goes into every child's environment, and to clients through `stipc/get_display`. Xwayland is restarted on exit if it ran for more than 5 s, the same rule wlroots uses. |
| 2 | X window manager | A Shingle-owned XWM, written with `x11rb` 0.14 `RustConnection`, a pure-Rust connection over the `-wm` socket pair, in a new crate `shingle-xwayland`. It implements wlroots' ICCCM/EWMH subset. Override-redirect windows are drawn unmanaged above the Top layer, at the position the X client chose. |
| 3 | Surface association | Only `xwayland_shell_v1`, which pairs a window and a surface by serial. `WL_SURFACE_ID` is not supported, so Xwayland 23.1 or newer is required; Debian trixie ships 24.1.6. Pairing works whichever side arrives first, and a window maps only once three things are true: it is associated, the WM has mapped it, and it has a committed buffer. |
| 4 | Window model | An X11 toplevel uses the same `ToplevelRole` as an xdg toplevel. `ToplevelRole.resource` becomes a `ToplevelHandle` enum with an `Xdg` and an `X11` variant, so tiling, rules, Scale, Expo, IPC and sessions need no X11-specific code. `app-id` is the class part of `WM_CLASS`, as in Wayfire. The PID comes from XRes, and the launch identity is read from `/proc` for that PID, so session placeholders match exactly as they do for Wayland windows. |
| 5 | Input, selection, DnD | Keyboard and pointer focus go to the X window's `wl_surface`. The XWM also follows the window's ICCCM input model (`SetInputFocus`, `WM_TAKE_FOCUS`) and sets `_NET_ACTIVE_WINDOW`. Clipboard and primary selection are bridged in both directions, with INCR, through a compositor-owned selection source. Drag and drop comes later, in its own workspace. |
| 6 | Scaling and geometry | Do what Wayfire and wlroots do: one X pixel is one logical pixel, X buffers have scale 1, and Shingle upscales them, so X11 clients look blurry at an output scale above 1. A sharp "zero-scaling" mode is a later, optional workspace. |
| 7 | Testing | Unit tests for the pure XWM logic through a recording connection trait. Real-client runs use `xterm`, `xeyes`, `xprop`, `xwininfo`, `xclip`, `xlsclients`, `xdpyinfo` and a small x11rb probe client in the workspace, against headless or nested Shingle inside a user, mount and network namespace with a private `/tmp/.X11-unix`, so nothing reaches the host's X displays. `xdotool` is not installed. |
| 8 | Plan | Seven ordered workspaces, XW-1 to XW-7: neutral toplevel role and client environment; server and XWM bring-up; managed windows; focus, override-redirect and window states; selections; drag and drop; and optionally HiDPI. |

## Crate and tool survey

| Item | Version (date) | Maintenance | Used for |
| --- | --- | --- | --- |
| `x11rb` | 0.14.0 (2026-07-16) | Active. psychon/x11rb was pushed 2026-11-11. The previous release is 0.13.2 (2025-08-29). | `RustConnection::connect_to_stream` (rust_connection/mod.rs:167) over `DefaultStream::from_unix_stream` (rust_connection/stream.rs:219), `Connection::poll_for_event`, and the `composite`, `xfixes`, `res`, `render` and `cursor` features. Pure Rust: its normal dependencies are `gethostname`, `rustix ^1.0` and `x11rb-protocol`. libxcb is only involved with `allow-unsafe-code` or `dl-libxcb`. |
| `x11rb-protocol` | 0.14.0 (2026-07-16) | Same repository. | Comes in through `x11rb`. Its sans-IO `connection::Connection` (`poll_for_reply`, connection/mod.rs:308) is the fallback in [2](#rejected-alternatives-1). |
| `wayland-protocols` | 0.32.13 | Already a workspace dependency, with `server` and `staging`. | `xwayland::shell::v1::server::{xwayland_shell_v1, xwayland_surface_v1}`, behind `staging` (src/xwayland.rs:5). |
| `rustix` | 1.1 | Already in the workspace with `process`, `net` and `fs`. | `pidfd_open` to watch the Xwayland process, `socketpair`, and the abstract and filesystem `bind`/`listen`. |
| `xcursor` | 0.3 | Already a dependency of `shingle`. | Loading `left_ptr` for the root window cursor. |
| `smithay` | 0.7.0 (2025-06-24); master pushed 2026-11-24 | Active. | Not used; see [2](#rejected-alternatives-1). Its `xwayland` and `xwm` modules (about 8,200 lines) are a second reference implementation. |
| `xcb` | 1.7.1 (2026-08-09) | Active, under rust-x-bindings/rust-xcb. Links libxcb. | Not used. |
| `x11`, `x11-dl` | 2.21.0 (2023-01-18) | Stale (Xlib). | Not used. |
| `Xwayland` binary | 24.1.6 installed (`2:24.1.6-1`, Debian trixie); upstream stable is 24.1.15 (2026-11-10) | Active. There are no 25.x tags; 26.1 is in release candidates. | The server itself. `Xwayland -help` on this host lists `-rootless`, `-listenfd`, `-wm`, `-displayfd`, `-terminate`, `-core`, `-shm`, `-glamor`, `-force-xrandr-emulation` and the rootful-only `-hidpi`. |

`x11rb` 0.13.2 is already in Cargo.lock (Cargo.lock:4770), because winit 0.30.13 requires `x11rb ^0.13` with `dl-libxcb`, `randr`, `xinput` and `xkb`. Choosing 0.14 means builds that include `shingle-nested` compile two x11rb versions until winit moves. That is accepted: 0.14 is the current release and the one smithay master uses, and `shingle` itself never links winit ([architecture.md](../architecture.md#design-rules)). XW-2 records the build-time cost. If it matters, pinning `x11rb = "0.13.2"` is a one-line change, since the APIs used here are the same in both.

## What exists today

- **No X server.** `stipc/get_display` hard-codes `"xwayland": ""` (crates/shingle/src/ipc.rs:2676), and `SUPPORTED_METHODS` (crates/shingle/src/ipc.rs:380) lists neither of Wayfire's `stipc/get_xwayland_display` and `stipc/get_xwayland_pid` (wayfire/plugins/ipc/stipc.cpp:308-309).
- **Host `DISPLAY` leaks into nested children.** Fixed in XW-1; this bullet describes the code before it. The nested backend is also selected when `DISPLAY` is set (crates/shingle-bin/src/backend.rs:29). Every client Shingle starts inherits the parent environment and gets only `WAYLAND_DISPLAY` overridden. That covers `--spawn` (crates/shingle-frontend/src/frontend.rs:222), the lock client (crates/shingle-frontend/src/frontend/session.rs:25), `stipc/run` through `launch_command` (crates/shingle-session/src/lib.rs:129), restored clients (crates/shingle-session/src/lib.rs:276) and headless `--spawn` (crates/shingle-bin/src/headless.rs:94). No code calls `env_remove`. Nested Shingle started from a terminal in the host Wayfire session therefore hands the host's `DISPLAY` to its children, and an X11 client started inside it opens on the host's Xwayland. This follows from the code; no run has shown it. XW-1 fixes it whether or not XWayland is enabled. The XW-1 run then showed it: before the fix, every child of a nested Shingle started with the host's `DISPLAY=:1` saw `:1`, and without `--ipc` also the host's `WAYFIRE_SOCKET` ([status.md](../status.md#with-real-clients)).
- **The toplevel role is xdg-only.** Changed in XW-1, see below; this bullet describes the code before it. `ToplevelRole` (crates/shingle/src/shell.rs:92) holds an `XdgToplevel` and lives inside `Role::Xdg { kind: Some(XdgKind::Toplevel(..)) }` (crates/shingle/src/shell.rs:32). That pattern appears 53 times in 10 files (30 of them in shell.rs), alongside the `Role::toplevel()` helper (crates/shingle/src/shell.rs:59).
- **Launch identity is per Wayland client.** Since XW-1, `launch::capture_pid` reads an identity for any PID, which XW-3 uses for X11 windows; the map below is unchanged. `launch_templates` is keyed by `ClientId` (crates/shingle/src/state.rs:172) and filled from the peer credentials of each connection (`launch::capture`, crates/shingle/src/launch.rs:12, used at crates/shingle/src/lib.rs:2111). Every X11 window shares one Wayland client, Xwayland, so this map cannot identify X11 applications.

## 1. Server management

### Decision

**Ownership.** A new library crate, `crates/shingle-xwayland`, holds everything that does not need `wayland-server`: display sockets, the server process, the XWM and the selection bridge. `shingle` depends on it and owns one `XwaylandServer` in `CompositorState` (crates/shingle/src/state.rs:121). Glue code in `crates/shingle/src/xwayland/` turns XWM events into compositor state changes. `shingle-frontend` and the backends need no changes, apart from building the client environment ([XW-1](#xw-1-shingle-toplevel-handle-shell-neutral-toplevel-and-client-environment)).

**Display sockets, bound when Shingle starts.** `Compositor::with_socket` (crates/shingle/src/lib.rs:560) calls `DisplaySockets::bind()`, unless `xwayland.mode` is `false`. It works the way wlroots' xwayland/sockets.c does:

1. For each `N` from 0 to 32, try `/tmp/.X<N>-lock` with `O_CREAT | O_EXCL`, and write the PID into it as `%10d\n`. If the file already exists, read the PID from it. A lock whose PID no longer exists is stale; unlink it and retry the same `N`. Any other existing lock means `N` is taken.
2. Bind `/tmp/.X11-unix/X<N>` and the abstract socket `\0/tmp/.X11-unix/X<N>`, then `listen(fd, 1)`. If either bind fails, release the lock and move to the next `N`.
3. Keep both descriptors, close-on-exec, until Shingle exits. On drop, remove the socket file and the lock.

`/tmp/.X11-unix` itself is created mode 1777 if it is missing. On this host it already holds `X0` and `X1`, which belong to the host session's Xwayland, so a nested Shingle picks `:2`. Real-client tests never touch the host's directory; see [7](#7-testing).

Binding first, as wlroots does, makes `DISPLAY` known before any client starts, in every mode. Restored and autostarted clients, which `Frontend::new` launches straight after the compositor is created (crates/shingle-frontend/src/frontend.rs:176-205), already get the right `DISPLAY`. With `lazy`, the first of them to connect starts the server.

**Spawning.** `XwaylandServer::start` creates two socket pairs, `wl` and `wm`, plus a `displayfd` pipe, and execs:

```text
Xwayland :N -rootless -core [-terminate DELAY]
  -listenfd X_FD0 -listenfd X_FD1 -displayfd READY_FD -wm WM_FD
```

This is wlroots' argv (xwayland/server.c:50-90). `-terminate 10` is passed in lazy mode only (this text first said `-terminate` was always passed; see the [XW-2 entry](#xw-2-shingle-xwayland-server-sockets-process-configuration-and-xwm-bring-up) for why that was wrong). The delay is 10 as wlroots' `wlr_xwayland_create` sets `terminate_delay = lazy ? 10 : 0` (xwayland/xwayland.c:147), which Wayfire calls at src/view/xwayland.cpp:258. The child environment has `WAYLAND_SOCKET` set to the child's end of `wl`, and the listen, `wm` and `displayfd` descriptors are the only ones without close-on-exec. The binary is `Xwayland` from `PATH`; `SHINGLE_XWAYLAND` overrides it, as `WLR_XWAYLAND` does in wlroots (server.c:124). Unlike wlroots, Shingle does not double-fork: it keeps the `Child` and watches it through a pidfd (`rustix::process::pidfd_open`) registered as a calloop `Generic` source, as the display and socket descriptors already are (crates/shingle/src/lib.rs:579-597). The compositor end of `wl` becomes a Wayland client through `DisplayHandle::insert_client`, the same call the accept paths use (crates/shingle/src/lib.rs:2074, 2099), with `ClientDataImpl` (crates/shingle/src/state.rs:52) gaining `xwayland: bool`.

**Readiness.** The `displayfd` pipe is a calloop source. When Xwayland writes the display number, the server is ready. Shingle then connects the XWM on the compositor end of `wm` ([2](#2-x-window-manager)), runs `xwayland.startup_script` through `launch_command` with the client environment (Wayfire runs `core/xwayland_startup_script` on ready, src/view/xwayland.cpp:236-255), and pushes the IPC event `xwayland-ready`. That event is Shingle's own: Wayfire has no IPC event for it.

**Modes.**

| `xwayland.mode` | Wayfire `[core] xwayland` | Behaviour |
| --- | --- | --- |
| `true` | `true` (Wayfire's default, metadata/core.xml:48) | Bind, then start right after the compositor is created, before restored clients launch. |
| `lazy` | `lazy` | Bind, and register both listen descriptors in calloop. On the first readable event, remove the watches and start, as wlroots' `server_start_lazy` and `xwayland_socket_connected` do (server.c:413, 425). Xwayland then accepts the waiting client itself. |
| `false` | `false` | Bind nothing. `DISPLAY` is removed from child environments, `stipc/get_display` reports `""`, and X11 clients fail to connect. |

The YAML model gains `ShingleConfig.xwayland: XwaylandConfig { mode, startup_script }` in crates/shingle-config/src/lib.rs, validated like `behavior.preferred_decoration_mode` (lib.rs:256), with WCM fields. The Wayfire importer maps `("core", "xwayland")` and `("core", "xwayland_startup_script")` next to `("core", "preferred_decoration_mode")` (crates/shingle-config/src/wayfire.rs:468). As in Wayfire, which reads the option once in `init_desktop_apis` (src/view/view-impl.cpp:166), a changed mode takes effect only after a restart. `apply_config` (crates/shingle/src/lib.rs:916) keeps the running server and logs that the change needs one, the same rule `behavior.disable_primary_selection` follows ([features.md](../features.md)). Shingle's default is `lazy` ([Decisions](#decisions)).

**Exporting `DISPLAY`.** `Compositor::client_environment()` (XW-1; it returns a `ClientEnvironment`, see the XW-1 deviations) returns `WAYLAND_DISPLAY`, plus `DISPLAY=:N` (or `DISPLAY` removed, when there is no server) and `_JAVA_AWT_WM_NONREPARENTING=1`, which Wayfire sets for every child (src/core/core.cpp:539). Every spawn site listed in [What exists today](#what-exists-today) takes this list instead of a bare socket name: `launch_saved`, `launch_autostart` and `launch_command` (crates/shingle-session/src/lib.rs:129-192), `--spawn`, the lock client, `stipc/run` and headless `--spawn`. Shingle never calls `set_var("DISPLAY")` on itself, because nested's winit may still read the host's `DISPLAY` (crates/shingle-bin/src/backend.rs:29).

**IPC.**

- `stipc/get_display` returns `{"wayland": ..., "xwayland": ":N"}` once sockets are bound, so it works before the lazy start too, as Wayfire's does (stipc.cpp:542-548; in lazy mode wlroots has `display_name` before the server starts).
- Two new methods, `stipc/get_xwayland_display` (`{"result": "ok", "display": ":N"}`, stipc.cpp:640-643) and `stipc/get_xwayland_pid` (the PID, or -1 when not running), raise `list-methods` from 77 to 79. docs/ipc.md and docs/status.md are updated with the counts.

**Crash and exit.** When the pidfd fires or the Xwayland client disconnects:

1. Destroy the XWM.
2. Unmap and detach every X11 window as if its surface had been destroyed, which leaves session placeholders exactly as a disconnecting Wayland client does (`detach_toplevel`, crates/shingle/src/shell.rs:614).
3. Restart only if the server ran for more than 5 s, lazily in `lazy` mode and eagerly otherwise, as wlroots' `handle_client_destroy` does (server.c:198-222). The rule avoids a crash loop.
4. If it ran 5 s or less, log the failure, keep the sockets and lock, and stay without X until Shingle restarts. In lazy mode the sockets are not watched again either, as in wlroots, so an X client then waits in the listen backlog until it gives up.

A lazy server that exits after its 10 s idle delay is restarted lazily by the same rule. On SIGTERM, shutdown stops the XWM and Xwayland before Wayland clients are destroyed, as Wayfire does (src/core/core.cpp:352-354). It then removes the sockets and the lock.

### Rejected alternatives

- **Let Xwayland pick the display (`-displayfd` only, no `-listenfd`).** `DISPLAY` would be unknown until the server is up, which rules out lazy start, and restored clients would launch without it. Rejected.
- **Always eager, like sway's `xwayland force`.** Costs a process and its memory in every nested test run, even though most use no X11 clients. Lazy mode is kept, with Wayfire's value names.
- **Rootful Xwayland (a single X screen in one Wayland window).** Gives no per-window management, which Shingle's tiling needs. Rejected.
- **Double-fork as wlroots does.** That exists so the compositor does not have to reap. Shingle already reaps children (`reap_ipc_children`, crates/shingle/src/state.rs:1040), and a pidfd gives exit notification without SIGCHLD. Rejected.
- **Setting `DISPLAY` in Shingle's own environment.** It would change what nested's winit connects to, and it races other threads. Rejected in favour of per-child environments.

## 2. X window manager

### Decision

The XWM is a Rust port of the wlroots behaviour Wayfire relies on, in `crates/shingle-xwayland/src/xwm/`. It talks to Xwayland through x11rb's pure-Rust `RustConnection`, built from the compositor end of the `-wm` socket pair with `DefaultStream::from_unix_stream` and `RustConnection::connect_to_stream(stream, 0)`, which is also what smithay does (src/xwayland/xwm/mod.rs:825-826).

**Event flow.** The connection's descriptor is a calloop `Generic` source. On readable, `Xwm::dispatch` drains `poll_for_event()` without blocking and returns a list of `XwmEvent`s. `crates/shingle/src/xwayland/mod.rs` applies them to `CompositorState`. Requests (configure, map, property changes, focus) are queued, and the queue is flushed once per `Compositor::poll` (crates/shingle/src/lib.rs:703), after `dispatch_clients`.

**Replies.** Property and attribute reads on CreateNotify, MapRequest and PropertyNotify block on their reply, but only after all of a window's requests have been pipelined. wlroots works the same way, with blocking `xcb_*_reply` calls in `read_surface_property` (xwm.c:1194). x11rb's `RustConnection` has no non-blocking reply poll. The XWM's request side sits behind a small `XConnection` trait, used by both the tests and the production connection. If a deadlock is ever observed, that trait lets the production side switch to a sans-IO connection on `x11rb-protocol` without touching the rest.

**Bring-up**, when the XWM connects:

1. Intern the atoms in one pipelined batch: wlroots' list at xwm.c:22-108, minus `WL_SURFACE_ID` and the Xdnd atoms until [XW-6](#xw-6-shingle-xwayland-dnd-drag-and-drop-later). Require Composite and XFixes, and send XFixes `QueryVersion(6, 0)` and `SetClientDisconnectMode(Terminate)`, as wlroots' `xwm_create` does, so that the XWM's own connection does not keep a lazy server from exiting (added in XW-2).
2. `composite::redirect_subwindows(root, Manual)` (xwm.c:2803).
3. Select `SubstructureRedirect | SubstructureNotify | PropertyChange` on the root.
4. Create the WM check window, and own `WM_S0` and `_NET_WM_CM_S0`.
5. Set `_NET_SUPPORTING_WM_CHECK`, and set `_NET_SUPPORTED` to wlroots' list (xwm.c:2807-2827).
6. (XW-4, done) Set the root cursor from the configured xcursor theme (`left_ptr`, through Render `CreateCursor`), as Wayfire's `xwayland_update_default_cursor` does (src/view/xwayland.cpp:283).
7. (XW-5) Select XFixes selection events for the [selection bridge](#5-input-selection-and-dnd).

**Window model.** `XWindow` is pure data: the X id, override-redirect, geometry, the parsed properties below, the association serial and the associated surface. Each property has a parser in `xwm/props.rs` and unit tests on byte buffers.

| Property | Use |
| --- | --- |
| `WM_CLASS` | `(instance, class)`. Shingle's `app_id` is `class`, as Wayfire's `class_t` (src/view/xwayland/xwayland-view-base.cpp:18, 26). |
| `_NET_WM_NAME` (UTF-8), else `WM_NAME` | Title. |
| `WM_TRANSIENT_FOR` | Parent window, through `set_toplevel_parent` (crates/shingle/src/shell.rs:651). As in Wayfire (xwayland-toplevel-view.hpp:174-189), the parent is dropped when it is not mapped or the window's type is `NORMAL`. |
| `WM_NORMAL_HINTS` | `min_size` and `max_size` on `ToplevelRole` (shell.rs:102-103), plus the initial position if `USPosition`/`PPosition` is set. |
| `WM_HINTS` | Input hint and urgency. If the input flag is missing, input is assumed true (xwm.c:1018). |
| `WM_PROTOCOLS` | `WM_DELETE_WINDOW`, `WM_TAKE_FOCUS`, `_NET_WM_PING`. |
| `_NET_WM_WINDOW_TYPE` | Classification (below). |
| `_NET_WM_STATE` | Initial fullscreen, maximized and hidden state. |
| `_NET_WM_PID` and XRes `QueryClientIds` | PID: XRes first, as wlroots' `read_surface_client_id` (xwm.c:248), with `_NET_WM_PID` as the fallback. |
| `_MOTIF_WM_HINTS` | Whether the client draws its own decorations, which only affects `xdg-decoration`-style reporting; Shingle draws no title bars. |
| `_NET_STARTUP_ID` | Kept, for xdg-activation tokens later. |

**Classification**, a port of Wayfire's `determine_type`, `is_unmanaged` and `is_dialog` (src/view/xwayland.cpp:106-160):

- **Drag icon.** `_NET_WM_WINDOW_TYPE_DND` (Shingle skips it until XW-6).
- **Unmanaged.** Override-redirect windows. Also a window with a parent that is not a dialog and has neither type `NORMAL` nor `UTILITY`; Wayfire's example is Android Studio's popups (xwayland.cpp:121-137).
- **Managed.** Everything else. A managed window is a dialog when it has type `DIALOG`, or has a parent and no type.

The classification is recomputed when override-redirect, `_NET_WM_WINDOW_TYPE` or `WM_TRANSIENT_FOR` changes. When the kind changes, the window is unmapped and mapped again as the new kind, as Wayfire's `recreate_view` does (xwayland.cpp:190).

**Requests the XWM handles.**

| X event | Handling |
| --- | --- |
| CreateNotify, DestroyNotify | Track `XWindow`s. Destroy also drops unpaired association entries. |
| MapRequest | Read properties, then map the window (`MapWindow`), set `WM_STATE` Normal and add it to `_NET_CLIENT_LIST`, as wlroots' `xwm_handle_map_request` does (xwm.c:1540). The Shingle window appears only when the [association](#3-surface-association) is complete. |
| UnmapNotify | Set `WM_STATE` Withdrawn, remove the window from the client lists, and unmap the Shingle window (it becomes a placeholder only if the surface also goes away; see [4](#4-integration-with-the-window-model)). |
| ConfigureRequest | Before the window is mapped, grant it as asked, and keep the position as the start position (Wayfire's `handle_client_configure`, xwayland-toplevel-view.hpp:40-62). After that, tiled and fullscreen windows get their current geometry back. Floating windows get the request clamped to the workarea, as Wayfire's `configure_request` does (lines 77-113), through `set_floating_geometry` (crates/shingle/src/shell.rs:1453). Override-redirect windows are not redirected and configure themselves. |
| PropertyNotify | Re-read that one property, then emit `TitleChanged`, `ParentChanged`, `SizeHintsChanged`, `UrgencyChanged` or a reclassification. |
| ClientMessage `WL_SURFACE_SERIAL` | [Association](#3-surface-association). |
| ClientMessage `_NET_WM_STATE` | Add, remove or toggle `FULLSCREEN`, `MAXIMIZED_VERT`/`HORZ` and `HIDDEN`. Mapped to `set_fullscreen`, `set_maximized` and `set_minimized` (crates/shingle/src/shell.rs:1092-1150). wlroots handles this at xwm.c:1762. |
| ClientMessage `WM_CHANGE_STATE` (IconicState) | `set_minimized(true)`. |
| ClientMessage `_NET_ACTIVE_WINDOW` | Focus the window if it is mapped and not already active, as Wayfire's `on_request_activate` does (xwayland-toplevel-view.hpp:151). |
| ClientMessage `_NET_WM_MOVERESIZE` | Codes 0-7 push `Event::BeginResize` with the edges and code 8 pushes `Event::BeginMove`, the events xdg's `Move` and `Resize` push (crates/shingle/src/shell.rs:2018-2032). The keyboard variants (9, 10) and cancel (11) are ignored. |
| ClientMessage `_NET_CLOSE_WINDOW` | Same as `close-view`. |
| FocusIn | If the newly focused window is not the one Shingle focused, and it does not belong to the same X client as the focused window, put focus back, as wlroots' `xwm_handle_focus_in` does (xwm.c:2052). |

**What the XWM sends.**

- **Configure.** `ConfigureWindow` with x, y, width, height and border 0. When only the position changed, a synthetic `ConfigureNotify` follows, as ICCCM 4.1.5 requires and wlroots' `wlr_xwayland_surface_configure` sends (xwm.c:2318-2358). A size of 0 is never sent, because Wayfire notes it "would freeze xwayland" (src/view/xwayland/xwayland-toplevel.cpp:150-152).
- **States.** `_NET_WM_STATE` is rewritten whenever Shingle's own state changes: `FULLSCREEN`, `MAXIMIZED_*`, `HIDDEN` and `FOCUSED`.
- **Stacking.** On raise, `ConfigureWindow` with `StackMode::Above`, which is Wayfire's `xwayland_bring_to_front` (xwayland.cpp:304). `_NET_CLIENT_LIST` (in map order, which fixes the FIXME at xwm.c:437) and `_NET_CLIENT_LIST_STACKING` follow Shingle's stack order.
- **Close.** `WM_DELETE_WINDOW` if the window lists it, otherwise `KillClient`, as wlroots' `wlr_xwayland_surface_close` does (xwm.c:2360).
- **Focus.** See [5](#5-input-selection-and-dnd).

**Override-redirect windows** (menus, tooltips, splash screens, drop-downs) have `Role::Xwayland` with kind `Unmanaged` and no `WindowId`. They are drawn at their X position, which is in Shingle's logical coordinates ([6](#6-scaling-and-geometry)). They sit above the Top layer and below Overlay, which is where Wayfire's `UNMANAGED` layer is (src/api/wayfire/scene.hpp:463-470). They are visible only while their X window is mapped. An unmanaged window takes keyboard focus only if it is not override-redirect, or if wlroots' `override_redirect_wants_focus` heuristic says so, which Wayfire uses at src/view/xwayland/xwayland-unmanaged-view.hpp:169-177. They do not appear in `list-views`, Scale or Expo. Shingle's `list-views` (crates/shingle/src/ipc.rs:2564) already lists only core windows, not layer-shell surfaces. Wayfire does list them, with `"type": "x-or"` and `"role": "unmanaged"` (plugins/ipc-rules/ipc-rules-common.hpp:140-151); [status.md](../status.md) records this as a deviation.

### Rejected alternatives

- **smithay's `xwayland` module as a library.** `X11Wm::start_wm` requires `D: XwmHandler + XWaylandShellHandler + SeatHandler + DndGrabHandler` and imports smithay's input, output, selection and compositor client state (src/xwayland/xwm/mod.rs:136-147, 804). Shingle uses none of those, so using it would mean adopting smithay's compositor framework. Its code is still read as a reference. Rejected as a dependency.
- **libxcb through the `xcb` crate, or x11rb's `XCBConnection`.** Adds a C library and `unsafe` for no gain over `RustConnection`. wlroots needs libxcb only because it is C. Rejected.
- **Xlib (`x11`/`x11-dl`).** Unmaintained since 2023, and its error handling is not thread-safe. Rejected.
- **A sans-IO XWM on `x11rb-protocol` from the start.** It never blocks, but every property read becomes a continuation, which roughly doubles the code. wlroots and smithay both block on replies in production. Kept as the fallback behind `XConnection`, not as the first implementation.
- **Running the XWM on its own thread**, as smithay's `X11Source` reads events on one. The XWM's decisions need `CompositorState` on every event, so a thread only adds a channel and does not stop the main thread waiting. Rejected.
- **Managing override-redirect windows.** ICCCM gives the WM no say over them. Wayfire does not manage them either. Rejected.

## 3. Surface association

### Decision

Shingle implements `xwayland_shell_v1` version 1 (wayland-protocols 0.32.13, `xwayland::shell::v1`, staging). Its global is visible only to the Xwayland client: `GlobalDispatch::can_view` checks `ClientDataImpl.xwayland`, in the way `security_context::can_view` filters globals (crates/shingle/src/security_context.rs:76). A bind from any other client is also refused, as wlroots' `shell_bind` does (xwayland/shell.c:143). Xwayland binds the global only when rootless (hw/xwayland/xwayland-screen.c:545). Since 23.1 it sends the serial whenever the global exists (xwayland-window.c:773-783).

**Protocol side**, in `crates/shingle/src/xwayland/shell.rs`:

- `get_xwayland_surface(id, surface)` gives a role-less surface the new `Role::Xwayland(Box<XwaylandRole>)` in `Role` (crates/shingle/src/shell.rs:32), with `serial: None`, `pending_serial: None` and `window: None`. A surface that already has a role gets error `role`.
- `set_serial(lo, hi)` stores `pending_serial`. A second serial gets `already_associated`, and a zero serial gets `invalid_serial`.
- On the surface's next commit (`committed`, crates/shingle/src/shell.rs:506), `pending_serial` becomes `serial`, and the surface goes into `XwaylandSurfaces.by_serial`. The serial is double-buffered: wlroots emits `new_surface` only on the first commit after `set_serial` (xwl_surface_role_commit, shell.c).

**X side.** The ClientMessage `WL_SURFACE_SERIAL` carries the 64-bit serial in `data32[0..2]` (low, high). A repeated message for a window that already has a serial is ignored (xwm.c:1638-1665).

**Pairing.** Both sides can arrive in either order, because they come over two sockets with no ordering between them.

```rust
/// Serial-keyed halves waiting for their partner.
pub struct Pairing {
  /// X windows that announced a serial before the surface committed it.
  windows: HashMap<u64, x11rb::protocol::xproto::Window>,
  /// Committed surfaces whose X window has not announced the serial yet.
  surfaces: HashMap<u64, SurfaceId>,
}

pub enum Paired {
  Now { window: Window, surface: SurfaceId },
  Waiting,
}

impl Pairing {
  pub fn window_serial(&mut self, window: Window, serial: u64) -> Paired;
  pub fn surface_serial(&mut self, surface: SurfaceId, serial: u64)
    -> Paired;
  pub fn window_destroyed(&mut self, window: Window);
  pub fn surface_destroyed(&mut self, surface: SurfaceId);
}
```

`Pairing` lives in `shingle-xwayland` and is pure. Every order and every destroy-before-pair case is a unit test. This is wlroots' `unpaired_surfaces` list (xwm.c:1633, 2283-2296), with the surface half kept too, because Shingle's protocol side and XWM are separate modules.

**Mapping requires all of:**

1. the pair is complete;
2. the XWM has mapped the window (after MapRequest), or it is override-redirect and has had MapNotify;
3. the surface has a committed buffer (`surface.buffer.is_some()`, the condition `request_map` uses, crates/shingle/src/shell.rs:422).

Whichever of the three comes last calls `xwayland::try_map`. For a managed window that leads to `materialize_toplevel` and `request_map` ([4](#4-integration-with-the-window-model)), so `view-pre-map` blocking (crates/shingle/src/shell.rs:422-486) works for X11 windows too, as it does in Wayfire, which emits `view_pre_map_signal` in `on_map` (xwayland.cpp:70-86).

**Races handled.**

- **Surface destroyed before pairing.** The serial entry is dropped and the X window waits for a serial that never comes. Xwayland creates a new surface and sends a new serial if the window maps again.
- **X window destroyed first.** The surface stays role-less, inert as wlroots' surfaces are, and is cleaned up when it is destroyed.
- **Unmap, then remap.** Xwayland destroys the `wl_surface` on unmap and creates a new one with a new serial on map. So an unmap dissociates; the Shingle window becomes unmapped, and a later pair re-associates it with the same `WindowId` while the X window lives. (XW-3 removes the Shingle window on unmap instead, and a remap creates a new one; see its entry.) This is the `associate`/`dissociate` cycle Wayfire listens to (xwayland.cpp:88-101).
- **Commit before the serial pair.** Buffers are kept on the surface but not drawn, since a role-less or unpaired `Role::Xwayland` surface is not in `visible_surfaces` (crates/shingle/src/state.rs:600).

### Rejected alternatives

- **`WL_SURFACE_ID` in addition.** It names the surface by Wayland object id, which libwayland can reuse once the object is destroyed, so a late message can pair the wrong surface. wlroots needs a compositor-wide `new_surface` listener to cover it (xwm.c:1599-1636, 2255). Only Xwayland before 23.1 needs it. 23.1.0 is the first release with `xwayland-shell-v1.xml` in hw/xwayland/meson.build; 22.1.9, Debian bookworm's version, does not have it. Shingle needs Rust 1.92 and targets trixie, where Xwayland is 24.1.6. Rejected. If the XWM receives a `WL_SURFACE_ID` message, it logs once that Xwayland is too old.
- **Associating on the X message alone, without the commit.** The protocol makes the serial take effect on commit, and acting on `set_serial` alone could draw a stale buffer. Rejected.

## 4. Integration with the window model

### Decision

**One toplevel role for both shells.** `ToplevelRole` (crates/shingle/src/shell.rs:92) keeps every field except `resource`, which becomes:

```rust
/// The protocol object behind a toplevel.
#[derive(Clone)]
pub(crate) enum ToplevelHandle {
  /// An `xdg_toplevel`.
  Xdg(XdgToplevel),
  /// A managed X11 window, by its X id.
  X11(x11rb::protocol::xproto::Window),
}
```

`Role::toplevel()` and a new `Role::toplevel_mut()` return the role for both `Role::Xdg { kind: Some(XdgKind::Toplevel(..)) }` and `Role::Xwayland` with a managed kind. XW-1 replaces the direct matches among those 53 occurrences with these helpers, so that the following work unchanged for X11 windows:

- tiling (`reconfigure_all`, crates/shingle/src/shell.rs:698);
- geometry (`surface_geometry`, crates/shingle/src/shell.rs:217);
- snapshots (`snapshot_metadata`, crates/shingle/src/shell.rs:132);
- stacking and visibility (`visible_surfaces`, crates/shingle/src/state.rs:600);
- Scale (`scale_surfaces`, crates/shingle/src/state.rs:808);
- focus (`object_for_window` and `window_for_object`, crates/shingle/src/input.rs:1512, 1531);
- placement (crates/shingle/src/placement.rs:21);
- size constraints (crates/shingle/src/shell.rs:1019);
- IPC (`view_json`, crates/shingle/src/ipc.rs:737);
- foreign-toplevel (crates/shingle/src/foreign_toplevel.rs:219).

Four places do different things for X11, each a `match` on `ToplevelHandle`:

- **`configure_toplevel`** (crates/shingle/src/shell.rs:807). An xdg toplevel gets `configure` plus `xdg_surface.configure(serial)`. An X11 window gets `Xwm::configure(window, geometry)` and `Xwm::set_states(window, states)`. X11 has no acknowledge, so `serials` stay xdg-only and `validate_commit` (crates/shingle/src/shell.rs:325) ignores X11 surfaces.
- **Close**, in `Compositor::close_window` (crates/shingle/src/lib.rs:879), `close-view` (crates/shingle/src/ipc.rs:2628) and foreign-toplevel (crates/shingle/src/foreign_toplevel.rs:224): `Xwm::close`.
- **Activation**: the `FOCUSED` state and X focus; see [5](#5-input-selection-and-dnd).
- **Destroy**: `detach_toplevel` (crates/shingle/src/shell.rs:614) also runs when the X window is destroyed or dissociated.

The frontend sees an X11 toplevel as a `SurfaceSnapshot` with `role: SurfaceRole::Toplevel` (crates/shingle/src/lib.rs:320, 363). Override-redirect windows get a new `SurfaceRole::Unmanaged`. The frontend's Toplevel-only filters, at crates/shingle-frontend/src/frontend/scene.rs:428 and 497 and crates/shingle-frontend/src/frontend/scale.rs:473, already leave them out of Scale and window animations. The animation view-type table (crates/shingle-frontend/src/frontend/animation.rs:27) maps the new role to `"x-or"`, Wayfire's name for it (src/core/view-access-interface.cpp:116).

**Lifecycle**, mirroring xdg:

| xdg (crates/shingle/src/shell.rs) | X11 |
| --- | --- |
| `GetToplevel` creates `ToplevelRole` with the client's launch template (1804-1840) | Pairing a managed window creates `ToplevelRole` with `ToplevelHandle::X11`, an identity from the window's PID (below), `app_id` from `WM_CLASS`, the title, the parent, the size hints and, if the client asked for one, `floating_geometry` from its pre-map ConfigureRequest. |
| `SetAppId` calls `materialize_toplevel` (874) | The class is already known at pairing. `materialize_toplevel` (932) runs on the first commit with a buffer, so the window exists only once it is drawable, as in `committed` (506). |
| First commit: `tile_by_default`, `placement::place`, `apply_created` (536-546) | Unchanged. Dialogs and transients follow whatever rule xdg windows with a parent follow. Wayfire's `can_tile_view` (plugins/tile/tile-plugin.cpp:32-45) keeps windows with a parent or a fixed size (min = max > 0) out of tiling. Many X11 dialogs have both, so XW-4 tests both cases and makes the X11 behaviour match Wayfire's. |
| `SetTitle`, `SetParent`, `SetMinSize`/`SetMaxSize` | PropertyNotify for `_NET_WM_NAME`/`WM_NAME`, `WM_TRANSIENT_FOR` and `WM_NORMAL_HINTS`, through the same `update_title`, `set_toplevel_parent` and size fields. |
| `SetFullscreen`, `SetMaximized`, `SetMinimized` | `_NET_WM_STATE` and `WM_CHANGE_STATE` messages, through the same `set_fullscreen`, `set_maximized` and `set_minimized`. |
| `Move`, `Resize` | `_NET_WM_MOVERESIZE`, giving the same `Event::BeginMove` and `Event::BeginResize`. |
| `xdg_toplevel.destroy`, surface destroyed | Dissociate, DestroyNotify or server exit, through `detach_toplevel`. |

Any X11 window that stays unmapped (WM `Withdrawn`) while its X id lives keeps its `WindowId`. It is unmapped in the core (`reconcile_unmapped`) and claimed again on remap, the way a Wayland toplevel that unmaps and maps again is. (Not so: XW-3 follows what a Wayland toplevel that unmaps really does, and XW-4 decided to keep that; see the XW-3 and XW-4 entries.)

**Tiling, floating and fullscreen.** These come from the same `LeafState` and the same `reconfigure_all`. What differs is the configure:

- **Tiled** windows get their tile rectangle.
- **Floating** windows get `floating_geometry`.
- **Fullscreen** windows get the output rectangle, and `_NET_WM_STATE_FULLSCREEN` is set on them.
- **Workspaces.** X windows on other workspaces are not moved. They keep their last rectangle, because Shingle only ever shows the current workspace, and moving them would cause needless X configures. X positions for windows on the current workspace always match what Shingle draws, which pointer delivery needs ([5](#5-input-selection-and-dnd)).
- **Temporary geometry.** Scale thumbnails, Expo, vswitch slides and rotation never send an X configure, which keeps to the design rule in [architecture.md](../architecture.md#design-rules).

**Window rules.** `window_rules::view_properties` (crates/shingle/src/window_rules.rs:126) reports `view_type: "toplevel"` for X11 toplevels, which is what Wayfire reports for them (view-access-interface.cpp:101-121). So `app_id is "XTerm"` rules match X11 windows by class, as they do in Wayfire.

**IPC view JSON.** `view_json` (crates/shingle/src/ipc.rs:737) changes only in its data sources:

- `"pid"` becomes the X client's PID; it is -1 today for every view (ipc.rs:785). Wayfire reports the XWayland surface's PID for X11 views (ipc-rules-common.hpp:35-57).
- `"app-id"` is the `WM_CLASS` class, and `"title"` the title.
- `"min-size"` and `"max-size"` come from `WM_NORMAL_HINTS`.
- `"type"` and `"role"` stay `"toplevel"`.

**Sessions.** The snapshot format (crates/shingle-core/src/session.rs) and `LaunchIdentity` (crates/shingle-core/src/core.rs:91) do not change, and neither does `reconcile_mapped` (crates/shingle-core/src/core.rs:918). What changes is how an X11 window's identity is built: `launch::capture` is split into `capture(stream)` and `capture_pid(pid)` (crates/shingle/src/launch.rs:12). The X11 identity is `capture_pid(xres_pid)`, which supplies `command`, `working_directory`, `SHINGLE_LAUNCH_ID` from `/proc/<pid>/environ` and `launch_group: pid-<pid>`, with `app_id` replaced by the `WM_CLASS` class and `title` by the window title.

An X11 placeholder therefore matches on what a Wayland one does: the app id, with a preferred `launch_id`. The saved command relaunches the X client with `DISPLAY` from the client environment, which in lazy mode starts Xwayland. Without a usable PID (no XRes reply and no `_NET_WM_PID`), the identity has only `app_id` and `title`. Such windows are restored only if something else launches them, the same as a Wayland client without credentials. Placeholders do not record which shell a window used, so an application that switches between X11 and Wayland, such as Firefox or Electron apps, keeps its slot whenever its class and app id agree. Where they differ only in case (`Code` against `code`), they do not match; Wayfire does not normalise app ids either.

### Rejected alternatives

- **A separate `X11ToplevelRole` and window type in `shingle-core`.** The core is shell-agnostic by design (no Wayland dependency, [architecture.md](../architecture.md)). Every consumer — rules, Scale, Expo, IPC, foreign-toplevel, sessions — would grow a second branch. Rejected.
- **Wrapping X11 windows as fake `xdg_toplevel` resources.** No such protocol object exists, and the client that would own it is Xwayland. Rejected.
- **Using Xwayland's own launch template for X11 windows.** Every X11 window would get Xwayland's command line and PID, and session restore would relaunch `Xwayland`. Rejected.
- **Putting the shell (`x11`/`wayland`) into `LaunchIdentity`.** It breaks restore for applications that change backend between sessions, and Wayfire keeps no such field. Rejected.

## 5. Input, selection and DnD

### Decision

**Keyboard focus.** `focus_object` (crates/shingle/src/input.rs:504) and `set_keyboard_focus` (crates/shingle/src/input.rs:1314) already send `wl_keyboard.enter` to the focused surface's client, which for an X11 window is Xwayland. Xwayland then delivers keys to the X focus window, which the WM has to set. On every keyboard focus change, `crates/shingle/src/xwayland/focus.rs` calls `Xwm::set_focus(Option<Window>)`, which follows wlroots' `xwm_set_focus_window` (xwm.c:~494) and Wayfire's map-time logic (xwayland-toplevel-view.hpp:335-395):

| ICCCM input model (`WM_HINTS.input`, `WM_TAKE_FOCUS`) | On focus |
| --- | --- |
| Passive (input, no take-focus) | `SetInputFocus(window, PointerRoot)`. |
| Locally active (input, take-focus) | `SetInputFocus` and `WM_TAKE_FOCUS`. |
| Globally active (no input, take-focus) | `WM_TAKE_FOCUS` only. On map, focus is offered only to `NORMAL` and `DIALOG` windows, as Wayfire does. |
| No input | Nothing, and the window does not take focus on map. |

Shingle then sets `_NET_ACTIVE_WINDOW` on the root and `_NET_WM_STATE_FOCUSED` on the window (wlroots' `xwm_set_focused_window`, xwm.c:530). When focus goes to a Wayland surface or nowhere, the X focus becomes `None`, `_NET_ACTIVE_WINDOW` becomes `None` and the previous window loses `FOCUSED`. `role_accepts_keyboard` (crates/shingle/src/input.rs:1453) accepts `Role::Xwayland` when the window is managed and not "no input", or unmanaged and wants focus. (XW-4 accepts every managed window, as Wayfire does; see its entry.)

**Pointer.** `surface_at` (crates/shingle/src/input.rs:1472) finds X11 surfaces through the snapshots, and `set_pointer_focus` (crates/shingle/src/input.rs:1358) sends `enter` and `motion` to Xwayland. Xwayland turns surface-local positions into X positions using the window's X geometry. So X positions must match where Shingle draws the window on the current workspace, and X stacking must match Shingle's. The XWM restacks a window Above whenever `raise_window` runs. Pointer grabs inside the X server (menus, scrollbars) work while the pointer stays on Xwayland surfaces; once it moves onto a Wayland surface, X sees no more motion. That limitation is the same under wlroots.

**Keyboard grab protocol.** `zwp_xwayland_keyboard_grab_manager_v1` is not implemented. Xwayland uses it for its rootful `-host-grab` mode; Shingle runs rootless.

**Cursor.** X clients' cursors arrive as `wl_pointer.set_cursor` from Xwayland, along the existing cursor-surface path. The root cursor set at bring-up covers windows that never set one.

**Text input.** X clients use XIM, not text-input-v3, so Shingle's input-method support (crates/shingle/src/input_method.rs) does not reach them. Out of scope.

**Clipboard and primary selection.** Both are bridged both ways, porting wlroots' `selection/` (selection.c 345 lines, incoming.c 542, outgoing.c 483). The XWM owns a hidden selection window, selects `XFixesSelectionNotify` for `CLIPBOARD` and `PRIMARY`, and converts into the `_WL_SELECTION` property.

- **Wayland to X.** When the Wayland selection changes, the XWM takes `CLIPBOARD` (or `PRIMARY`) with `SetSelectionOwner`. Today that happens in `data_device::sync_selection` (crates/shingle/src/data_device.rs:179) and `primary_selection::sync_selection` (crates/shingle/src/primary_selection.rs:170), which `poll` calls (crates/shingle/src/lib.rs:735-738). Requests are then answered as follows:
  - **`TARGETS`** gets the MIME types translated to atoms: `text/plain;charset=utf-8` is `UTF8_STRING`, `text/plain` is `TEXT` and `STRING`, and any other type is the atom of the same name, as wlroots does.
  - **`TIMESTAMP`** is answered too.
  - **Data.** The XWM opens a pipe, calls `data_device::receive_from(source, mime, write_end)` (crates/shingle/src/data_device.rs:122), and reads the read end as a calloop source. It writes the result to the requestor's property, switching to INCR above 64 KiB, wlroots' `INCR_CHUNK_SIZE` (include/xwayland/selection.h:8).
- **X to Wayland.** When an X client takes ownership, the XWM converts `TARGETS` and registers an X11 selection source with those MIME types. When a Wayland client asks for one of them, the XWM converts that target and copies the property, or the INCR chunks, into the client's descriptor without blocking.
- **Shingle's data model.** `Source` in data_device.rs (crates/shingle/src/data_device.rs:23) and in primary_selection.rs (crates/shingle/src/primary_selection.rs:24) has a client `WlDataSource` and an optional `ZwlrDataControlSourceV1`. It gains a `SourceOrigin::{Client, Control, X11}` field, with the `WlDataSource` resource optional, because an X11 source has no Wayland object. `set_control_selection` (crates/shingle/src/data_device.rs:87) already shows how a selection owned by a non-`wl_data_device` party is installed. The X11 path is the same with a different origin, so wlr-data-control clipboard managers see X11 selections too.
- **Who can read.** Wayland clients still only receive offers while focused (data_device.rs:179). X clients can read the Wayland selection whenever they ask, as under wlroots, because X11 has no focus gate.
- **`behavior.disable_primary_selection`** (crates/shingle-config/src/lib.rs:688) also turns off the `PRIMARY` bridge.

**Drag and drop** waits for XW-6. In both directions it needs Xdnd: wlroots' selection/dnd.c (393 lines), smithay's xwm/dnd.rs (1,222). It also needs X windows as drop targets inside `data_device::sync_drag` (crates/shingle/src/data_device.rs:231), which today follows `wl_pointer` focus only. Until then, a drag stays inside its own world: X to X works inside Xwayland, and Wayland to Wayland works as today.

### Rejected alternatives

- **Focusing only the `wl_surface` and leaving X focus to Xwayland.** Rootless Xwayland does not move X focus on `wl_keyboard.enter`. Clients that never call `SetInputFocus` (passive ones) would get no keys. Rejected.
- **Bridging the clipboard only from Wayland to X, the cheaper half.** Copying from `xterm` into Wayland applications is the most common case. Rejected.
- **DnD in the first selection workspace.** It doubles the size of XW-5 and is the least used of the three. Deferred, not rejected.

## 6. Scaling and geometry

### What Wayfire and wlroots do

Xwayland's rootless X screen is the union of the outputs' logical rectangles, which it learns from `wl_output` and `xdg_output`. It commits every window with buffer scale 1. Shingle already advertises both (crates/shingle/src/xdg_output.rs and `set_output_scale` with `wl_output.scale`, crates/shingle/src/state.rs:962). So one X pixel is one logical pixel, and at output scale 2 the compositor stretches each X buffer to twice its size. wlroots' `xwayland/` has no scale code at all; its only related option is `-force-xrandr-emulation`. Wayfire adds nothing on top. Xwayland's `-hidpi` works only rootful (its man page), so rootless HiDPI is up to the compositor. KWin does it itself with `XwaylandClientsScale`: Xwayland gets a scale override and KWin scales down (kwin src/wayland_server.cpp:691, src/workspace.cpp:697). sway has no such option; its `xwayland force` only means starting eagerly.

### Decision

Match Wayfire. X windows are configured in Shingle's logical coordinates, with X positions equal to Shingle positions inside `state.output` (crates/shingle/src/state.rs:136). Their buffers are drawn through the ordinary scale-1 path (`place` already derives logical size from `buffer.scale`, crates/shingle/src/placement.rs:40-52). With fractional or integer scale above 1, X11 clients are therefore blurry, exactly as under Wayfire. Xwayland does not bind `wp_fractional_scale_v1` for X windows, so nothing changes in crates/shingle/src/fractional_scale.rs.

Geometry details:

- **Output origin.** Shingle has one output today, so the X root window is that output's logical rectangle. When the multi-output roadmap item lands, X positions become positions in the layout, which is what Xwayland expects for several outputs. Wayfire adds the output's layout origin for the same reason, in `set_output_offset` (src/view/xwayland/xwayland-toplevel.cpp:56).
- **Output resize, transform and scale.** Resizes (`set_output`, crates/shingle/src/state.rs:997) and transforms (`reconfigure_all` at crates/shingle/src/state.rs:910) reach X toplevels through `configure_toplevel`, the same path xdg toplevels take. A scale change leaves logical geometry unchanged, so X windows get no configure.
- **Margins and decorations.** None. Shingle draws no server-side frames on X11 windows, so the X geometry is the window geometry.

### Rejected alternatives

- **"Zero scaling" now**: X in physical pixels, scaled down by Shingle, as KWin does. It needs xdg_output and wl_output values specific to the Xwayland client, a conversion at every X configure, position and input path, and a policy for mixed-scale outputs, and it changes `xrandr` for X clients. It is the right later step for HiDPI users, so it is XW-7 and optional, not part of the first delivery.
- **Rootful Xwayland with `-hidpi`.** That means no per-window management. Rejected.

## 7. Testing

### Installed on this host (2026-11-25)

| Tool | Package | Present |
| --- | --- | --- |
| `Xwayland` | xwayland 2:24.1.6-1 | Yes |
| `xterm` | xterm 398-1 | Yes |
| `xeyes`, `xlogo`, `xclock` | x11-apps 7.7+11+b1 | Yes |
| `xprop`, `xwininfo`, `xlsclients`, `xev`, `xdpyinfo` | x11-utils 7.7+7 | Yes |
| `xclip` | xclip 0.13-4 | Yes |
| `xrandr`, `xset`, `xsetroot`, `xkill`, `xrdb`, `xmodmap` | x11-xserver-utils 7.7+11 | Yes |
| `xmessage`, `xcalc`, `xedit`, `xwd` | x11-apps / x11-utils | Yes |
| `glxgears` | mesa-utils 9.0.0-2+b2 | Yes |
| `wl-copy`, `wl-paste` | wl-clipboard 2.2.1-2 | Yes |
| `grim`, `wayland-info` | | Yes |
| `xdotool` | trixie has 1:3.20160805.1-5.1 | **No** |
| `wmctrl`, `xsel`, `python3-xlib`, `python3-xcffib`, `foot` | | **No** |

Development headers `libx11-dev` and `libxcb1-dev` are installed, though `RustConnection` does not need them. trixie's `xdotool` predates `windowstate`, so it could not set `_NET_WM_STATE` anyway. Instead, the client side of the tests is a small x11rb program in the workspace (below), so nothing has to be installed. The operator decided not to install `xdotool` ([Decisions](#decisions)); STIPC's `feed_key`, `feed_button` and `move_cursor` already cover that inside Shingle.

### Layer 1: unit tests, no X server

In `shingle-xwayland`:

- property parsers (`WM_CLASS`, `WM_NORMAL_HINTS`, `WM_HINTS`, `_NET_WM_STATE`, `_MOTIF_WM_HINTS`), tested on byte buffers;
- classification (managed, unmanaged, DnD, dialog) against Wayfire's rules;
- `Pairing` in every order, and with destroys on either side;
- the ICCCM input-model table;
- `_NET_WM_STATE` add/remove/toggle;
- the `_NET_WM_MOVERESIZE` edge mapping;
- ConfigureRequest policy for unmapped, tiled, fullscreen and floating windows;
- MIME and atom mapping, and INCR chunking;
- display-number selection and stale-lock detection against a temporary directory (the socket directory is a parameter of `DisplaySockets::bind_in`);
- the restart rule as a pure function of run time and mode.

The XWM runs against a recording fake of its `XConnection` trait. A test feeds x11rb event structs in and checks the requests that come out, in the same style as Shingle's in-process Wayland test client (crates/shingle/src/test_client.rs).

In `shingle`, with the in-process test client and a fake XWM:

- `xwayland_shell_v1` is visible only to a client marked `xwayland`;
- the role, serial and commit rules and their protocol errors;
- an X11 `ToplevelRole` tiles, floats, fullscreens and minimizes;
- `view_json` gives `pid`, `app-id` and `title`;
- `view-pre-map` blocks an X11 window;
- `close-view` reaches the XWM;
- an X11 window's identity comes from `capture_pid`;
- placeholder reconciliation by class;
- `client_environment` with and without a server.

### Layer 2: real clients in a private namespace

A Python harness, `tools/xwayland_test/run.py`, starts headless Shingle (`--headless`, no host contact at all) for protocol-level checks, or nested Shingle for pixel checks, under `unshare --user --map-root-user --mount --net --pid --fork --mount-proc`:

- **X sockets.** A private directory is bind-mounted over `/tmp/.X11-unix`, so neither the host's `X0`/`X1` nor its abstract sockets are visible.
- **Signals.** The PID namespace confines every signal the harness sends to processes it started, and it checks each PID before signalling it (positive, above 1, and the expected program, such as `Xwayland`), because `kill(-1)` from a mapped-root user namespace without a PID namespace reaches every process of the operator's user; the first XW-2 run did exactly that.
- **Devices.** Empty tmpfs mounts cover `/dev/dri` and `/dev/input`, so neither Xwayland nor anything else in the namespace can open the host's GPU or input devices.
- **Other directories.** There are private `XDG_RUNTIME_DIR`, `XDG_CONFIG_HOME`, `XDG_STATE_HOME`, `HOME` and `--ipc` paths, and `/tmp/.X<N>-lock` lives in a private `/tmp` bind as well.
- **Nested runs.** `WAYLAND_DISPLAY` is set to the host socket's absolute path ([development.md](../development.md#running)). Pathname Unix sockets still connect across network namespaces. No host input is sent; input comes only from STIPC.

The namespace was checked on 2026-11-25. `unshare -U -r -m -n` with a bind mount over `/tmp/.X11-unix` works outside the Claude sandbox, and showed an empty directory while the host's still held `X0` and `X1`. Inside the sandbox, `unshare` fails with "Operation not permitted", so like cargo the harness runs outside it.

**The probe client.** `crates/shingle-xwayland/examples/xw_probe.rs` is an x11rb client that:

- creates windows with a given class, title, type, transient-for, size hints, input model, override-redirect flag and initial `_NET_WM_STATE`;
- sends `_NET_WM_STATE`, `_NET_ACTIVE_WINDOW`, `WM_CHANGE_STATE` and `_NET_WM_MOVERESIZE` messages;
- reports its ConfigureNotify geometry, FocusIn/FocusOut and `WM_DELETE_WINDOW` as JSON lines.

It is the X11 counterpart of the in-process Wayland client.

**Checks, by workspace:**

- **Server (XW-2).**
  - `xdpyinfo` and `xprop -root _NET_SUPPORTED _NET_SUPPORTING_WM_CHECK` succeed on `stipc/get_display`'s `DISPLAY`.
  - In lazy mode no `Xwayland` process exists before the first X client starts.
  - `kill -9` of Xwayland after 6 s restarts it, after 2 s does not.
  - SIGTERM removes the lock and the socket.
  - A `--spawn` child sees `DISPLAY=:N`.
- **Windows (XW-3).**
  - `xterm`, `xeyes` and `xlogo` map, tile in mapping order and fill the workarea.
  - `xwininfo -id` matches `view-info` geometry.
  - `list-views` shows `app-id` `XTerm` and a real `pid`.
  - `close-view` closes `xterm` through `WM_DELETE_WINDOW` and kills `xeyes`, which does not list it. (On this host `xeyes` lists it too, so `KillClient` is checked with the probe client and `--no-delete`.)
  - `configure-view` reaches the X geometry.
- **Behaviour (XW-4).**
  - Keys from `stipc/feed_key` reach the focused `xterm`, checked by `xterm -e sh -c 'read l; echo "$l" > out'`.
  - Focus moves between an X11 and a Wayland window.
  - xterm's Ctrl+button menus (override-redirect) appear above the window at the click position.
  - Probe windows with each input model, transient dialogs, fixed-size dialogs, fullscreen, maximize and minimize requests.
  - Scale and Expo include X windows.
  - A session saved with `xterm` open restores it into its slot.
- **Selections (XW-5).**
  - `xclip -selection clipboard` to `wl-paste`, and `wl-copy` to `xclip -o`, for small payloads and for 1 MiB (INCR).
  - The same with `-selection primary` and `wl-paste --primary`.
  - Primary is not bridged when `disable_primary_selection` is set.
- **Pixels (nested only).** `grim` through screencopy shows `xlogo`'s pattern at its view geometry.

Results are recorded in [status.md](../status.md), separate from unit tests, per [development.md](../development.md#real-client-tests). Temporary sockets, logs and namespace directories are removed afterwards.

### Rejected test strategies

- **Running X clients against the host's Xwayland, or binding `/tmp/.X11-unix` on the host.** Clients could open on the operator's session, and a crash could leave stale locks in the shared directory. Rejected in favour of the namespace.
- **Xvfb or Xephyr as a stand-in.** Neither is installed, and neither exercises `xwayland_shell_v1` or rootless association, which are what is being tested. Rejected.
- **Installing `xdotool`, `wmctrl` or `python3-xlib` by default.** The probe client covers EWMH messages exactly and reproducibly, and STIPC covers input. Left to the operator.

## 8. Implementation plan

The workspaces are ordered by dependency. Each can be finished and verified on its own. Every workspace also follows the repository rules: unit tests for new code, clippy clean, 80 columns, docs/status.md, docs/roadmap.md, docs/ipc.md and docs/features.md updated, real-client results recorded apart from unit tests, and signed commits. XWayland does not depend on the native backend or on DMA-BUF. Without `zwp_linux_dmabuf_v1` or `wl_drm`, Xwayland's glamor cannot share buffers, and it commits `wl_shm` buffers through its software path. XW-2 confirms this from Xwayland's log, and passes `-shm` only if Xwayland fails without it.

### XW-1 `shingle-toplevel-handle`: shell-neutral toplevel and client environment

Make `ToplevelRole` carry `ToplevelHandle` ([4](#4-integration-with-the-window-model)) with only the `Xdg` variant used. Add `Role::toplevel_mut`, and replace the direct `XdgKind::Toplevel` matches with the helpers. Route configure, close and activation through `ToplevelHandle` methods. Add `Compositor::client_environment()` and use it at every spawn site, removing the inherited `DISPLAY`. Split `launch::capture` into `capture` and `capture_pid`.

Acceptance:

- No behaviour change for Wayland clients: all tests pass, and the nested smoke test, IPC fixtures and session round trip from [status.md](../status.md#with-real-clients) are repeated with identical results.
- `rg 'XdgKind::Toplevel' crates/shingle/src` matches only shell.rs's role helpers and xdg request dispatch.
- A unit test and a real run show that children of nested Shingle no longer see the host's `DISPLAY`, and still get `WAYLAND_DISPLAY`, `WAYFIRE_SOCKET` and `_JAVA_AWT_WM_NONREPARENTING=1`.

Done, 2026-11-25 (branch `shingle-toplevel-handle`). Results are in [status.md](../status.md#with-real-clients). Where the code differs from the text above, the code was followed:

- **No `X11` variant yet.** `ToplevelHandle` (crates/shingle/src/shell.rs) has only `Xdg`. An `X11(x11rb::...::Window)` variant would need `x11rb` as a dependency of `shingle` before XW-2 adds it, and an unconstructed variant fails `clippy -D warnings`. Every `match` on the handle is written so XW-3 adds one arm.
- **Activation has no handle method.** For xdg, being the active window is a configure state, so `configure_toplevel` takes a shell-neutral `ToplevelStates { tiled, activated, maximized, fullscreen }` and the `Xdg` arm encodes it (`ToplevelStates::xdg_states`). X focus (XW-4) will be a separate call on focus change, not part of configure.
- **Close is one function.** `shell::close_window` serves `Compositor::close_window`, `close-view` and foreign-toplevel. The xdg-only protocols (xdg-dialog, xdg-foreign, xdg-toplevel-icon) reach the `xdg_toplevel` through `ToplevelHandle::xdg`, which returns `None` for other shells.
- **`rg 'XdgKind::Toplevel' crates/shingle/src`** matches three lines: the two role helpers and `GetToplevel` in xdg request dispatch. The other xdg request handlers use the helpers too.
- **The environment is a type.** `client_environment` returns `shingle_session::ClientEnvironment` (re-exported as `shingle::ClientEnvironment`) instead of a bare `Vec`, because the session crate's launch functions, which `shingle` depends on, take it. Its constructor `ClientEnvironment::new(wayland_display, x_display, ipc_socket)` is the one place the environment is built. Besides `WAYLAND_DISPLAY`, `DISPLAY` and `_JAVA_AWT_WM_NONREPARENTING`, it removes `WAYLAND_SOCKET`, which libwayland prefers over `WAYLAND_DISPLAY`, and sets `WAYFIRE_SOCKET` only when Shingle serves IPC, removing it otherwise; without `--ipc`, nested children used to get the host's. So the acceptance line above holds with `WAYFIRE_SOCKET` only under `--ipc`.
- **More spawn sites.** Besides those listed in [What exists today](#what-exists-today), config command bindings, IPC binding commands (run by the frontend, crates/shingle-frontend/src/frontend/bindings.rs, or by the compositor without a frontend, crates/shingle/src/ipc.rs) and the lock client use it. `--spawn` and the lock client go through the new `launch_program`. `launch_saved` takes `Option<&ClientEnvironment>`; `None` keeps the caller's environment for `RestoreReport::launch`, which Shingle does not call. (Later, on the `shingle-doc-bugs` branch, `SessionStore::restore` and `RestoreReport` were removed and `launch_saved` takes a `&ClientEnvironment`, so no saved client can start in the host's environment.)
- **`WAYFIRE_SOCKET` is no longer set on the process.** The binary used to `set_var` it. `Frontend::new` now takes the IPC path and enables IPC before starting any client, which the nested backend used to do after `Frontend::new` returned.
- **Pinned first.** Before the refactor, `wayland_toplevels_look_unchanged` (crates/shingle/src/wayland_pins.rs) recorded IPC replies and events, configures, surface snapshots and sessions for xdg toplevels in crates/shingle/testdata/wayland_pins.txt; the refactor left the file unchanged.

### XW-2 `shingle-xwayland-server`: sockets, process, configuration and XWM bring-up

Create `crates/shingle-xwayland` with `sockets.rs` (`DisplaySockets`), `server.rs` (argv, environment, pidfd, displayfd, the restart rule) and `xwm/` with connection, atoms and bring-up only. Add `XwaylandConfig` and the Wayfire import of `core/xwayland` and `core/xwayland_startup_script`, plus the WCM fields. Mark the Xwayland Wayland client in `ClientDataImpl`. Fill in `DISPLAY` in `client_environment`, `stipc/get_display`, and the new `stipc/get_xwayland_display` and `stipc/get_xwayland_pid`. Push the `xwayland-ready` event and run the startup script.

Acceptance:

- Unit tests for display selection, stale locks, argv, the restart rule, the config model and the importer.
- Real runs in the namespace harness:
  - `xdpyinfo` and `xprop -root` work;
  - eager mode starts before restored clients;
  - lazy mode starts on the first connection;
  - Xwayland is restarted, or not, by run time;
  - SIGTERM leaves no lock or socket;
  - `list-methods` returns 79 methods.
- Xwayland's log confirms the shm path, and the compile-time cost of a second `x11rb` in nested builds is recorded.
- With `mode: false` nothing is bound and `DISPLAY` is absent.

Done, 2026-11-25 (branch `shingle-xwayland-server`). Results are in [status.md](../status.md#with-real-clients): the namespace harness passed 47/47 checks, and Xwayland's log confirms its shared-memory path without `-shm` (`Failed to initialize glamor, falling back to sw`). Building x11rb 0.14 with the features used takes about 2.6 s in a debug build. Where the code differs from the text above, the code was followed:

- **`-terminate` only in lazy mode.** Xwayland's `-terminate` fires when every remaining client has asked XFixes to be ignored (`XFixesShouldDisconnectClient`, dix/dispatch.c). Without that request from the XWM, a lazy server never exited when idle; the first harness run showed it still running 40 s after its last client. The XWM now sends `SetClientDisconnectMode(Terminate)` as wlroots does. With it, a bare `-terminate` made an eager server exit as soon as its last X client left, so eager mode passes no `-terminate`, which matches wlroots with `terminate_delay` 0.
- **No restart after a short run, in either mode.** Step 4 of "Crash and exit" now says so; wlroots does not re-arm the lazy watch either.
- **Ownership.** `Compositor` owns the glue (`xwayland::Xwayland` in crates/shingle/src/xwayland.rs, holding the `XwaylandServer`, the `Xwm` and the calloop tokens), because starting needs the display handle and the event loop. `CompositorState.xwayland` mirrors what IPC and the client environment need (display, PID) and the flags the sources set. Each source is removed before its descriptor closes.
- **Exit detection** is the pidfd alone. A closed Xwayland Wayland connection is only logged (`ClientDataImpl.xwayland`); Xwayland exits with it, and the pidfd follows.
- **XWM events are read every turn**, not only when the socket is readable, because x11rb may buffer events while it waits for a reply. Bring-up uses checked requests for the Composite redirect and the root event mask, so a second window manager fails bring-up; a failed bring-up stops the server.
- **`xwayland.mode` is an enum** (`shingle_config::XwaylandMode`), written in YAML as `true`, `false` or `lazy` (booleans or strings), rather than a validated string.
- **Library default.** `shingle::Config::default()` disables XWayland, so tests and embedders bind nothing in `/tmp`; the configuration file's default is `lazy` and reaches the compositor through `From<ShingleConfig>`. `XwaylandSettings` also carries the socket directory and the program, which tests point at a temporary directory and a bash stand-in for Xwayland. The startup script's output follows `behavior.discard_command_output`.
- **Root cursor and XFixes selection events** (bring-up steps 6 and 7) are left to XW-4 and XW-5, as the plan already put the cursor in XW-4.
- **The probe client** (`examples/xw_probe.rs`) is not written yet: XW-2's checks need only `xdpyinfo` and `xprop`. XW-3 adds it.
- **`DISPLAY` is printed** by headless and nested next to `WAYLAND_DISPLAY` when an X display is bound.

### XW-3 `shingle-xwayland-windows`: association and managed toplevels

Add `xwayland_shell_v1`, `Role::Xwayland`, `Pairing` and `try_map`. Build `XWindow` property reading and classification, and the MapRequest, ConfigureRequest, PropertyNotify, UnmapNotify and DestroyNotify handling. Managed windows become `ToplevelRole`s with `ToplevelHandle::X11`. Add X configure, states and close, `_NET_CLIENT_LIST`, X11 identity via XRes and `capture_pid`, and the `view_json` sources. Override-redirect windows are tracked but not drawn yet.

Acceptance:

- Unit tests for pairing, parsers, classification and configure policy, plus `shingle` tests with a fake XWM.
- Real runs: `xterm`, `xeyes` and `xlogo` tile and match `xwininfo`; titles and classes appear in `list-views` with real PIDs; `close-view` uses `WM_DELETE_WINDOW` or `KillClient`; `configure-view` and `simple-tile/set-layout` reach X geometry; a `view-pre-map` subscriber holds an X window until `unblock-map`.
- An X window destroyed before its surface commits, and a surface destroyed before the serial message, both leave no stray window. This is checked with the probe client.

Done, 2026-11-25 (branch `shingle-xwayland-windows`). Results are in [status.md](../status.md#with-real-clients): the namespace harness passed 87/87 checks with `xterm`, `xeyes`, `xlogo`, `xprop`, `xwininfo` and the probe client, including the XW-2 groups. Where the code differs from the text above, the code was followed:

- **Unmap removes the Shingle window.** On `UnmapNotify` the X window is dissociated, its toplevel role dropped and its Shingle window removed, and a remap creates a new `WindowId`, as a Wayland toplevel that commits a null buffer gets one (`unmap_toplevel`). Keeping the `WindowId` as a placeholder (`reconcile_unmapped`) would let another window of the same class claim it through `reconcile_mapped` while the first is withdrawn, and the core has no "unmapped but owned" state. XW-4 can add one if session restore or rules need the ID to survive.
- **Module layout.** The glue is `crates/shingle/src/xwayland/` with `mod.rs` (XW-2's server glue), `shell.rs` (`xwayland_shell_v1`, `XwaylandRole`) and `windows.rs` (events, pairing, `try_map`, the X half of configure, close). `Role::Xwayland(Box<XwaylandRole>)` holds the role resource, the pending and committed serials, the paired window and, for a managed window, `Option<Box<ToplevelRole>>`, which `Role::toplevel` returns. `Pairing<S>` is generic over the surface key, and `shingle` uses `SurfaceId`.
- **Requests are queued in `CompositorState`.** The XWM is owned by `Compositor`, not the state, so `configure_toplevel`, `close_window` and the `ConfigureRequest` handler push `XRequest`s (configure, `_NET_WM_STATE`, close) to `state.xwayland.requests`, and `Xwayland::flush` hands them to the XWM once per turn. The XWM drops a configure or state identical to the last one it sent, so `reconfigure_all` causes no needless X traffic; a `ConfigureRequest` is always answered. X windows on other workspaces are therefore not singled out, as section 4 proposed: they keep their rectangle because their tile does not change, and nothing is sent for an X window before it is a Shingle window.
- **`ConfigureRequest`** carries the current geometry with the masked fields applied. Before the window is a Shingle window the request is granted and becomes its `floating_geometry`, used if it floats; the stacking fields are ignored until XW-4 restacks.
- **Initial `_NET_WM_STATE`** is parsed into `WindowInfo` but not applied; client state requests in both directions are XW-4. So are all client messages except `WL_SURFACE_SERIAL`, including `_NET_CLOSE_WINDOW`. `_NET_CLIENT_LIST_STACKING` has the same contents as `_NET_CLIENT_LIST` until XW-4 restacks X windows.
- **Unmanaged windows** keep `Role::Xwayland` with no toplevel, so they are paired but neither drawn nor listed; `SurfaceRole::Unmanaged` comes with drawing them in XW-4.
- **`pid` in view JSON** is set for X11 views only (`ToplevelRole.pid`); Wayland views keep -1, so `wayland_toplevels_look_unchanged` still matches its recording.
- **Role object first.** Xwayland destroys `xwayland_surface_v1` before the `wl_surface`; destroying it marks the role dead for `has_live_role`, so the surface may go without the `defunct_role_object` error, and the association stays as the protocol says.
- **Quiet `BadWindow`.** Every close produced a `BadWindow` for `WM_STATE` on the `UnmapNotify` that precedes `DestroyNotify`; the XWM no longer reports `BadWindow` errors for windows it has already forgotten, as wlroots ignores them.
- **`WL_SURFACE_ID`** is not interned, so the XWM cannot recognise it; it logs once that it ignored an unknown client message type and that a `WL_SURFACE_ID` would mean an Xwayland older than 23.1.
- **The probe client** is `crates/shingle-xwayland/examples/xw_probe.rs`, driven by command-line options and stdin commands; it already sends the XW-4 messages (`_NET_WM_STATE`, `_NET_ACTIVE_WINDOW`, `WM_CHANGE_STATE`, `_NET_WM_MOVERESIZE`). It refuses to run unless `SHINGLE_XWAYLAND_TEST_NAMESPACE=1`, which only the harness sets.
- **X clients run as uid 1000** in a nested user namespace inside the harness's, because `xterm` run as the namespace's root tries to give its pty to group `tty`, which is not mapped, and exits. The harness's signals all go through `signal_checked` (PID above 1 and the expected `/proc/<pid>/comm`), which `kill_xwayland` now uses.

### XW-4 `shingle-xwayland-behaviour`: focus, unmanaged windows and states

Add ICCCM focus and the FocusIn guard, `_NET_ACTIVE_WINDOW`, `_NET_WM_STATE` handling in both directions, `WM_CHANGE_STATE`, `_NET_WM_MOVERESIZE`, X restacking, transients and dialog classification, and `role_accepts_keyboard` for X roles. Draw override-redirect windows in the Unmanaged layer, with `SurfaceRole::Unmanaged`. Set the root cursor. Verify Scale, Expo, the switchers, window rules and session restore with X windows.

Acceptance:

- Unit tests for the input-model table, state messages and moveresize mapping.
- Real runs:
  - typed text reaches the focused `xterm`, and focus switches cleanly between X and Wayland windows;
  - xterm's override-redirect menus appear at the pointer, take no keyboard focus and disappear on release;
  - probe windows with each input model, a transient dialog and a fixed-size dialog behave as Wayfire specifies;
  - fullscreen, maximize and minimize work from both the client and IPC;
  - Scale and Expo show X windows;
  - an `app_id is "XTerm"` window rule applies;
  - a saved session with `xterm` restores into its slot.

Done, 2026-11-25 (branch `shingle-xwayland-behaviour`). Results are in [status.md](../status.md#with-real-clients): the namespace harness passed 173/173 checks in ten groups, 86 of them in the new `behaviour`, `session` and `nested` groups. The glue is `crates/shingle/src/xwayland/` with `focus.rs` (the X focus and stacking Shingle hands the window manager each turn), `requests.rs` (client requests and what a managed window gets when it maps) and `unmanaged.rs`; the window manager's part is in `crates/shingle-xwayland/src/xwm/` (`mod.rs`, `messages.rs`, `cursor.rs`). Where the code differs from the text above, the code was followed:

- **Every managed X window accepts the keyboard.** Section 5 has `role_accepts_keyboard` refuse managed "no input" windows. Wayfire's X toplevels are always focusable (it clears `kb_focus_enabled` only for unmanaged views, src/view/xwayland/xwayland-unmanaged-view.hpp:169), so a click focuses a no-input or globally active window in Shingle too. The input model decides what happens on map (`requests::focus_on_map`: passive and locally active windows are focused, globally active `NORMAL` and `DIALOG` windows are offered the focus, others are not focused) and what the window manager sends: a globally active window gets `WM_TAKE_FOCUS` and no `SetInputFocus`, and a no-input window gets the X focus set to `None`, so that keys do not reach the X window that had it; wlroots leaves the X focus alone there.
- **Override-redirect menus may take the keyboard.** The acceptance list says xterm's menus take no keyboard focus. With the rule section 2 gives, which is Wayfire's (`kb_focus_enabled` from wlroots' `wlr_xwayland_or_surface_wants_focus`, and an input model other than no-input), they do: xterm's menus carry no `_NET_WM_WINDOW_TYPE` and no `WM_HINTS`, so the heuristic allows focus and the input model is passive. Shingle follows Wayfire: the menu's `wl_surface` gets the keyboard on its first buffer, while the X focus, `_NET_ACTIVE_WINDOW` and xterm's activated state stay on xterm (`focus::Target::Keep`), and the keyboard returns to xterm when the menu unmaps. A menu that says it is one (`_NET_WM_WINDOW_TYPE_POPUP_MENU` and the like) takes no keyboard. The harness checks both. Moving the pointer focus along with the keyboard, as `focus_object` does for windows, ended xterm's pointer grab and closed the menu at once, so an unmanaged window's focus moves only the keyboard (`input::focus_surface`), as Wayfire's `focus_request` does.
- **`_NET_WM_STATE_FOCUSED` belongs to the window manager.** `Xwm::set_state` ignores the `focused` it is given and `Xwm::set_focus` writes it, which also covers a client moving the focus between its own windows through the `FocusIn` exception, which the compositor is not told about, as in wlroots.
- **Focus and stacking are read once per turn.** `Xwayland::flush` takes Shingle's keyboard focus and stacking order (`xwayland/focus.rs`) instead of hooking `focus_object` and `raise_window`. The stacking order is the one `visible_surfaces` draws (other workspaces first, then the current one by stacking mode and stack order); when it changes, each window is stacked just above the one below it, as wlroots' `wlr_xwayland_surface_restack` does with a sibling. The stacking fields of a client's `ConfigureRequest` are still ignored, as Wayfire ignores them.
- **`_NET_CLOSE_WINDOW`** is handled, but `_NET_SUPPORTED` keeps wlroots' 11 atoms, which do not list it.
- **Tiling rules apply to X toplevels only.** Wayfire's `can_tile_view` (a parent, or equal positive minimum and maximum sizes) floats X toplevels when they are created (`requests::can_tile`). xdg toplevels with a parent still tile, as before, which keeps `wayland_toplevels_look_unchanged` unchanged; making them follow Wayfire too is a separate change. A transient is centred on its parent and kept in the workarea, as Wayfire's `reposition_relative_to_parent` does, and a window that says it is `NORMAL` has no parent, as in Wayfire's `on_set_parent`.
- **Initial state.** Fullscreen, maximized (either direction, as Wayfire's `handle_map_request` reads it) and hidden apply when the window is created, after window rules; a `_NET_WM_STATE` or `WM_CHANGE_STATE` message before the map changes that initial state.
- **An unmapped and remapped X window gets a new `WindowId`**, as in XW-3, and this stays the decision. The window manager keeps the X window, but the Shingle window goes. Keeping the ID would need an "unmapped but owned" state in `shingle-core`, without which another window of the same class could claim the withdrawn window's placeholder; and a Wayland toplevel that unmaps with a null buffer also comes back with a new ID. Session restore does not depend on it: the placeholders come from the saved session at startup, and each is claimed by the first map of its relaunched client, which the `session` group checks with xterm and xlogo. Window rules run again on a remap, as for any new window.
- **Two fixes outside the XWayland code.** The real runs found that X's root window was 0x0, which clamped every X pointer position to (0, 0), because Shingle's xdg-output version 3 ended its details with the deprecated `zxdg_output_v1.done` instead of a `wl_output.done` (crates/shingle/src/xdg_output.rs). And headless Shingle had no STIPC devices and reported no frames, so STIPC input did nothing there and a nested Shingle inside it drew one frame; headless now registers a keyboard, a pointer and a touch device, as Wayfire's stipc plugin does, and reports a frame at the output's refresh rate, as wlroots' headless backend does.
- **The root cursor** is `cursor_shape`'s image (the theme's `default`, else `left_ptr`, at `input.cursor_size`), set when the window manager connects; a cursor theme changed in the config reaches X at the next Xwayland start.
- **`_NET_WM_MOVERESIZE`** is checked by unit tests only: headless has no frontend to run a move or resize, and the nested group does not drive one.
- **wlroots reference.** The focus, state and moveresize handling was compared with wlroots' xwm.c from its GitHub mirror (2022), since gitlab.freedesktop.org refused automated access. The `WM_TAKE_FOCUS` table in section 5 (locally and globally active windows only) is what wlroots master does; the 2022 code also sent it to passive windows.

### XW-5 `shingle-xwayland-selection`: clipboard and primary selection

Add `SourceOrigin` in data_device.rs and primary_selection.rs, the XWM selection window, XFixes owner tracking, both transfer directions with INCR, and MIME/atom mapping.

Acceptance:

- Unit tests for the MIME mapping, INCR chunking and source replacement.
- Real runs with `xclip`, `wl-copy` and `wl-paste` in both directions, for both selections, at 10 bytes and 1 MiB.
- wlr-data-control sees X selections.
- `disable_primary_selection` stops the primary bridge.
- No transfer blocks the compositor: a slow writer stalls only its own transfer, which the harness checks by delaying a writer with `sleep`.

### XW-6 `shingle-xwayland-dnd`: drag and drop (later)

Bridge Xdnd in both directions with `data_device`'s drag (crates/shingle/src/data_device.rs:231), and add DnD-type windows as drag icons. It can start any time after XW-5.

Acceptance: a drag from an X client (`xedit`, or the probe client acting as an Xdnd source) into a Wayland client, and the reverse, deliver the data, with unit tests for the Xdnd state machine.

### XW-7 `shingle-xwayland-hidpi`: zero-scaling mode (optional)

An `xwayland.zero_scaling` option ([6](#rejected-alternatives-5)). It is deferred until after the multi-output item ([Decisions](#decisions)).

Acceptance: at output scale 2, `xdpyinfo` reports the physical size and `xlogo` is drawn sharp (pixel check through `grim`), with input and override-redirect positions correct.

XW-1 has no dependencies and can start right away, in parallel with the remaining items ahead of XWayland in the roadmap. XW-2 needs XW-1 for the client environment. XW-3 needs XW-2, and XW-4 needs XW-3. XW-5 needs XW-3 (selections need a focused X window to be useful, though not XW-4's focus rules) and can run in parallel with XW-4. XW-6 and XW-7 come last, in either order.

## Decisions

The operator answered the former open questions on 2026-11-25, approving the proposed defaults:

1. **Shingle's default `xwayland.mode` is `lazy`.** The Wayfire importer keeps Wayfire's `true` when the option is missing.
2. **No extra X tools are installed.** Tests use the in-tree probe client and STIPC.
3. **XW-7 is deferred** until after the multi-output item.
4. **Override-redirect windows stay out of `list-views`.**

The questions as they were posed:

1. **Shingle's default `xwayland.mode`.** Wayfire's default is `true`, which is eager (metadata/core.xml:48-63). The proposal is `lazy` for Shingle's own default: nested test runs would not start an X server they do not use, and `DISPLAY` is known either way. The Wayfire importer would keep Wayfire's meaning, so an imported config without the key gets `true`. The alternative is `true` everywhere, which matches Wayfire exactly.
2. **Installing test tools.** Should `xdotool` (and perhaps `wmctrl`) be installed for manual checks, or is the in-tree probe client plus STIPC enough? The design assumes nothing is installed.
3. **XW-7 (zero-scaling HiDPI).** Wanted at all, and if so, before or after the multi-output item? Wayfire does not offer it.
4. **Override-redirect windows in `list-views`.** Wayfire lists them with `"type": "x-or"`. The proposal leaves them out, consistent with layer-shell surfaces, which Shingle also omits today. Listing them would need IDs outside `shingle-core`'s window table.
