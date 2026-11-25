# Roadmap

Work items in priority order. Refer to items by name, not number, from other documents, since numbers change as items finish. Finish and verify one before starting the next compatibility feature. When an item is done, move what it delivered into [status.md](status.md) and delete it here.

## 1. Host input checks after the rename

The smoke test, IPC fixtures, session round trip and Tilefire migration were repeated with real clients on 2026-11-25 ([status.md](status.md#with-real-clients)). What remains needs physical input from the operator: input-device gating of real host keyboard, pointer and touch events, the nested switcher and drag clean-up on disable, modifiers held across host keyboard focus changes, and a clean quit followed by a restore. The steps are in [status.md](status.md#needs-operator-host-input); record the results there and delete this item.

## 2. Native backend

Add DRM/KMS and libinput with careful failure handling. The design is [design/native-backend.md](design/native-backend.md).

In short, the interactive frontend moves out of `shingle-nested` into a shared `shingle-frontend` crate, and `shingle-render` loses winit. A single `shingle` binary picks its backend like Wayfire: nested inside a Wayland session, native from a TTY. The native backend uses libseat for the session and VT switching and libinput for input, and drives atomic KMS itself through the `drm` crate, rendering zero-copy into GBM buffers imported into wgpu, with a dumb-buffer copy fallback. Native code is built and unit-tested on this host but only ever run on a dedicated test machine, which is being set up; no VMs. The work is split into ten workspaces, each with acceptance criteria in the design:

1. NB-1 `shingle-render-targets`: shared GPU context and render targets, no winit in `shingle-render`. Done ([status.md](status.md#compositor-and-renderer)).
2. NB-2 `shingle-compositor-host-api`: done; see [status.md](status.md#backend-host-api).
3. NB-3 `shingle-frontend-extract`: done; see [status.md](status.md#configuration-and-wcm) and [status.md](status.md#with-real-clients).
4. NB-4 `shingle-frontend-host`: done; `App` is `shingle_frontend::Frontend` behind the `Host` trait and `InputEvent`, nested is a thin winit host, and key repeat is the frontend's own timer. See [status.md](status.md#compositor-and-renderer) and [status.md](status.md#with-real-clients).
5. NB-5 `shingle-input-registry`: done; see [status.md](status.md#backend-host-api).
6. NB-6 `shingle-native-session`: native backend in the `shingle` binary with libseat, libinput and VT switching, no display. The binary and its backend selection exist; NB-6 replaces the native stub in crates/shingle-bin/src/main.rs. Needs the test machine.
7. NB-7 `shingle-native-kms`: atomic KMS, copy scanout, page-flip pacing, DPMS and the failure handling (test machine).
8. NB-8 `shingle-native-zero-copy`: GBM scanout with modifier negotiation.
9. NB-9 `shingle-native-outputs`: hotplug and Wayfire-compatible mode configuration.
10. NB-10 `shingle-native-planes`: cursor plane, VRR, tearing and fences.

NB-1 to NB-5 are done, and so is the single `shingle` binary of decision 4, with nested and headless backends and native as a stub that says it is not built yet ([status.md](status.md#the-shingle-binary)); NB-6 adds `shingle-native` to it. NB-6 is next and, like everything after it, waits for the test machine. The operator's decisions are recorded in the design's [Decisions](design/native-backend.md#decisions) section.

## 3. DMA-BUF

Add DMA-BUF import and remove unnecessary CPU texture copies.

## 4. XWayland

Run X11 applications through a rootless `Xwayland` with Shingle's own X window manager. The design is [design/xwayland.md](design/xwayland.md), proposed on 2026-11-25 and waiting on the operator's [open questions](design/xwayland.md#open-questions).

In short:

- **Server.** Shingle binds the X display (`:N`, lock file, filesystem and abstract sockets) when it starts. It execs `Xwayland -rootless` with `-listenfd`, `-wm` and `-displayfd`, eagerly or on the first connection. The new option `xwayland.mode` takes Wayfire's `true`, `false` and `lazy`, and is imported from `[core] xwayland`.
- **Environment.** `DISPLAY` goes into every child's environment and into `stipc/get_display`. Since XW-1 every child gets one client environment, and the host's `DISPLAY` no longer reaches nested children.
- **XWM.** The XWM uses `x11rb` 0.14 (pure Rust) in a new `shingle-xwayland` crate and ports the ICCCM/EWMH subset of wlroots' XWM, which Wayfire uses. Surfaces are associated only through `xwayland_shell_v1`, which needs Xwayland 23.1 or newer.
- **Window model.** X11 toplevels share `ToplevelRole` with xdg toplevels through a `ToplevelHandle` enum. Tiling, rules, Scale, Expo, IPC and sessions work unchanged: `app-id` comes from the `WM_CLASS` class, and the PID from XRes. Override-redirect windows are drawn unmanaged above the Top layer.
- **Selections and scaling.** Clipboard and primary selection are bridged both ways. Scaling matches Wayfire: X11 clients are blurry above scale 1.
- **Testing.** Real-client tests run inside a user, mount and network namespace with a private `/tmp/.X11-unix`.

The work is split into seven workspaces, each with acceptance criteria in the design:

1. XW-1 `shingle-toplevel-handle`: done; the toplevel role is shell-neutral and every child gets one client environment, which stopped the host `DISPLAY` and `WAYFIRE_SOCKET` leaks. See [status.md](status.md#the-shingle-binary) and [status.md](status.md#with-real-clients).
2. XW-2 `shingle-xwayland-server`: the `shingle-xwayland` crate, display sockets, the server process, the restart rule, configuration and import, the XWM bring-up, and the `stipc` display methods.
3. XW-3 `shingle-xwayland-windows`: `xwayland_shell_v1`, serial pairing, and managed X11 toplevels as Shingle windows with configure, close, IPC and session identity.
4. XW-4 `shingle-xwayland-behaviour`: ICCCM focus, override-redirect windows, `_NET_WM_STATE`, move and resize requests, transients and dialogs, and checks with Scale, Expo, rules and session restore.
5. XW-5 `shingle-xwayland-selection`: clipboard and primary selection in both directions, with INCR.
6. XW-6 `shingle-xwayland-dnd`: Xdnd drag and drop (later).
7. XW-7 `shingle-xwayland-hidpi`: optional zero-scaling HiDPI mode.

XW-2 is next. The design's open questions are still unanswered, so it proceeds with their stated defaults.

## 5. Multi-output

Complete output lifecycle, focus, workspaces, sessions and rendering across outputs, including headless outputs over IPC where Wayfire offers them.

The Wayfire import must keep one output configuration per `[output:NAME]` section: today every section writes into the single `OutputConfig`, so with several sections the last one wins (crates/shingle-config/src/wayfire.rs, `parse`). Found while documenting the crates and left for this item.

## 6. Remaining Wayfire compatibility

Continue the missing plugins and configuration options, using Wayfire's source as the behavioural reference.

## 7. Soak tests and measurements

Add long-running nested and native soak tests. Measure CPU, RSS, frame latency and GPU behaviour before making any resource or stability claims.

## Open decisions

- **Removing Tilefire fallbacks.** The rename kept fallbacks for old config, session, PAM service and environment variable names. Decide when they can be dropped.
