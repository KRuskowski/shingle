# Roadmap

Work items in priority order. Finish and verify one before starting the next compatibility feature. When an item is done, move what it delivered into [status.md](status.md) and delete it here.

## 1. Finish the input-device IPC slice

`input/list-devices` and `input/configure-device` were interrupted partway. Neither is reachable over IPC yet.

Already present in `crates/shingle/src/input.rs`:

- `InputDeviceKind::{Keyboard, Pointer, Touch}`.
- Nested device metadata with stable IDs 1, 2 and 3 in `INPUT_DEVICES`.
- A three-entry `devices_enabled` array in `InputState`, with `input_device_enabled` and `set_input_device_enabled`.
- Gating of pointer motion and pointer buttons.

Remaining:

1. Add both methods to IPC dispatch and `SUPPORTED_METHODS`, bringing the count to 77. Update [ipc.md](ipc.md).
2. Return Wayfire's fields: `id`, `name`, `vendor`, `product`, `type`, `enabled`. Unknown vendor and product strings are fine for nested virtual devices.
3. Validate device IDs and require a boolean `enabled`. Unknown IDs return an error.
4. Gate keyboard, modifiers, scroll, touch, gestures and nested host input. Decide how disabling a device mid-hold releases pressed keys, buttons and touches so clients are not left with stuck input.
5. Test with real IPC calls: list three devices; disable the pointer and check STIPC cursor movement is suppressed; disable the keyboard and check a registered binding does not fire; re-enable both; send bad requests.

The three current dead-code warnings come from this partial state and are the only thing keeping `cargo clippy --workspace --all-targets -- -D warnings` from passing. Finishing the work should remove them; do not silence them with `allow` attributes.

## 2. Repair Gloomy's examples and designer

In the Gloomy repository, `gloomy-designer` and the examples no longer compile against the current widget API. This was already the case upstream, where it went unnoticed because the missing `assets/` fonts stopped the build earlier. The libraries and their 184 unit tests pass. Shingle does not depend on the broken targets.

## 3. Repeat real-client verification after the rename

Run the nested smoke test and the IPC fixtures again under the Shingle names, including migration from an existing `tilefire` config and session. See [development.md](development.md#real-client-tests).

## 4. Documentation and tests for the core

- Add unit tests to `shingle-core` for tree operations, split weights, focus and session round-trips.
- Add doc comments to public APIs. Most public items lack them. Priority areas are state-machine transitions, session invariants, map blocking, focus rules, output transforms and non-obvious protocol behaviour.

## 5. Native backend

Add DRM/KMS and libinput with careful failure handling.

## 6. DMA-BUF

Add DMA-BUF import and remove unnecessary CPU texture copies.

## 7. XWayland

## 8. Multi-output

Complete output lifecycle, focus, workspaces, sessions and rendering across outputs, including headless outputs over IPC where Wayfire offers them.

## 9. Remaining Wayfire compatibility

Continue the missing plugins and configuration options, using Wayfire's source as the behavioural reference.

## 10. Soak tests and measurements

Add long-running nested and native soak tests. Measure CPU, RSS, frame latency and GPU behaviour before making any resource or stability claims.

## Open decisions

- **Removing Tilefire fallbacks.** The rename kept fallbacks for old config, session, PAM service and environment variable names. Decide when they can be dropped.
