# Screen sharing and capture

Written 2026-11-27 in the `shingle-screen-sharing` workspace, as built. It covers how clients capture outputs and windows (wlr-screencopy, ext-image-capture-source and ext-image-copy-capture), how screen sharing through xdg-desktop-portal-wlr and PipeWire uses them, and the clipboard managers' ext-data-control. What runs and how it was checked is in [status.md](../status.md#screen-sharing-and-capture).

## Wayfire's behaviour

Wayfire leaves all of this to wlroots. As of Wayfire `b36615a7` (src/core/core.cpp), it creates:

- `zwlr_screencopy_manager_v1` (version 3), `wlr_export_dmabuf_manager_v1` and `zwlr_data_control_manager_v1`, as it has for years;
- since 2025-09-24 (`0175b526`, "Add support for ext-copy-capture and ext-foreign-toplevel-list protocols"), `ext_image_copy_capture_manager_v1` version 1, `ext_output_image_capture_source_manager_v1` version 1 and `ext_foreign_toplevel_list_v1`, but no `ext_foreign_toplevel_image_capture_source_manager_v1`: Wayfire's clients can capture outputs, not single windows;
- since 2025-09-25 (`5ae6213f`), `ext_data_control_manager_v1` version 1.

Its default `security-context-v1/privileged_protocols` still names only `zwlr_foreign_toplevel_manager_v1`, `zwlr_screencopy_manager_v1`, `zwlr_data_control_manager_v1` and `zwp_keyboard_shortcuts_inhibit_manager_v1`, so a sandboxed client sees the ext capture and data-control globals.

What wlroots does that clients rely on, and Shingle now does the same way:

- A screencopy buffer is in the output's buffer pixels: scaled and turned back by the output's transform. The `buffer` event names the shm format; version 3 adds `linux_dmabuf` with the renderer's format and `buffer_done`.
- `copy_with_damage` waits until the output changed since that client's last capture of it, and sends `damage` before `ready`. `copy` never waits.
- `overlay_cursor` decides whether the cursor is in the capture.
- An ext-image-copy-capture session announces the shm formats, the DMA-BUF device and formats and the buffer size, then `done`, and again whenever they change; `stopped` when the source goes. A frame's first capture is answered at once, later ones once the source changed; each is answered with `transform`, `damage`, `presentation_time` and `ready`, or `failed` with a reason.

## What Shingle does

### One capture path

crates/shingle/src/capture.rs holds what both protocols share: a capture's geometry (`CaptureGeometry`, which was screencopy's), its target (a region of an output, or a window), the damage trackers and the copy itself. Without a renderer, as in headless Shingle, the compositor composites the surfaces' CPU pixels into the buffer, leaving out the cursor unless it is wanted. With frame capture on, which every frontend that renders turns on, a copy becomes `Event::CaptureRequested(CaptureRequest)`, and the frontend draws what it shows and answers: into a shared-memory buffer through `finish_capture` with the source's logical pixels, which the compositor scales into the buffer (copied as they are when nothing needs scaling), or into the client's DMA-BUF itself through `finish_capture_into`.

### Damage

