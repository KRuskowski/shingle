# Native backend design

Status: accepted with the operator's [decisions](#decisions), 2026-11-25. Roadmap item "Native backend" ([roadmap.md](../roadmap.md#3-native-backend)).

Shingle today runs only as a window inside another compositor (`crates/shingle-nested`, winit). This document decides how Shingle drives real hardware from a TTY: DRM/KMS for display, libinput for input, libseat for device access and VT switching. It is the brief for the implementation workspaces listed in [section 8](#8-implementation-plan). NB-1 is implemented (see [`shingle-render` without winit](#shingle-render-without-winit)) and so is NB-2 (see [its entry](#nb-2-shingle-compositor-host-api-what-a-real-backend-needs-from-compositor)), each with notes on where the code differs from this design; the rest is not.

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
- the keysym name for `xkb:` bindings (`matches_xkb`, crates/shingle-config/src/lib.rs:1434);
- the UTF-8 text for the Scale filter;
- real depressed, latched, locked and group masks for `Compositor::keyboard_modifiers` (crates/shingle/src/lib.rs:929).

Key repeat for compositor bindings moves into the shared module as a timer, so native, which gets no repeats from libinput, behaves like nested. Nested then drops host repeats. The tests at main.rs:5058 and main.rs:5094 are ported to the evdev table.

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

**GPU selection.** udev enumerates `drm` `card*` devices and picks the one with `boot_vga=1`, or the first one, unless `SHINGLE_DRM_DEVICE=/dev/dri/cardN` is set (like `WLR_DRM_DEVICES`). The wgpu adapter is the one whose `AdapterInfo::device_pci_bus_id` matches the card's PCI slot. The copy path may use any adapter, so virtio-gpu (no PCI match for lavapipe) works.

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

- `DeviceEvent::Added` registers the device with the compositor, applies configuration (below) and records which capabilities it has. `Removed` unregisters it.
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
- Only `mouse_scroll_speed`, `mouse_natural_scroll`, `left_handed_mode` and `middle_emulation` exist in `InputConfig` today (crates/shingle-config/src/lib.rs:1320). NB-6 adds the rest to `InputConfig` and to the Wayfire importer (crates/shingle-config/src/wayfire.rs:188-219).
- Left-handed mode and middle emulation come from libinput on native. Nested keeps its software `MouseButtons`.

**Input-device IPC** (crates/shingle/src/input_device.rs).

- Today there are three fixed devices in `DEVICES` (input_device.rs:63-80), gated by kind. NB-5 replaces that with a registry: `Compositor::add_input_device(InputDeviceInfo { name, vendor, product, kind }) -> InputDeviceId`, `remove_input_device(id)`, and an `InputDeviceId` on each entry point. Held keys, buttons and touches are then tracked per device, so disabling one keyboard does not release keys held on another.
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
- Today the Wayfire importer drops the `@refresh` part and stores only `width`/`height` (crates/shingle-config/src/wayfire.rs:594-604, `OutputConfig` lib.rs:524). NB-9 adds `name`, `refresh_mhz`, `mode` (`auto`/`off`/`highres`/`highrr`/explicit) and `vrr` to `OutputConfig`. A single `[output]` section stays valid.
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
- wlr-output-management `apply` requests (output_management.rs) that change mode or turn the output off are validated with `TEST_ONLY` before they are applied.

**Room for multi-output (roadmap item "Multi-output").**

- Everything per output (scanout, scheduler, page-flip routing by `PageFlipEvent::crtc`, mode config, VRR) already lives in `kms::Output`.
- CRTC assignment for more than one output is a bipartite match over `encoder.possible_crtcs()`, done when `enabled` grows past one entry.
- What multi-output must still add lives in `shingle` and `shingle-core`: more than one `Rect`, per-output `wl_output` globals, and per-output frame callbacks. It does not need to change the native crate's structure.
- Mirroring and multi-GPU (outputs on a second card) are out of scope.

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

The operator's Wayfire session on this host must never be stopped or disturbed. This host has one GPU (NVIDIA GA106, RTX 3060) running that session, and no `/dev/dri` is visible inside the Claude sandbox. So no native test ever runs on this host: not on another VT, not with a second seat on the same GPU. Native code runs only in VMs or on separate hardware.

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

### NB-4 `shingle-frontend-host`: move `App` into `Frontend`

Move the rest of the module plan ([1](#module-plan)): `Frontend`, `Host`, `InputEvent`, `scene`, `drag`, `switcher`, `animation`, `effects`, `session`. Nested becomes a winit host of about 600 lines.

Acceptance:

- `crates/shingle-nested/src/main.rs` contains only winit glue, `MouseButtons` and argument parsing.
- `FakeHost` input-script tests cover Scale, Expo, the switcher, a drag and a workspace switch.
- The nested real-client smoke test (docs/development.md#real-client-tests) passes unchanged.

### NB-5 `shingle-input-registry`: per-device seat devices

Replace `DEVICES` and kind gating with the registry: `add_input_device`, `remove_input_device`, `InputDeviceId` on the entry points, per-device held input, and `Event::InputDeviceChanged { id, enabled }`.

Acceptance:

- Nested's IPC output for `input/list-devices` is unchanged; this is a fixture test.
- New tests cover two keyboards with independent holds, removing a device while keys are held, and configure-device on an unknown id.
- docs/ipc.md is updated.

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

### NB-7 `shingle-native-kms`: atomic KMS with copy scanout

Add `kms/` (`KmsDevice` trait, property cache, `in_formats`, `modes`, atomic builder), `scanout/copy.rs`, `pacing.rs`, the GPU-selection rule, DPMS, presentation feedback from page flips, the startup order and the failure table of [6](#6-failure-handling).

Acceptance (test machine):

- A client is visible on the virtio-gpu display, checked with `grim` through screencopy.
- `wp_presentation` timestamps match page-flip times within 1 ms.
- Frames are paced by flips: no frame callback before the previous flip completes.
- DPMS off and on works through `wlr-output-power-management`.
- A VT switch away and back resumes with a fresh modeset.
- `kill -9` leaves a usable tty1.
- Every row of the failure table that can be produced on the test machine is exercised and recorded.

### NB-8 `shingle-native-zero-copy`: GBM scanout

Add `scanout/gbm.rs`: modifier negotiation (KMS `IN_FORMATS` ∩ Vulkan ∩ single-plane), `texture_from_dmabuf_fd` import, the `PlanarBuffer` wrapper and the automatic fallback to copy.

Acceptance:

- Unit tests for negotiation.
- Forcing the fallback (for example with an environment override) selects the copy path with a logged reason.
- On the test machine, zero-copy is selected on at least one of i915, amdgpu or nvidia-drm, and `wp_presentation` reports `ZERO_COPY`.

### NB-9 `shingle-native-outputs`: hotplug and mode configuration

Add udev DRM hotplug, connector diffing, switching the enabled output, headless waiting with no connector, EDID make/model/serial, `OutputConfig` gaining `name`, `mode` (auto/off/highres/highrr/explicit with refresh) and `vrr`, the importer handling `@refresh`, and `TEST_ONLY` for output-management `apply`. From NB-2 it also takes the parts of output description that were deferred: the mode list in `OutputInfo` for wlr-output-management, renaming the core output to the connector name, and recreating the `wl_output` global when the enabled output changes, since a bound output cannot be renamed.

Acceptance:

- Mode-selection tests cover Wayfire's heuristics.
- On the test machine, unplugging and replugging a monitor triggers a re-probe and a new mode.
- Disconnecting all outputs keeps Shingle and its clients alive, and reconnecting restores the picture with the tiling tree intact.
- `wlr-randr` (or an equivalent client) can change the mode.

### NB-10 `shingle-native-planes`: cursor plane, VRR, tearing and fences

Add the hardware cursor plane with software fallback, `VRR_ENABLED` from config, `PAGE_FLIP_ASYNC` for `wp_tearing_control` when supported, and optionally a sync_file `IN_FENCE_FD` from an exported Vulkan semaphore instead of the blocking wait.

Acceptance:

- Unit tests for the atomic requests.
- On the test machine: the cursor moves without full-frame renders (frame counter); VRR toggles `VRR_ENABLED` and the monitor reports it; a tearing client gets async flips.
- Each feature falls back cleanly where unsupported, verified by forcing each fallback on the test machine.

NB-1, NB-2 and NB-5 do not depend on each other and can run in parallel. NB-3 needs NB-1 (render types in the scene), and NB-4 needs NB-2 and NB-3. NB-6 needs NB-4 and NB-5, and NB-7 needs NB-6. NB-8 and NB-9 both need NB-7 but not each other. NB-10 comes last. The DMA-BUF roadmap item (client buffer import) can start after NB-8, reusing its Vulkan feature setup. Multi-output can start after NB-9.

## Decisions

The operator answered the open questions on 2026-11-25:

1. **No VM testing.** Device-level work is verified on a dedicated test machine the operator is setting up ([7](#layer-2-dedicated-test-machine)). NB-6 onwards waits for it; NB-1 to NB-5 do not.
2. **Real hardware.** That test machine is the hardware for zero-copy, VRR, the cursor plane and GPU recovery. Until it exists, those criteria stay open in [status.md](../status.md).
3. **Development headers.** `libinput-dev`, `libseat-dev` and `libpam0g-dev` are installed on the host, so native crates build and unit-test in the normal workspace. They are never run here.
4. **One binary.** There is a single `shingle` binary that picks its backend the way Wayfire does: nested when `WAYLAND_DISPLAY` (or `DISPLAY`) is set, native from a TTY, and headless with `--headless`, with `--backend nested|native|headless` to override. `shingle-native` and the nested frontend become library crates linked into it, and the separate `shingle-nested` binary is dropped once the switch is in place (NB-4 or NB-6). Session files and docs refer to `shingle` only.
5. **wgpu `drm` feature.** Removed in NB-1, since Shingle does not use VK_KHR_display. Re-adding it later costs nothing.
