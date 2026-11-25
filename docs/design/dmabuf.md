# DMA-BUF design

Status: proposed, 2026-11-25, waiting for the operator's answers to the [open questions](#open-questions). Roadmap item "DMA-BUF" ([roadmap.md](../roadmap.md#3-dma-buf)), designed together with the native backend's NB-8 `shingle-native-zero-copy` ([native-backend.md](native-backend.md#nb-8-shingle-native-zero-copy-gbm-scanout)), because both need the same Vulkan external-memory and DRM-modifier setup.

Shingle accepts only `wl_shm` and single-pixel buffers today, and copies every committed shared-memory buffer twice on the CPU before the GPU sees it. GPU clients therefore either fall back to software rendering or, on drivers that need it, do not run at all. This document decides how Shingle imports client DMA-BUFs through `zwp_linux_dmabuf_v1` into wgpu, how it synchronises with clients implicitly and through `linux-drm-syncobj-v1`, how formats and modifiers are negotiated with Vulkan and, on the native backend, with KMS `IN_FORMATS`, which CPU copies of `wl_shm` buffers go away, how failures fall back, and how all of it is verified with real GPU clients on this host without touching its display. It is the brief for the workspaces in [section 11](#11-implementation-plan). Nothing in it is built yet.

Line numbers refer to branch `shingle-dmabuf-design`, cut from `master` at `c8839a9`.

Sources and dates (all read on 2026-11-25):

- **Crates** were read from `~/.cargo/registry` at the versions in Cargo.lock: wgpu, wgpu-core, wgpu-hal and wgpu-types 30.0.2, ash 0.38.0+1.3.281, drm 0.15.0, drm-ffi 0.9.1, wayland-server 0.31.14, wayland-protocols 0.32.13 and rustix 1.1.6. `gbm` 0.18.0 is not in Cargo.lock yet; it was read from its published `.crate` file. Its latest release is still 0.18.0 (2024-12-03).
- **wlroots** was read on master `56f15f8` (2026-11-21), cloned from gitlab.freedesktop.org with `git`. Wayfire builds on wlroots 0.19 (wayfire/meson.build:48).
- **Wayfire** was read at `/home/karl/dev/wayfire`, commit `b36615a7` (2026-02-22).
- **smithay** was read on master `54cf9c1` (2026-11-24).
- **Mesa** was read on master `799db94` and at tag `mesa-25.0.7`, the version installed here; **Xwayland** at tag `xwayland-24.1.6`, also the installed version; **egl-wayland** on master `f6ea5d3` (tag 1.1.24; 1.1.20 is installed with the NVIDIA driver) and **egl-wayland2** on main `e76d3b7` (1.0.1 installed).
- **This host** was read with `vulkaninfo`, `/sys/class/drm`, `dpkg` and a read-only `DRM_IOCTL_GET_CAP` query on the render node `/dev/dri/renderD128`. No card node was opened.

## Summary of decisions

| # | Topic | Decision |
| --- | --- | --- |
| 1 | Shared Vulkan setup | `shingle-render` opens its device through wgpu-hal's `open_with_callback`, adding `VK_KHR_external_semaphore_fd` and `VK_EXT_queue_family_foreign` to what `Features::VULKAN_EXTERNAL_MEMORY_DMA_BUF` enables. A new `DmabufCaps` query (ash) lists, per DRM format, the single-plane modifiers Vulkan can import and sample, and those it can render to, with their maximum sizes, plus the device's render node. One import function wraps `texture_from_dmabuf_fd` for both client buffers and NB-8's scanout buffers, and records queue-family ownership transfers to and from `VK_QUEUE_FAMILY_FOREIGN_EXT` as wlroots does. |
| 2 | Client import | `zwp_linux_dmabuf_v1` version 5 with default and per-surface feedback: one tranche, the render node, 8-bit ARGB/XRGB/ABGR/XBGR with the modifiers from decision 1. The global exists only when a renderer offers it; headless without a GPU has none. `create` does a real test import through an importer the frontend registers, so `failed` is meaningful. Imported textures are cached per `wl_buffer`. `wl_drm` is not offered. |
| 3 | Synchronisation | Implicit sync: a commit with a DMA-BUF waits on the CPU, in the event loop, until the buffer's fds poll readable, as smithay does. Explicit sync: `wp_linux_drm_syncobj_manager_v1` version 1 when the render node supports timeline syncobjs and `DRM_IOCTL_SYNCOBJ_EVENTFD` (this host's NVIDIA node does); acquire points are waited on through an eventfd, release points are signalled once the GPU has finished reading. Wayfire offers no syncobj protocol; Shingle does, because this host's NVIDIA clients need it to avoid a CPU stall. |
| 4 | Buffer lifetime | A buffer is released when no surface state and no in-flight GPU work holds it any more, through reference-counted buffer locks like wlroots' `wlr_buffer_lock`. Commits blocked on fences queue per surface in order and are never merged, and any buffer replaced before it was shown is released. |
| 5 | CPU copies | `wl_shm` buffers are uploaded straight from the client's mapping into a texture of the matching byte order, only for damaged rows, and released after the upload. The intermediate RGBA `Vec` and the per-pixel swizzle go away; CPU pixels are produced only on demand for the CPU-only headless backend. |
| 6 | Screencopy | When a frontend renders, screencopy captures the frame as drawn, through an offscreen draw and readback, instead of compositing client pixels on the CPU. This is needed for DMA-BUF surfaces and matches Wayfire, which captures its rendered output. |
| 7 | Scanout | NB-8 negotiates its own scanout buffers as the native design says: plane `IN_FORMATS` ∩ Vulkan render modifiers (decision 1) ∩ single plane, allocated with GBM. Direct scanout of client buffers, with a scanout tranche in the surface feedback, is a later, optional workspace (DB-7), as Wayfire does it for full-screen opaque surfaces. |
| 8 | Nested and native | The same code runs in both; the main device is the render node of the wgpu adapter. Nested is verified on this host with real NVIDIA clients through a new GPU-rendering headless mode (`--backend offscreen`) inside a user, mount, network and PID namespace with a private `/dev` that holds only the render node and NVIDIA's compute nodes. Native parts are verified on the test machine. |
| 9 | Plan | Seven workspaces, DB-1 to DB-7, plus a revised NB-8: Vulkan setup; offscreen backend, frame screencopy and GPU harness; buffer lifetime; `zwp_linux_dmabuf_v1`; `linux-drm-syncobj-v1`; shm upload without copies; and later client direct scanout. NB-8 starts after DB-1. |

## Crate, protocol and tool survey

| Item | Version | Used for |
| --- | --- | --- |
| `wgpu` | 30.0.2 | `Adapter::as_hal` (wgpu-30.0.2/src/api/adapter.rs:131), `Adapter::create_device_from_hal` (adapter.rs:77), `Device::as_hal` (device.rs:668), `Device::create_texture_from_hal(hal_texture, desc, initial_state)` (device.rs:341; the `initial_state: TextureUses` argument is new since the native design's survey), `CommandEncoder::as_hal_mut` (command_encoder.rs:277), `Queue::write_texture` (queue.rs:253) and `Queue::on_submitted_work_done` (queue.rs:322). `wgpu::hal` is wgpu-hal re-exported (lib.rs:247). |
| `wgpu-hal` | 30.0.2 | `vulkan::Device::texture_from_dmabuf_fd(fd, desc, modifier, stride, offset)` (src/vulkan/device.rs:525): single plane only, takes ownership of the fd, dedicated allocation, `ImageTiling::DRM_FORMAT_MODIFIER_EXT` with an explicit plane layout. `vulkan::Adapter::open_with_callback` (adapter.rs:2995) with `CreateDeviceCallbackArgs` (mod.rs:1703) to add device extensions. `Device::queue_family_index`, `raw_device`, `raw_physical_device` (device.rs:818-833). `Queue::add_wait_semaphore` and `add_signal_semaphore` (mod.rs:1524-1572), kept for later GPU-side waits. `Features::VULKAN_EXTERNAL_MEMORY_DMA_BUF` needs `VK_KHR_external_memory_fd`, `VK_EXT_external_memory_dma_buf` and `VK_EXT_image_drm_format_modifier` (adapter.rs:1061-1066); wgpu-hal enables those three when present (adapter.rs:1344-1357) but never `VK_KHR_external_semaphore_fd` or `VK_EXT_queue_family_foreign`, and always uses queue family 0 (adapter.rs:3005). |
| `wgpu-core` | 30.0.2 | `create_texture_from_hal` registers the texture with `TextureClearMode::None`, so wgpu treats it as initialised, and inserts `initial_state` into the tracker (src/device/resource.rs:1245-1275). `write_texture` needs no row alignment and copies into staging memory before it returns (src/device/queue.rs:853, 892). |
| `ash` | 0.38.0 | Already in Cargo.lock through wgpu-hal. `vk::DrmFormatModifierPropertiesListEXT` (src/vk/definitions.rs:26577), `ImageDrmFormatModifierInfoEXT`, `ExternalImageFormatProperties`, `PhysicalDeviceDrmPropertiesEXT`, `vk::QUEUE_FAMILY_FOREIGN_EXT` (src/vk/constants.rs:20), `ext::queue_family_foreign::NAME` and `ext::physical_device_drm::NAME` (src/extensions_generated.rs:2100, 5412), `khr::external_semaphore_fd`. |
| `drm` | 0.15.0 | Already a dependency of `shingle-native`. On a render-node fd: `control::Device::prime_fd_to_buffer` and `close_buffer` (src/control/mod.rs:784, 660), `create_syncobj`, `fd_to_syncobj`, `syncobj_timeline_wait`, `syncobj_timeline_transfer`, `syncobj_timeline_signal`, `syncobj_to_fd`, `syncobj_eventfd` (mod.rs:829-967), `Device::get_driver_capability` with `DriverCapability::TimelineSyncObj` (src/lib.rs:156, 309). |
| `rustix` | 1.1.6 | `memfd_create` and `fcntl_add_seals` for the format table (src/fs/memfd_create.rs:20, src/fs/fcntl.rs:73), `fcntl_get_seals` for sealed shm pools, `eventfd` (src/event/eventfd.rs:18), `poll`, and `ioctl` (src/ioctl/mod.rs:91) for `DMA_BUF_IOCTL_EXPORT_SYNC_FILE` (`_IOWR('b', 2, ...)`, /usr/include/linux/dma-buf.h:180), which no crate wraps. |
| `wayland-protocols` | 0.32.13 | `wp::linux_dmabuf::zv1::server` (src/wp.rs:251; the XML is at version 6) and `wp::linux_drm_syncobj::v1::server` (src/wp.rs:277, `staging`, version 1). Both features are already enabled in the workspace. No crate ships `wl_drm`. |
| `gbm` | 0.18.0 | NB-8 only, unchanged from the native design: `Device::create_buffer_object_with_modifiers2`, `format_modifier_plane_count`, `BufferObject::{fd, modifier, stride, offset, plane_count, handle}` (src/device.rs:110, 238; src/buffer_object.rs:197-259). Its optional `drm` feature still needs `drm 0.14`. |
| smithay | master `54cf9c1` | Not used; a reference. `DmabufState::create_global_with_default_feedback` offers version 6 (src/wayland/dmabuf/mod.rs:641, 716), `DmabufHandler::dmabuf_imported` imports at creation (mod.rs:1000; anvil/src/udev.rs:170), `Dmabuf::generate_blocker` polls the fds for implicit fences (src/backend/allocator/dmabuf.rs:272, 582-610) and anvil blocks commits on it or on a syncobj acquire point (anvil/src/shell/mod.rs:141-168). |

### Installed on this host

| Item | Version | Notes |
| --- | --- | --- |
| GPU | NVIDIA GeForce RTX 3060, PCI `0000:01:00.0` | `card0` and `renderD128`; the only DRM device. |
| NVIDIA driver | 590.48.01 (installer, not Debian packages) | Vulkan 1.4.325 with `VK_EXT_external_memory_dma_buf`, `VK_EXT_image_drm_format_modifier`, `VK_EXT_queue_family_foreign`, `VK_EXT_physical_device_drm` (render node 226:128), `VK_KHR_external_memory_fd`, `VK_KHR_external_semaphore_fd` and `VK_KHR_timeline_semaphore`. The render node reports `DRM_CAP_PRIME` 3, `DRM_CAP_SYNCOBJ` 1, `DRM_CAP_SYNCOBJ_TIMELINE` 1, and `DRM_IOCTL_SYNCOBJ_EVENTFD` on handle 0 fails with `ENOENT`, which is wlroots' test for eventfd support. |
| lavapipe | Mesa 25.0.7 | Has the dma-buf, modifier and foreign-queue extensions but no `VK_EXT_physical_device_drm`, so no render node to advertise. |
| EGL | libglvnd with NVIDIA first (`10_nvidia.json`), Mesa 25.0.7 second; egl-wayland 1.1.20 and egl-wayland2 1.0.1; NVIDIA GBM backend `nvidia-drm_gbm.so` | |
| Kernel | 6.12.94 | `DMA_BUF_IOCTL_EXPORT_SYNC_FILE` and `DRM_IOCTL_SYNCOBJ_EVENTFD` are both older than 6.12. `/dev/udmabuf` is accessible to the operator's user through an ACL. |
| Xwayland | 24.1.6 | Glamor needs linux-dmabuf 4 or `wl_drm`, and supports syncobjs. |

## What exists today

- **Globals.** `wl_shm` version 2 with ARGB8888, XRGB8888, ABGR8888 and XBGR8888 (crates/shingle/src/state.rs:559, 1772-1793; crates/shingle/src/buffer.rs:368-389), and `wp_single_pixel_buffer_manager_v1`. No `zwp_linux_dmabuf_v1`, `wl_drm` or `wp_linux_drm_syncobj_manager_v1`: `rg -i 'dmabuf|syncobj' crates` finds nothing.
- **Buffers.** `wl_shm_pool.create_buffer` stores an `Attachment` (crates/shingle/src/buffer.rs:89) in `CompositorState.buffers` (state.rs:166, 1850); `wl_surface.attach` clones it into the pending state (state.rs:1637-1667).
- **Two full CPU copies per commit.** When a commit applies, `Attachment::snapshot` (state.rs:1335) maps the whole pool, converts every pixel of the buffer, whatever the damage, into a newly allocated tightly packed RGBA `Vec` (buffer.rs:186-311, the loop at 257) and releases the buffer at once. The frontend later passes that `Vec` to `update_texture` (crates/shingle-frontend/src/frontend/scene.rs:365-383), which uploads the full buffer again with `Queue::write_texture` (crates/shingle-render/src/gpu.rs:187-201, 446), and wgpu copies it a second time into staging memory before the GPU copy. Damage (`BufferSnapshot::damage`, crates/shingle/src/lib.rs:519-523) is computed but not used for the upload. Geometry animations upload the previous buffer's `Vec` once more (scene.rs:404-410).
- **CPU readers of client pixels.** wlr-screencopy composites `BufferSnapshot::rgba` on the CPU (crates/shingle/src/screencopy.rs:105-160) and offers only `wl_shm` ARGB8888 frames (screencopy.rs:41-46). The session-lock check reads alpha from it (crates/shingle/src/shell.rs:680-691). Toplevel icons must be `wl_shm` by protocol (xdg-toplevel-icon-v1.xml:181) and already reject other buffers (crates/shingle/src/toplevel_icon.rs:203).
- **Textures.** Every client texture is `Rgba8UnormSrgb`, `TEXTURE_BINDING | COPY_DST` (gpu.rs:398-445), and `GpuContext::new` opens the device with `Adapter::request_device` (gpu.rs:81-91). Textures of surfaces that are not drawn are dropped every frame (scene.rs:439).
- **Replaced buffers are never released.** FIFO and commit-timing defer commits and merge a later one into a waiting one with `merge_pending` (state.rs:1422-1430; crates/shingle/src/commit_timing.rs:64-77), which drops a replaced `PendingBuffer::Attach` without sending `wl_buffer.release`. A client that commits twice behind a FIFO barrier therefore never gets its first buffer back. This follows from the code; no run has shown it yet. DB-3 fixes it.
- **Xwayland** runs without glamor and commits `wl_shm` buffers (XW-2 in [xwayland.md](xwayland.md#xw-2-shingle-xwayland-server-sockets-process-configuration-and-xwm-bring-up)).
- **Native.** NB-7 renders into an RGBA `OffscreenTarget`, reads it back and copies it into a dumb buffer (crates/shingle-native/src/gpu.rs:240-360, crates/shingle-native/src/scanout/copy.rs). `kms/in_formats.rs:87` already parses `IN_FORMATS`. The `Scanout` trait (crates/shingle-native/src/scanout.rs:27) takes a CPU `Frame`.

## 1. Shared Vulkan setup

Client import (this roadmap item) and NB-8's zero-copy scanout need the same device features, the same modifier knowledge and the same import call. They live in `shingle-render`, in a new `dmabuf.rs`, and are built first (DB-1).

### Decision

**Opening the device.** `GpuOptions` gains `dmabuf: bool`. With it, and when `Adapter::features()` contains `VULKAN_EXTERNAL_MEMORY_DMA_BUF`, `GpuContext::new` opens the device through wgpu-hal instead of `request_device`:

```rust
// shingle-render/src/dmabuf.rs, sketch
let hal_adapter = adapter.as_hal::<wgpu::hal::api::Vulkan>()?;
let open = hal_adapter.open_with_callback(
  features | Features::VULKAN_EXTERNAL_MEMORY_DMA_BUF,
  &limits,
  &MemoryHints::default(),
  Some(Box::new(|args| {
    args.extensions.push(ash::khr::external_semaphore_fd::NAME);
    args.extensions.push(ash::ext::queue_family_foreign::NAME);
  })),
)?;
let (device, queue) = adapter.create_device_from_hal(open, &descriptor)?;
```

Both extensions are added only if `vulkan::Adapter::physical_device_capabilities().supports_extension` reports them. `VK_EXT_queue_family_foreign` is required for client import, as wlroots requires it (render/vulkan/vulkan.c:526-545); `VK_KHR_external_semaphore_fd` is optional and only recorded, for NB-10's `IN_FENCE_FD` and for later GPU-side waits. Without the feature or the foreign queue family the device is opened as today and `DmabufCaps` is `None`.

**Capabilities.** `DmabufCaps::query(&GpuContext) -> Option<DmabufCaps>` uses ash on the raw physical device, as wlroots' `query_modifier_usage_support` does (render/vulkan/pixel_format.c:282-349):

- `vkGetPhysicalDeviceProperties2` with `VkPhysicalDeviceDrmPropertiesEXT` gives the render node's `dev_t`. Without `VK_EXT_physical_device_drm` or a render node (lavapipe) there is nothing to advertise. As built, the caps then still exist with `render_node: None`, so that lavapipe's import can be tested, and DB-4 offers no global without a render node ([DB-1 as built](#db-1-as-built)).
- For each supported DRM format, `vkGetPhysicalDeviceFormatProperties2` with `VkDrmFormatModifierPropertiesListEXT` lists modifiers. A modifier is kept for **sampling** if its tiling features include `SAMPLED_IMAGE | SAMPLED_IMAGE_FILTER_LINEAR` (wlroots' `dma_tex_features`, pixel_format.c:272), its plane count is 1, and `vkGetPhysicalDeviceImageFormatProperties2` with `VkPhysicalDeviceExternalImageFormatInfo { DMA_BUF }` and `VkPhysicalDeviceImageDrmFormatModifierInfoEXT` succeeds with `IMPORTABLE` set; its `maxExtent` is kept. It is kept for **rendering** (NB-8) if it also has `COLOR_ATTACHMENT | COLOR_ATTACHMENT_BLEND`.
- The single-plane rule follows `texture_from_dmabuf_fd`, which "currently only supports single-plane DMA-bufs" (wgpu-hal device.rs:514). It also excludes the compression modifiers that carry metadata in extra planes (AMD DCC, Intel CCS). `DRM_FORMAT_MOD_INVALID` is never listed: Vulkan can import only explicit modifiers, and wlroots' Vulkan renderer does not advertise it either.

**Formats.** One table maps DRM fourccs to wgpu formats, the same way wlroots' table does (pixel_format.c:10-60): DRM names are little-endian, Vulkan names are in byte order.

| DRM format | wgpu format | Alpha |
| --- | --- | --- |
| `ARGB8888` | `Bgra8UnormSrgb` | yes |
| `XRGB8888` | `Bgra8UnormSrgb` | forced opaque |
| `ABGR8888` | `Rgba8UnormSrgb` | yes |
| `XBGR8888` | `Rgba8UnormSrgb` | forced opaque |

These are the formats `wl_shm` offers today and the ones GPU clients pick by default. 10-bit and half-float formats (`ABGR2101010` as `Rgb10a2Unorm`, `ABGR16161616F` as `Rgba16Float`) wait for colour management, since they are not sRGB-encoded 8-bit content; `ARGB2101010` has no wgpu format at all. YUV formats are excluded by the single-plane importer.

**sRGB and opaque formats.** The texture is imported with the sRGB format when the modifier supports it, so sampling decodes sRGB as `Rgba8UnormSrgb` does today. If a modifier supports only the UNORM variant, the texture is imported as UNORM and the shader decodes, as wlroots does with `WLR_VK_TEXTURE_TRANSFORM_SRGB` when it cannot use a mutable sRGB view (render/vulkan/pass.c:832-848). wgpu has no component swizzle in `TextureViewDescriptor` (wgpu-types-30.0.2/src/texture.rs:453-478), so X formats cannot use `VK_COMPONENT_SWIZZLE_ONE` as wlroots does (render/vulkan/texture.c:305-307). Instead each tile's vertices carry a small flags attribute (`OPAQUE`, `DECODE_SRGB`) that `GpuTexture` sets from its format, and `fs_main` (crates/shingle-render/src/shader.wgsl) applies it before the colour multiply. `wl_shm` X formats use the same flag once DB-6 uploads them without the CPU swizzle.

**Import.** `GpuContext::import_dmabuf(&DmabufDescriptor, DmabufUse) -> Result<ImportedTexture, ImportError>`:

- `DmabufDescriptor` holds width, height, DRM format, modifier and one plane (fd, offset, stride). `DmabufUse` is `Sample` (client buffers) or `Render` (NB-8).
- It checks the pair and the size against `DmabufCaps`, `dup`s the fd (the importer takes ownership and closes it on failure, device.rs:521-522), calls `texture_from_dmabuf_fd` through `Device::as_hal`, and wraps the result with `create_texture_from_hal`.
- The `initial_state` is `TextureUses::RESOURCE` for `Sample` and `UNINITIALIZED` for `Render`. wgpu never changes a sampled texture's state, because Shingle uses imported client textures only as `TEXTURE_BINDING`.

**Ownership transfer.** The image belongs to the client's driver between frames. wlroots acquires each foreign texture before a pass (old layout `GENERAL`, source family `VK_QUEUE_FAMILY_FOREIGN_EXT`, new layout `SHADER_READ_ONLY_OPTIMAL`) and releases it after (render/vulkan/pass.c:330-360); render buffers get the same treatment (pass.c:455-480). wgpu records no such barriers and has no API for them, so Shingle records them itself: `ForeignAccess` collects the imported textures a frame uses and records the acquire barriers into a separate `CommandEncoder` through `as_hal_mut` and the raw command buffer, and the release barriers into another. The three command buffers go to one `queue.submit([acquire, frame, release])`, so the frame's own commands run between them. Because the texture's wgpu state is `RESOURCE` at both ends, wgpu emits no conflicting transition. Without this step, drivers that keep compression metadata in single-plane modifiers (NVIDIA's block-linear kinds) may read garbage; DB-1 runs its pixel test on NVIDIA with and without the barriers and records both.

### Rejected alternatives

- **`Adapter::request_device` with only `VULKAN_EXTERNAL_MEMORY_DMA_BUF`.** It works for import, but leaves no way to enable `VK_EXT_queue_family_foreign`, so the import would rely on the `UNDEFINED` to `SHADER_READ_ONLY` transition wgpu emits for an `UNINITIALIZED` texture (wgpu-hal conv.rs:240-244), which allows the driver to discard the contents. Rejected.
- **Importing as UNORM always and decoding sRGB in the shader.** Simpler, but it filters in sRGB space, which is visible in scaled Scale and Expo thumbnails. Kept only as the fallback for modifiers without sRGB support.
- **Raw Vulkan import outside wgpu.** It would duplicate wgpu-hal's memory-type selection and lifetime rules for no gain; `texture_from_dmabuf_fd` does exactly what is needed for one plane. Rejected.

## 2. Client import: `zwp_linux_dmabuf_v1`

### What Wayfire, wlroots and smithay do

- **Wayfire** calls `wlr_renderer_init_wl_display` (src/core/core.cpp:105), which creates `wl_shm` and, if the renderer has DMA-BUF texture formats and a DRM fd, `zwp_linux_dmabuf_v1` version 4 with the renderer's default feedback (wlroots render/wlr_renderer.c:83-91). `wl_drm` exists only with `--legacy-wl-drm` (src/main.cpp:47, 338; src/core/core.cpp:118-123). Wayfire does not create `linux-drm-syncobj-v1`: neither `src/` nor `plugins/` mention it. It does scan out a full-screen opaque client buffer directly when nothing else is drawn (src/view/wlr-surface-node.cpp:339-381; src/output/render-manager.cpp:969-982), unless `WAYFIRE_DISABLE_DIRECT_SCANOUT` is set (render-manager.cpp:935-944).
- **wlroots** implements up to version 5 (types/wlr_linux_dmabuf_v1.c:24). `params.add` rejects a second plane with a different modifier (wlr_linux_dmabuf_v1.c:180-189). `create` rejects unknown flag bits with `invalid_format` and every known flag, including `y_invert`, as `failed` (wlr_linux_dmabuf_v1.c:270-283), checks sizes against `lseek(fd, 0, SEEK_END)` (313-338), and checks that the main device can import the buffer with `drmPrimeFDToHandle` on its render node (208-227, opened at 900-905). A failed `create_immed` is the fatal `invalid_wl_buffer` (383-396). The texture is imported later, when a commit applies the buffer (`surface_apply_damage`, types/wlr_compositor.c:407-440, through `wlr_client_buffer_create`, types/buffer/client.c:105-107). The format table is a sealed shm file (509-560). `wl_drm` is deprecated "and will be dropped in a future wlroots version" (include/wlr/types/wlr_drm.h:35).
- **smithay** offers version 6 when given a default feedback and version 3 otherwise (src/wayland/dmabuf/mod.rs:716). It calls `DmabufHandler::dmabuf_imported` at creation, and anvil really imports the buffer into its renderer there before answering (anvil/src/udev.rs:170-185).

### Decision

**Version 5.** It is what wlroots, and so Wayfire, implement: feedback (4) and the same-modifier rule for all planes (5). Version 6 drops `main_device` in favour of a `sampling` tranche flag and adds `set_sampling_device` (linux-dmabuf-v1.xml:400-418, 490-519, 594-617); only multi-GPU setups gain from it, and current clients handle 5: Mesa 25.0.7 EGL binds at most 4 (platform_wayland.c:2093-2097 at that tag), Mesa's Vulkan WSI at most 4 (wsi_common_wayland.c:912-917), Xwayland 24.1.6 at most 4 (hw/xwayland/xwayland-dmabuf.c:662), egl-wayland at most 6 (wayland-egldisplay.c:483-487).

**When the global exists.** The frontend registers an importer when its renderer has `DmabufCaps` (`Frontend::set_dmabuf_importer`, which calls a new `Compositor::enable_dmabuf(DmabufFeedback, Box<dyn DmabufImporter>)`). Headless without a GPU, a device without the features, lavapipe and `SHINGLE_DMABUF=0` get no global, and clients use `wl_shm` as today. The global must exist before clients start, since most bind their globals once. Native already creates its renderer before `Frontend::new` creates the socket and starts clients (crates/shingle-native/src/lib.rs:130-192). Nested does not: `Frontend::new` runs in `App::new` (crates/shingle-nested/src/lib.rs:230) and the window and renderer only in `resumed` (lib.rs:365). DB-4 therefore splits client start-up (`--spawn`, autostart and session restore) out of `Frontend::new` into a call that nested makes once its renderer exists. A global added later is still announced to clients already connected, so nothing breaks for a client that connects early by itself.

**Feedback.** The default feedback and every surface feedback are the same single tranche: target device and `main_device` are the render node's `dev_t`, flags 0, and the indices of all (format, modifier) pairs from `DmabufCaps`'s sampling list. The format table is written once into a `memfd`, sealed against writes, shrinking and growing, and every client gets a fresh read-only fd for it, as the protocol requires and wlroots does (wlr_linux_dmabuf_v1.c:534-560). DB-7 adds a scanout tranche on the native backend; until then surface feedback is never re-sent.

**Params and create.** Validation follows the protocol and wlroots, in `crates/shingle/src/linux_dmabuf.rs`:

- `add`: `plane_idx` below 4, no plane set twice, the same modifier on every plane (`invalid_format`), and the pair advertised (`invalid_format`, required since version 4, linux-dmabuf-v1.xml:248-250).
- `create`/`create_immed`: planes without gaps, width and height positive, offset plus stride times height within `lseek(SEEK_END)` for plane 0, the plane count equal to the modifier's (1), no flags (Shingle does not draw `y_invert` or interlaced buffers; such a buffer gets `failed`, as in wlroots), and the size within the modifier's `maxExtent`.
- Then `DmabufImporter::test_import`, which really imports the buffer through `GpuContext::import_dmabuf` and keeps the texture in the frontend's cache, the way smithay's handler does it. Success sends `created` (or makes the `create_immed` buffer); failure sends `failed`, or the fatal `invalid_wl_buffer` for `create_immed`, as wlroots does. Mesa and NVIDIA clients react to `failed` by trying the next modifier, which a cheaper `drmPrimeFDToHandle` check would not catch.

`DmabufImporter` is a trait in `shingle` so that the compositor crate keeps no wgpu dependency ([architecture.md](../architecture.md#design-rules)); its implementation in `shingle-frontend` holds a clone of the `wgpu::Device` (which is reference-counted) and the texture cache shared with the scene.

**Buffer object.** A `DmabufBuffer` (in `shingle`) owns the plane fds, format, modifier, size and a serial, behind an `Arc`, and becomes a new `BufferContent::Dmabuf` variant of the attachment ([4](#4-buffer-lifetime-and-the-commit-queue)). The fds stay open for the buffer's lifetime, so a texture can be imported again after a GPU loss.

**Texture cache.** The frontend keys imported textures by buffer serial, not by surface, because clients cycle through two to four buffers and re-importing every frame would mean an allocation and bind group per frame. A cached texture is dropped when the compositor reports the `wl_buffer` destroyed and the GPU has finished with it. Committing a DMA-BUF needs no upload; marking the surface dirty only makes the scene pick the buffer's texture.

**`wl_drm` is not offered.**

- No client installed here needs it with linux-dmabuf 4 or later. Mesa 25.0.7 EGL takes its device from the default feedback and falls back to `wl_drm` only if there is none (platform_wayland.c:2202-2236 at that tag); Mesa master builds `wl_drm` only with `HAVE_BIND_WL_DISPLAY` (platform_wayland.c:2355-2360). Xwayland 24.1.6 needs `wl_drm` only below linux-dmabuf 4 (hw/xwayland/xwayland-glamor.c:122-128) and opens the feedback's render node (xwayland-glamor-gbm.c:1665-1708). egl-wayland prefers the feedback device and uses `wl_drm` only if there is none (wayland-egldisplay.c:1020-1045). Mesa's Vulkan WSI never uses it.
- wlroots deprecated it, Wayfire disables it by default, no Rust crate has its XML, and the protocol's `authenticate` and flink paths are legacy.

### Rejected alternatives

- **Version 6.** See above; it can be added when multi-GPU work needs it.
- **Version 3 without feedback.** Mesa EGL and Xwayland glamor would need `wl_drm` to find the device. Rejected.
- **A `drmPrimeFDToHandle` check instead of a test import.** It proves only that the device can see the memory, not that Vulkan accepts the layout. Kept as a cheap first check only where no importer exists, which is never the case when the global exists. Rejected as the sole check.
- **Importing in the compositor crate.** It would make `shingle` depend on wgpu. Rejected for the importer trait.

## 3. Synchronisation

### Implicit sync

The protocol's default is implicit synchronisation through the DMA-BUF's reservation object (linux-dmabuf-v1.xml:87-90). Vulkan does not wait on those fences when it samples an imported image.

**Decision: wait on the CPU before applying the commit.** When a commit attaches a DMA-BUF and the surface has no syncobj object, each plane fd is polled with `POLLIN` and a zero timeout. If it is not readable, the client's write fences are still pending, and the commit becomes a blocked entry in the surface's commit queue ([4](#4-buffer-lifetime-and-the-commit-queue)) with a one-shot calloop `Generic` source on that fd; when it fires, the queue applies everything that is ready. This is smithay's `DmabufSource` (src/backend/allocator/dmabuf.rs:572-610, "monitoring for READ-access will monitor the state of the most recent write or exclusive fence") and anvil's commit blocker (anvil/src/shell/mod.rs:157-168). Its cost is the event-loop wake-up when the client's GPU finishes, which is at most the time the compositor would otherwise wait on the GPU.

If an exporter attaches no fences, the fd is readable at once and the wait is skipped. NVIDIA's EGL platforms do not rely on implicit sync on the same device: egl-wayland2 uses it only for PRIME buffers (src/wayland/wayland-swapchain.c:403-411) and otherwise calls `glFinish` before committing (wayland-swapchain.c:907-916), and Xwayland marks NVIDIA as having no implicit sync (xwayland-glamor-gbm.c:1787). Their buffers are complete when committed, and explicit sync below removes that CPU stall on the client side.

**Release.** A client may reuse a buffer as soon as it gets `wl_buffer.release`, and with implicit sync its next render would wait on the compositor's read fences, but Vulkan attaches none to an imported image. Shingle therefore sends `release` only after the last submission that sampled the buffer has completed (`Queue::on_submitted_work_done`, checked with `Device::poll(PollType::Poll)` each turn), which is the buffer-lock rule of [4](#4-buffer-lifetime-and-the-commit-queue). wlroots instead imports its render-done sync file into the DMA-BUF (`vulkan_sync_render_pass_release`, render/vulkan/renderer.c:1147), which would need an exported semaphore; that is the later GPU-side optimisation below.

### Explicit sync: `linux-drm-syncobj-v1`

**Decision: offer `wp_linux_drm_syncobj_manager_v1` version 1** when the DMA-BUF global exists and the render node passes wlroots' check: `DRM_CAP_SYNCOBJ_TIMELINE` is 1 and `DRM_IOCTL_SYNCOBJ_EVENTFD` on handle 0 fails with `ENOENT` (types/wlr_linux_drm_syncobj_v1.c:430-446; smithay's `supports_syncobj_eventfd`, src/wayland/drm_syncobj/mod.rs:71-78). This host passes both. `SHINGLE_SYNCOBJ=0` turns it off. Wayfire has no syncobj support; Shingle adds it because the NVIDIA driver here has no implicit sync on one device, so without it every NVIDIA client waits for its own GPU on the CPU before each commit, and because Mesa's Vulkan WSI (wsi_common_wayland.c:918-921), egl-wayland (wayland-egldisplay.c:495-501), egl-wayland2 and Xwayland 24.1 all use it when offered.

- **Render node.** The compositor opens the render node itself, from the `dev_t` in `DmabufFeedback` through `/sys/dev/char/MAJOR:MINOR/uevent`, read-write and close-on-exec, and wraps it in a type implementing `drm::Device` and `drm::control::Device`. A render node gives no display control; the card node is never opened.
- **Timelines.** `import_timeline` turns the client's fd into a syncobj handle with `fd_to_syncobj`; a bad fd is `invalid_timeline`. `get_surface` on a surface that already has one is `surface_exists`.
- **Commit rules,** as in wlroots (wlr_linux_drm_syncobj_v1.c:272-325): with a syncobj surface, a commit with a buffer must have acquire and release points (`no_acquire_point`, `no_release_point`), points without a buffer are `no_buffer`, a buffer that is not a DMA-BUF is `unsupported_buffer`, and equal timelines need acquire below release (`conflicting_points`). Points on a destroyed `wl_surface` are `no_surface`.
- **Acquire.** If `syncobj_timeline_wait` with a zero timeout says the point has signalled, the commit applies. Otherwise an `eventfd` is registered with `syncobj_eventfd(handle, point, eventfd, 0)`, which signals when the point is signalled, and the commit waits in the queue on a calloop source for it, as smithay's `DrmSyncPoint::generate_blocker` does (src/wayland/drm_syncobj/sync_point.rs:263) and as anvil uses it (anvil/src/shell/mod.rs:141-155). wlroots waits only for the point to materialise (`WAIT_AVAILABLE`) and hands the fence to the GPU (lock_surface_commit, wlr_linux_drm_syncobj_v1.c:238-270; render/vulkan/pass.c:373-414); Shingle waits for the signal on the CPU, for the same reason as with implicit sync.
- **Release.** When the buffer lock of that commit is dropped, that is, when the buffer was replaced and the GPU finished reading it, Shingle signals the release point with `syncobj_timeline_signal`. A buffer committed several times with several release points signals all of them, as wlroots' merger does (wlr_linux_drm_syncobj_v1.c:501-560). `wl_buffer.release` is still sent too, which the protocol allows.

### Rejected alternatives

- **GPU-side waits now.** wgpu-hal 30 can make the next `submit` wait on a semaphore (`Queue::add_wait_semaphore`, mod.rs:1552), so a sync file from `DMA_BUF_IOCTL_EXPORT_SYNC_FILE` or from a syncobj point (transferred to a binary syncobj and exported, as wlroots' render/drm_syncobj.c:105-130 does) could be imported as a temporary semaphore. That needs `VK_KHR_external_semaphore_fd` handling, semaphore lifetimes across wgpu submits, and still a CPU path for clients whose fence never materialises. It is kept for later, after measurements in the soak-test item, and the device setup of decision 1 already enables the extension.
- **Ignoring implicit sync.** Mesa clients on AMD and Intel commit before their rendering finishes; sampling then shows partial frames. Rejected.
- **A timeout on blocked commits.** Neither wlroots nor smithay has one; a client whose fence never signals stalls only its own surface. Rejected.

## 4. Buffer lifetime and the commit queue

**Buffer content.** `Attachment` becomes `{ resource, content: BufferContent, width, height }` with `BufferContent::{Shm(ShmView), SinglePixel([u8; 4]), Dmabuf(Arc<DmabufBuffer>)}`. `BufferSnapshot::rgba` (lib.rs:518) is replaced by a `BufferSnapshot::content` that is either CPU pixels (shm until DB-6, single-pixel), an shm view to upload (DB-6) or a DMA-BUF.

**Locks.** A `BufferLock` is a reference-counted handle to a committed buffer. The surface's current state holds one; the frontend clones it for every frame that samples the buffer and drops the clone when that submission has completed. When the last lock drops, the compositor queues `wl_buffer.release` and signals any syncobj release points, and flushes them on the next dispatch. A destroyed `wl_buffer` simply gets no event. This is wlroots' `wlr_buffer_lock`/`unlock` with the `release` signal (types/wlr_linux_dmabuf_v1.c:127-133) and gives one rule for every buffer type: shm buffers today are released at commit because the copy is complete then (buffer.rs:297-299); DB-6 keeps the lock until the upload has been queued.

**Commit queue.** Today a surface commit applies at once, or waits for a synchronized parent, FIFO or commit timing, where later commits are merged into the waiting one (state.rs:1222-1275). Fence waits cannot be merged: a later buffer may be ready before an earlier one. So a per-surface queue sits in front of the existing logic in `commit_inner`: each commit with unsignalled fences becomes an entry with its blockers; later commits of that surface queue behind it; when an entry's blockers clear, it and the ready entries after it go through the existing path in order. smithay keeps blockers per commit in the same way. A surface destroyed with entries queued drops them and releases their buffers.

**Replaced buffers.** Wherever pending state is merged (`merge_pending`, the FIFO and commit-timing deferrals, synchronized subsurfaces), a replaced attached buffer drops its lock, so it is released. That fixes the leak described in [What exists today](#what-exists-today) for `wl_shm` too.

**GPU loss.** When the frontend drops its GPU resources after a loss (`Frontend::drop_gpu_resources`), cached DMA-BUF textures go with them and are imported again from the kept fds on the next draw; the locks held for in-flight work are dropped, because a lost device runs nothing more.

## 5. Removing CPU copies of `wl_shm` buffers

**Decision (DB-6).**

- **No intermediate buffer.** `BufferContent::Shm` carries the pool mapping, offset, stride, format and the commit's damage. The frontend uploads it with `Queue::write_texture` straight from the mapping, with `bytes_per_row` set to the client's stride, which wgpu allows without alignment (wgpu-core queue.rs:892), into a texture of the matching byte order: `Bgra8UnormSrgb` for ARGB/XRGB, `Rgba8UnormSrgb` for ABGR/XBGR, with the opaque flag of [1](#1-shared-vulkan-setup) for X formats. The per-pixel swizzle (buffer.rs:257-296) and the allocation go away.
- **Damage only.** Only the bounding rows of each damage rectangle (already in buffer pixels, lib.rs:519-523) are written; a new buffer, a size change or a texture without content uploads everything.
- **When.** The upload happens in `Frontend::process`, right after the dispatch that applied the commit, for every committed surface, not only visible ones, so that the buffer can be released at once and an invisible surface never holds a client's buffer. The lock is dropped after `write_texture` returns, because wgpu has then copied the data into staging memory. Textures are kept for every mapped surface, not only drawn ones (scene.rs:439 changes accordingly), which moves the same memory from the CPU copies held today to the GPU.
- **Mapping.** A pool stays mapped while it lives instead of being mapped per commit (buffer.rs:228-240), and is remapped on resize. A client that truncates its file could fault the compositor with `SIGBUS` while it reads, which the per-commit `fstat` check today narrows but does not close. So the persistent mapping is used only for pools whose fd carries `F_SEAL_SHRINK` (`fcntl_get_seals`); other pools keep today's `fstat` check before each access.
- **CPU pixels on demand.** The CPU-only headless backend, the session-lock check (shell.rs:680) and tests get pixels from `BufferSnapshot::rgba()`, which converts the mapping when called. For a DMA-BUF the lock check uses the format instead: an opaque format covering the output is ready; for a format with alpha, the surface counts as ready once it covers the output, since the GPU content cannot be read on the CPU.

After DB-6 an shm commit costs one CPU copy of the damaged rows into wgpu's staging memory and one GPU copy, instead of two full CPU passes, an allocation and a full GPU copy.

**Rejected:** importing shm pages as Vulkan memory (`VK_EXT_external_memory_host`), which wgpu does not expose and which needs page-aligned pools; and uploading lazily at draw time, which would hold clients' buffers while their surfaces are hidden.

## 6. Screencopy and other CPU readers

A DMA-BUF surface has no CPU pixels, so wlr-screencopy cannot keep compositing on the CPU (screencopy.rs:105-160). **Decision (DB-2):** when a frontend renders, `Compositor::set_frame_capture(true)` makes `copy` and `copy_with_damage` emit `Event::CaptureRequested { id, region }` instead of compositing; the frontend draws the same scene into an `OffscreenTarget` of the region's size on its next turn, reads it back with `read_rgba_within` and answers with `Compositor::finish_capture(id, Result<Vec<u8>, _>)`, which writes the shm buffer and sends `ready`, or `failed` on error. The headless backend without a GPU keeps the CPU composite. This captures what is drawn, including inversion, Scale, Expo and animations, which is what Wayfire's screencopy shows, since wlroots copies the rendered output. Captures recorded in [status.md](../status.md#with-real-clients) as byte-identical comparisons were taken with the CPU composite; runs after DB-2 compare against new references. Screencopy into client DMA-BUFs stays out of scope.

## 7. Scanout: `IN_FORMATS` and NB-8

**Compositor frames (NB-8).** NB-8 keeps its design ([native-backend.md](native-backend.md#2-display-path)) and takes its Vulkan side from DB-1: the modifier list is the primary plane's `IN_FORMATS` for XRGB8888 ∩ `DmabufCaps`'s render modifiers for `XRGB8888` (`B8G8R8A8_SRGB` with colour attachment) ∩ single plane (`gbm::Device::format_modifier_plane_count`). GBM allocates three buffers with `create_buffer_object_with_modifiers2(.., SCANOUT | RENDERING)`; each is imported with `import_dmabuf(.., DmabufUse::Render)` and becomes a framebuffer with `add_planar_framebuffer` and `MODIFIERS`. Frames are drawn into the imported texture, with the foreign acquire before and the release after as in [1](#1-shared-vulkan-setup), so KMS reads a released image. `Scanout::prepare` stops taking a CPU `Frame`, as NB-7 anticipated. The fallback to copy scanout and `SHINGLE_SCANOUT=copy` stay as designed.

**Client buffers on a plane (DB-7, later and optional).** Wayfire scans a full-screen, opaque client buffer out directly when no effect or overlay is drawn (wlr-surface-node.cpp:339-381). Shingle can do the same on the native backend once NB-8 exists: if the visible scene is exactly one opaque surface covering the output at its scale and transform, and its DMA-BUF's (format, modifier) is in the primary plane's `IN_FORMATS`, a `TEST_ONLY` commit tries it as the plane's framebuffer. Then that surface's feedback gets a first tranche with the `scanout` flag, the KMS device as target and `IN_FORMATS` ∩ the sampling list, before the default tranche, and is re-sent when the surface stops qualifying. Presentation feedback gets `ZERO_COPY`. Because the buffer is then read by KMS after Shingle's frame, the plane needs the client's acquire fence as `IN_FENCE_FD` and the buffer stays locked until the next flip completes, which needs NB-10's fences. Nested never scans out.

## 8. Nested and native

Everything in sections 1 to 6 is backend-neutral and runs in nested, in the new offscreen backend and in native alike. The differences:

- **Device.** Nested uses the adapter wgpu picks for its window surface (crates/shingle-render/src/renderer.rs:25); native uses the adapter matching its KMS card's PCI slot (crates/shingle-native/src/gpu.rs:282-300); the offscreen backend uses wgpu's high-performance adapter. In every case the advertised main device is that adapter's render node, so clients allocate on the GPU that samples their buffers. On a hybrid laptop running nested, that may be a different GPU than the host compositor's; nested's own window then crosses GPUs, which the host handles.
- **Nested's own window** is a client of the host and presents through the host's linux-dmabuf; nothing changes there.
- **VT switch (native).** Client DMA-BUFs and syncobjs stay valid while the session is paused; blocked commits keep waiting; the device is not lost.
- **Scanout.** Only native has NB-8 and DB-7.
- **What can be verified where.** Nested and offscreen run on this host with real clients ([10](#10-testing)); NB-8, DB-7 and the native runs of DB-4 and DB-5 need the test machine.

## 9. Failure handling

| Situation | Detection | What Shingle does |
| --- | --- | --- |
| Adapter lacks the DMA-BUF feature, the foreign queue family or `VK_EXT_physical_device_drm` (lavapipe) | `Adapter::features`, extension query | No `zwp_linux_dmabuf_v1`, no syncobj global; one log line naming what is missing. Clients use `wl_shm`. |
| No format with a usable modifier | Empty sampling list | Same. |
| `SHINGLE_DMABUF=0` | Environment | Same, logged as forced. |
| Render node cannot be opened, or no timeline or eventfd support | `open` fails, `get_driver_capability`, eventfd probe | No syncobj global; DMA-BUF import still offered, with implicit sync. |
| `SHINGLE_SYNCOBJ=0` | Environment | Same, logged as forced. |
| Bad params (plane index, duplicate plane, mixed modifiers, unadvertised pair, gaps, size, bounds, unknown flag bits) | Protocol checks | The protocol error the spec names, as wlroots. |
| Known flag (`y_invert`, interlaced), size beyond `maxExtent`, or the test import fails | Checks, `import_dmabuf` | `failed` for `create`; fatal `invalid_wl_buffer` for `create_immed`, as wlroots. Logged once per client. |
| Import fails later (re-import after a GPU loss) | `import_dmabuf` | The surface draws nothing until its next buffer; logged once per buffer; the buffer is released normally. |
| A fence never signals | Blocked commit stays queued | Only that surface stops updating; its frame callbacks wait with the commit. Destroying the surface or the client drops the queue and releases the buffers. |
| Bad timeline fd | `fd_to_syncobj` | `invalid_timeline`. |
| Syncobj protocol misuse | Commit checks | `no_buffer`, `no_acquire_point`, `no_release_point`, `conflicting_points`, `unsupported_buffer`, `no_surface`, `surface_exists`. |
| eventfd registration fails | `syncobj_eventfd` or calloop error | `wl_client` gets `no_memory`, as wlroots posts it. |
| GPU lost (native) | NB-7's `GpuHealth` | Textures dropped and re-imported from the kept fds; locks of lost submissions dropped; the globals stay, with the same feedback if the new device has the same render node, otherwise re-sent. |
| Frame capture readback fails or times out | `read_rgba_within` | Screencopy `failed`. |
| NB-8 zero-copy impossible | Empty intersection, GBM or import error, refused `TEST_ONLY`, `SHINGLE_SCANOUT=copy` | Copy scanout, as designed in NB-8. |

## 10. Testing

### Clients

| Client | Package (version) | Present | Exercises |
| --- | --- | --- | --- |
| `vkcube` (`--wsi wayland --c N`) | vulkan-tools 1.4.304.0 | Yes | NVIDIA Vulkan WSI: DMA-BUF, syncobj. With `VK_ICD_FILENAMES` pointing at lavapipe: Mesa WSI on `wl_shm` (wsi_common_wayland.c:906-910, 2988-2997). |
| `es2gears_wayland`, `eglgears_wayland` | mesa-utils-bin 9.0.0 | Yes | NVIDIA EGL through egl-wayland or egl-wayland2. |
| `glxgears` | mesa-utils-bin 9.0.0 | Yes | GLX through Xwayland glamor and DRI3. |
| `kitty` | kitty 0.41.1 | Yes | A GL terminal. |
| `firefox` | firefox-esr 140.12.0esr | Yes | WebRender on EGL. |
| `chromium` (`--ozone-platform=wayland`) | chromium 149.0.7827.196 | Yes | Chromium's GPU process. |
| `wayland-info` | wayland-utils 1.2.0 | Yes | Prints globals and the default DMA-BUF feedback. |
| `grim` | grim 1.4.0 | Yes | wlr-screencopy captures. |
| nested `shingle` | this repository | Yes | wgpu on NVIDIA as a client of Shingle, and as the compositor under test. |
| `weston-simple-dmabuf-egl`, `weston-simple-dmabuf-feedback`, `weston-simple-egl` | weston 14.0.2 | **No**: `sudo apt-get install weston` | Precise DMA-BUF and feedback clients; `-feedback` prints what it received. Only the client binaries are used, never `weston` itself on the host. |
| `mpv` (`--vo=gpu-next --gpu-api=vulkan` and `--gpu-api=opengl`) | mpv 0.40.0 | **No**: `sudo apt-get install mpv` | A video client. |

`drm_info` (package drm-info) is deliberately not used: it opens card nodes.

### Layer 1: unit tests, no device

- `shingle-render`: the fourcc table; `DmabufCaps` filtering on recorded `VkDrmFormatModifierPropertiesEXT` lists (single plane, feature bits, `IMPORTABLE`, extents); `ForeignAccess` barrier lists.
- `shingle`, with the in-process test client (crates/shingle/src/test_client.rs) and a fake `DmabufImporter`: every params error; `created`, `failed` and `create_immed`; the format table's layout and seals; default and surface feedback events; no global without an importer; the commit queue (blocked, ready out of order, applied in order, destroyed while blocked); buffer locks and release timing for shm, single-pixel and DMA-BUF; the release of replaced buffers under FIFO, commit timing and synchronized subsurfaces; every syncobj protocol error; acquire and release through a fake timeline trait; frame-capture events.
- `shingle-frontend`, with a recording `RenderTarget`: cached imports per buffer, re-import after `drop_gpu_resources`, damage-limited shm uploads (bytes written equal to the damaged rows), the opaque and sRGB flags, capture answers.
- `crates/shingle/testdata/wayland_pins.txt` must stay unchanged by DB-3 and DB-6.

### Layer 1b: device tests that skip without the device

- An import test creates a linear XRGB8888 DMA-BUF from a sealed memfd through `/dev/udmabuf`, imports it with `import_dmabuf`, draws it into an `OffscreenTarget` and compares pixels; it skips when `/dev/udmabuf` or a capable adapter is missing. On this host it runs on NVIDIA; with `VK_ICD_FILENAMES` set to lavapipe it shows whether lavapipe imports linear buffers, which is recorded either way. Under development.md's `bwrap --dev /dev` run, it skips. As built, NVIDIA turned out not to show udmabuf memory, so the test also imports buffers exported by a second Vulkan device ([DB-1 as built](#db-1-as-built)).
- A syncobj test creates a timeline on the render node, signals a point, and checks the eventfd path; it skips without a render node.

### Layer 2: real GPU clients in a private namespace

**Why a new backend.** Nested Shingle must not connect to the host's Wayland display, so it needs a compositor of its own to run in. The XWayland harness nests Shingle on lavapipe inside headless Shingle ([xwayland.md](xwayland.md#layer-2-real-clients-in-a-private-namespace)), but lavapipe has no render node to advertise, cannot import NVIDIA's buffers, and its WSI never uses DMA-BUF, so nothing here would be exercised. And headless Shingle has no renderer, so it cannot import NVIDIA nested Shingle's window buffers either. **Decision:** DB-2 adds an offscreen backend, `shingle --backend offscreen`: the headless compositor plus the `Frontend`, drawing with the GPU into an `OffscreenTarget` at the configured output size, frames paced by a timer at the output refresh, presentation feedback from that timer, and screencopy from the drawn frame. It needs no display and no input device, and is what wlroots' headless backend is (it renders too). The chain on this host is then:

```text
offscreen Shingle (NVIDIA, imports nested's window buffers)
  <- nested Shingle (NVIDIA, the compositor under test)
       <- vkcube, es2gears_wayland, kitty, ... (NVIDIA clients)
```

Clients also connect to offscreen Shingle directly, which tests the same import code without nesting.

**The namespace.** A Python harness, `tools/gpu_test/run.py`, reuses the XWayland harness's namespace (`unshare --user --map-root-user --mount --net --pid --fork --mount-proc`, private `/tmp`, `XDG_RUNTIME_DIR`, `XDG_CONFIG_HOME`, `XDG_STATE_HOME`, `HOME` and IPC paths, PIDs checked before every signal, run outside the Claude sandbox like cargo) and differs in its devices:

- A tmpfs is mounted over `/dev`, and only these nodes are bind-mounted into it: `null`, `zero`, `full`, `random` and `urandom`, a private `shm` and `pts`, `dri/renderD128`, `nvidia0`, `nvidiactl`, `nvidia-uvm`, `nvidia-uvm-tools` and `udmabuf`. `card0`, `nvidia-modeset`, `/dev/input` and every TTY are absent, so no process in the namespace can open them. Each run lists the device fds of every process at the end and fails if any is outside that set.
- A tmpfs over `/run/user/1000` hides the host's Wayland sockets, D-Bus and PipeWire even by absolute path, and the network namespace hides abstract sockets. `WAYLAND_DISPLAY` is the private socket's name, `DISPLAY` is Shingle's own X display or unset, and `DBUS_SESSION_BUS_ADDRESS` is unset.
- No input is sent except through STIPC. Captures are taken with `grim` on the private sockets.
- `SHINGLE_TRACE_BUFFERS=1` (new in DB-4) makes Shingle log each buffer's path (`shm upload N bytes`, `dmabuf import`, `dmabuf cached`, `blocked implicit|syncobj N µs`, `release`), so the harness can tell which path a client took.

If NVIDIA's libraries turn out to need `nvidia-modeset` for rendering or WSI, the harness reports it and stops; adding that node is an [open question](#open-questions).

**Checks by workspace** are listed with each workspace in [11](#11-implementation-plan). Results go to [status.md](../status.md), separate from unit tests; temporary directories, sockets and logs are removed afterwards.

**Test machine.** NB-8, DB-7 and the native runs of DB-4 and DB-5 (vkcube and es2gears on the native output, a VT switch while a client renders, a GPU-loss recovery with DMA-BUF clients) become new steps in [native-testing.md](../native-testing.md), written by those workspaces.

### Rejected test strategies

- **Running clients or nested Shingle against the host's Wayfire.** Forbidden, and it would test Wayfire's DMA-BUF path for nested's window, not Shingle's. Rejected.
- **Weston's headless backend as the outer compositor.** It needs `weston` installed and its GL renderer on NVIDIA headless is unverified; it would also test weston instead of Shingle's import. Kept as an optional cross-check if the operator installs weston.
- **Only in-process tests.** They cannot show what real drivers allocate, which modifiers they pick or how their sync behaves. Rejected as the only layer.

## 11. Implementation plan

Workspaces are ordered by dependency, each finished and verified on its own, with the repository rules: unit tests for new code, clippy clean, 80 columns, [status.md](../status.md), [roadmap.md](../roadmap.md), [features.md](../features.md) and [architecture.md](../architecture.md) updated, real-client results recorded apart from unit tests, and signed commits.

### DB-1 `shingle-gpu-dmabuf`: Vulkan external memory and modifier setup

Add `shingle-render/src/dmabuf.rs`: `GpuOptions::dmabuf`, the device opened through `open_with_callback` with the extra extensions, `DmabufCaps` (render node `dev_t`, sampling and render modifier lists with extents), the fourcc table, `GpuContext::import_dmabuf`, `ForeignAccess` and the vertex flags for opaque and sRGB decoding. Add `ash` 0.38 as a direct dependency of `shingle-render`. Add an example, `shingle-render/examples/dmabuf_caps.rs`, that prints the caps of every adapter. Nested and native open their devices with `dmabuf: true`; nothing uses imports yet.

Acceptance:

- Unit tests for the table, the caps filter and the barrier lists; the udmabuf import test with a pixel comparison, run on NVIDIA in the GPU namespace (from DB-2's harness, or a minimal `unshare` wrapper until then) and recorded for lavapipe.
- On this host, `dmabuf_caps` lists the NVIDIA adapter with render node 226:128 and the modifiers for the four formats; the list is recorded in status.md, since vulkaninfo does not show it.
- The import pixel test passes with the acquire and release barriers; whether it also passes without them on NVIDIA is recorded.
- Nested still runs on this host with unchanged screencopy output, and the offscreen pixel tests (crates/shingle-render/tests/offscreen_pixels.rs) pass.

### DB-1 as built

Built on branch `shingle-gpu-dmabuf`, 2026-11-26, in crates/shingle-render/src/dmabuf.rs, gpu.rs, draw.rs and shader.wgsl, with the example crates/shingle-render/examples/dmabuf_caps.rs, the device test crates/shingle-render/tests/dmabuf_import.rs and the namespace tools/gpu_test/run.py. The API follows section 1, with these differences, each because the code needed it:

- **`GpuContext::dmabuf()`** returns `Result<&DmabufCaps, &DmabufUnavailable>` instead of a separate `DmabufCaps::query(&GpuContext) -> Option`. The caps are queried on the adapter (`DmabufCaps::for_adapter`, which the example uses without opening a device) before the device is opened, and kept. `DmabufUnavailable` names what is missing (not requested, not Vulkan, the wgpu feature, `VK_EXT_queue_family_foreign`, Vulkan 1.1, no modifiers, or a failed open), which is the log line of [section 9](#9-failure-handling); `GpuContext::dmabuf_summary` formats it, and nested and native log it at start.
- **`render_node` is an `Option<(major, minor)>`**, and the caps exist without one. Section 1 had the query return `None` on lavapipe, but then lavapipe's import, which works, could not be tested. DB-4 must not create `zwp_linux_dmabuf_v1` when `render_node` is `None`.
- **`DmabufUse::Sample` carries the `TextureFilter`**, because the sampler is part of the texture's bind group, as for shm textures. `ImportedTexture::gpu_texture()` is the drawable `GpuTexture` of a sampled import; `texture()` and `foreign()` serve NB-8's render imports.
- **`draw_with_access`** is `draw` with a `ForeignAccess` the caller fills, for a frame that renders into an imported image (NB-8), which `draw` cannot see. `draw` itself adds every imported tile texture. `ForeignAccess::submit` sends `[acquire, frame, release]` in one `queue.submit`, as designed; `GpuContext::set_foreign_barriers(false)` turns the barriers off for the experiment below.
- **Layouts.** Every acquire, including the first, goes from `GENERAL` to `SHADER_READ_ONLY_OPTIMAL` (sampled) or `COLOR_ATTACHMENT_OPTIMAL` (render), and every release back to `GENERAL`, with `TOP_OF_PIPE` and `BOTTOM_OF_PIPE` on the foreign side.
- **The import test** has three buffer sources, because NVIDIA's driver does not show the contents of udmabuf memory: it imports the buffer without error but samples zeros or unrelated data, also after `DMA_BUF_IOCTL_SYNC` and with the data written before or after creating the buffer. The udmabuf tests (XRGB8888 with a zero alpha byte, ABGR8888) therefore skip on NVIDIA and run on lavapipe. On every driver the test also imports a linear image that a second Vulkan device exported and the CPU wrote through a host-visible mapping, and an image of the second device in a render modifier (NVIDIA's block-linear `0x0300000000606015`; linear on lavapipe) that the second device imported with `DmabufUse::Render` and drew into with `draw_with_access`, which is how GPU clients produce their buffers and exercises the render import too. Each draws its buffer twice and compares every pixel.
- **The GPU namespace** (tools/gpu_test/run.py) is the "minimal `unshare` wrapper" of the acceptance list: it runs one command in the namespace of [layer 2](#layer-2-real-gpu-clients-in-a-private-namespace) with its private `/dev` and samples the devices every process opens. A user namespace may not bind `/dev` without its locked submounts, so the host's `/dev` is bound recursively to a work directory, the nodes are bound from there, and the copy is detached before the command runs. NVIDIA's Vulkan driver needed neither `nvidia-modeset` nor `nvidia-uvm`: the processes opened only `renderD128`, `nvidia0`, `nvidiactl`, `udmabuf` and `null`. DB-2 builds its checks on it.

Found on this host's NVIDIA driver (590.48.01), recorded in [status.md](../status.md#with-real-clients): every format has the same modifiers, six block-linear ones (`0x0300000000606010` to `...6015`) and `LINEAR` for sampling, all with sRGB, and only the six block-linear ones for rendering, so NB-8's scanout buffers cannot be linear on this GPU. The import tests pass with and without the barriers; the block-linear modifiers listed carry no compression bits, so the barriers are kept for drivers that need them, as wlroots does.

### DB-2 `shingle-offscreen-backend`: GPU offscreen backend, frame capture and the GPU harness

Add `--backend offscreen` in `shingle-bin` (the frontend on an `OffscreenRenderer`, a frame timer, presentation feedback from it), `Compositor::set_frame_capture` with `Event::CaptureRequested` and `finish_capture`, the frontend's capture draw, and `tools/gpu_test/run.py` with its private `/dev`.

Acceptance:

- Unit tests for the CLI, the capture events and answers, and the harness's pure helpers (`python3 -m unittest`).
- In the namespace: offscreen Shingle starts on NVIDIA, a `wl_probe` shm client maps, and `grim` shows its pattern at the view geometry; inversion is visible in a capture; nested Shingle on lavapipe inside offscreen Shingle shows its client in captures of the outer output.
- The baseline before DMA-BUF is recorded: what `vkcube`, `es2gears_wayland` and nested Shingle on NVIDIA do against a compositor without linux-dmabuf (fail, or fall back, and with which message).
- The device check passes: no process opened anything outside the allowed set.

### DB-3 `shingle-buffer-lifetime`: buffer content, locks and the commit queue

Add `BufferContent`, `BufferLock` with release on the last drop, the per-surface commit queue with a blocker interface (no real blockers yet), and the release of replaced buffers in every merge path. `BufferSnapshot` gains `content` and keeps `rgba()` for its callers.

Acceptance:

- Unit tests as in layer 1; `wayland_toplevels_look_unchanged` passes without re-blessing.
- In the namespace against offscreen Shingle: a `wl_probe` mode that commits two buffers behind a FIFO barrier gets `release` for both (it got one before, which the run on `master` shows first); the shm checks of DB-2 still pass.

### DB-4 `shingle-linux-dmabuf`: client DMA-BUF import with implicit sync

Add `crates/shingle/src/linux_dmabuf.rs` (global version 5, params, feedback, the format table, `DmabufBuffer`), the `DmabufImporter` trait and `Compositor::enable_dmabuf`, the frontend's importer and texture cache, the implicit-sync blocker, `SHINGLE_DMABUF` and `SHINGLE_TRACE_BUFFERS`. Split client start-up out of `Frontend::new` so that nested starts clients after its renderer exists ([2](#2-client-import-zwp_linux_dmabuf_v1)).

Acceptance:

- Unit tests as in layer 1, including every protocol error.
- In the namespace, against offscreen Shingle and against nested Shingle inside it:
  - `wayland-info` shows `zwp_linux_dmabuf_v1` version 5 with main device 226:128 and the caps' format table;
  - `vkcube --wsi wayland --c 600` runs to the end, captures show the cube changing between frames, and the trace shows at most one import per swapchain image followed by cached use;
  - `es2gears_wayland`, `eglgears_wayland`, `kitty`, `firefox` and `chromium --ozone-platform=wayland` map and draw, checked in captures and in the trace;
  - `glxgears` through Xwayland draws and Xwayland's log shows glamor initialised on the NVIDIA render node;
  - nested Shingle on NVIDIA presents through offscreen Shingle's DMA-BUF import, and its own clients draw;
  - `VK_ICD_FILENAMES` set to lavapipe keeps `vkcube` on `wl_shm`;
  - with `SHINGLE_DMABUF=0` there is no global and the DB-2 baseline repeats;
  - after `sudo apt-get install weston mpv` (if approved): `weston-simple-dmabuf-feedback` prints the advertised tranche, `weston-simple-dmabuf-egl` draws, and `mpv` plays a test pattern (`mpv av://lavfi:testsrc`) with Vulkan and with OpenGL output.

### DB-5 `shingle-drm-syncobj`: explicit synchronisation

Add the render-node wrapper, `crates/shingle/src/linux_drm_syncobj.rs` (manager, timelines, surface object, commit rules), the eventfd blocker, release-point signalling from buffer locks, and `SHINGLE_SYNCOBJ`.

Acceptance:

- Unit tests for every protocol error and for acquire and release through the fake timeline; the device-backed syncobj test on the render node.
- In the namespace: `wayland-info` lists `wp_linux_drm_syncobj_manager_v1`; the trace shows syncobj acquire waits for `vkcube`, `es2gears_wayland`, Xwayland's `glxgears` and nested Shingle; all DB-4 client checks pass again; with `SHINGLE_SYNCOBJ=0` they pass on implicit sync.
- A client stalled on a never-signalled acquire point (a probe mode) stops only its own surface, and destroying it releases its buffers.

### DB-6 `shingle-shm-upload`: shared-memory buffers without CPU copies

Implement [5](#5-removing-cpu-copies-of-wl_shm-buffers): the shm view, direct damage-limited uploads in matching byte order, persistent mappings for sealed pools, upload in `process`, textures kept for mapped surfaces, and `rgba()` on demand. Can start after DB-3; it does not need DB-4 or DB-5.

Acceptance:

- Unit tests for the upload rectangles, strides, formats and the seal rule; pins unchanged.
- In the namespace: the shm captures of DB-2 match the same run before DB-6 pixel for pixel; a `wl_probe` mode that damages 10×10 pixels of a 1000×1000 buffer uploads 40 rows' worth of bytes, not 4 MB, per the trace; the CPU time of offscreen Shingle with a full-frame shm animation before and after is measured and recorded, without claims beyond the measurement.

### NB-8 `shingle-native-zero-copy`: GBM scanout (revised)

As in [native-backend.md](native-backend.md#nb-8-shingle-native-zero-copy-gbm-scanout), now built on DB-1: the render modifiers come from `DmabufCaps`, buffers are imported with `import_dmabuf(.., Render)`, frames are drawn into them between foreign acquire and release barriers, and `Scanout::prepare` no longer takes a CPU `Frame`. Needs NB-7 and DB-1. Acceptance is unchanged; the negotiation unit tests use recorded `IN_FORMATS` blobs and caps.

### DB-7 `shingle-native-client-scanout`: direct scanout of client buffers (later, optional)

Implement [the second half of 7](#7-scanout-in_formats-and-nb-8): the qualification check, the scanout tranche in surface feedback, `TEST_ONLY` before using a client buffer on the primary plane, the acquire fence as `IN_FENCE_FD`, locks held until the next flip, `ZERO_COPY` feedback and `SHINGLE_DISABLE_DIRECT_SCANOUT`. Needs NB-8, NB-10 and DB-5.

Acceptance (test machine): a full-screen `vkcube` is scanned out directly (`ZERO_COPY`, and the trace shows no composition), a notification on top or Scale falls back to composition within one frame, and the fallback forced by the variable composes.

DB-1, DB-2 and DB-3 do not depend on each other. DB-4 needs all three, and DB-5 needs DB-4. DB-6 needs DB-3. NB-8 needs NB-7 and DB-1, and DB-7 comes last. Everything but NB-8 and DB-7 can be verified on this host, so DB-1 to DB-6 can proceed while the native workspaces wait for the test machine.

## Decisions

The operator asked for the work to continue without stopping for questions, so these were decided on the operator's behalf on 2026-11-25 with the defaults below. Any of them can be revisited; each is cheap to change before the workspace that depends on it lands.

1. **The offscreen backend** is a separate `--backend offscreen`. `--headless` stays CPU-only, so its existing tests and harnesses are unchanged.
2. **Installing clients.** `weston` and `mpv` are installed with `sudo apt-get install weston mpv`; the operator allowed installing dev and test packages.
3. **NVIDIA device nodes.** The harness may expose `/dev/nvidia-modeset` if NVIDIA's userspace needs it. Every NVIDIA GL client on the operator's desktop opens it already, and it grants no modeset control without a DRM master. `card0` stays hidden.
4. **Explicit sync.** Shingle offers `linux-drm-syncobj-v1`, beyond Wayfire, because NVIDIA clients otherwise stall before every commit.
5. **linux-dmabuf version 5**, as wlroots and Wayfire.
6. **Screencopy from rendered frames** is accepted, with effects visible in captures as in Wayfire. References that depended on the CPU composite are re-recorded in the workspace that changes it.
7. **Order.** DB-1 to DB-6 start now, because they can be verified on this host, while NB-8 and NB-10 wait for the test machine.
