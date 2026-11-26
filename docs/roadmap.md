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
6. NB-6 `shingle-native-session`: built and unit-tested through fakes: `shingle-native` runs the frontend with libseat, libinput and VT switching and draws offscreen, and `shingle` runs it from a VT login and refuses SSH logins ([status.md](status.md#native-backend-seat-and-input)). Open: steps 1 to 8 of [native-testing.md](native-testing.md) on the test machine.
7. NB-7 `shingle-native-kms`: built and unit-tested through a fake card: one output driven with atomic KMS and copy scanout, page-flip pacing with presentation feedback from the flips, DPMS, pause and resume with a fresh modeset, and the failure handling ([status.md](status.md#native-backend-kms-display)). Open: steps 9 to 15 of [native-testing.md](native-testing.md) on the test machine.
8. NB-8 `shingle-native-zero-copy`: GBM scanout with modifier negotiation, on the Vulkan setup of DMA-BUF's DB-1 ([design/dmabuf.md](design/dmabuf.md#7-scanout-in_formats-and-nb-8)).
9. NB-9 `shingle-native-outputs`: built and unit-tested through a fake card and a fake udev monitor: DRM hotplug, the next output lit when one is unplugged and none while every monitor is gone, Wayfire's per-output `mode` with its heuristics, EDID make, model and serial, the core output renamed and the `wl_output` global replaced with the connector, and wlr-output-management mode changes with `TEST_ONLY` first ([status.md](status.md#native-backend-outputs)); the parts without a card were checked with `wlr-randr` in headless and nested Shingle. Open: steps 16 to 22 of [native-testing.md](native-testing.md) on the test machine.
10. NB-10 `shingle-native-planes`: cursor plane, VRR, tearing and fences.

NB-1 to NB-5 are done, and so is the single `shingle` binary of decision 4 ([status.md](status.md#the-shingle-binary)). NB-6, NB-7 and NB-9 are built and wait for their hardware checks; NB-8 is next and, like everything after it, needs the test machine to be verified. The operator's decisions are recorded in the design's [Decisions](design/native-backend.md#decisions) section.

## 3. DMA-BUF

Add DMA-BUF import and remove unnecessary CPU texture copies. The design is [design/dmabuf.md](design/dmabuf.md), from 2026-11-25; its open questions were decided with defaults on the operator's behalf ([Decisions](design/dmabuf.md#decisions)).

In short:

- **Vulkan setup.** `shingle-render` opens its device through wgpu-hal with the DMA-BUF feature, `VK_EXT_queue_family_foreign` and `VK_KHR_external_semaphore_fd`, queries which single-plane modifiers Vulkan can import and render to, and has one import call for client buffers and for NB-8's scanout buffers.
- **Protocol.** `zwp_linux_dmabuf_v1` version 5 with feedback naming the render node, real test imports at `create`, textures cached per buffer. `wl_drm` is not offered: Mesa, Xwayland and NVIDIA's EGL find the device through the feedback.
- **Sync.** Implicit sync by polling the buffer's fds before a commit applies, and `linux-drm-syncobj-v1`, which Wayfire lacks but this host's NVIDIA clients need. Buffers are released only after the GPU has finished reading them.
- **CPU copies.** `wl_shm` buffers are uploaded from the client's mapping, damaged rows only, without the RGBA copy and swizzle; screencopy captures the rendered frame.
- **Testing.** A new GPU offscreen backend lets real NVIDIA clients and nested Shingle run in a namespace with a private `/dev` that holds only the render node, never touching the host's display.

The work is split into seven workspaces, DB-1 to DB-7, each with acceptance criteria in the design: Vulkan setup, offscreen backend and GPU harness, buffer lifetime, `zwp_linux_dmabuf_v1`, `linux-drm-syncobj-v1`, shm uploads without copies, and later direct scanout of client buffers. NB-8 builds on DB-1.

DB-1 `shingle-gpu-dmabuf` is done ([status.md](status.md#compositor-and-renderer), [design/dmabuf.md](design/dmabuf.md#db-1-as-built)). DB-2 `shingle-offscreen-backend` is done too ([status.md](status.md#the-shingle-binary), [design/dmabuf.md](design/dmabuf.md#db-2-as-built)): `shingle --backend offscreen`, screencopy from drawn frames, and `tools/gpu_test/run.py --checks`, which records that without linux-dmabuf NVIDIA's Vulkan WSI cannot present and its EGL is not used. DB-3 `shingle-buffer-lifetime` is done ([status.md](status.md#compositor-and-renderer), [design/dmabuf.md](design/dmabuf.md#db-3-as-built)): buffer content, reference-counted buffer locks with release on the last drop, the per-surface commit queue with its blocker interface, and the fix for buffers replaced behind a FIFO barrier, a commit-timing deadline or a synchronized parent, which were never released. DB-4 `shingle-linux-dmabuf` is done ([status.md](status.md#compositor-and-renderer), [design/dmabuf.md](design/dmabuf.md#db-4-as-built)): `zwp_linux_dmabuf_v1` version 5 with feedback on the render node, real test imports at `create`, textures cached per buffer, release after the GPU, the implicit-sync wait and clients started after the renderer. In the GPU namespace Vulkan clients, Chromium, Xwayland's glamor, `weston-simple-dmabuf-egl` and nested Shingle present DMA-BUFs; NVIDIA's EGL clients still need a run with the card node, on the test machine. Found and fixed while checking it: FIFO and commit-timing updates were merged rather than queued, which let NVIDIA's Vulkan clients run ahead of their commit-timing deadlines and freeze about one run in six; they now queue in order with their own gates and deadlines, as wlroots does, and the vkcube checks passed 10 runs out of 10 ([design/dmabuf.md](design/dmabuf.md#db-3-as-built)). DB-5 is next; DB-6 can start too.

## 4. XWayland

Run X11 applications through a rootless `Xwayland` with Shingle's own X window manager. The design is [design/xwayland.md](design/xwayland.md), from 2026-11-25; the operator has decided its open questions ([Decisions](design/xwayland.md#decisions)).

In short:

- **Server.** Shingle binds the X display (`:N`, lock file, filesystem and abstract sockets) when it starts. It execs `Xwayland -rootless` with `-listenfd`, `-wm` and `-displayfd`, eagerly or on the first connection. The new option `xwayland.mode` takes Wayfire's `true`, `false` and `lazy`, and is imported from `[core] xwayland`.
- **Environment.** `DISPLAY` goes into every child's environment and into `stipc/get_display`. Since XW-1 every child gets one client environment, and the host's `DISPLAY` no longer reaches nested children.
- **XWM.** The XWM uses `x11rb` 0.14 (pure Rust) in a new `shingle-xwayland` crate and ports the ICCCM/EWMH subset of wlroots' XWM, which Wayfire uses. Surfaces are associated only through `xwayland_shell_v1`, which needs Xwayland 23.1 or newer.
- **Window model.** X11 toplevels share `ToplevelRole` with xdg toplevels through a `ToplevelHandle` enum. Tiling, rules, Scale, Expo, IPC and sessions work unchanged: `app-id` comes from the `WM_CLASS` class, and the PID from XRes. Override-redirect windows are drawn unmanaged above the Top layer.
- **Selections and scaling.** Clipboard and primary selection are bridged both ways. Scaling matches Wayfire: X11 clients are blurry above scale 1.
- **Testing.** Real-client tests run inside a user, mount, network and PID namespace with a private `/tmp/.X11-unix`.

The work is split into seven workspaces, each with acceptance criteria in the design:

1. XW-1 `shingle-toplevel-handle`: done; the toplevel role is shell-neutral and every child gets one client environment, which stopped the host `DISPLAY` and `WAYFIRE_SOCKET` leaks. See [status.md](status.md#the-shingle-binary) and [status.md](status.md#with-real-clients).
2. XW-2 `shingle-xwayland-server`: done; Shingle binds its X display, supervises Xwayland (eager or lazy, with the restart rule) and brings its XWM up; `DISPLAY`, the `stipc` display methods, `xwayland-ready` and the startup script work. See [status.md](status.md#with-real-clients).
3. XW-3 `shingle-xwayland-windows`: done; `xwayland_shell_v1` pairs X windows with surfaces, and managed X11 toplevels are Shingle windows with configure, `_NET_WM_STATE`, close, IPC (class, title, PID) and session identity. See [status.md](status.md#with-real-clients).
4. XW-4 `shingle-xwayland-behaviour`: done; ICCCM focus and the `FocusIn` guard, `_NET_ACTIVE_WINDOW`, `_NET_WM_STATE` both ways, `WM_CHANGE_STATE`, move, resize and close requests, X stacking, transients and dialogs, override-redirect windows in the Unmanaged layer and the root cursor; Scale, Expo, the switchers, rules and session restore checked with X windows. See [status.md](status.md#with-real-clients).
5. XW-5 `shingle-xwayland-selection`: done; the clipboard and primary selection are bridged between X and Wayland in both directions, with INCR and without blocking the compositor, and wlr-data-control clients see X selections. See [status.md](status.md#with-real-clients).
6. XW-6 `shingle-xwayland-dnd`: Xdnd drag and drop (later).
7. XW-7 `shingle-xwayland-hidpi`: optional zero-scaling HiDPI mode.

XW-6 is next, when drag and drop between X and Wayland is wanted; XW-7 waits for the multi-output item.

## 5. Multi-output

Complete output lifecycle, focus, workspaces, sessions and rendering across outputs, including headless outputs over IPC where Wayfire offers them.

The configuration already keeps one entry per `[output:NAME]` section (NB-9, `ShingleConfig::outputs`), and the native backend's output model is per connector; what remains is in `shingle` and `shingle-core`: more than one output rectangle, per-output `wl_output` globals and frame callbacks, and lighting more than one connector.

## 6. Remaining Wayfire compatibility

Continue the missing plugins and configuration options, using Wayfire's source as the behavioural reference.

## 7. Soak tests and measurements

Add long-running nested and native soak tests. Measure CPU, RSS, frame latency and GPU behaviour before making any resource or stability claims.

## Open decisions

- **Removing Tilefire fallbacks.** The rename kept fallbacks for old config, session, PAM service and environment variable names. Decide when they can be dropped.
