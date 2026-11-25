# Roadmap

Work items in priority order. Refer to items by name, not number, from other documents, since numbers change as items finish. Finish and verify one before starting the next compatibility feature. When an item is done, move what it delivered into [status.md](status.md) and delete it here.

## 1. Repeat real-client verification after the rename

Run the nested smoke test and the IPC fixtures again under the Shingle names, including migration from an existing `tilefire` config and session. See [development.md](development.md#real-client-tests).

Also exercise input-device gating with real host keyboard, pointer and touch input, including the nested switcher and drag clean-up on disable; the 2026-11-25 real-client run covered STIPC input only (see [status.md](status.md#with-real-clients)). In that run `shingle-nested` ignored SIGTERM and SIGINT. The likely cause: `Compositor::with_core` blocks both signals in a calloop `Signals` source that only sets an internal `shutdown` flag (crates/shingle/src/lib.rs:496-502), and nested never reads it. The native design fixes this in NB-2 ([design/native-backend.md](design/native-backend.md#6-failure-handling)).

## 2. Documentation for the other crates

`shingle-core` is documented and enforces `#![warn(missing_docs)]`. Add doc comments to the public APIs of the other crates, where most public items still lack them. Priority areas are compositor state-machine transitions, map blocking, focus rules, output transforms and non-obvious protocol behaviour. Enable `missing_docs` in each crate as it is finished.

## 3. Native backend

Add DRM/KMS and libinput with careful failure handling. The design is [design/native-backend.md](design/native-backend.md).

In short, the interactive frontend moves out of `shingle-nested` into a shared `shingle-frontend` crate, and `shingle-render` loses winit. A single `shingle` binary picks its backend like Wayfire: nested inside a Wayland session, native from a TTY. The native backend uses libseat for the session and VT switching and libinput for input, and drives atomic KMS itself through the `drm` crate, rendering zero-copy into GBM buffers imported into wgpu, with a dumb-buffer copy fallback. Native code is built and unit-tested on this host but only ever run on a dedicated test machine, which is being set up; no VMs. The work is split into ten workspaces, each with acceptance criteria in the design:

1. NB-1 `shingle-render-targets`: shared GPU context and render targets, no winit in `shingle-render`.
2. NB-2 `shingle-compositor-host-api`: `Compositor` gains output info, page-flip presentation, timestamps, session activity and `shutdown_requested`.
3. NB-3 `shingle-frontend-extract`: new crate with the pure modules and an xkb-based shared keyboard.
4. NB-4 `shingle-frontend-host`: `App` becomes `Frontend` behind a `Host` trait; nested becomes a thin winit host.
5. NB-5 `shingle-input-registry`: per-device seat devices for `input/*` IPC.
6. NB-6 `shingle-native-session`: native backend in the `shingle` binary with libseat, libinput and VT switching, no display. Needs the test machine.
7. NB-7 `shingle-native-kms`: atomic KMS, copy scanout, page-flip pacing, DPMS and the failure handling (test machine).
8. NB-8 `shingle-native-zero-copy`: GBM scanout with modifier negotiation.
9. NB-9 `shingle-native-outputs`: hotplug and Wayfire-compatible mode configuration.
10. NB-10 `shingle-native-planes`: cursor plane, VRR, tearing and fences.

NB-1, NB-2 and NB-5 can start now. NB-6 onwards waits for the test machine. The operator's decisions are recorded in the design's [Decisions](design/native-backend.md#decisions) section.

## 4. DMA-BUF

Add DMA-BUF import and remove unnecessary CPU texture copies.

## 5. XWayland

## 6. Multi-output

Complete output lifecycle, focus, workspaces, sessions and rendering across outputs, including headless outputs over IPC where Wayfire offers them.

## 7. Remaining Wayfire compatibility

Continue the missing plugins and configuration options, using Wayfire's source as the behavioural reference.

- `stipc/feed_key` fires only bindings registered over IPC. Wayfire feeds STIPC keys through a headless keyboard into the normal input path (`plugins/ipc/stipc.cpp`), so configured key bindings fire too.

## 8. Soak tests and measurements

Add long-running nested and native soak tests. Measure CPU, RSS, frame latency and GPU behaviour before making any resource or stability claims.

## Open decisions

- **Removing Tilefire fallbacks.** The rename kept fallbacks for old config, session, PAM service and environment variable names. Decide when they can be dropped.