A capture that may wait (`copy_with_damage`, and every ext frame after a session's first) waits while its damage tracker is clean. A tracker becomes clean when one of its captures is answered and dirty when its source is damaged. Screencopy has one tracker per client, output and choice of `overlay_cursor`; xdg-desktop-portal-wlr is one client for all its streams, and a stream started with the other cursor mode would otherwise have waited for its first frame. Each ext session has its own.

Damage comes from the frontend: an output is damaged when the frontend draws a frame of it for anything but a capture: a commit, a pointer move, an animation, a layout change ([`Compositor::output_damaged`](../../crates/shingle/src/lib.rs)). A capture asks for a frame of its output without damaging it (`Frontend::redraw_for_capture`), so a portal's request for the next frame does not cause one. The captures released by a damaged frame are answered from that same frame. A window's captures are damaged by its output's damage. Without a frontend, any turn in which clients changed something damages every source. Every damage event covers the whole buffer; the protocols allow more than what changed.

### wlr-screencopy

Version 3, as before, with `ARGB8888` in the `buffer` event and, once the frontend has said which DMA-BUFs it draws into (`Compositor::set_capture_dmabuf`), `linux_dmabuf` with `XRGB8888`, as wlroots names its render format. xdg-desktop-portal-wlr 0.7.1 fails every version 3 frame that names no DMA-BUF format ("unable to receive a valid format from wlr_screencopy"), so before this the portal could not share Shingle's screen at all. `overlay_cursor` and `copy_with_damage` are honoured; before, every copy was answered at once with the cursor. Capture buffers are no longer released: the protocols leave `wl_buffer.release` unused.

### ext-image-capture-source and ext-image-copy-capture

crates/shingle/src/image_copy_capture.rs. Output sources, and, beyond Wayfire, toplevel sources made from ext-foreign-toplevel-list handles, offered with that list (`core/plugins` `ext-toplevel`). An output session's buffer is the output's buffer pixels, and each frame's `transform` is the output's transform, as in wlroots. A toplevel session's buffer is the toplevel surface's rectangle at the scale of the output the window is on, with `transform` normal; a window capture draws the window's surfaces, subsurfaces and popups over transparent black, without the server-side frame, the cursor or anything above or below it, and popups beyond the toplevel are cut off. The constraints are announced again when the size changes, and frames of the old size fail with `buffer_constraints`. `stopped` goes out when the output or window goes, and later frames fail with `stopped`. Every protocol error is raised: `invalid_option`, `duplicate_frame`, `no_buffer`, `invalid_buffer_damage`, `already_captured` and `duplicate_session`.

Cursor sessions report `enter` and `leave` when the cursor image meets or leaves the source, the hotspot's `position` over the source in buffer pixels and the `hotspot` in the image, and their capture session's frames copy the cursor image, waiting while it does not change. These come from the compositor's own copy of the cursor's pixels, which it keeps for cursor surfaces as it does for the native backend's cursor plane; a buffer committed before its surface became the cursor was already uploaded and given back, so it has no pixels until the next commit, and the session waits.

### Capturing into DMA-BUFs

The frontend announces the render node and the modifiers of `XRGB8888` and `ARGB8888` its device can render to (crates/shingle-frontend/src/dmabuf.rs, `capture_formats`). A copy into such a DMA-BUF reaches the frontend with the buffer; it imports it for rendering, draws the output's frame into it with the output's scale and transform (a window with its output's scale), waits for the GPU and answers (crates/shingle-render/src/target.rs, `capture_into_dmabuf`). Ext sessions announce every format and modifier; screencopy can only name the format, so a screencopy client picks a modifier from linux-dmabuf's feedback, which lists the modifiers Shingle samples. A buffer whose modifier the renderer cannot draw into fails. Headless Shingle announces no DMA-BUFs and fails DMA-BUF copies.

### ext-data-control

crates/shingle/src/data_control.rs keeps the sources, devices and offers of both data-control protocols together, so a source of either is a selection like any other for clients of both. ext-data-control devices always have the primary selection; wlr-data-control ones from version 2.

## Decisions

1. Toplevel sources are offered although Wayfire has none: window sharing in xdg-desktop-portal-wlr 0.8 and OBS needs them, and Shingle already lists toplevels through ext-foreign-toplevel-list.
2. Damage is whole-buffer and comes from the frames the frontend draws, not from tracked regions: the frontend already redraws exactly when something changes, and the protocols allow reporting more damage than there was.
3. The default privileged protocols stay Wayfire's, so sandboxed clients see the ext capture and data-control globals as in Wayfire. `security_context.privileged_protocols` can add `ext_image_copy_capture_manager_v1`, `ext_output_image_capture_source_manager_v1`, `ext_foreign_toplevel_image_capture_source_manager_v1` and `ext_data_control_manager_v1`.
4. Shared-memory captures keep DB-2's path: drawn at the output's logical size and scaled into the buffer on the CPU, copied as they are when the scale is 1 and the output is not turned. DMA-BUF captures are drawn at the buffer's size on the GPU.

## Fixed along the way

- An ext-foreign-toplevel-list bound after a window had mapped was never told about it (crates/shingle/src/ext_toplevel.rs); a portal's window picker binds the list when it needs it.
- Screencopy's damage and cursor handling, and the missing DMA-BUF format, as above.

## How it is checked

Unit tests cover the geometry, the blending of the CPU composite, both protocols' events and errors through the in-process test client, damage from commits and from the frontend, window captures, cursor sessions, DMA-BUF constraints and requests, the frontend's damage, cursor and window drawing, and the late-bound toplevel list.

The GPU harness has three groups in tools/gpu_test/screencast_checks.py, all inside its namespace against offscreen Shingle on the host's GPU:

- `screencast` starts a private D-Bus session bus (with /run/dbus covered, so the host's system bus is out of reach), PipeWire, WirePlumber, xdg-desktop-portal 1.20 and xdg-desktop-portal-wlr 0.7.1 with `chooser_type=none` and the output named in its config. tools/gpu_test/portal_client.py creates a ScreenCast session through the portal's D-Bus API, starts it and reads the PipeWire stream with GStreamer's `pipewiresrc`, which tools/gpu_test/fetch_screencast_deps.py extracts from Debian's gstreamer1.0-pipewire under target/, since this host does not have it installed. It checks the stream's pixels, its pacing, both cursor modes and a DMA-BUF stream.
- `image-copy-capture` speaks ext-image-copy-capture from a raw-protocol client (tools/gpu_test/capture_wire.py, built on tools/output_test/wayland_wire.py). NVIDIA's GBM cannot map its tiled buffers, so the capture into a GBM DMA-BUF is shown in a window and `grim` must see the captured quadrants there.
- `ext-data-control` copies and pastes through ext-data-control against `wl-copy` and `wl-paste`, which use wlr-data-control.

## Open

- A DMA-BUF with a modifier the renderer samples but cannot draw into, such as `LINEAR` on this host's NVIDIA, fails its capture. A screencopy client choosing from linux-dmabuf's feedback can pick one; xdg-desktop-portal-wlr does when its consumer only takes linear buffers. Copying from an offscreen target into such a buffer would cover it.
- With a native cursor plane the cursor is not in the frame, so an output capture leaves it out even with `overlay_cursor` or `paint_cursors`. Not run: the native backend has not run on a card.
- A region of a turned output is copied into shared memory only; a DMA-BUF copy of it fails.
- xdg-desktop-portal-wlr 0.8, grim 1.5 and OBS, which use ext-image-copy-capture, are not installed here; the raw-protocol client stands in for them. xdg-desktop-portal-wlr 0.7.1 offers no metadata cursor mode, so cursor sessions are checked only from the raw client.
- `pipewiresrc` 1.4 fails when xdg-desktop-portal-wlr's allocation fixes another modifier than the first of several offered, so the DMA-BUF stream asks for one modifier; its frames are counted, not mapped, since NVIDIA's tiled buffers cannot be read on the CPU.
