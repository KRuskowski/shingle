# Roadmap

Work items in priority order. Finish and verify one before starting the next compatibility feature. When an item is done, move what it delivered into [status.md](status.md) and delete it here.

## 1. Repair Gloomy's examples and designer

In the Gloomy repository, `gloomy-designer` and the examples no longer compile against the current widget API. This was already the case upstream, where it went unnoticed because the missing `assets/` fonts stopped the build earlier. The libraries and their 184 unit tests pass. Shingle does not depend on the broken targets.

## 2. Repeat real-client verification after the rename

Run the nested smoke test and the IPC fixtures again under the Shingle names, including migration from an existing `tilefire` config and session. See [development.md](development.md#real-client-tests).

Also exercise input-device gating with real host keyboard, pointer and touch input, including the nested switcher and drag clean-up on disable; the 2026-11-25 real-client run covered STIPC input only (see [status.md](status.md#with-real-clients)). In that run `shingle-nested` ignored SIGTERM and SIGINT; find out why.

## 3. Documentation and tests for the core

- Add unit tests to `shingle-core` for tree operations, split weights, focus and session round-trips.
- Add doc comments to public APIs. Most public items lack them. Priority areas are state-machine transitions, session invariants, map blocking, focus rules, output transforms and non-obvious protocol behaviour.

## 4. Native backend

Add DRM/KMS and libinput with careful failure handling.

## 5. DMA-BUF

Add DMA-BUF import and remove unnecessary CPU texture copies.

## 6. XWayland

## 7. Multi-output

Complete output lifecycle, focus, workspaces, sessions and rendering across outputs, including headless outputs over IPC where Wayfire offers them.

## 8. Remaining Wayfire compatibility

Continue the missing plugins and configuration options, using Wayfire's source as the behavioural reference.

- `stipc/feed_key` fires only bindings registered over IPC. Wayfire feeds STIPC keys through a headless keyboard into the normal input path (`plugins/ipc/stipc.cpp`), so configured key bindings fire too.

## 9. Soak tests and measurements

Add long-running nested and native soak tests. Measure CPU, RSS, frame latency and GPU behaviour before making any resource or stability claims.

## Open decisions

- **Removing Tilefire fallbacks.** The rename kept fallbacks for old config, session, PAM service and environment variable names. Decide when they can be dropped.
