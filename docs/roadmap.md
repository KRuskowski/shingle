# Roadmap

Work items in priority order. Refer to items by name, not number, from other documents, since numbers change as items finish. Finish and verify one before starting the next compatibility feature. When an item is done, move what it delivered into [status.md](status.md) and delete it here.

## 1. Host input checks after the rename

The smoke test, IPC fixtures, session round trip and Tilefire migration were repeated with real clients on 2026-11-25 ([status.md](status.md#with-real-clients)). What remains needs physical input from the operator: input-device gating of real host keyboard, pointer and touch events, the nested switcher and drag clean-up on disable, modifiers held across host keyboard focus changes, and a clean quit followed by a restore. The steps are in [status.md](status.md#needs-operator-host-input); record the results there and delete this item.

## 2. Documentation for the other crates

`shingle-core` is documented and enforces `#![warn(missing_docs)]`. Add doc comments to the public APIs of the other crates, where most public items still lack them. Priority areas are compositor state-machine transitions, map blocking, focus rules, output transforms and non-obvious protocol behaviour. Enable `missing_docs` in each crate as it is finished.

## 3. Native backend

Add DRM/KMS and libinput with careful failure handling. The design is [design/native-backend.md](design/native-backend.md).

In short, the interactive frontend moves out of `shingle-nested` into a shared `shingle-frontend` crate, and `shingle-render` loses winit. A single `shingle` binary picks its backend like Wayfire: nested inside a Wayland session, native from a TTY. The native backend uses libseat for the session and VT switching and libinput for input, and drives atomic KMS itself through the `drm` crate, rendering zero-copy into GBM buffers imported into wgpu, with a dumb-buffer copy fallback. Native code is built and unit-tested on this host but only ever run on a dedicated test machine, which is being set up; no VMs. The work is split into ten workspaces, each with acceptance criteria in the design:

1. NB-1 `shingle-render-targets`: shared GPU context and render targets, no winit in `shingle-render`. Done ([status.md](status.md#compositor-and-renderer)).
2. NB-2 `shingle-compositor-host-api`: done; see [status.md](status.md#backend-host-api).
3. NB-3 `shingle-frontend-extract`: done; see [status.md](status.md#configuration-and-wcm) and [status.md](status.md#with-real-clients).
4. NB-4 `shingle-frontend-host`: done; `App` is `shingle_frontend::Frontend` behind the `Host` trait and `InputEvent`, nested is a thin winit host, and key repeat is the frontend's own timer. See [status.md](status.md#compositor-and-renderer) and [status.md](status.md#with-real-clients).
5. NB-5 `shingle-input-registry`: done; see [status.md](status.md#backend-host-api).
6. NB-6 `shingle-native-session`: native backend in the `shingle` binary with libseat, libinput and VT switching, no display. Needs the test machine.
7. NB-7 `shingle-native-kms`: atomic KMS, copy scanout, page-flip pacing, DPMS and the failure handling (test machine).
8. NB-8 `shingle-native-zero-copy`: GBM scanout with modifier negotiation.
9. NB-9 `shingle-native-outputs`: hotplug and Wayfire-compatible mode configuration.
10. NB-10 `shingle-native-planes`: cursor plane, VRR, tearing and fences.

NB-1 to NB-5 are done. NB-6 is next and, like everything after it, waits for the test machine. The operator's decisions are recorded in the design's [Decisions](design/native-backend.md#decisions) section.

## 4. DMA-BUF

Add DMA-BUF import and remove unnecessary CPU texture copies.

## 5. XWayland

## 6. Multi-output

Complete output lifecycle, focus, workspaces, sessions and rendering across outputs, including headless outputs over IPC where Wayfire offers them.

## 7. Remaining Wayfire compatibility

Continue the missing plugins and configuration options, using Wayfire's source as the behavioural reference.

## 8. Soak tests and measurements

Add long-running nested and native soak tests. Measure CPU, RSS, frame latency and GPU behaviour before making any resource or stability claims.

## Open decisions

- **Removing Tilefire fallbacks.** The rename kept fallbacks for old config, session, PAM service and environment variable names. Decide when they can be dropped.
