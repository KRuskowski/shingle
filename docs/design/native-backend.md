# Native backend design

Status: accepted with the operator's [decisions](#decisions), 2026-11-25. Roadmap item "Native backend" ([roadmap.md](../roadmap.md#2-native-backend)).

Shingle today runs only as a window inside another compositor (`crates/shingle-nested`, winit) or headless. This document decides how Shingle drives real hardware from a TTY: DRM/KMS for display, libinput for input, libseat for device access and VT switching. It is the brief for the implementation workspaces listed in [section 8](#8-implementation-plan). NB-1 to NB-5 and the single `shingle` binary are implemented (see [The `shingle` binary](#the-shingle-binary), [`shingle-render` without winit](#shingle-render-without-winit) and the entries for [NB-2](#nb-2-shingle-compositor-host-api-what-a-real-backend-needs-from-compositor), [NB-3](#nb-3-shingle-frontend-extract-create-the-shared-crate-with-the-pure-modules), [NB-4](#nb-4-shingle-frontend-host-move-app-into-frontend) and [NB-5](#nb-5-shingle-input-registry-per-device-seat-devices)), each with notes on where the code differs from this design. NB-6, NB-7 and NB-9 are built and unit-tested through fakes ([NB-6 as built](#nb-6-as-built), [NB-7 as built](#nb-7-as-built), [NB-9 as built](#nb-9-as-built)); their hardware checks wait for the test machine ([native-testing.md](../native-testing.md)). The rest is not built.

Line numbers refer to branch `shingle-native-design` at `d2568d2`. Crate versions and dates were read from crates.io and GitHub on 2026-11-25; crate APIs were read from the crate sources (wgpu, drm, calloop and xkbcommon from `~/.cargo/registry`, the others from their published `.crate` files).

## Summary of decisions

| # | Topic | Decision |
| --- | --- | --- |
| 1 | Frontend split | New library crate `shingle-frontend` takes everything in `shingle-nested` except winit. Nested and native become thin hosts behind a `Host` trait and a neutral `InputEvent`. |
| 2 | Display path | Atomic KMS through the `drm` crate. wgpu renders into GBM buffers imported with `texture_from_dmabuf_fd` (zero-copy), with a dumb-buffer copy path as fallback. wgpu's own `drm` surface (VK_KHR_display) is not used. `shingle-render` loses winit. |
| 3 | Session and seat | `libseat` (seatd, logind or builtin). Ctrl+Alt+F1..F10 switches VT as in Wayfire. On pause, stop committing, suspend libinput and release held input, then acknowledge. On resume, re-probe outputs and modeset again. |
| 4 | Input | `input` crate (libinput) on a udev context, opening devices through libseat. Each libinput device becomes a Shingle seat device, so `input/list-devices` lists real devices and `input/configure-device` uses libinput's send-events mode, as Wayfire does. |
| 5 | Outputs | One enabled output first, chosen and moded the way Wayfire does it. Hotplug comes from udev. The data model is per connector and keyed by connector name, so the multi-output roadmap item can enable more outputs without changing the structure. |
| 6 | Failure handling | Fail before launching clients when the display cannot work. Treat losing DRM master as a pause, not an error. Recover from a lost GPU device, and rely on libseat and the kernel to restore the TTY even after a crash. |
| 7 | Testing | Most logic is pure and unit-tested on the host behind traits. Everything that touches devices runs on a dedicated test machine that the operator is setting up; VMs are not used. Nothing touches this host's DRM, input or VT. |
| 8 | Plan | Ten ordered workspaces, NB-1 to NB-10: render split, compositor host API, two frontend extraction steps, device registry, native session and input, KMS copy path, zero-copy, hotplug and modes, then cursor plane, VRR and tearing. |

## Crate survey

Every crate this design names, checked on 2026-11-25.

| Crate | Version (date) | Maintenance | API used here |
| --- | --- | --- | --- |
| `wgpu` | 30.0.2 (2026-11-12) | Active; gfx-rs/wgpu pushed 2026-11-25. Already a workspace dependency. | `Device::create_texture_from_hal` (wgpu-30.0.2/src/api/device.rs:341), `Device::as_hal`, `Adapter::as_hal`, `Device::poll(PollType::Wait { .. })`, `Device::set_device_lost_callback`, `Features::VULKAN_EXTERNAL_MEMORY_DMA_BUF`, `AdapterInfo::device_pci_bus_id`. |
| `wgpu-hal` | 30.0.2 (2026-11-12) | Same repository. | `vulkan::Device::texture_from_dmabuf_fd` (wgpu-hal-30.0.2/src/vulkan/device.rs:525), `vulkan::Adapter::raw_physical_device` (adapter.rs:2500), `vulkan::Instance::raw_instance` (instance.rs:180). |
| `ash` | 0.38.0+1.3.281 (2024-04-01) | Latest release; wgpu-hal 30 depends on `^0.38`, so it adds no new version. | `vkGetPhysicalDeviceFormatProperties2` with `VkDrmFormatModifierPropertiesListEXT`, to learn which modifiers Vulkan can render to. |
| `drm` | 0.15.0 (2026-03-19) | Active; Smithay/drm-rs pushed 2026-09-25. Pure Rust over `rustix`, no libdrm. wgpu-hal 30 already requires `^0.15`. | `Device::{acquire_master_lock, set_client_capability, get_driver_capability}`, `control::Device::{resource_handles, plane_handles, get_connector, get_crtc, get_plane, get_properties, get_property, create_property_blob, atomic_commit, add_planar_framebuffer, create_dumb_buffer, map_dumb_buffer, receive_events}`, `atomic::AtomicModeReq::add_property`, `AtomicCommitFlags::{TEST_ONLY, NONBLOCK, PAGE_FLIP_EVENT, ALLOW_MODESET}`, `Event::PageFlip(PageFlipEvent { frame, duration, crtc })` (drm-0.15.0/src/control/mod.rs). |
| `drm-fourcc` | 2.2.0 (2021-09-05) | Stable and unchanged; `drm` 0.15 depends on it. | `DrmFourcc::{Xrgb8888, Argb8888}`, `DrmModifier`. |
| `gbm` | 0.18.0 (2024-12-03) | Slow. The last commit was 2025-06-05, and PR #55 "Bump `drm`" (opened 2026-11-23) is not merged. Its `drm-support` feature requires `drm ^0.14`. | With `default-features = false`: `Device::new`, `Device::create_buffer_object_with_modifiers2`, `Device::format_modifier_plane_count`, `BufferObject::{fd, modifier, stride, offset, plane_count, handle}`. |
| `input` | 0.10.0 (2026-04-05) | Active; Smithay/input.rs pushed 2026-11-12. `input-sys` 1.19.0 was released the same day. | `Libinput::new_with_udev`, `udev_assign_seat`, `dispatch`, `suspend`, `resume`, `AsFd`, the `LibinputInterface` trait, `Event::{Device, Keyboard, Pointer, Touch, Gesture}`, and `Device::config_*` (accel, tap, natural scroll, left-handed, middle emulation, send-events, calibration). Feature `libinput_1_19` provides `PointerEvent::ScrollWheel` with `scroll_value_v120`. |
| `libseat` | 0.2.4 (2025-12-07) | Active; PolyMeilex/libseat-rs pushed 2026-11-14, with one open issue. Links the system libseat; Debian trixie ships `libseat1`/`libseat-dev` 0.9.1-1. | `Seat::open(callback)`, `SeatEvent::{Enable, Disable}`, `SeatRef::{open_device, close_device, disable, switch_session, name, get_fd, dispatch}`. |
| `udev` | 0.9.3 (2025-01-23) | Quiet but current; Smithay/udev-rs was last pushed 2025-01-23. | `Enumerator::{new, match_subsystem, match_sysname, scan_devices}`, `Device::{devnode, sysname, attribute_value, property_value}`, `MonitorBuilder::new().match_subsystem("drm").listen()` (a `Socket` that is `AsFd` and iterable). |
| `xkbcommon` | 0.9.0 (2025-08-09) | Already a dependency of `shingle`. | `xkb::State::{new, update_key, serialize_mods, key_get_one_sym, key_get_utf8, led_name_is_active}`, `xkb::keysym_get_name`. |
| `calloop` | 0.14.5 (2026-10-01) | Already in the workspace. | `EventLoop`, `generic::Generic`, `timer::Timer`; `EventLoop` is `AsFd` (calloop-0.14.5/src/loop_logic.rs:760). |
| `smithay` | 0.7.0 (2025-06-24) | Active on master (pushed 2026-11-24), but master still pins `drm 0.14` and `gbm 0.18`. | Not used; see [2](#2-display-path). |
| `libudev` | 0.3.0 (2021-01-17) | Unmaintained. | Not used; `udev` replaces it. |

New system build dependencies are `libinput-dev`, `libseat-dev` and `libudev-dev`, plus `libgbm-dev` for NB-8. This host already has the pkg-config files for `libudev` 257, `gbm` 25.0.7 and `libdrm` 2.4.124, and `libinput-dev` and `libseat-dev` were installed on 2026-11-25 (see [Decisions](#decisions)).

## 1. Frontend split

### Decision

Add a library crate, `crates/shingle-frontend`, that owns all interactive behaviour now in `crates/shingle-nested`: Scale, Expo, the switchers, workspace transitions, extra gestures, drags, wrot rotation, zoom, inversion, alpha, animations, the scene build, binding dispatch, config reload, session saving and the lock client. It knows nothing about winit, DRM or libinput. A frontend binary gives it three things:

- **Input** as `shingle_frontend::InputEvent`, with evdev key and button codes and positions in output pixels.
- **Host services** through a `Host` trait.
- **A render target** from `shingle-render`; see [2](#2-display-path).

```rust
/// What a frontend binary provides to the shared frontend.
pub trait Host {
  /// Asks for `Frontend::render` to be called soon.
  fn request_redraw(&mut self);
  /// Moves the host pointer, in output pixels. Nested warps the host
  /// cursor; native only records the new position.
  fn warp_pointer(&mut self, x: f64, y: f64);
  /// Switches to virtual terminal `vt`. Returns false if unsupported.
  fn switch_vt(&mut self, vt: u32) -> bool;
  /// Ends the frontend's event loop after the session is saved.
  fn exit(&mut self);
}

/// Frontend-neutral input, in output pixels and evdev codes.
pub enum InputEvent {
  Key { code: u32, pressed: bool, time_usec: u64 },
  PointerMotion { dx: f64, dy: f64, ux: f64, uy: f64, time_usec: u64 },
  PointerMotionAbsolute { x: f64, y: f64, time_usec: u64 },
  PointerButton { code: u32, pressed: bool, time_usec: u64 },
  PointerAxis(Axis),
  Touch(Touch),
  Swipe(Gesture),
  Pinch(Gesture),
  Hold(Gesture),
  /// Host keyboard focus or seat lost: release held keys and buttons.
  Reset,
}
```

`Frontend` is today's `App` (crates/shingle-nested/src/main.rs:172) without its `window` and `renderer` fields. Its public surface is `Frontend::new(config, spawn, config_path)`, `handle_input(&mut self, host, device, event)`, `process(&mut self, host)` (now `about_to_wait`, main.rs:4517), `render(&mut self, target)` (now main.rs:3380), `output_resized(size)`, `next_deadline()` (the earliest animation, repeat or workspace-switch timer) and `shutdown()`.

### Keyboard handling becomes shared and xkb-based

Today nested matches bindings against winit's names. `binding_key_name` (main.rs:4684) maps winit `KeyCode`s to Wayfire names, `xkb_symbol` (main.rs:4634) maps winit logical keys to keysym names, and modifiers come from the host's `ModifiersState` (main.rs:799, main.rs:821). Clients, meanwhile, receive evdev scancodes interpreted with Shingle's own keymap (main.rs:3008). The two can disagree when the host layout differs from Shingle's.

A shared `keyboard` module will hold an `xkb::State` built from `config.input.load_keymap()`. Both frontends feed it evdev codes; nested already gets them from `PhysicalKey::to_scancode()`. The module produces:

- the Wayfire key name, from an evdev-code table replacing `binding_key_name` (Wayfire names keys after `KEY_*`, so the table follows `linux/input-event-codes.h`);
- the keysym name for `xkb:` bindings (`matches_xkb`, crates/shingle-config/src/lib.rs:2032);
- the UTF-8 text for the Scale filter;
- real depressed, latched, locked and group masks for `Compositor::keyboard_modifiers` (crates/shingle/src/lib.rs:929).

Key repeat for compositor bindings moves into the shared module as a timer, so native, which gets no repeats from libinput, behaves like nested. Nested then drops host repeats. The tests at main.rs:5058 and main.rs:5094 are ported to the evdev table.

As built in NB-3 (crates/shingle-frontend/src/keyboard.rs), with these differences from the plan:

```rust
pub fn key_name(code: u32) -> Option<&'static str>;
pub enum KeyState { Pressed, Released, Repeated }
pub struct Key {
  pub code: u32,
  pub state: KeyState,
  pub name: Option<&'static str>,
  pub symbol: Option<String>,
  pub text: Option<String>,
  pub modifiers: shingle_config::Modifiers,
  pub modifiers_changed: bool,
}
pub struct Keyboard;
impl Keyboard {
  pub fn new(keymap: &str) -> Result<Self, KeymapError>;
  pub fn set_keymap(&mut self, keymap: &str) -> Result<(), KeymapError>;
  pub fn key(&mut self, code: u32, state: KeyState) -> Key;
  pub fn modifiers(&self) -> shingle::Modifiers;
  pub fn binding_modifiers(&self) -> shingle_config::Modifiers;
  pub fn sync_locked(&mut self, locked: u32, group: u32) -> bool;
  pub fn release_all(&mut self) -> Vec<u32>;
  pub fn is_held(&self, code: u32) -> bool;
}
```

- **Key repeat stayed with the host until NB-4**, which moved it into the frontend as planned (see the NB-4 entry). The repeat timer needed `Frontend::next_deadline()`, which only existed once `App` moved. Until then nested passed winit's repeats in as `KeyState::Repeated`, which reports names, keysym and text but never changes the xkb state.
- **Names and keysyms come from the state before the event**, as xkb clients do, which also matches winit's order (key event, then `ModifiersChanged`): pressing Super reports no modifiers, releasing it reports Super. `Key::modifiers_changed` tells the caller to send `Keyboard::modifiers()` and to check whether the switcher's modifiers were released.
- **The compositor still owns locks and the layout index**, because the initial Caps Lock and Num Lock (`capslock_default`, `numlock_default`) and `wayfire/set-keyboard-state` set them there. Before each key nested calls `sync_locked` with `Compositor::locked_modifiers()` and the new `Compositor::keyboard_layout_index()` (crates/shingle/src/lib.rs), so `xkb:` bindings and Scale text follow an IPC layout switch. Locks and layout changes made by keys (Caps Lock, `grp:` options) now reach clients; before, nested always sent the compositor's initial locks and group 0.
- **Focus changes are host glue** (crates/shingle-nested/src/host_keyboard.rs). winit reports no releases for keys held when the window loses focus and no presses for keys held when it gains it. On focus loss nested calls `release_all`. On focus gain, the first `ModifiersChanged` presses a left-hand stand-in key for each modifier the host reports held, which is released when the host stops reporting it; this replaces the old "host modifier resync on focus-in". Otherwise the host's modifiers are ignored, so a keymap option such as `ctrl:nocaps` or `altwin:swap_alt_win` in Shingle's keymap decides the binding modifiers.
- **The name table** has every `KEY_*` with a numeric value in the Linux 6.12 header (508 codes). `KEY_ESC` is `escape`, which is what the importers normalise `esc` to; aliases defined by another constant's name (`KEY_SCREENLOCK` for `KEY_COFFEE` and eight others) are left out, so only the original name matches. winit-only names that had no Wayfire equivalent no longer match: `digit1` is now `1` (which makes the default `super+1` workspace bindings work in nested), `period` is `dot`, `bracketleft` is `leftbrace`, `shiftleft` is `leftshift` and so on.
- **`Keyboard` is one keyboard's state.** NB-6 can keep one per libinput keyboard or one per seat; either way the disabled keyboard's keys have to leave the xkb state on `InputDeviceChanged` (see [NB-5](#nb-5-shingle-input-registry-per-device-seat-devices)).

### Module plan

| From `crates/shingle-nested/src` | To | Notes |
| --- | --- | --- |
| `expo.rs`, `workspace_transition.rs`, `extra_gestures.rs`, `scale_title.rs`, `icon_theme.rs` | `shingle-frontend/src/` unchanged | None of these use winit today; `expo.rs` has a single `winit` reference to replace. |
| `ScaleState`, `ScaleTitle` (main.rs:100-118), `toggle_scale` … `scale_key` (main.rs:1891-2523), and the free functions `normalize_filter_text` … `fit_scale_tile`, `rect_contains` (main.rs:3880-4064) | `scale.rs` | `scale_pointer_input` takes an evdev button code instead of `winit::MouseButton`. |
| `SwitcherKind`, `SwitcherState` (main.rs:82-98), `start_switcher` … `switcher_window_destroyed` (main.rs:1381-1521) | `switcher.rs` | |
| Expo glue: `toggle_expo` … `expo_key` (main.rs:1533-1890), plus `expo_touch` | `expo.rs` | `expo_pointer_input` takes a button code. |
| `DragState`, `RotationDrag` (main.rs:44-66), `abandon_drag`, `window_geometry`, `drop_direction`, `rotate_pointer`, `update_drop_preview`, `move_drag`, `resize_drag` (main.rs:656, 974-1045, 1132-1289) | `drag.rs` | |
| `GeometryAnimation`, `ClosingAnimation` (main.rs:68-80), `animation_matches` … `expire_scale_exit` (main.rs:3013-3139), `apply_opening_animation` (main.rs:4103) | `animation.rs` | |
| `append_*` (main.rs:3140-3379), `render` (main.rs:3380-3877), `apply_buffer` (main.rs:4065), `render_output_transform` (main.rs:4549) | `scene.rs` | `render` builds `RenderTile`s and draws them into whatever target the host passes. The texture cache (`textures`, `previous_surfaces`) stays here. |
| `pointer_button_action`, `run_matching_command*`, `grid_slot`, `binding_modifiers`, `modifiers_empty`, `modifiers_held`, `modifier_matches` (main.rs:843-973, 1290-1380, 4566-4633), and the binding part of `keyboard_input` (main.rs:2524-3012) | `bindings.rs` | `keyboard_input` takes the `keyboard` module's output instead of `winit::event::KeyEvent`. |
| `update_modifiers`, `config_modifiers`, `xkb_symbol`, `binding_key_name` | `keyboard.rs` | Rewritten on top of xkb, as described above. |
| Zoom, inversion and alpha state (`zoom_*`, `inverted`, `alpha`), `map_zoom_input` (main.rs:830), `toggle_inversion` (main.rs:1522), and the zoom/alpha branch of `MouseWheel` (main.rs:4436-4486) | `effects.rs` | |
| `process_events` (main.rs:418-618), `input_device_changed` (main.rs:619) | `frontend.rs` | `Event::PointerWarp` (around main.rs:570-595) calls `Host::warp_pointer`. |
| `start_lock_client`, `save`, `reload_config`, `reload_if_changed` (main.rs:666-798), `session_children` | `session.rs` | |
| The dispatch bodies of `CursorMoved`, `MouseInput`, `Touch` and `MouseWheel` in `window_event` (main.rs:4223-4487) | `frontend.rs`, `handle_input` | The host scale and transform mapping (`map_output_point` with `window.scale_factor()`) stays in nested. |

What stays in `shingle-nested` (about 600 lines):

- `ApplicationHandler for App` (main.rs:4127-4547): window creation, resize and redraw.
- The translation from winit events to `InputEvent`, including host-scale mapping and host modifier resync on focus-in.
- The pending-warp echo filter (`pending_warp`).
- `MouseButtons` (main.rs:120-170), the software middle-button emulation and left-handed swap: on native these come from libinput, so they are host-specific.
- `window_event_device` (main.rs:4667).
- Argument parsing in `main` (main.rs:4722).

Both binaries share the argument parser through `shingle_frontend::cli`.

### Rejected alternatives

- **Duplicate the frontend in the native crate.** About 4,500 lines of Wayfire behaviour would drift between two copies. Rejected.
- **Move the interactive state into `shingle` (the compositor crate).** That would mix wgpu textures and animation timing into the protocol crate, against the rule in [architecture.md](../architecture.md#design-rules) that Scale's interactive state and thumbnail transforms live in the frontend. `shingle` also has no wgpu dependency and should keep it that way, because `shingle-lock` and the headless binary depend on it. Rejected.
- **Make nested a native backend that uses winit as its "device".** winit's event model (logical keys, host-accelerated pointer, host cursor) does not map onto libinput, and wrapping libinput in winit's API loses device identity. Rejected.
- **A generic `Backend` trait with input pulled by the frontend.** Both hosts are push-driven event loops, and a callback trait inverts control in nested's `ApplicationHandler`. A small `Host` trait plus pushed `InputEvent`s fits both. Rejected.

## 2. Display path

### What wgpu 30's `drm` feature actually is

The workspace enables wgpu `drm` (Cargo.toml, `[workspace.dependencies]`). In wgpu 30.0.2 that feature gives you `SurfaceTargetUnsafe::Drm { fd, plane, connector_id, width, height, refresh_rate }` (wgpu-30.0.2/src/api/surface.rs:415), which is backed by `create_surface_from_drm` in wgpu-hal-30.0.2/src/vulkan/drm.rs. It hands the display to the Vulkan driver through VK_EXT_acquire_drm_display and VK_KHR_display, and then presents through an ordinary Vulkan swapchain. Reading the code:

- It calls `.expect()` on `get_drm_display`, `acquire_drm_display`, `get_display_mode_properties` and `create_display_plane_surface` (drm.rs, the last part of `create_surface_from_drm`). A driver refusal panics the compositor instead of returning an error.
- The mode must already be set and must match a Vulkan display mode exactly. `create_surface_from_drm_plane` derives connector and mode from the plane's current CRTC.
- The swapchain owns presentation. There are no page-flip events or timestamps for `wp_presentation`, and no atomic control over DPMS, VRR, gamma LUT, the cursor plane or modesets on hotplug.
- There is no handling for losing DRM master on a VT switch, and wgpu never releases the acquired display.
- Lavapipe and virtio-gpu do not expose VK_EXT_acquire_drm_display, so this path cannot be tested in a VM.

`shingle-render` does not use the feature anywhere today.

### Decision

Shingle drives KMS itself with atomic modesetting through `drm` 0.15, and wgpu only renders.

**Device setup.**

- Open the card through libseat and call `set_client_capability(UniversalPlanes, true)` and `set_client_capability(Atomic, true)`. A driver without atomic support is reported and not supported; every current KMS driver (i915, amdgpu, nouveau, nvidia-drm with `modeset=1`, virtio-gpu) has it.
- Cache the property IDs of each connector, CRTC and plane once: `CRTC_ID`, `MODE_ID`, `ACTIVE`, `FB_ID`, `SRC_*`, `CRTC_*`, `IN_FORMATS`, `vrr_capable`, `VRR_ENABLED`, `GAMMA_LUT`.
- Parse the primary plane's `IN_FORMATS` blob. `drm` has no helper for it, so it is about 40 lines in `kms/in_formats.rs`.

**Scanout strategies.** Two strategies sit behind one `Scanout` trait in the new native crate:

1. **Zero-copy (`GbmScanout`).** Keep three GBM buffer objects, allocated with `create_buffer_object_with_modifiers2(w, h, Xrgb8888, modifiers, SCANOUT | RENDERING)`. Each one's dma-buf fd, modifier, stride and offset are imported into wgpu through `wgpu_hal::vulkan::Device::texture_from_dmabuf_fd` and `wgpu::Device::create_texture_from_hal`. `add_planar_framebuffer` with `FbCmd2Flags::MODIFIERS` turns each one into a KMS framebuffer. The modifier list is the intersection of three sets:
   - the plane's `IN_FORMATS` for XRGB8888;
   - Vulkan's `VkDrmFormatModifierPropertiesEXT` for `B8G8R8A8_SRGB` with `COLOR_ATTACHMENT`;
   - modifiers whose plane count is 1 (`gbm::Device::format_modifier_plane_count`).

   The plane-count restriction matches the importer, which "currently only supports single-plane DMA-bufs" (device.rs:514). It also excludes compression modifiers (AMD DCC, Intel CCS), which would need a queue-family ownership transfer to `VK_QUEUE_FAMILY_FOREIGN_EXT` that wgpu does not record. The device must be opened with `Features::VULKAN_EXTERNAL_MEMORY_DMA_BUF` (wgpu-types-30.0.2/src/features.rs:1246). That feature also covers the DMA-BUF roadmap item, client DMA-BUF import, which uses the same importer.
2. **Copy (`DumbScanout`).** Render into a wgpu `Bgra8UnormSrgb` texture. `OffscreenTarget` (crates/shingle-render/src/target.rs) is `Rgba8UnormSrgb`, so this path either adds a BGRA offscreen target or swaps channels while copying rows; `draw` already accepts any target format. Then `copy_texture_to_buffer`, map the buffer, and copy rows into a mapped `create_dumb_buffer` (XRGB8888, linear) framebuffer, using two dumb buffers. This works on any KMS driver with any Vulkan adapter, including lavapipe. It is the fallback when zero-copy is unavailable and is tested by forcing it on the test machine. It costs one GPU readback and one CPU copy of the frame (about 8 MB at 1080p), which is acceptable as a fallback and is logged as such.

The native crate first tries zero-copy. It falls back to copy if the feature is missing, the intersection is empty, the GBM allocation fails or the first `TEST_ONLY` commit fails. `SHINGLE_SCANOUT=copy` forces the fallback.

**GPU selection.** udev enumerates `drm` `card*` devices and picks the one with `boot_vga=1`, or the first one, unless `SHINGLE_DRM_DEVICE=/dev/dri/cardN` is set (like `WLR_DRM_DEVICES`). The wgpu adapter is the one whose `AdapterInfo::device_pci_bus_id` matches the card's PCI slot. The copy path may use any adapter, so a card without a matching Vulkan adapter (a display-only card, or lavapipe as the only Vulkan driver) still works.

**Synchronisation.** After `queue.submit`, wait for completion with `device.poll(PollType::Wait { submission_index, timeout: Some(100 ms) })`, then commit. wgpu cannot export a sync_file for `IN_FENCE_FD`, and Vulkan does not attach implicit fences to non-WSI dma-bufs, so an explicit wait is the only correct option today. With Shingle's simple composition the wait is short. Exporting a semaphore as a sync_file through raw Vulkan is left as a later optimisation (NB-10).

**Page-flip pacing.** A `FrameScheduler` state machine in `pacing.rs` has the states `Idle`, `Rendering`, `FlipPending` and `Paused`, with at most one commit in flight.

- A render starts when the frontend needs a frame (`changed`, `Compositor::needs_frame()`, a running animation) and no flip is pending.
- Commits use `NONBLOCK | PAGE_FLIP_EVENT`. Modesets add `ALLOW_MODESET` and are always preceded by a `TEST_ONLY` commit.
- On `Event::PageFlip`, the scheduler calls the new `Compositor::frame_presented` with the flip's timestamp, sequence number and refresh interval. That sends frame callbacks and `wp_presentation` feedback, and the next frame may start.

This replaces the fixed 16 ms `WaitUntil` (main.rs:4540) and the hard-coded `16_666_667` refresh and "now" timestamp in `presentation::frame` (crates/shingle/src/presentation.rs:35-64). Feedback carries `VSYNC | HW_CLOCK | HW_COMPLETION`, plus `ZERO_COPY` on the GBM path.

**VRR.** Off by default. Wayfire's `[output:NAME] vrr` option sets `VRR_ENABLED` on the CRTC when the connector's `vrr_capable` is 1. Pacing does not change: Shingle already renders only when there is damage. NB-10.

**Tearing.** When `Compositor::allows_tearing()` is true and the driver reports `DriverCapability::AtomicASyncPageFlip`, primary-plane-only flips use `PAGE_FLIP_ASYNC`. This is the native counterpart of `Renderer::set_tearing`. NB-10.

**Gamma, DPMS and cursor.**

- wlr-gamma-control stays in the shader LUT (`Renderer::set_gamma_ramp`), so both scanout paths and nested behave the same. Moving it to the KMS `GAMMA_LUT` is optional later.
- `Event::DpmsChanged` commits `ACTIVE=0` or `ACTIVE=1` on the CRTC.
- The cursor is already composited as a `SurfaceRole::Cursor` snapshot (crates/shingle/src/state.rs:718). Native keeps it that way first and turns on `Compositor::set_default_cursor(true)` (NB-2), which draws the theme's `default` arrow (falling back to `left_ptr`) when no client cursor applies. A hardware cursor plane is NB-10.

### `shingle-render` without winit

`shingle-render` uses winit only for `Arc<Window>` in `Renderer::new` and for `PhysicalSize` (crates/shingle-render/src/lib.rs:10, 309, 456, 519). Its two renderers duplicate the device, pipeline and bind-group setup (`Renderer::new` lib.rs:309-454 and `OffscreenRenderer::new` lib.rs:1119-1240). The split:

- `GpuContext`: instance, adapter, device, queue, pipeline, layouts, sampler, white texture, gamma LUT and effects. Created by `GpuContext::new(GpuOptions { compatible_surface, features, adapter_filter })`. `create_texture` and `update_texture` move here.
- `RenderState`: scale factor, output transform, zoom and nearest filter (the setters at lib.rs:460-516).
- `fn draw(&GpuContext, &RenderState, view: &TextureView, size: (u32, u32), tiles, clear) -> SubmissionIndex`: the pass from lib.rs:599-653, shared by every target.
- Targets:
  - `WindowTarget` wraps a `wgpu::Surface<'static>` made from any `impl Into<wgpu::SurfaceTarget<'static>>`, so nested passes its winit window and `shingle-render` needs no winit;
  - `OffscreenTarget` replaces `OffscreenRenderer`, keeping its readback (used by `shingle-lock` and the tests);
  - `ImportedTarget` wraps a `wgpu::Texture` made from a dma-buf.

Sizes become `(u32, u32)`. `Renderer` and `OffscreenRenderer` stay as thin wrappers until their callers move. The wgpu `drm` feature is removed from the workspace, because Shingle deliberately does not use VK_KHR_display; `drm` 0.15 stays a direct dependency of the native crate.

**As implemented in NB-1.** The code follows the split above, with these differences, each forced by how wgpu works or by keeping behaviour unchanged:

- `GpuContext::new(instance, GpuOptions)` takes the `wgpu::Instance` (from `GpuContext::create_instance()`). A compatible surface has to be created on the same instance before the adapter is chosen, so the context cannot create its own instance. Without an `adapter_filter` the adapter comes from `request_adapter` exactly as before; with one, the first enumerated adapter that passes the filter and supports the surface is used, else `RenderError::NoMatchingAdapter`.
- The pipeline depends on the target format, which for a window is only known once the surface is configured against the device. `GpuContext` therefore keeps one pipeline per format, built the first time `draw` sees that format (from `view.texture().format()`), so `draw` keeps the planned signature and one context serves window, offscreen and future imported targets.
- The nearest filter lives in `RenderState`, but a texture's sampler is fixed in its bind group when it is created. `GpuContext` holds a linear and a nearest sampler, `create_texture` takes a `TextureFilter`, and `RenderState::texture_filter()` supplies it. `set_nearest_filter` still returns whether it changed, so callers recreate textures as before.
- Gamma LUT and inversion live in `GpuContext` as planned, so they apply to every target drawn with it.
- `WindowTarget::new(&gpu, surface, size)` takes an already created surface (`WindowTarget::create_surface(instance, window)`), and `OffscreenTarget::read_rgba` submits its copy separately from `draw`'s pass. A surface with no formats is now `RenderError::NoSurfaceFormat` instead of a panic.
- `ImportedTarget` is left to NB-8, the first step with a dma-buf to wrap. `draw` takes any `TextureView`, so it needs no renderer change.

The offscreen pixel tests (crates/shingle-render/tests/offscreen_pixels.rs) compare against reference images rendered by the renderer before the split, and nested screencopy captures before and after were identical ([status.md](../status.md#with-real-clients)).

### Rejected alternatives

- **wgpu `SurfaceTargetUnsafe::Drm` (VK_KHR_display).** It panics on driver refusal, offers no page-flip events, atomic control or VT-switch handling, and cannot be tested in a VM, as described above. Rejected as the main path. If it ever matters, it could become a third `Scanout` later, but there is no need for it.
- **GBM surface with EGL/GLES.** wgpu's GLES backend has no GBM surface support (wgpu-hal-30.0.2/src/gles/egl.rs handles no DRM or GBM window handle), so this would need a second renderer (glow) next to wgpu, duplicating every effect shader. Rejected.
- **Smithay's DRM backend (`DrmCompositor`, `GbmBufferedSurface`).** It is mature and maintained. But smithay 0.7.0 (2025-06-24) and current master pin `drm 0.14`, while wgpu-hal 30 uses `drm 0.15`. Its renderers are GLES and pixman, not wgpu. It also comes with its own Wayland and seat abstractions, which Shingle's protocol layer already implements. Using only its `backend::drm` module still means adopting its allocator, renderer and buffer traits. Rejected; Smithay, anvil and niri remain useful references for atomic and modifier details.
- **Allocate exportable images in Vulkan and import them into KMS with `prime_fd_to_buffer`.** This avoids GBM, but needs raw Vulkan for export, which wgpu-hal does not expose, and gives no scanout-capability guarantee such as `GBM_BO_USE_SCANOUT`. Rejected.
- **Legacy KMS (`set_crtc`/`page_flip`).** Atomic is universal on supported hardware and gives `TEST_ONLY`. Supporting both doubles the testing. Rejected.
- **`gbm` with `drm-support`.** That pulls `drm 0.14` next to `0.15`. With `default-features = false`, Shingle implements `drm::buffer::PlanarBuffer` for a small `ScanoutBuffer` wrapper using `BufferObject::{handle, stride, offset, modifier}` instead. Once gbm.rs PR #55 is released, the feature can be enabled again.

## 3. Session and seat

### Decision

Use `libseat` 0.2.4 in a `session.rs` wrapper behind a `Session` trait, with `open_device`, `close_device`, `switch_vt`, `is_active` and the fd to poll, so that tests can use a fake.

**Startup.**

- `Seat::open` is called with a callback that only pushes `SeatEvent` into an `Rc<RefCell<VecDeque<_>>>`. The seat fd (`get_fd`) is a calloop `Generic` source whose handler calls `dispatch(0)`.
- Shingle waits up to 5 s for the first `Enable` before opening any device.
- The backend is chosen by libseat (seatd, then logind, then builtin), and can be forced with `LIBSEAT_BACKEND`.
- Started from an SSH shell or another context without a seat, `Seat::open` fails or never enables. Shingle then exits with a message naming the cause; see [6](#6-failure-handling).

**VT switching.**

- The shared `keyboard` module sees Ctrl+Alt+F1..F10 before any binding, even while locked or when shortcuts are inhibited, and calls `Host::switch_vt(n)`. Native maps that to `SeatRef::switch_session(n)`, which is Wayfire's `check_vt_switch` (wayfire/src/core/seat/keyboard.cpp:226). Nested returns false and lets the key through.
- The key press that triggers a switch is not forwarded to clients.

**Pause (`SeatEvent::Disable`).** The frontend works through these steps synchronously, in this order:

1. Set the scheduler to `Paused` and drop any pending flip; its event will not arrive.
2. Call `libinput.suspend()`, so libinput closes its fds through `close_restricted`.
3. Call `Compositor::set_session_active(false)`. It releases every held key, button and touch with the same code as `release_held` (crates/shingle/src/input_device.rs:127), so no client is left with a stuck key. It also stops sending frame callbacks.
4. Acknowledge with `SeatRef::disable()`.

The DRM fd stays open. Clients keep running and their commits are still processed; nothing is drawn.

**Resume (`SeatEvent::Enable`).**

1. Call `libinput.resume()`.
2. Re-probe connectors, because hotplug may have happened while Shingle was away.
3. Do a full `ALLOW_MODESET` commit, because the other session may have changed modes.
4. Call `Compositor::set_session_active(true)`.
5. Resync keyboard LEDs and modifiers.
6. Request a frame.

GPU textures survive the switch, since the Vulkan device does not depend on DRM master.

**Shutdown.** SIGINT, SIGTERM, the `quit` binding and IPC `request_quit` all end in `Frontend::shutdown()`: save the session, drop libinput, close every libseat device, drop `Seat`. Shingle does not try to restore the console mode itself. Restoring the VT is the seat daemon's job, and the kernel's fbdev emulation restores the console framebuffer when the last DRM master closes the device. Both also happen when Shingle is killed or crashes, which is why direct VT ioctls are rejected below.

### Rejected alternatives

- **logind over D-Bus directly (`TakeControl`, `TakeDevice`, `PauseDevice`, `ResumeDevice`).** It only works under systemd-logind, needs a D-Bus client crate and its own async plumbing, and libseat already wraps it. Rejected.
- **Direct VT ioctls (`VT_SETMODE`, `KDSETMODE`) with root or setuid.** A crash leaves the VT in graphics mode with keyboard input off, which is exactly the "no usable TTY" failure. It also means running as root. Rejected.
- **Pausing by closing the DRM fd.** libseat and logind revoke master rather than the fd. Closing and reopening would lose the GEM handles of the scanout buffers. Rejected.

## 4. Input

### Decision

**Context.**

- `input` 0.10 with `default-features = false, features = ["udev", "libinput_1_19"]`. Debian trixie ships libinput 1.28.1.
- `Libinput::new_with_udev(SeatInterface)`, then `udev_assign_seat(seat.name())`. `SeatInterface` implements `LibinputInterface` over the `Session` trait: `open_restricted` calls `open_device` and keeps a map from fd to libseat device id for `close_restricted`.
- The libinput fd is a calloop source; `dispatch()` is followed by draining the iterator.

**Devices.**

- `DeviceEvent::Added` registers the device with the compositor, applies configuration (below) and records which capabilities it has. A Shingle seat device has exactly one kind, so a libinput device with several capabilities (a keyboard with a touchpad, say) is registered once per capability, as wlroots splits it into one `wlr_input_device` per type. `Removed` unregisters each of them.
- No input devices at startup is not an error; see [6](#6-failure-handling).
- Keyboard LEDs follow xkb state through `Device::led_update`.

**Mapping onto the `Compositor` API** (crates/shingle/src/lib.rs). The translation goes through `shingle_frontend::InputEvent`, so bindings, Scale, Expo and drags behave exactly as in nested.

| libinput event | `InputEvent` | Compositor call (through the frontend) |
| --- | --- | --- |
| `KeyboardEvent::Key` (`key`, `key_state`, `time_usec`) | `Key` | `keyboard_key` (lib.rs:925), then `keyboard_modifiers` (lib.rs:929) with masks from xkb |
| `PointerEvent::Motion` (`dx`, `dy`, `dx_unaccelerated`, `dy_unaccelerated`) | `PointerMotion` | The frontend clamps the position to the output layout and calls `pointer_motion` (lib.rs:817). It calls `pointer_relative_motion` (lib.rs:821) always, which is better than nested, where relative motion is only sent while constrained (main.rs:4489). |
| `PointerEvent::MotionAbsolute` (`absolute_x_transformed(w)`) | `PointerMotionAbsolute` | `pointer_motion` |
| `PointerEvent::Button` | `PointerButton` | `pointer_button` (lib.rs:879), or a binding |
| `PointerEvent::ScrollWheel` (`scroll_value`, `scroll_value_v120`) | `PointerAxis` (wheel) | `pointer_axis_detailed` (lib.rs:893), with steps = v120 / 120 |
| `PointerEvent::ScrollFinger`, `ScrollContinuous` | `PointerAxis` (finger, continuous) | `pointer_axis_detailed` with 0 steps; NB-6 adds the axis source and stop to the API |
| `TouchEvent::{Down, Motion, Up, Cancel, Frame}` (`seat_slot`, `x_transformed`, `y_transformed`) | `Touch` | `touch_down`, `touch_motion`, `touch_up`, `touch_cancel` (lib.rs:909-921), through `map_output_point` (lib.rs:1596) for the output transform, as nested does |
| `GestureEvent::{Swipe, Pinch, Hold}` | `Swipe`, `Pinch`, `Hold` | `pointer_swipe_*`, `pointer_pinch_*`, `pointer_hold_*` (lib.rs:841-877). Nested never feeds these, so touchpad gestures reach clients for the first time. |
| `SwitchEvent`, tablet events | ignored | Shingle has no tablet protocol yet. |

All `time_usec` values are passed through, so clients see libinput timestamps instead of the compositor's own clock. The frontend calls `Compositor::set_input_time(Some(Duration::from_micros(time_usec)))` before feeding an event and `set_input_time(None)` afterwards (NB-2). Without a backend time, input timestamps also come from `CLOCK_MONOTONIC`, the clock libinput uses, so both sources share a base.

**Pointer acceleration and device configuration.**

- libinput does acceleration; Shingle never accelerates in software.
- On `Added`, the native crate applies Wayfire's `[input]` options (wayfire/metadata/input.xml) through `Device::config_*`: `mouse_accel_profile`, `mouse_cursor_speed`, `touchpad_accel_profile`, `touchpad_cursor_speed`, `tap_to_click`, `click_method`, `scroll_method`, `natural_scroll`, `disable_touchpad_while_typing`, `drag_lock`, `tap_and_drag`, `left_handed_mode` and `middle_emulation`.
- Only `mouse_scroll_speed`, `mouse_natural_scroll`, `left_handed_mode` and `middle_emulation` exist in `InputConfig` today (crates/shingle-config/src/lib.rs:1860). NB-6 adds the rest to `InputConfig` and to the Wayfire importer (crates/shingle-config/src/wayfire.rs:188-219).
- Left-handed mode and middle emulation come from libinput on native. Nested keeps its software `MouseButtons`.

**Input-device IPC** (crates/shingle/src/input_device.rs).

- Before NB-5 there were three fixed devices in `DEVICES` (input_device.rs:63-80), gated by kind. NB-5 replaced that with a registry (as built: [NB-5](#nb-5-shingle-input-registry-per-device-seat-devices)): `Compositor::add_input_device(InputDeviceInfo { name, vendor, product, kind }) -> InputDeviceId`, `remove_input_device(id)`, and an `InputDeviceId` on each entry point. Held keys, buttons and touches are then tracked per device, so disabling one keyboard does not release keys held on another.
- `input/list-devices` (`list_json`, input_device.rs:163) reports real names, vendor and product. Wayfire reports the same fields (wayfire/plugins/ipc-rules/ipc-input-methods.hpp:59-73).
- Nested registers its three devices with today's names and ids 1-3, so its IPC output does not change.
- `input/configure-device` (`configure`, input_device.rs:183) keeps its validation and error messages, and still releases held input and closes the gate (`set_enabled`, input_device.rs:108).
- The resulting `Event::InputDeviceChanged` now carries the id. Native reacts with `config_send_events_set_mode(DISABLED)` or `ENABLED`, which is what Wayfire does (wayfire/src/core/seat/seat.cpp:570-590). Because Shingle's gate closes first, it does not matter whether libinput emits release events of its own: any that arrive are dropped.

### Rejected alternatives

- **Reading evdev directly (`evdev` crate).** You lose touchpad handling, acceleration, gestures, palm rejection, quirks and device configuration, all of which Wayfire users expect. Rejected.
- **libinput's path backend (`new_from_path`) as the main path.** You lose udev hotplug and seat assignment. It stays useful in tests with uinput devices.
- **Handling device hotplug through a separate udev monitor.** The libinput udev backend already does it, and a second monitor would race it. udev is still monitored for DRM; see [5](#5-outputs).

## 5. Outputs

### Decision

**Model.**

- A `kms::Output` per connector holds the connector handle, name (`DP-1`, `HDMI-A-1` from the connector type and type id), make, model and serial (from the EDID blob), physical size, mode list, CRTC, primary plane, property ids, scanout and scheduler.
- The `kms::Outputs` map is keyed by connector name, and `enabled: Vec<String>` lists which outputs Shingle uses. In NB-7 that list has exactly one entry; other connected outputs are turned off (`ACTIVE=0`, `CRTC_ID=0`) and kept in the map.

**Choosing the output and mode.**

- The output is the first connected connector in `resource_handles()` order, preferring internal panels (eDP, LVDS, DSI). A config entry whose output name matches wins.
- The mode follows Wayfire's `find_matching_mode` and default heuristic (wayfire/src/core/output-layout.cpp:150-191 and 455-520). `auto` or no config means the preferred mode. `WxH@R` means an exact match, otherwise the closest refresh at that size. `highres` and `highrr` behave as in Wayfire, and `off` disables the output.
- Today the Wayfire importer drops the `@refresh` part and stores only `width`/`height` (crates/shingle-config/src/wayfire.rs:594-604, `OutputConfig` lib.rs:524). NB-9 adds `name`, `refresh_mhz`, `mode` (`auto`/`off`/`highres`/`highrr`/explicit) and `vrr` to `OutputConfig`. A single `[output]` section stays valid. (As built, the refresh rate is part of the explicit `mode` rather than a field of its own; see [NB-9 as built](#nb-9-as-built).)
- Mode selection is a pure function in `kms/modes.rs`, unit-tested with mode lists.

**Compositor output description.**

- Today the output is hard-coded as `HEADLESS-1`, 1280x720, 60 Hz, 310x175 mm (crates/shingle/src/lib.rs:80-82, state.rs:241, state.rs:1748-1781, state.rs:889, state.rs:965, xdg_output.rs:25, output_management.rs:153).
- NB-2 added `Compositor::set_output_info(OutputInfo { name, make, model, serial, physical_size_mm, subpixel, refresh_mhz })`, used for `wl_output.geometry`/`mode`/`name`/`description`, xdg-output, wlr-output-management, ext-workspace ids and the IPC output name. The default stays `HEADLESS-1` for nested and headless use. The mode list and current-mode index of the original design are left to NB-9, which is the first step that offers more than one mode to wlr-output-management; until then the mode clients see is the output rectangle at `refresh_mhz`.
- A bound `wl_output` (and xdg-output and output-management head) keeps the name it was first sent, because the protocols send an output's name once. The native backend therefore describes its output before the first `dispatch_pending`, and NB-9 recreates the globals when the enabled output changes.
- `set_output` (lib.rs:1654) keeps receiving the logical rectangle from `logical_output_size` (lib.rs:1583).
- The current core output id (`OutputId::new(OUTPUT_NAME)`, state.rs:241) is to be renamed to the connector name when the first output is described, so sessions persist per connector name, which multi-output needs anyway. NB-2 does not do this yet: `shingle-core` has no output rename, and renaming decides which saved session a native start restores, so it moves to NB-9 together with hotplug.

**Hotplug.**

- A `udev::MonitorBuilder` on subsystem `drm` delivers `change` events with `HOTPLUG=1` for the card. The native crate re-reads connectors and diffs them against `kms::Outputs`.
- If the enabled output disconnects, the next connected one is enabled and moded, and the compositor gets `set_output_info` and `set_output`. Windows keep their tiling trees because the core output is renamed, not recreated.
- If nothing is connected, Shingle keeps running with the last output size, stops scheduling frames and waits. Wayfire behaves the same way, keeping its views without a physical output.
- wlr-output-management `apply` requests (output_management.rs) that change mode or turn the output off are validated with `TEST_ONLY` before they are applied. (As built, turning the only output off is refused, as DPMS covers it; see [NB-9 as built](#nb-9-as-built).)

**Room for multi-output (roadmap item "Multi-output").**

- Everything per output (scanout, scheduler, page-flip routing by `PageFlipEvent::crtc`, mode config, VRR) already lives in `kms::Output`.
- CRTC assignment for more than one output is a bipartite match over `encoder.possible_crtcs()`, done when `enabled` grows past one entry.
- What multi-output must still add lives in `shingle` and `shingle-core`: more than one `Rect`, per-output `wl_output` globals, and per-output frame callbacks. It does not need to change the native crate's structure.
- Mirroring and multi-GPU (outputs on a second card) are out of scope.
- The multi-output design ([multi-output.md](multi-output.md#decision-native-extends-nb-9)) takes this up in its MO-7: a list of lit outputs instead of NB-9's one, wlroots' CRTC matching, combined modesets and flips per CRTC. Built, and unit-tested through the fake card; see [MO-7 as built](multi-output.md#mo-7-as-built) and steps 25 to 35 of [native-testing.md](../native-testing.md).

### Rejected alternatives

- **Enabling every connected output at once in NB-7.** `shingle` has only one output rectangle and global, and making that plural is the multi-output item's work. Lighting a second screen with a duplicate or a blank image would be confusing. Rejected.
- **Using the mode already set on the CRTC (taken over from fbcon) as the default.** It is often a low firmware mode, and Wayfire picks the preferred mode instead. Kept only as a last fallback when the preferred mode fails `TEST_ONLY`.

## 6. Failure handling

Two principles.

- **Fail fast before the session exists.** The display is fully set up, and a `TEST_ONLY` commit has passed, before `Frontend::new` creates the Wayland socket and launches autostart and session-restore children (main.rs:256-330). A failed start therefore leaves no orphaned clients.
- **After startup, stay alive.** Recover where possible, otherwise save the session and exit cleanly. Nothing in the native crate calls `expect` or `unwrap` on a device result.

Diagnostics go to stderr and to `$XDG_STATE_HOME/shingle/native.log`, because stderr is invisible once the VT is in graphics mode. A panic hook writes the panic to the log; with unwinding, the `Session` and `Libinput` destructors still run.

| Situation | Detection | What Shingle does |
| --- | --- | --- |
| No seat (started over SSH, no seatd or logind session) | `Seat::open` returns `Err`, or no `Enable` within 5 s | Exit 1 with "no seat: run from a VT login or start seatd". Nothing has been opened, so the host's DRM and input are untouched. |
| Session exists but is not on the active VT | No `Enable` within 5 s | Same as above. Shingle never steals the display: libseat only grants devices to the active session. |
| libseat `builtin` backend (running as root without seatd) | `Seat::open` succeeds with the builtin backend | Allowed only with `--allow-builtin-seat`, because builtin cannot switch VT and so offers no way back to a TTY. |
| No DRM device, or `SHINGLE_DRM_DEVICE` invalid | udev enumeration is empty, or `open_device` fails | Exit 1 before the socket exists, naming the devices it tried. |
| No DRM master (another master holds the card) | `acquire_master_lock`, or the first commit fails with `EACCES`/`EBUSY` | Exit 1 with a message. This cannot happen under libseat on the active VT unless something else bypasses the seat. |
| No atomic support | `set_client_capability(Atomic)` fails | Exit 1, "atomic KMS required". |
| No connected output at startup | No connector in `Connected` state | Start anyway, without scanning out, and wait for hotplug. Clients run, and the frontend renders nothing. |
| Mode or first commit rejected | `TEST_ONLY` fails | Try the next mode (preferred, then modes by area, then the current CRTC mode). Then try copy scanout instead of zero-copy. If everything fails, exit 1 before the socket exists. |
| Zero-copy unavailable | Missing feature, empty modifier set, GBM or import error | Use copy scanout and log the reason once. |
| No Vulkan adapter | `request_adapter` fails | Exit 1. Lavapipe counts as an adapter; there is no GPU-less renderer. |
| No input devices | No `Added` events after the first `dispatch` | Log a warning and keep running; devices may appear later. SIGTERM still exits cleanly. |
| VT switch while rendering | `SeatEvent::Disable` arrives during the frame | Everything runs on one thread, so the current frame completes. The pause sequence ([3](#3-session-and-seat)) then runs before the next commit. A commit that fails with `EACCES` or `EPERM` while not active counts as paused, not as an error. |
| Page flip never completes | No `PageFlip` within 1 s of a commit | Log, drop the pending state and do a blocking `ALLOW_MODESET` commit. After three consecutive timeouts, save and exit 2. |
| GPU device lost or hung | `set_device_lost_callback` fires, `poll` returns an error, or the 100 ms wait times out three times in a row | Drop the `GpuContext` and scanout and recreate them. The texture cache is cleared and refilled from `visible_surfaces()` snapshots, which already carry their RGBA. After three failures within 60 s, save the session and exit 2. |
| Output unplugged | udev hotplug | See [5](#5-outputs). |
| SIGINT or SIGTERM | The `Compositor` signalfd sets `shutdown` | `Frontend` checks `Compositor::shutdown_requested()` (NB-2) after each dispatch, saves and exits 0. |
| Panic | Panic hook | Log, then unwind. libseat and fbdev restore the TTY. |
| SIGKILL, abort or segfault | none | The seat daemon closes the session's devices and restores the VT, and the kernel restores the fbdev console on the last DRM close. The operator gets the TTY back, or can switch VT with Ctrl+Alt+Fn, which the seat daemon handles once Shingle is gone. |

**Getting back to a usable TTY.** There are three independent routes:

- the user's Ctrl+Alt+Fn, which works while Shingle runs (shared keyboard) and after it dies (seat daemon);
- a clean exit on SIGTERM from another VT or over SSH;
- kernel and seat-daemon cleanup after a crash.

A deliberate design point: no code path in Shingle puts the VT into graphics mode itself.

**A bug this design found and fixes.** `Compositor::with_core` installs a calloop `Signals` source for SIGINT and SIGTERM (crates/shingle/src/lib.rs:496-502). That source blocks both signals and only sets `state.shutdown`. `shingle-nested` never reads that flag (there is no accessor) and exits only on its own `quit` flag (main.rs:4518). That is why nested ignored SIGTERM and SIGINT in the 2026-11-25 run (roadmap item "Host input checks after the rename"). `Compositor::request_quit` (lib.rs:731) has the same problem. NB-2 added `shutdown_requested()` and made nested honour it; a real nested run then exited with status 0 within about 0.1 s on both signals and saved its session ([status.md](../status.md#with-real-clients)).

## 7. Testing

The operator's Wayfire session on this host must never be stopped or disturbed. This host has one GPU (NVIDIA GA106, RTX 3060) running that session, and no `/dev/dri` is visible inside the Claude sandbox. So no native test ever runs on this host: not on another VT, not with a second seat on the same GPU. Native code runs only on the dedicated test machine.

### Layer 1: host unit tests, no devices

Run with `cargo test --workspace` and the [development.md](../development.md#build-test-and-lint) rules.

- **`shingle-frontend`.**
  - Every existing nested test moves with its module.
  - A `FakeHost` records redraw requests, warps, VT switches and exit.
  - Input scripts run against `Compositor::headless()` plus `OffscreenTarget`, covering bindings, Scale and Expo selection, drags, the zoom mapping, VT-switch interception, key repeat and modifier masks from real xkb state.
  - Scene tests compare `RenderTile` lists, not pixels.
- **`shingle-native`.** Every device interaction is behind a trait: `Session`, `KmsDevice` (properties, `atomic_commit` and `receive_events` as a fake that records requests), `InputSource` (libinput events as plain structs). Pure logic tested on the host:
  - mode selection against Wayfire's heuristics;
  - the `IN_FORMATS` parser;
  - modifier intersection;
  - atomic request building, checking the exact property set for modeset, flip, DPMS and VRR;
  - the `FrameScheduler` and pause/resume state machines, including a VT switch with a flip pending;
  - hotplug diffing;
  - the startup error decisions in the [6](#6-failure-handling) table.
- **`shingle`.** Tests for the device registry, `set_session_active` releasing held input, `frame_presented` feedback values, `OutputInfo` reaching `wl_output`, and `shutdown_requested`.

### Layer 2: dedicated test machine

The operator decided against VM testing (2026-11-25) and is setting up a separate machine for native work. Everything that needs a seat, input devices or KMS is verified there: NB-6 onwards. Until it exists, NB-1 to NB-5 proceed, since they are verified on this host with unit tests and nested runs, and native code is only built and unit-tested here.

What the test machine covers:

- libseat with seatd or logind, Shingle started from a TTY;
- libinput hotplug with real devices, plus uinput virtual keyboards, mice and touchscreens created by a Python harness using `python3-evdev` for repeatable scripts;
- `input/list-devices` and `input/configure-device` with real libinput devices, and held-input release on disable and on VT switch;
- modeset, mode selection, page-flip pacing, presentation timestamps and DPMS;
- connector hotplug by unplugging and replugging a monitor;
- zero-copy scanout with real modifiers, VRR, async flips, the hardware cursor plane and GPU-hang recovery;
- the failure table in [6](#6-failure-handling), including recovery after `kill -9`;
- SIGTERM cleanup.

A test client that records `wl_keyboard`/`wl_pointer` events checks what clients receive, and pixel checks use Shingle's own wlr-screencopy (crates/shingle/src/screencopy.rs) with `grim`. Test harnesses are Python scripts under `tools/native_test/`, run on the test machine over SSH. Once the machine exists it should be registered as a takt target so workspaces can claim it exclusively.

The steps for NB-6 are in [native-testing.md](../native-testing.md), with `wev` as the recording client and tools/native_test/uinput.py (virtual keyboard, mouse and touchscreen through `python3-evdev`, refusing to run without the test machine's marker file) and tools/native_test/ipc.py for IPC. The NB-7 steps follow them there, with `grim`, `wlopm`, the presentation probe (crates/shingle-native/examples/present_probe.rs) and tools/native_test/present_check.py, which compares the probe's feedback with the page flips Shingle logs under `SHINGLE_TRACE_FLIPS=1`. NB-8's zero-copy steps (36 to 42) use the same probe, with `present_check.py --scanout zero-copy|copy` checking the `ZERO_COPY` flag.

### Rejected test strategies

- **A second seat or another VT on this host.** It shares the GPU and the kernel DRM device with Wayfire, and a VT switch pauses it. Forbidden.
- **takt VMs (plain, or with a virtio-gpu display).** They would cover session, input and copy-path KMS, but the operator ruled out VM testing. Real hardware also exercises paths a VM cannot: real modifiers, VRR, cursor planes and GPU recovery.
- **virgl or venus.** Guest GPU work would run on the host GPU that drives the operator's session.

## 8. Implementation plan

The workspaces are ordered by dependency, and each one can be finished and verified on its own. Names are suggestions for `takt` workspaces. Every workspace also follows the repository rules: unit tests for new code, clippy clean, 80 columns, docs/status.md and docs/roadmap.md updated, signed commits.

### NB-1 `shingle-render-targets`: renderer without winit

Split `shingle-render` into `GpuContext`, `RenderState`, `draw` and the `WindowTarget`/`OffscreenTarget` targets ([2](#shingle-render-without-winit)). Remove the wgpu `drm` feature.

Acceptance:

- `crates/shingle-render/Cargo.toml` has no `winit`.
- `Renderer` and `OffscreenRenderer` are thin wrappers over the shared setup, with no duplicated pipeline code.
- Nested and `shingle-lock` build unchanged.
- Existing render tests pass, plus a test that `draw` into an `OffscreenTarget` produces the expected pixels.
- A nested smoke run shows no visual change.

### NB-2 `shingle-compositor-host-api`: what a real backend needs from `Compositor`

Add to `shingle`:

- `impl AsFd for Compositor` (the calloop epoll fd);
- `shutdown_requested()`;
- `set_output_info(OutputInfo)`;
- `frame_presented(PresentInfo { time, refresh_ns, seq, flags })`, replacing the constants in `presentation::frame`;
- input entry points that take a timestamp;
- `set_session_active(bool)`;
- a default `left_ptr` cursor option.

Make nested honour `shutdown_requested()`.

Acceptance:

- Unit tests for each new API.
- Nested exits cleanly on SIGTERM and SIGINT, verified with a real run. This closes that part of the real-client verification roadmap item.
- With no output info set, `wl_output` still reports `HEADLESS-1`.

Done on branch `shingle-compositor-host-api`. The API as built (crates/shingle/src/lib.rs):

```rust
impl AsFd for Compositor;
pub fn shutdown_requested(&self) -> bool;
pub fn set_output_info(&mut self, info: OutputInfo);
pub fn output_info(&self) -> &OutputInfo;
pub fn frame_presented(&mut self, info: PresentInfo);
pub fn set_input_time(&mut self, time: Option<Duration>);
pub fn set_session_active(&mut self, active: bool);
pub fn set_default_cursor(&mut self, enabled: bool);

pub struct OutputInfo {
  pub name: String,
  pub make: String,
  pub model: String,
  pub serial: Option<String>,
  pub physical_size_mm: (u32, u32),
  pub subpixel: wl_output::Subpixel,
  pub refresh_mhz: u32,
}
pub struct PresentInfo {
  pub time: Duration,
  pub refresh_ns: u32,
  pub seq: u64,
  pub flags: PresentationFlags,
}
// wp_presentation_feedback::Kind, re-exported.
pub use ...::wp_presentation_feedback::Kind as PresentationFlags;
```

Where the code differs from the plan above, and why:

- **Timestamps.** Instead of a timestamped variant of every entry point (about fourteen methods), one `set_input_time(Option<Duration>)` sets the `CLOCK_MONOTONIC` time used by all seat entry points until it is cleared. The frontend's `handle_input` sets it from `InputEvent::time_usec` and clears it at the end, so bindings and compositor actions triggered by the event share its time. Default input timestamps, frame callback times and relative-pointer `utime` now use `CLOCK_MONOTONIC` instead of the time since startup, so libinput times and the compositor's own share a base, as in Wayfire.
- **`AsFd`.** The descriptor is calloop's epoll fd, as planned. calloop keeps its timers in the poll timeout, though, so the 50 ms tick that runs idle timeouts and serves IPC (IPC is polled, not event-driven) would never have made that fd readable. The tick is now a timerfd source. A backend polling the fd therefore wakes at least every 50 ms; IPC latency stays up to 50 ms as before.
- **`OutputInfo`.** No mode list or current-mode index yet, and no core output rename; see [5](#5-outputs).
- **`frame_presented`.** `frame()` stays for nested and is `frame_presented` with the current time, the `OutputInfo` refresh rate, sequence 0 and no flags. The default 60 Hz refresh is still reported as 16 666 667 ns.
- **`set_session_active(false)`** also drops seat and STIPC input until the session is active again and makes `frame_presented` a no-op, so pending presentation feedback waits for the first frame after resume instead of being discarded.
- **Default cursor.** A method, `set_default_cursor(bool)`, rather than a `Config` option, because it is a property of the backend, not user configuration, and `apply_config` would otherwise reset it on every reload.

### NB-3 `shingle-frontend-extract`: create the shared crate with the pure modules

Create `crates/shingle-frontend`. Move into it `expo.rs`, `workspace_transition.rs`, `extra_gestures.rs`, `scale_title.rs`, `icon_theme.rs`, the Scale free functions, the binding helpers and a new xkb-based `keyboard` module with the evdev name table. Switch nested's key handling to the shared keyboard module.

Acceptance:

- The moved tests pass in the new crate.
- The new evdev-name tests reproduce every expectation of main.rs:5058-5130.
- Nested key bindings and `xkb:` bindings work in a real run with a non-US Shingle keymap.
- `shingle-frontend` does not depend on winit.

Done on branch `shingle-frontend-extract`. Where it differs from the plan:

- `ScaleState` moved to `scale.rs` with the free functions, because `scale_geometry_at` takes it; the Scale methods and `ScaleTitle` (which holds a `GpuTexture`) stay in nested until NB-4.
- `bindings.rs` holds only the free helpers (`grid_slot`, `binding_modifiers`, `modifiers_empty`, `modifiers_held`, `modifier_matches`). `pointer_button_action`, `run_matching_command*` and `keyboard_input` are `App` methods and move with it in NB-4; `keyboard_input` already takes a `keyboard::Key`.
- The keyboard module differs as described in [Keyboard handling](#keyboard-handling-becomes-shared-and-xkb-based): repeats still come from the host, the compositor keeps owning locks and the layout index, and focus changes are handled in nested.

### NB-4 `shingle-frontend-host`: move `App` into `Frontend`

Move the rest of the module plan ([1](#module-plan)): `Frontend`, `Host`, `InputEvent`, `scene`, `drag`, `switcher`, `animation`, `effects`, `session`. Nested becomes a winit host of about 600 lines.

Acceptance:

- `crates/shingle-nested/src/main.rs` contains only winit glue, `MouseButtons` and argument parsing.
- `FakeHost` input-script tests cover Scale, Expo, the switcher, a drag and a workspace switch.
- The nested real-client smoke test (docs/development.md#real-client-tests) passes unchanged.

Done on branch `shingle-frontend-host`. The API as built:

```rust
// crates/shingle-frontend/src/host.rs
pub trait Host {
  fn request_redraw(&mut self);
  fn warp_pointer(&mut self, x: f64, y: f64);
  fn switch_vt(&mut self, vt: u32) -> bool;
  fn input_device_changed(&mut self, device: InputDeviceId, enabled: bool);
  fn exit(&mut self);
}

pub enum InputEvent {
  Key { code: u32, pressed: bool },
  PointerMotion { dx: f64, dy: f64, dx_unaccel: f64, dy_unaccel: f64 },
  PointerMotionAbsolute { x: f64, y: f64 },
  PointerButton { code: u32, pressed: bool },
  PointerAxis(Axis),
  Touch(Touch),
  Swipe(Gesture),
  Pinch(Gesture),
  Hold(Gesture),
  Reset,
}

// crates/shingle-frontend/src/frontend.rs
impl Frontend {
  pub fn new(config, spawn, config_path) -> Result<Self, Box<dyn Error>>;
  pub fn with_compositor(config, compositor, store, children, config_path)
    -> Result<Self, Box<dyn Error>>;
  pub fn handle_input(&mut self, host: &mut dyn Host, device: InputDeviceId,
    event: InputEvent, time: Option<Duration>);
  pub fn update_keyboard(&mut self, host: &mut dyn Host,
    device: InputDeviceId, update: impl FnOnce(&mut Keyboard) -> bool);
  pub fn process(&mut self, host: &mut dyn Host);
  pub fn render(&mut self, host: &mut dyn Host,
    target: &mut dyn RenderTarget) -> Result<(), Box<dyn Error>>;
  pub fn set_output_size(&mut self, width: u32, height: u32) -> Result<..>;
  pub fn output_resized(&mut self, width: u32, height: u32) -> Result<..>;
  pub fn next_deadline(&self) -> Option<Instant>;
  pub fn shutdown(&mut self);
  pub fn compositor(&self) -> &Compositor;
  pub fn compositor_mut(&mut self) -> &mut Compositor;
  pub fn left_handed_mode(&self) -> bool;
  pub fn middle_emulation(&self) -> bool;
}
```

Where it differs from the plan above, and why:

- **`Host::input_device_changed`** was added. The frontend calls it after dropping its own state for a device that IPC enabled or disabled. Nested resets its button emulation there, which `App` used to do inline; NB-6 needs the same hook to apply libinput's send-events mode.
- **Event times** are one `Option<Duration>` argument of `handle_input` instead of a `time_usec` in every variant, because nested has none; with `None` clients get the time the compositor received the event, as before. `PointerMotion` is relative motion for `zwp_relative_pointer_v1` and is forwarded only while the pointer is constrained, as in nested; it does not move the pointer, so NB-6 sends `PointerMotionAbsolute` too.
- **Rendering** goes through a `RenderTarget` trait (crates/shingle-frontend/src/render_target.rs), implemented for `Renderer` and `OffscreenRenderer`, instead of a concrete target type. `render` and `process` take the host so the frontend can ask for the next frame; the frontend collects redraw requests during a call and passes one on at its end.
- **Construction** is split: `Frontend::new` restores the session, launches clients, prints `WAYLAND_DISPLAY` and starts `--spawn`, as `App::new` did; `Frontend::with_compositor` builds the frontend around an existing compositor, for tests. The host registers its seat devices with `compositor_mut()` afterwards, and the frontend maps device IDs to kinds with `Compositor::input_device` instead of nested's fixed devices. Modifiers are sent for the keyboard the key came from.
- **Focus handling** stays nested glue, as NB-3 left it: focus loss sends `InputEvent::Reset`, and the stand-in modifier presses on focus gain go through `update_keyboard`, which changes the xkb state without dispatching keys.
- **Key repeat** is `keyboard::KeyRepeat`, polled in `process` and reported by `next_deadline`. It repeats the most recently pressed key that repeats in Shingle's keymap, at `repeat_rate` and `repeat_delay`, until that key is released or `Reset` arrives, as Wayland clients do. Nested drops winit's repeats. This is the one intended behaviour change: repeats now follow Shingle's settings, not the host's. `next_deadline` covers only key repeat; nested still wakes at least every 16 ms for config reload, the drag and gesture timers and child reaping, and NB-6 should do the same or extend it.
- **VT switching** is in: a press producing `XF86Switch_VT_1` to `XF86Switch_VT_12` goes to `Host::switch_vt` before anything else, also on the lock screen. Nested returns false, so the key is handled as before.
- **Module layout.** The `Frontend` glue for Scale, Expo and bindings lives in child modules of `frontend` (crates/shingle-frontend/src/frontend/scale.rs, expo.rs, bindings.rs), next to `switcher.rs`, `drag.rs`, `effects.rs`, `animation.rs`, `scene.rs` and `session.rs`, rather than in the crate-level `scale.rs`, `expo.rs` and `bindings.rs`, which keep the pure layout and helper code. The zoom and alpha wheel handling is `effects.rs`'s `pointer_axis`.
- **Button emulation** stays in nested (`MouseButtons`) but now translates the button code, as libinput does. Before, `left_handed_mode` swapped only the names bindings matched, clients still got the physical button, and Scale and Expo saw raw buttons before any emulation. Now clients, bindings and the overviews all see the swapped or emulated button, as in Wayfire. Both options are off by default; with them off nothing changes.
- **The argument parser** stays in nested's `main.rs`; there is no `shingle_frontend::cli` yet, and the switch to one `shingle` binary (decision 4) is left to NB-6, where the second host appears. It was done ahead of NB-6 after all, in `shingle-single-binary`; see [The `shingle` binary](#the-shingle-binary) under NB-6.
- Nested's `main.rs` is 870 lines, about 600 without its tests.
- **Animation frames.** As built, `render` decided after drawing, with a fresh clock reading, whether animations had finished, so a frame slower than what was left of an animation ended it on an intermediate frame. Since the `shingle-frontend-fixes` branch a frame takes one time before drawing and both draws and ends every animation with it, so the final state is always drawn. Hosts need no change: they keep drawing while the frontend asks for frames.

Behaviour was pinned before the move with a scripted session of 38 steps (switchers, Scale with keys, pointer, touch and the title filter, Expo with keys and pointer, workspace moves, focus and move bindings, a move drag, a resize drag, grid snapping, inversion, command bindings, close and quit), recording IPC replies and screencopy captures of both the inner compositor and the frame nested drew; see [status.md](../status.md#with-real-clients).

### NB-5 `shingle-input-registry`: per-device seat devices

Replace `DEVICES` and kind gating with the registry: `add_input_device`, `remove_input_device`, `InputDeviceId` on the entry points, per-device held input, and `Event::InputDeviceChanged { id, enabled }`.

Acceptance:

- Nested's IPC output for `input/list-devices` is unchanged; this is a fixture test.
- New tests cover two keyboards with independent holds, removing a device while keys are held, and configure-device on an unknown id.
- docs/ipc.md is updated.

Done on branch `shingle-input-registry`. The API as built (crates/shingle/src/lib.rs, crates/shingle/src/input_device.rs):

```rust
pub fn add_input_device(&mut self, info: InputDeviceInfo) -> InputDeviceId;
pub fn remove_input_device(&mut self, id: InputDeviceId) -> bool;
pub fn input_device(&self, id: InputDeviceId) -> Option<&InputDeviceInfo>;
pub fn input_device_enabled(&self, id: InputDeviceId) -> bool;
pub fn with_socket_path(config: Config, path: &Path) -> Result<Self, Error>;

// Every seat entry point takes the device first, for example:
pub fn keyboard_key(&mut self, device: InputDeviceId, key: u32, pressed: bool);
pub fn pointer_swipe_begin(&mut self, device: InputDeviceId, fingers: u32);

pub struct InputDeviceId(u32); // get() -> u32, the IPC id
pub struct InputDeviceInfo {
  pub name: String,
  pub vendor: Option<u32>,
  pub product: Option<u32>,
  pub kind: InputDeviceKind,
}
Event::InputDeviceChanged { id: InputDeviceId, enabled: bool }
```

Where the code differs from the plan above, and why:

- **Vendor and product** are `Option<u32>`. Wayfire reports libinput's numeric IDs as JSON numbers and the string `"unknown"` for other devices (wayfire/plugins/ipc-rules/ipc-input-methods.hpp:59-70), so `input/list-devices` does the same; nested passes `None` and keeps `"unknown"`.
- **One kind per device.** Input of another kind than the registered one is dropped, like input from an unknown, removed or disabled device. See the note on multi-capability libinput devices under [Devices](#4-input).
- **Shared holds.** Holds are per device, but clients see one seat. A key, button or touch point held by two devices is released for clients only when the last of them lets go, whether by an ordinary release or by disabling or removing a device. Depressed and latched modifiers are cleared on disable only once no keyboard holds any key; while another keyboard holds keys, the modifier masks stay as the frontend last sent them, because only the frontend's xkb state can say which modifiers the remaining keys produce. NB-6's frontend has to drop the disabled keyboard's keys from its xkb state and resend modifiers when it handles `InputDeviceChanged`.
- **Gestures** belong to the pointer that began them. Only that device updates or ends the gesture, and disabling or removing it cancels it.
- **Removal** emits no `InputDeviceChanged`; the backend removed the device and already knows. IDs are never reused, so a stale ID simply drops input.
- **STIPC** input goes through the registered device of each kind with the lowest ID (nested's own devices), and is dropped with an `ok` reply when there is none. Wayfire instead feeds STIPC through dedicated headless devices that `input/list-devices` lists. NB-6 should decide whether native registers such devices; nested does not, so its list stays unchanged. Since the `shingle-frontend-fixes` branch, a compositor with a frontend (`Compositor::set_stipc_key_events`) passes STIPC keys on as `Event::StipcKey` for that keyboard instead of matching IPC bindings itself, and the frontend feeds them through `key_event` like a key from the device, so configured, `xkb:` and IPC bindings all fire, as with Wayfire's headless keyboard. Keys are tracked while the keyboard is disabled and dispatched only while it is enabled, as for a real key; they are still dropped while the session is inactive. If native registers a dedicated STIPC keyboard, it only has to be the device `stipc_device` picks.
- **`input_device(id)`** was added so a frontend can map the ID in `InputDeviceChanged` back to a kind. **`Compositor::with_socket_path`** was added for the nested fixture test, which needs a full compositor with its Wayland socket outside `XDG_RUNTIME_DIR`.

### NB-6 `shingle-native-session`: native binary with seat and input, no display

New library crate `crates/shingle-native` with modules `session`, `udev`, `input`, `host`, used by the single `shingle` binary (see [Decisions](#decisions)). It runs `Frontend` with libseat, libinput and the Ctrl+Alt+Fn VT switch, and renders offscreen only (no KMS). Also add the extra `[input]` options to `InputConfig` and the Wayfire importer.

Acceptance (test machine):

- `shingle` started from tty1 selects the native backend and runs under seatd or logind; inside a Wayland session it still selects nested.
- Refuses to start over SSH with the "no seat" message.
- uinput keyboard, mouse and touch events reach a test client with libinput timestamps.
- Hotplugged uinput devices appear in and disappear from `input/list-devices`.
- `configure-device` disables a real device and releases its held keys.
- `chvt` away releases held input and `chvt` back restores it.
- SIGTERM exits 0 and tty1 is usable again.

#### The `shingle` binary

The single binary of [decision 4](#decisions) was built ahead of the rest of NB-6, on branch `shingle-single-binary`, with the nested and headless backends; native is a stub. What NB-6 finds:

- **Crate layout.** The binary is package `shingle-bin` (crates/shingle-bin) with a `[[bin]]` named `shingle`. It cannot live in the `shingle` package: the backends depend on the `shingle` library, so the library cannot depend on them, and the library must not gain winit anyway. It does not live in `shingle-native` either, as this entry first planned, so that the native crate stays a library like `shingle-nested` and the binary is the one place that knows every backend. `shingle-nested` is now a library with one entry point, `shingle_nested::run(config, config_path, ipc_path, spawn)`; its old `main.rs` is its `lib.rs` without argument parsing. The headless loop that was `crates/shingle/src/main.rs` is crates/shingle-bin/src/headless.rs. NB-6 adds `shingle-native` as a dependency of `shingle-bin` and replaces the `Backend::Native` arm of `start` in crates/shingle-bin/src/main.rs.
- **`shingle-nested` binary.** Removed, not kept as an alias, because this decision drops it once the switch is in place and session files are to name `shingle` only. `shingle --backend nested` is the explicit equivalent, and inside a session plain `shingle` selects nested anyway.
- **Selection** is the pure function `backend::select(requested, env)` (crates/shingle-bin/src/backend.rs), unit-tested with an injected environment lookup. It follows wlroots' `wlr_backend_autocreate` (backend/backend.c, read on the GitHub mirror's `master`), which Wayfire calls at wayfire/src/main.cpp:422: an explicit choice first, then `WAYLAND_DISPLAY` or `WAYLAND_SOCKET`, then `DISPLAY` select nested, and with none of them native. Shingle's explicit choices are `--backend`/`--headless`, then `SHINGLE_BACKEND`, the counterpart of `WLR_BACKENDS` (one name, not a comma-separated list, since Shingle runs one backend at a time). As in wlroots, an unknown name is an error, not a fallback to detection, and a nested backend that cannot connect to its host is an error rather than a reason to try native. Unlike wlroots, which only checks that `getenv` returns non-NULL, an empty variable counts as unset, so a stray `WAYLAND_DISPLAY=` in a TTY login does not make Shingle try to nest. `WAYLAND_SOCKET` is included because winit's Wayland connection honours it as libwayland does.
- **Native stub.** With native selected, `shingle` printed that the native backend is not built yet, why it was selected (for example "none of WAYLAND_DISPLAY, WAYLAND_SOCKET or DISPLAY is set") and what to use instead, and exited with status 1 before loading the configuration or starting any client. NB-6 replaced the stub with `shingle_native::run`; the "no seat" refusal keeps the reason and the advice ([NB-6 as built](#nb-6-as-built)).
- **Command line** (crates/shingle-bin/src/cli.rs): the union of the old binaries' options, `--backend`, `--headless`, `--config`, `--ipc` and `--spawn PROGRAM [ARG]...`, plus `-h`/`--help` and `--version`. Values may follow `=`. `--spawn` takes the rest of the command line. Repeated `--config` or `--ipc`, conflicting backends, unknown options, positional arguments and unknown backends are usage errors with exit status 2. Headless gained `--ipc` and `--spawn`, which only nested had.
- **`WAYFIRE_SOCKET`** is now exported before any client starts. Nested used to set it after `Frontend::new`, which had already launched the restored session, autostart and `--spawn`, so those clients did not see it; Wayfire's IPC plugin sets it before autostart. Since XW-1 the binary no longer sets it in its own environment: `Frontend::new` takes the IPC path and enables IPC first, and every child gets it from `Compositor::client_environment` ([xwayland.md](xwayland.md#xw-1-shingle-toplevel-handle-shell-neutral-toplevel-and-client-environment)).
- The startup line `shingle: using the nested backend (WAYLAND_DISPLAY is set)` goes to standard error; standard output still carries only `WAYLAND_DISPLAY=...`, which fixtures read.

#### NB-6 as built

Done on branch `shingle-native-session`: built, clippy-checked and unit-tested on this host; nothing in it has run on a seat yet. The acceptance items above are steps 1 to 7 of [native-testing.md](../native-testing.md). The API (crates/shingle-native/src):

```rust
// session.rs
pub enum SeatEvent { Enable, Disable }
pub trait Session {
  fn seat_name(&self) -> &str;
  fn open_device(&mut self, path: &Path) -> io::Result<OwnedFd>;
  fn close_device(&mut self, fd: OwnedFd) -> io::Result<()>;
  fn switch_vt(&mut self, vt: u32) -> io::Result<()>;
  fn dispatch(&mut self, timeout: Option<Duration>)
    -> io::Result<Vec<SeatEvent>>;
  fn acknowledge_disable(&mut self) -> io::Result<()>;
  fn poll_fd(&self) -> Option<BorrowedFd<'_>>;
}
pub struct LibSeatSession; // the real one
pub fn preflight(env) -> Result<(), NoSeat>;
pub fn wait_for_enable(session, timeout, now) -> Result<(), NoSeat>;
pub fn check_backend(backend: Option<&str>, allow_builtin: bool)
  -> Result<(), NoSeat>;

// input.rs
pub enum NativeEvent { Added(DeviceDescription), Removed(DeviceKey),
  Input { device: DeviceKey, time_usec: u64, input: DeviceInput } }
pub trait InputSource {
  fn dispatch(&mut self, output_size: (u32, u32))
    -> io::Result<Vec<NativeEvent>>;
  fn suspend(&mut self, output_size: (u32, u32)) -> Vec<NativeEvent>;
  fn resume(&mut self, output_size: (u32, u32))
    -> io::Result<Vec<NativeEvent>>;
  fn configure(&mut self, device: &DeviceKey, settings: &[Setting]);
  fn set_send_events(&mut self, device: &DeviceKey, mode: SendEvents);
  fn set_leds(&mut self, leds: Leds);
  fn poll_fd(&self) -> Option<BorrowedFd<'_>>;
}
pub struct LibinputSource; // the real one, input/libinput.rs
pub struct Translator; // DeviceInput -> InputEvent
pub fn device_settings(config, device) -> Vec<Setting>;

// udev.rs
pub struct DeviceRegistry; // libinput devices <-> seat devices

// host.rs
pub struct Backend<S: Session, I: InputSource>;
impl Backend {
  pub fn new(frontend, session: Rc<RefCell<S>>, input: I,
    config: InputConfig, output_size, renderer) -> Result<Self, ..>;
  pub fn step(&mut self) -> bool;
  pub fn run(self) -> Result<(), Box<dyn Error>>;
  pub fn finish(self);
}

// lib.rs
pub fn run(config, config_path, ipc_path, spawn, options: Options)
  -> Result<(), Box<dyn Error>>;
```

Elsewhere: `Frontend::set_session_active` and `Frontend::keyboard_leds` (crates/shingle-frontend/src/frontend.rs), `Keyboard::leds`, `Compositor::set_device_natural_scroll`, the new `[input]` options, and `--allow-builtin-seat` in crates/shingle-bin/src/cli.rs.

Where the code differs from the plan, and why:

- **Modules.** `session`, `udev`, `input` and `host` as planned, plus `diagnostics`. `udev` holds the registry of hotplugged input devices rather than a udev monitor of its own: input hotplug comes from libinput's udev backend, as [4](#4-input) decides, and the DRM monitor is NB-9's. There is no separate VT trait: `Session::switch_vt` is libseat's `switch_session`, and nothing calls a VT ioctl.
- **The SSH refusal comes first.** Before libseat is opened, `shingle-bin`'s `plan` calls `shingle_native::preflight`, which refuses a login with `SSH_CONNECTION`, `SSH_CLIENT` or `SSH_TTY` and no `XDG_SEAT` with "no seat: started over SSH (...); run from a VT login or start seatd", status 1. Waiting for libseat to fail is not enough: under seatd an SSH client in the seat group can be handed the seat of the active VT. libseat failing and the 5 s enable timeout still give "no seat" messages, as [6](#6-failure-handling) says.
- **Which libseat backend.** libseat has no call that names the backend it chose. The `libseat` crate passes libseat's log to the `log` crate, so Shingle installs a logger and reads libseat's own "Seat opened with backend 'NAME'" line. `builtin` and also `noop` (neither switches VT) need `--allow-builtin-seat`; when the line is missing, the backend counts as unknown and is allowed. The real check is step 8 of the test plan.
- **Devices survive a pause.** `libinput_suspend` removes every device and `libinput_resume` adds them again. The registry keeps the seat devices of a suspended device and gives them back to the device that reappears on the same event node with the same capabilities, so IPC ids and IPC-disabled state survive a VT switch; devices that do not come back are unregistered after the resume. wlroots recreates its devices, so in Wayfire both change.
- **Send-events per libinput device.** A libinput device with several capabilities is several seat devices; it is set to `DISABLED` only when IPC has disabled all of them, and the gate drops the kinds that are disabled. Enabling restores the configured mode, which for a touchpad with `disable_touchpad_while_mouse` is `DISABLED_ON_EXTERNAL_MOUSE`, where Wayfire sets `ENABLED`.
- **One xkb state per seat.** The frontend keeps one `Keyboard`, as NB-3 allowed. When IPC disables a keyboard, the backend releases the keys that device holds through the now disabled device, which updates the xkb state without dispatching anything; the registry tracks held keys per device and drops libinput's own releases of keys already released, so a disabled keyboard leaves no modifier behind when it is enabled again. A key held on two keyboards at once is released from the xkb state when either is disabled.
- **The pause is the frontend's too.** `Frontend::set_session_active` pauses the frontend's own state (key repeat, xkb, switcher, drags, rotation, Expo pointer, touch gestures) before calling `Compositor::set_session_active`, so nothing held before a switch continues after it. The order of [3](#3-session-and-seat) is kept without the KMS steps: suspend libinput, pause, acknowledge; on resume, resume libinput, unregister vanished devices, resume, resend the LEDs and draw. Device input in the events of a suspend or resume is dropped.
- **Natural scrolling** is libinput's, as in Wayfire: `mouse_natural_scroll` for pointers and `natural_scroll` for touchpads, where the device supports it. `Compositor::set_device_natural_scroll(true)` stops the compositor's own inversion, which nested keeps.
- **Device options** follow Wayfire's `update_options` order. An acceleration profile of `none` changes nothing, as libinput rejects `ACCEL_PROFILE_NONE` when Wayfire passes it; a click method of `none` is set through `input`'s raw bindings, which have no variant for it. Options are applied when a device is added and again when it returns after a pause, not on a config reload, which Wayfire does.
- **Pointer motion.** Relative motion moves the pointer by libinput's accelerated delta, kept inside the output, and is always passed on for relative-pointer clients. While a client constrains the pointer it does not move. Wheels pass libinput's value (15 per click) with whole clicks from v120 as steps, keeping fractions of high-resolution wheels; finger and continuous scrolls stop on a zero value.
- **STIPC devices.** Native registers none, so `input/list-devices` lists only libinput devices; STIPC input goes through the registered device of each kind with the lowest id, as NB-5 has it, and is dropped while there is none.
- **Drawing.** With no KMS, the output is `HEADLESS-1` at 1280x720 (`OUTPUT_SIZE`), drawn into an `OffscreenRenderer` when the frontend asks, at most once per refresh interval, which sends frame callbacks. The default cursor is on.
- **Diagnostics** from [6](#6-failure-handling) are in already: messages go to standard error and `$XDG_STATE_HOME/shingle/native.log`, and a panic hook logs panics, because the seat daemon puts the VT into graphics mode in NB-6 too.
- **The loop** polls the seat, libinput and compositor descriptors with a 16 ms tick, like nested, and the next key repeat or frame.

Unit tests cover selection and the refusal (crates/shingle-bin/src/main.rs), the startup decisions (session.rs), event translation and device settings (input.rs), the registry with hotplug and pauses (udev.rs), and in crates/shingle-native/src/host/tests.rs the backend with a fake seat and fake libinput around a real frontend, compositor, IPC socket and Wayland client: libinput timestamps in the client's key, motion, button, axis and touch events, `input/list-devices` following hotplug, configure-device setting the send-events mode and releasing held keys from client and xkb state, Ctrl+Alt+F2 asking the seat to switch without the client seeing F2, a VT switch releasing held keys and buttons and the switch back keeping ids and settings, vanished and disabled devices across a switch, LEDs and SIGTERM saving the session and letting go of the seat. What only hardware can show is listed in [status.md](../status.md#native-backend-seat-and-input).

### NB-7 `shingle-native-kms`: atomic KMS with copy scanout

Add `kms/` (`KmsDevice` trait, property cache, `in_formats`, `modes`, atomic builder), `scanout/copy.rs`, `pacing.rs`, the GPU-selection rule, DPMS, presentation feedback from page flips, the startup order and the failure table of [6](#6-failure-handling).

Acceptance (test machine):

- A client is visible on the test machine's display, checked with `grim` through screencopy.
- `wp_presentation` timestamps match page-flip times within 1 ms.
- Frames are paced by flips: no frame callback before the previous flip completes.
- DPMS off and on works through `wlr-output-power-management`.
- A VT switch away and back resumes with a fresh modeset.
- `kill -9` leaves a usable tty1.
- Every row of the failure table that can be produced on the test machine is exercised and recorded.

This list first said "virtio-gpu display", from when VMs were planned for testing; the operator decided on real hardware instead ([Decisions](#decisions)), so the test machine's own monitor is the display.

#### NB-7 as built

Done on branch `shingle-native-kms`: built, clippy-checked and unit-tested on this host through a fake card (`kms::fake::FakeKms`), the fake seat and a fake renderer; nothing in it has opened a DRM device yet. The acceptance items are steps 10 to 15 of [native-testing.md](../native-testing.md). The API (crates/shingle-native/src):

```rust
// kms.rs: plain ids (ConnectorId, CrtcId, PlaneId, PropertyId, BlobId,
// FramebufferId, BufferId), Mode, ConnectorInfo, CrtcInfo, PlaneInfo,
// Property, DumbFramebuffer, PageFlip, CommitFlags
pub trait KmsDevice {
  fn enable_atomic(&mut self) -> io::Result<()>;
  fn monotonic_timestamps(&mut self) -> bool;
  fn resources(&mut self) -> io::Result<Resources>;
  fn connector(&mut self, id: ConnectorId) -> io::Result<ConnectorInfo>;
  fn crtc(&mut self, id: CrtcId) -> io::Result<CrtcInfo>;
  fn plane(&mut self, id: PlaneId) -> io::Result<PlaneInfo>;
  fn properties(&mut self, object: Object) -> io::Result<Vec<Property>>;
  fn blob(&mut self, id: u64) -> io::Result<Vec<u8>>;
  fn create_mode_blob(&mut self, mode: &Mode) -> io::Result<BlobId>;
  fn destroy_blob(&mut self, blob: BlobId) -> io::Result<()>;
  fn commit(&mut self, request: &AtomicRequest, flags: CommitFlags)
    -> io::Result<()>;
  fn create_dumb_framebuffer(&mut self, width: u32, height: u32)
    -> io::Result<DumbFramebuffer>;
  fn write_dumb_framebuffer(&mut self, buffer: &DumbFramebuffer,
    write: &mut dyn FnMut(&mut [u8])) -> io::Result<()>;
  fn destroy_dumb_framebuffer(&mut self, buffer: &DumbFramebuffer)
    -> io::Result<()>;
  fn read_events(&mut self) -> io::Result<Vec<PageFlip>>;
  fn poll_fd(&self) -> Option<BorrowedFd<'_>>;
}
pub struct DrmDevice; // kms/drm.rs, the real one over drm 0.15
// kms/props.rs: PropertyTable, ConnectorProps, CrtcProps, PlaneProps
// kms/in_formats.rs: parse(blob) -> Result<InFormats, InFormatsError>
// kms/modes.rs: ModeChoice, select_default_mode, find_matching_mode,
//   candidates(modes, choice, current) -> Vec<Mode>
// kms/atomic.rs: AtomicRequest, modeset, flip, set_active
// kms/output.rs: probe(device) -> Card, Card::choose_connector,
//   Card::output -> Output { props, teardown, current_mode, in_formats }

// scanout.rs, scanout/copy.rs
pub struct Frame { pub width: u32, pub height: u32, pub rgba: Vec<u8> }
pub trait Scanout {
  fn size(&self) -> (u32, u32);
  fn current(&self) -> FramebufferId;
  fn prepare(&mut self, device: &mut dyn KmsDevice, frame: &Frame)
    -> io::Result<FramebufferId>;
  fn presented(&mut self);
  fn zero_copy(&self) -> bool;
  fn destroy(&mut self, device: &mut dyn KmsDevice);
}
pub struct CopyScanout;

// pacing.rs
pub enum State { Idle, Rendering, FlipPending { since: Instant }, Paused }
pub struct FrameScheduler; // begin_frame, committed, abandon, page_flip,
                           // pause, resume, deadline, check_timeout

// gpu.rs
pub fn enumerate_cards(root: &Path) -> io::Result<Vec<Card>>;
pub fn card_order(cards: &[Card], forced: Option<&OsStr>) -> Vec<Card>;
pub fn adapter_matches(bus_id: &str, slot: &str) -> bool;
pub trait FrameRenderer {
  fn draw(&mut self, frontend: &mut Frontend, host: &mut dyn Host)
    -> Result<Frame, RenderFailure>;
  fn resize(&mut self, size: (u32, u32)) -> Result<(), RenderFailure>;
}
pub struct GpuRenderer; // the real one
pub struct GpuHealth;   // Skip, Recreate or GiveUp after a failure

// display.rs
pub struct Display; // prepare, start, begin_frame, present, dispatch,
                    // check_timeout, pause, resume, set_dpms, output_info
pub enum StartError; // exit status 1, before the socket
pub struct Fatal(pub String); // exit status 2, after saving

// host.rs
pub struct Rendering { pub renderer: Box<dyn FrameRenderer>,
  pub factory: RendererFactory }
Backend::new(frontend, session, input, config, display: Display,
  rendering: Rendering) -> Result<Self, Box<dyn Error>>;

// lib.rs
pub fn open_display(session: &mut dyn Session, cards: &[Card],
  open: &mut dyn FnMut(OwnedFd) -> io::Result<Box<dyn KmsDevice>>)
  -> Result<Prepared, StartError>;
pub fn startup(display, rendering, frontend) -> Result<..>;
```

Elsewhere: `Compositor::dpms_state` (crates/shingle/src/lib.rs), `Frontend::drop_gpu_resources` (crates/shingle-frontend/src/frontend.rs), `OffscreenTarget::read_rgba_within` and `RenderError::Timeout` (crates/shingle-render), exit status 2 for `Fatal` in crates/shingle-bin/src/main.rs, `SHINGLE_TRACE_FLIPS`, the presentation probe (crates/shingle-native/examples/present_probe.rs) and tools/native_test/present_check.py.

Where the code differs from the plan, and why:

- **Startup order.** `shingle_native::run` opens the seat, then `startup` runs three steps in order: the display (`open_display`: open the card through the seat, `enable_atomic`, probe, choose the output and pass a `TEST_ONLY` modeset), the renderer, and only then libinput and `Frontend::new`, which creates the socket and starts clients. `Backend::new` then describes the output with `set_output_info` before the first dispatch and makes the real modeset. A unit test checks that nothing but the passed test was committed when the frontend step runs, and that a failing display or renderer never reaches it.
- **Cards come from sysfs, not udev.** `gpu::enumerate_cards` reads `cardN` entries, `device/boot_vga` and `PCI_SLOT_NAME` from `device/uevent` under `/sys/class/drm`. That is the data udev's enumeration would return, without a libudev context, and the reader is unit-tested on a temporary directory tree. The DRM hotplug monitor of NB-9 still uses udev. When the seat refuses a card, the next one in order is tried; `SHINGLE_DRM_DEVICE` names the only card tried.
- **No `acquire_master_lock`.** libseat hands out the card already as DRM master. "No DRM master" is detected where it would show: `EACCES` or `EBUSY` from the first `TEST_ONLY` commit stops the mode search at once with "cannot become DRM master". After startup, a flip refused with `EACCES` or `EPERM` pauses the display until the seat's next resume.
- **Mode choice.** NB-7 always uses `ModeChoice::Auto`. `OutputConfig`'s `width` and `height` are nested's window size and default to 1280x720, so taking them as a mode would be wrong; reading `[output:NAME] mode` is NB-9's. `kms/modes.rs` already implements `highres`, `highrr` and `WxH@R` as Wayfire does and is tested with mode lists. The fallback order is the chosen mode, the preferred mode, every mode by area (faster first at equal size), then the CRTC's current mode.
- **The modeset turns off the rest.** Other connectors on a CRTC get `CRTC_ID=0`, other active CRTCs `ACTIVE=0` and `MODE_ID=0`, and every plane on a CRTC other than ours, including our CRTC's cursor and overlay planes, `FB_ID=0` and `CRTC_ID=0`. `VRR_ENABLED` is set to 0 where the CRTC has it, so a VRR left on by another session does not stay on; NB-10 sets it from the configuration.
- **Copy path details.** The renderer draws into the existing RGBA `OffscreenTarget` and reads it back with `read_rgba_within(100 ms)`; the copy into the XRGB8888 dumb buffer swaps red and blue, so no BGRA target was added. Dumb buffers are mapped for each copy instead of staying mapped, since `drm`'s mapping borrows the buffer. `Scanout::prepare` takes a CPU `Frame`; NB-8's zero-copy path renders into the buffer itself and will change that method (it did: [NB-8 as built](#nb-8-as-built)).
- **Frames.** A frame starts when the frontend asked for one and the scheduler is idle, so there is no fixed interval any more: the next frame waits for the flip. A commit is a flip that sets only the primary plane's `FB_ID`, with `NONBLOCK | PAGE_FLIP_EVENT`. The frontend's `RenderTarget::render` returns false for the KMS target, so the frontend does not call `Compositor::frame`; the page-flip event calls `frame_presented` with the flip's time, its vblank counter extended to 64 bits, the mode's refresh and `VSYNC | HW_CLOCK | HW_COMPLETION`. The compositor sends every pending callback of a visible surface on a flip, also for content committed after the frame was drawn; it waits only for the flip. If the driver's timestamps are not `CLOCK_MONOTONIC`, the compositor's clock is used and `HW_CLOCK` dropped.
- **DPMS.** The backend follows `Compositor::dpms_state` (new; it covers wlr-output-power-management, the idle timeouts and activity) rather than `Event::DpmsChanged`, which the frontend consumes and which is reported twice. Off is a commit of only `ACTIVE=0`, keeping mode and planes; on is a full modeset with `ACTIVE=1`. Both are tested with `TEST_ONLY` first and wait until a pending flip completed; a change while paused is applied by the resume's modeset.
- **Page-flip timeout.** After 1 s the pending flip is dropped and a `TEST_ONLY` plus blocking `ALLOW_MODESET` commit shows the current buffer again; a failed recovery lets the next frame try a flip. The third timeout without a completed flip in between ends the loop with `Fatal`.
- **GPU loss.** `GpuHealth` skips a frame that missed the 100 ms wait (and draws it again), treats three in a row or a lost device (wgpu's device-lost callback, or a readback error after it fired) as a loss, and gives up at the third loss within 60 s. A loss drops the renderer, calls `Frontend::drop_gpu_resources` (surface textures, geometry animations' old frames, Scale titles and icons) and creates a new renderer from the factory; the copy scanout's dumb buffers belong to the card, not to the Vulkan device, so they are kept. A renderer that cannot be created counts as another loss at the next frame.
- **No connected output.** The display starts without a lit output, nothing is committed and nothing is drawn; the compositor keeps the default `HEADLESS-1` at 1280x720 and clients run, but get no frame callbacks. NB-7 has no DRM hotplug monitor, so an output is picked up by the next resume's probe, after a VT switch; the monitor is NB-9's.
- **Resume.** The card is probed again. The lit connector stays if it is still connected and still offers the mode, with its properties and the teardown read afresh; otherwise the output is chosen again with new buffers and blob, and the frontend, renderer and input mapping get the new size. A different output gets `set_output_info`, which clients that bound the old `wl_output` do not see until NB-9 recreates the globals.
- **Output description.** The connector name, physical size, subpixel layout and the mode's refresh. Make and model are `Unknown`, as wlroots reports without EDID; EDID parsing is NB-9's.
- **Exit statuses.** `StartError` (no card, no atomic, no master, probe error, no usable CRTC or plane, every mode rejected, no Vulkan adapter) is status 1 before the socket. `Fatal` (three flip timeouts, three GPU losses in 60 s) is status 2, after the loop saved the session; `shingle-bin` maps it.
- **Tracing.** `SHINGLE_TRACE_FLIPS=1` logs every presented flip, so the test machine can compare presentation feedback with the flips (step 11 of the test plan).

Unit tests cover the atomic request contents for modeset (with and without VRR, with teardown, display off), flip and DPMS; the `IN_FORMATS` parser on kernel-layout blobs, including bad ones; Wayfire's mode heuristics and the fallback order; the choice of connector, CRTC and primary plane, refused planes and connectors, and teardown; the copy into padded XRGB8888 rows and the double buffering; the scheduler's states, timeouts and their count; card enumeration on a temporary sysfs tree, the boot VGA order and `SHINGLE_DRM_DEVICE`; adapter matching; the GPU health rules; the display on a fake card (`TEST_ONLY` before anything real, mode fallback, every mode rejected, `EACCES` and `EBUSY` as no master, no atomic, no connector, the blocking first modeset, flips to the back buffer with exact `PresentInfo`, sequence wrap, timeout recovery and the exit after three, a flip refused for lack of master, a pause with a flip pending and the fresh modeset on resume, teardown on resume, an output unplugged and plugged while away, a new mode list, DPMS off and on, DPMS waiting for a flip and DPMS changed while paused); and, in crates/shingle-native/src/host/tests.rs, the backend with a real frontend, compositor and Wayland client: the startup commit sequence, the connector as the client's `wl_output` name, frame callbacks and `wp_presentation` feedback with the flip's time, sequence, refresh and flags only after the flip, frames waiting for the previous flip, wlr-output-power-management turning the CRTC off and on, a VT switch with a flip pending, a lost GPU replaced, repeated GPU timeouts, the exit after a third loss with the session saved, and no output at startup with the output lit after a resume. What only hardware can show is listed in [status.md](../status.md#native-backend-kms-display).

### NB-8 `shingle-native-zero-copy`: GBM scanout

Add `scanout/gbm.rs`: modifier negotiation (KMS `IN_FORMATS` ∩ Vulkan ∩ single-plane), `texture_from_dmabuf_fd` import, the `PlanarBuffer` wrapper and the automatic fallback to copy.

The Vulkan side (device extensions, the render-modifier query and the import call with its queue-family ownership transfer) is shared with client DMA-BUF import and is built first, as DB-1 of [dmabuf.md](dmabuf.md#db-1-shingle-gpu-dmabuf-vulkan-external-memory-and-modifier-setup); NB-8 then needs NB-7 and DB-1, and `Scanout::prepare` stops taking a CPU `Frame` ([dmabuf.md](dmabuf.md#nb-8-shingle-native-zero-copy-gbm-scanout-revised)). Note that in wgpu 30.0.2 `Device::create_texture_from_hal` takes a third argument, the texture's `initial_state` (wgpu-30.0.2/src/api/device.rs:341).

Acceptance:

- Unit tests for negotiation.
- Forcing the fallback (for example with an environment override) selects the copy path with a logged reason.
- On the test machine, zero-copy is selected on at least one of i915, amdgpu or nvidia-drm, and `wp_presentation` reports `ZERO_COPY`.

#### NB-8 as built

Done on branch `shingle-native-zero-copy`: built, clippy-checked and unit-tested on this host through the fake card, its fake GBM and a fake renderer; it has never allocated a GBM buffer or made a framebuffer of one. The hardware acceptance item is steps 36 to 42 of [native-testing.md](../native-testing.md). The API (crates/shingle-native/src):

```rust
// scanout.rs
pub struct ScanoutDmabuf { pub serial: u64, pub fd: OwnedFd, pub width,
  pub height, pub fourcc, pub modifier, pub stride, pub offset }
pub enum DrawTarget<'a> { Readback, Dmabuf(&'a ScanoutDmabuf) }
pub enum Drawn { Pixels(Frame), InPlace }
pub trait Scanout { fn size(&self) -> (u32, u32);
  fn current(&self) -> FramebufferId;
  fn target(&self) -> DrawTarget<'_>;
  fn prepare(&mut self, device: &mut dyn KmsDevice, drawn: &Drawn)
    -> io::Result<FramebufferId>;
  fn presented(&mut self); fn zero_copy(&self) -> bool;
  fn destroy(&mut self, device: &mut dyn KmsDevice); }
pub trait ScanoutImporter {
  fn render_modifiers(&self, fourcc: u32, size: (u32, u32))
    -> Result<Vec<u64>, String>;
  fn prepare_buffers(&mut self, output: &str,
    buffers: &[&ScanoutDmabuf]) -> Result<(), String>; }
pub enum ScanoutChoice { Auto, Copy }  // ScanoutChoice::from_env
pub enum ScanoutStatus { Untried, ZeroCopy(u64), Copy(String) }

// scanout/allocator.rs
pub trait BufferAllocator {
  fn plane_count(&self, fourcc: u32, modifier: u64) -> Option<u32>;
  fn allocate(&mut self, width, height, fourcc, modifiers: &[u64])
    -> io::Result<AllocatedBuffer>; }
pub struct GbmAllocator; // the real one, over the gbm crate

// scanout/gbm.rs
pub fn negotiate(plane: &[u64], render: &[u64],
  plane_count: &dyn Fn(u64) -> Option<u32>) -> Vec<u64>;
pub struct GbmScanout; // GbmScanout::create(device, allocator,
                       //   importer, output, in_formats, size, serial)

// kms.rs: KmsDevice gains
fn add_framebuffer(&mut self, buffer: &PlanarFramebuffer)
  -> io::Result<FramebufferId>;
fn remove_framebuffer(&mut self, framebuffer: FramebufferId)
  -> io::Result<()>;
fn buffer_allocator(&mut self) -> io::Result<Box<dyn BufferAllocator>>;

// display.rs
Display::set_scanout_choice(&mut self, ScanoutChoice);
Display::upgrade_scanout(&mut self, name: &str,
  importer: &mut dyn ScanoutImporter);
Display::draw_target(&self, name: &str) -> DrawTarget<'_>;
Display::scanout_failed(&mut self, name: &str, reason: &str);
Display::scanout_status(&self, name: &str) -> Option<ScanoutStatus>;
Display::present_on(&mut self, name: &str, drawn: &Drawn, now: Instant);

// gpu.rs
pub trait FrameRenderer: ScanoutImporter { fn draw(&mut self,
  frontend: &mut Frontend, host: &mut dyn Host, output: &OutputHandle,
  size: (u32, u32), target: DrawTarget<'_>)
  -> Result<Drawn, RenderFailure>; ... }
RenderFailure::Scanout(String);
```

Where the code differs from the plan, and why:

- **Zero-copy comes after the first modeset, per output, through a tested blocking flip.** Section 2 has the native crate try zero-copy first and fall back when "the first `TEST_ONLY` commit fails", which reads as choosing the scanout before startup's test. But the negotiation needs the renderer's render modifiers and the import needs its device, and the renderer is created after the display passed its test, so that a display that cannot work fails before anything else ([6](#6-failure-handling)); and every later modeset with fresh buffers (hotplug, reloaded modes, output management, a resume that relights) happens inside `Display`, which does not own the renderer. So every output is lit as NB-7 lit it, with two black dumb buffers. Before its next frame, once it is on, the seat is enabled and no flip of it is pending, the host calls `Display::upgrade_scanout` with the renderer: the negotiation, three GBM buffers, a framebuffer of each, and the renderer importing them with `DmabufUse::Render` and clearing them to black; then a `TEST_ONLY` commit of the primary plane's `FB_ID` alone with a GBM framebuffer, which is the test the design names, and the same commit blocking, after which the dumb buffers are off screen and freed. A new mode starts on copy buffers again and is tried again. The first frame of a zero-copy output is therefore always black, as with copy scanout.
- **No `IN_FORMATS`, no zero-copy.** wlroots assumes `LINEAR` and the implicit modifier for a plane without `IN_FORMATS`; Shingle keeps the design's intersection literally, so such a plane, which current kernels give only to drivers without modifier support, gets copy scanout. `DRM_FORMAT_MOD_INVALID` is never offered, since Vulkan imports explicit modifiers only.
- **Render modifiers allow the mode's size.** The renderer's `render_modifiers` leaves out modifiers whose `maxExtent` is smaller than the mode, so that GBM is never offered a modifier the import would then refuse.
- **The adapter must be on the card's GPU.** Section 2 lets the copy path render on any adapter when none is on the card's PCI slot. Zero-copy is refused then ("the renderer's adapter is not the card's GPU (PCI ...)"), because a GBM buffer of the card would be imported into another GPU; a card without a PCI slot (a platform display controller) still tries, since its render GPU is another device by nature, as wlroots handles it.
- **The fallback also works after the start.** Beyond the design's list (empty intersection, GBM or import error, refused `TEST_ONLY`, `SHINGLE_SCANOUT=copy`), a zero-copy output falls back to copy buffers, by the same tested blocking flip, when a renderer cannot import its buffers any more (`RenderFailure::Scanout`, as after a GPU loss onto a device that cannot import) or when the card refuses a flip to one of them. A GPU loss alone keeps zero-copy: the GBM buffers belong to the card, and the new renderer imports them again on its first frame. A commit refused for lack of DRM master changes nothing and is tried again.
- **Framebuffers through `drm-ffi`.** `drm`'s `add_planar_framebuffer` asserts that the modifier is valid; `DrmDevice::add_framebuffer` calls `drm_ffi::mode::add_fb2` with `DRM_MODE_FB_MODIFIERS` itself and refuses the invalid modifier with an error, so no `PlanarBuffer` wrapper was needed. The GBM device is made on a duplicate of the card's descriptor, which shares the card's open file, so GEM handles of its buffers are valid for the card's framebuffers; each buffer keeps the device alive.
- **Three buffers, least recently shown first.** With one flip in flight two would do; the third is the design's, and the buffer drawn next is the one that left the screen longest ago.
- **Synchronisation** is the design's: the frame is drawn between the foreign acquire and release barriers (`draw_with_access`), and the renderer waits at most 100 ms for that submission (`PollType::Wait`) before the flip is committed. The renderer keeps no offscreen target for a zero-copy output; screencopy still draws its capture separately (`capture_frame`).
- **Logs.** The line of a lit output is `output NAME: WxH at R Hz`, without the scanout, which follows as `output NAME: zero-copy scanout, modifier M` or `output NAME: copy scanout, because REASON`; a fallback logs `output NAME: back to copy scanout, because REASON`. `rendering on ADAPTER` no longer says that zero-copy is not built. `Display::scanout_status` keeps the reason for tests. `SHINGLE_SCANOUT` other than `copy` or `auto` is logged and treated as `auto`.

Unit tests cover the negotiation (the plane's order kept, metadata planes, modifiers unknown to GBM and the implicit modifier left out), its reasons, the GBM scanout's allocation, framebuffers, imports, buffer rotation and freeing, and every failure freeing what it made (crates/shingle-native/src/scanout/gbm.rs); `SHINGLE_SCANOUT` parsing (scanout.rs); the display through the fake card (crates/shingle-native/src/display/zero_copy_tests.rs: the tested blocking flip with only `FB_ID`, the dumb buffers freed, frames presented with `ZERO_COPY`, `SHINGLE_SCANOUT=copy`, no GBM, a renderer without a GPU, no shared modifier, no `IN_FORMATS`, a refused test, a test refused for lack of master tried again, waiting for a pending flip, a pause and DPMS off, resume and DPMS modesets showing a GBM buffer, the fallback when the renderer cannot draw and when a flip is refused, a new mode tried again at its size, and a refused mode change keeping zero-copy); and the host with a real compositor and client (host/tests.rs: frames drawn into the GBM buffers after the tested blocking flip and presented to the client with `ZERO_COPY`, `SHINGLE_SCANOUT=copy` reading frames back without it, the fallback after a renderer cannot draw into the buffers, and a GPU loss drawing on into the same buffers). What only hardware can show is listed in [status.md](../status.md#native-backend-zero-copy-scanout).

### NB-9 `shingle-native-outputs`: hotplug and mode configuration

Add udev DRM hotplug, connector diffing, switching the enabled output, headless waiting with no connector, EDID make/model/serial, `OutputConfig` gaining `name`, `mode` (auto/off/highres/highrr/explicit with refresh) and `vrr`, the importer handling `@refresh`, and `TEST_ONLY` for output-management `apply`. From NB-2 it also takes the parts of output description that were deferred: the mode list in `OutputInfo` for wlr-output-management, renaming the core output to the connector name, and recreating the `wl_output` global when the enabled output changes, since a bound output cannot be renamed.

Acceptance:

- Mode-selection tests cover Wayfire's heuristics.
- On the test machine, unplugging and replugging a monitor triggers a re-probe and a new mode.
- Disconnecting all outputs keeps Shingle and its clients alive, and reconnecting restores the picture with the tiling tree intact.
- `wlr-randr` (or an equivalent client) can change the mode.

#### NB-9 as built

Done on branch `shingle-native-outputs`: built, clippy-checked and unit-tested on this host through the fake card and a fake hotplug monitor; nothing in it has received a udev event or read a real EDID yet. The device-specific acceptance items are steps 16 to 22 of [native-testing.md](../native-testing.md); the parts that are not device-specific were checked with `wlr-randr` in headless and nested Shingle ([status.md](../status.md#with-real-clients)). The API:

```rust
// shingle-config: output.rs
pub enum OutputMode { Auto, Off, HighRes, HighRr,
  Explicit { width: u32, height: u32, refresh_mhz: u32 } }
pub struct OutputConfig { pub name: Option<String>, pub mode: OutputMode,
  pub x, pub y, pub width, pub height, pub scale, pub transform,
  pub vrr: bool }
ShingleConfig::outputs: Vec<OutputConfig>; // one per [output:NAME]
ShingleConfig::output_config(&self, name: &str) -> &OutputConfig;

// shingle-core
Shingle::rename_output(&mut self, output: &OutputId, to: OutputId);

// shingle: output.rs
pub struct OutputModeInfo { pub width, pub height, pub refresh_mhz,
  pub preferred: bool }
OutputInfo::modes: Vec<OutputModeInfo>; OutputInfo::current_mode;
pub struct OutputPlacement { x, y, width, height, scale, transform,
  refresh_mhz }
Config::output_placements: BTreeMap<String, OutputPlacement>;
Config::output_refresh_mhz: u32;
pub enum RequestedMode { Listed(usize),
  Custom { width, height, refresh_mhz } }
pub struct OutputRequest { pub id: OutputRequestId,
  pub mode: RequestedMode, pub test_only: bool }
Compositor::take_output_requests(&mut self) -> Vec<OutputRequest>;
Compositor::finish_output_request(&mut self, OutputRequestId, ok: bool);

// shingle-frontend
Frontend::take_reloaded_config(&mut self) -> Option<ShingleConfig>;

// shingle-native
pub trait DeviceMonitor { fn dispatch(&mut self) -> Vec<DrmEvent>;
  fn poll_fd(&self) -> Option<BorrowedFd<'_>>; }    // hotplug.rs
pub struct UdevMonitor;                             // the real one
pub fn classify(action, sysname, card, hotplug, connector)
  -> Option<DrmEvent>;
pub struct OutputPolicy;                            // outputs.rs
pub fn choose(card, policy, keep) -> Option<&ConnectorInfo>;
pub fn diff(before, after) -> Vec<ConnectorChange>;
pub fn requested_mode(modes, RequestedMode) -> Option<Mode>;
pub struct Edid { make, model, serial }             // kms/edid.rs
Display::prepare(device, policy: OutputPolicy);
Display::hotplug(&mut self) -> Resumed;
Display::set_policy(&mut self, OutputPolicy) -> Resumed;
Display::test_mode / set_mode(&mut self, RequestedMode);
Backend::set_device_monitor(&mut self, Box<dyn DeviceMonitor>);
```

Where the code differs from the plan, and why:

- **No `kms::Outputs` map.** [Section 5](#5-outputs) planned a map of every connector keyed by name with an `enabled` list. NB-7 built `Display` with one lit output (`Lit`) instead, and NB-9 keeps that: the display remembers every connector's name, connection and modes from the last probe (`outputs::ConnectorState`) only to diff them, and probes the card again whenever something changes. A per-connector map pays off when more than one output is lit, which is the multi-output item's work; until then it would hold state that a probe refreshes anyway.
- **Choosing the output.** The output that is lit stays while it is connected and not configured `off`. Otherwise the first connected connector named in `outputs` (in config order) wins, then an internal panel, then the first connected connector, skipping any configured `off`. Keeping the lit output avoids switching screens when a second monitor is plugged in, since only one is lit.
- **Mode configuration.** `OutputConfig` has `mode` (parsed exactly as wf-config parses Wayfire's option: `auto` or `default`, `off`, `highres`, `highrr`, `WxH` and `WxH@R` with `R` in hertz below 1000 and in millihertz otherwise, and nothing after the refresh) instead of a separate `refresh_mhz`. `ShingleConfig::output` is the default for every output, and the new `outputs` list holds one entry per `[output:NAME]` section, which fixes the importer bug the roadmap's multi-output item noted (every section wrote into one `OutputConfig`). Wayfire's `mirror` imports as `off`, since Shingle lights one output. The i3 importer reads sway's `output NAME mode WxH@RHz` with decimal hertz, `disable` and `adaptive_sync`. `vrr` is read and stored; NB-10 applies it.
- **Wayfire's heuristics.** `kms/modes.rs` from NB-7 already followed Wayfire's `select_default_mode` and `find_matching_mode` (wayfire/src/core/output-layout.cpp, checked again against `b36615a7`): `auto` the first preferred mode, `highres` the first mode of the largest area, `highrr` the first largest mode among those of the highest refresh, else the last mode; an explicit mode of a size the card does not list falls back to `auto`, as Wayfire's `is_mode_supported` does for DRM. NB-9 feeds it the configured mode.
- **wlr-output-management.** Heads list every mode of the connector, with the preferred one marked and the current one as `current_mode`; a new mode list finishes the old mode objects and announces new ones. With a mode list, a mode change becomes an `OutputRequest` that the native backend tests with a `TEST_ONLY` modeset (with fresh buffers and blob) and, for `apply`, sets with a blocking modeset after another test; the client gets `succeeded` or `failed` only then. Position, scale and transform are the compositor's and are applied when the backend reports success. A mode change waits while a page flip is pending, because the kernel refuses a modeset then; a test does not wait. A custom mode must match a listed mode (with no refresh, the preferred or fastest one of that size): Wayfire would try a custom mode, which wlroots turns into a generated CVT mode, but Shingle does not generate modes. `disable_head` on the only output fails, as before; DPMS turns the display off.
- **Renaming the output.** `set_output_info` with a new name renames the core output (`Shingle::rename_output`), so windows keep their tiling trees and workspaces and sessions are saved under the connector name, and replaces the `wl_output` global: clients see `global_remove` and a new global, every surface gets `leave` for the old output, foreign toplevels `output_leave`, and xdg-outputs, power controls (`failed`), gamma controls (`failed`), output-management heads (`finished`) and workspace groups (rebuilt with new ids) of the old output end. The old global stays bindable for 5 s, as wlroots' `wlr_global_destroy_safe` keeps it, and an output bound from it is described once but not tracked, so a client racing the removal gets no protocol error. The configured placement for the new name (position, scale, transform from `Config::output_placements`) is applied around the mode's size.
- **Sessions across names.** A session restored with outputs of other names, such as a native session restored in nested, has those outputs merged into the compositor's one output when it starts, so no window is lost on an output that is not shown. `rename_output` merges the same way when the new name exists already.
- **Hotplug.** `UdevMonitor` follows wlroots' session (backend/session/session.c, `handle_udev_event`): a `change` event of the card with `HOTPLUG=1` means probe again (the `CONNECTOR` id is read but not used to narrow the probe), and `remove` is logged. Hotplugs and reloaded output modes wait for a pending flip like mode changes; a flip that never completes times out within a second. While the seat is disabled, events are read and dropped, since the resume probes anyway. A monitor that cannot be created is a warning, not a refusal; outputs are then probed only on resume, as in NB-7.
- **No output.** With every monitor unplugged, the display lights nothing and draws nothing; the compositor keeps the last output description and size, so windows keep their places and clients run (they get no frame callbacks). When a monitor returns, it is lit and described, which renames the output if it is another connector.
- **EDID.** `kms/edid.rs` reads make, model and serial as libdisplay-info does for wlroots (`di_info_get_make`, `_model`, `_serial`): the PNP id named through hwdata's `pnp.ids`, which libdisplay-info compiles in and Shingle reads at runtime (falling back to the id), the product name descriptor or the product code as `0x%04X`, and the serial descriptor or a non-zero serial number as `0x%08X`. Without an EDID the output stays `Unknown`, as in wlroots.
- **Config reload.** `Frontend::take_reloaded_config` hands the reloaded config to the host; native applies its output modes (`Display::set_policy`), relighting only when the lit output's mode, or the output, changes. `apply_config` keeps the size of a backend that lists modes and places it as configured; nested sizes its output from its window again after a reload.
- **Nested and headless.** Where it makes sense without a card: an explicit configured mode (physical pixels, divided by the scale) sizes the headless output and opens the nested window, and its refresh rate is the one clients are told; a named `HEADLESS-1` entry replaces the default; wlr-output-management custom modes resize the output and set the refresh rate, `--dryrun` tests change nothing, and scale and transform changes keep the physical size, as in Wayfire (they used to keep the logical size). `Frontend::set_output_size` keeps the output's position, which it used to reset to 0,0.

Unit tests cover: the mode parser against wf-config's rules, printing modes back, sway modes, YAML round trips of named outputs and their validation, and the Wayfire and i3 importers keeping each output apart (crates/shingle-config/src/output.rs); renaming and merging outputs in the core; in `shingle` with the in-process client (crates/shingle/src/output/tests.rs), the old global removed and a new one announced, the old head finished and a new one named, the window's tree and location under the new name, the new global naming the connector, an inert late bind and the retired global's removal after 5 s, the placement of a new name and its reload, headless sizes and refresh rates from the config, the mode list with its preferred and current modes and its replacement, the rectangle mode without a list, mode changes waiting for the backend (a refused test, an applied change answered after it, custom modes), local custom modes, refusing `disable_head` and a restored session joining the one output; EDID parsing; the policy, connector choice, diffing and requested modes (crates/shingle-native/src/outputs.rs); udev event classification; the display on the fake card (configured, `off` and `highres` modes, a hotplug that changes nothing and makes no commit, unplugging the lit output lighting the next one and replugging keeping it, every output unplugged and back with a stale flip ignored, a replugged monitor with other modes, hotplugs while paused, a reloaded policy, mode requests tested before they are made and a rejected one keeping the old mode, the EDID and mode list in the output description); and, in crates/shingle-native/src/host/tests.rs, a monitor plugged in later lit without a VT switch with the client seeing the connector after rebinding, unplugging the lit monitor moving to the next with the tiling tree intact, every monitor unplugged with clients alive and the picture and frame callbacks back on replug, a hotplug during a pending flip waiting for it, and wlr-output-management mode changes tested and set through the backend with `TEST_ONLY` first. What only hardware can show is listed in [status.md](../status.md#native-backend-outputs).

### NB-10 `shingle-native-planes`: cursor plane, VRR, tearing and fences

Add the hardware cursor plane with software fallback, `VRR_ENABLED` from config, `PAGE_FLIP_ASYNC` for `wp_tearing_control` when supported, and optionally a sync_file `IN_FENCE_FD` from an exported Vulkan semaphore instead of the blocking wait.

Acceptance:

- Unit tests for the atomic requests.
- On the test machine: the cursor moves without full-frame renders (frame counter); VRR toggles `VRR_ENABLED` and the monitor reports it; a tearing client gets async flips.
- Each feature falls back cleanly where unsupported, verified by forcing each fallback on the test machine.

NB-1, NB-2 and NB-5 do not depend on each other and can run in parallel. NB-3 needs NB-1 (render types in the scene), and NB-4 needs NB-2 and NB-3. NB-6 needs NB-4 and NB-5, and NB-7 needs NB-6. NB-8 and NB-9 both need NB-7 but not each other. NB-10 comes last. The DMA-BUF roadmap item (client buffer import) was first planned to start after NB-8, reusing its Vulkan feature setup; its design ([dmabuf.md](dmabuf.md#11-implementation-plan)) turns that round, so the shared setup is its DB-1 and NB-8 builds on it. Multi-output can start after NB-9.

## Decisions

The operator answered the open questions on 2026-11-25:

1. **No VM testing.** Device-level work is verified on a dedicated test machine the operator is setting up ([7](#layer-2-dedicated-test-machine)). NB-6 onwards waits for it; NB-1 to NB-5 do not.
2. **Real hardware.** That test machine is the hardware for zero-copy, VRR, the cursor plane and GPU recovery. Until it exists, those criteria stay open in [status.md](../status.md).
3. **Development headers.** `libinput-dev`, `libseat-dev` and `libpam0g-dev` are installed on the host, so native crates build and unit-test in the normal workspace. They are never run here.
4. **One binary.** There is a single `shingle` binary that picks its backend the way Wayfire does: nested when `WAYLAND_DISPLAY` (or `DISPLAY`) is set, native from a TTY, and headless with `--headless`, with `--backend nested|native|headless` to override. `shingle-native` and the nested frontend become library crates linked into it, and the separate `shingle-nested` binary is dropped once the switch is in place (NB-6; NB-4 kept the binaries as they were). Session files and docs refer to `shingle` only. Done ahead of NB-6 in `shingle-single-binary`, with native as a stub; the binary is its own package, `shingle-bin` (see [The `shingle` binary](#the-shingle-binary)).
5. **wgpu `drm` feature.** Removed in NB-1, since Shingle does not use VK_KHR_display. Re-adding it later costs nothing.
