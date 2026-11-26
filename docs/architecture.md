# Architecture

## Crates

| Crate | Role |
| --- | --- |
| `shingle-core` | Tiling trees, workspaces, geometry and the serializable session model. No Wayland dependency. |
| `shingle` | Wayland protocol handling, input routing, output state, IPC and the compositor event loop. Library only; no winit. |
| `shingle-render` | wgpu rendering without a windowing dependency: shared `GpuContext`, per-output `RenderState`, the `draw` pass, window and offscreen targets, and DMA-BUF import with its Vulkan setup. |
| `shingle-frontend` | The interactive frontend shared by the nested and native hosts, without winit: `Frontend` with Scale, Expo, the switchers, drags, animations, effects, the scene build, binding dispatch, key repeat and session handling, behind the `Host` trait and `InputEvent`. |
| `shingle-bin` | The `shingle` binary: argument parsing, backend selection and the headless and offscreen backends. Its own package because the `shingle` library must not depend on the backends that depend on it. |
| `shingle-native` | Native backend library: libseat session and VT switching, libinput devices with udev hotplug, atomic KMS with copy scanout and page-flip pacing, DRM hotplug and the configured choice of output and mode, the Vulkan renderer for it, and the event loop that runs `Frontend` on a seat, each device interaction behind a trait (`Session`, `InputSource`, `KmsDevice`, `DeviceMonitor`, `FrameRenderer`) with fakes in the tests. Entry point `shingle_native::run`. |
| `shingle-nested` | Nested backend library, a thin winit host for `Frontend`: the window and renderer, winit-to-`InputEvent` translation, host focus handling and button emulation. Entry point `shingle_nested::run`. |
| `shingle-xwayland` | XWayland without `wayland-server`: X display sockets and lock, the supervised Xwayland process with its restart rule, and the X window manager (x11rb) behind an `XConnection` trait: property parsers, classification, `Pairing` of windows and surfaces by serial, the `ConfigureRequest` policy, ICCCM focus and the `FocusIn` guard, client messages, restacking and the root cursor, the clipboard and primary selection bridge with its MIME mapping and non-blocking transfers, and the `xw_probe` test client (examples/). |
| `shingle-config` | Validated YAML model, Wayfire INI and i3 imports, XKB keymap generation. |
| `shingle-session` | Atomic session persistence and relaunching saved applications. |
| `shingle-idle` | Idle, DPMS, fullscreen suppression and lock coordination. |
| `shingle-auth` | PAM authentication for the lock client. |
| `shingle-lock` | `ext-session-lock-v1` client with swaylock-style options. |
| `shingle-wcm` | Graphical configuration manager built on [Gloomy](https://github.com/KRuskowski/gloomy). |

## Where things live

| Area | File |
| --- | --- |
| Persistent windows and mapped presence | `crates/shingle-core/src/core.rs` |
| Tiling order, focus, masks and splits | `crates/shingle-core/src/tree.rs` |
| Session snapshot format | `crates/shingle-core/src/session.rs` |
| Environment of every child process (`ClientEnvironment`), launching clients | `crates/shingle-session/src/environment.rs`, `crates/shingle-session/src/lib.rs` |
| Public compositor API and renderer events | `crates/shingle/src/lib.rs` |
| Protocol timestamps (`CLOCK_MONOTONIC`) | `crates/shingle/src/clock.rs` |
| In-process Wayland test client | `crates/shingle/src/test_client.rs` |
| Shared state and workspace transitions | `crates/shingle/src/state.rs` |
| Client buffers: content (`BufferContent`), shm pools, CPU copies | `crates/shingle/src/buffer.rs` |
| Buffer locks and `wl_buffer.release` (`BufferLock`, `ReleaseQueue`) | `crates/shingle/src/buffer_lock.rs` |
| Per-surface commit queue, its blocker interface and the fd wait of the implicit and syncobj blockers (`FdWait`) | `crates/shingle/src/commit_queue.rs` |
| `zwp_linux_dmabuf_v1`: global, feedback and format table, params checks, `DmabufBuffer`, the `DmabufImporter` trait, the implicit-sync fence wait | `crates/shingle/src/linux_dmabuf.rs`; tests in `linux_dmabuf/tests.rs` |
| `wp_linux_drm_syncobj_manager_v1`: timelines, surface objects, commit rules, the acquire wait; release points | `crates/shingle/src/linux_drm_syncobj.rs`; the render node and timeline seams in `linux_drm_syncobj/timeline.rs`; release points in `buffer_lock.rs`; tests in `linux_drm_syncobj/tests.rs` |
| `SHINGLE_TRACE_BUFFERS` buffer-path log | `crates/shingle/src/trace.rs` |
| Keymap sent to clients, focus, input routing | `crates/shingle/src/input.rs` |
| Seat device registry, per-device holds and gating, `input/*` IPC | `crates/shingle/src/input_device.rs` |
| XDG roles, the shell-neutral toplevel role (`ToplevelRole`, `ToplevelHandle`), map lifecycle, geometry, window rules | `crates/shingle/src/shell.rs` |
| Launch identity from `/proc` (`capture`, `capture_pid`) | `crates/shingle/src/launch.rs` |
| Pinned outside view of Wayland toplevels | `crates/shingle/src/wayland_pins.rs`, `crates/shingle/testdata/wayland_pins.txt` |
| IPC transport, dispatch, method list, events | `crates/shingle/src/ipc.rs` |
| XWayland glue: display, Xwayland start and exit, XWM connection, `xwayland-ready` | `crates/shingle/src/xwayland/mod.rs` |
| `xwayland_shell_v1` and `Role::Xwayland` | `crates/shingle/src/xwayland/shell.rs` |
| Managed X windows: pairing, toplevels, X configure, states and close | `crates/shingle/src/xwayland/windows.rs` |
| X focus and stacking handed to the XWM each turn | `crates/shingle/src/xwayland/focus.rs` |
| X client requests and what a managed X window gets on map | `crates/shingle/src/xwayland/requests.rs` |
| Unmanaged X windows (menus, tooltips) in the Unmanaged layer | `crates/shingle/src/xwayland/unmanaged.rs` |
| Clipboard and primary selection with X: X11 sources, reports to the XWM, transfer watches | `crates/shingle/src/xwayland/selection.rs`; sources in `crates/shingle/src/data_device.rs`, `primary_selection.rs`, `data_control.rs` |
| The XWM's selection bridge: selection windows, XFixes owners, `TARGETS`, INCR transfers; MIME and target mapping | `crates/shingle-xwayland/src/xwm/selection.rs`, `xwm/mime.rs` |
| X display sockets, Xwayland process, XWM | `crates/shingle-xwayland/src/sockets.rs`, `server.rs`, `xwm/` |
| XWayland real-client harness (namespaces), with the Wayland probe client | `tools/xwayland_test/run.py`, `crates/shingle/examples/wl_probe.rs` |
| Host seam: `Host` trait, `InputEvent` | `crates/shingle-frontend/src/host.rs` |
| `Frontend` struct, public API, input dispatch | `crates/shingle-frontend/src/frontend.rs` |
| Frontend areas: bindings, Scale, Expo, switcher, drags, effects, animations, scene build, session | `crates/shingle-frontend/src/frontend/*.rs` |
| What the scene is drawn into (`RenderTarget`) | `crates/shingle-frontend/src/render_target.rs` |
| The frontend's DMA-BUF importer, texture cache by buffer serial, locks of frames in flight, the linux-dmabuf and syncobj decisions | `crates/shingle-frontend/src/dmabuf.rs`; `Frontend::enable_dmabuf` and `start_clients` in `frontend.rs` |
| Frontend input-script tests (`FakeHost`) | `crates/shingle-frontend/src/frontend/tests.rs` |
| Command line, `--help` text | `crates/shingle-bin/src/cli.rs` |
| Backend selection from arguments and environment | `crates/shingle-bin/src/backend.rs` |
| What a start does (print, fail or run a backend) | `crates/shingle-bin/src/main.rs` |
| Headless backend | `crates/shingle-bin/src/headless.rs` |
| Offscreen backend: the frontend on the GPU without a display, frame pacing, presentation | `crates/shingle-bin/src/offscreen.rs` |
| Screencopy: CPU composite, or frame capture answered by the frontend (`set_frame_capture`, `finish_capture`) | `crates/shingle/src/screencopy.rs`; the frontend's capture draw in `crates/shingle-frontend/src/frontend/scene.rs` (`answer_captures`), `shingle_render::capture_frame` in `crates/shingle-render/src/target.rs` |
| GPU namespace and DB-2's and DB-3's real-client checks (offscreen, nested, baseline, buffers) | `tools/gpu_test/run.py`, `tools/gpu_test/checks.py` |
| DB-4's and DB-5's real-client checks (dmabuf, browsers, xwayland, nested-gpu, nodmabuf, weston, syncobj, nosyncobj) | `tools/gpu_test/dmabuf_checks.py`; the stall probe `crates/shingle/examples/syncobj_probe.rs` |
| winit host: window, input translation, button emulation | `crates/shingle-nested/src/lib.rs` |
| Native seat: `Session`, libseat, startup checks (SSH, enable timeout, backend) | `crates/shingle-native/src/session.rs` |
| libinput events, translation to `InputEvent`, device options | `crates/shingle-native/src/input.rs`, `input/libinput.rs` |
| Hotplugged input devices and their seat devices | `crates/shingle-native/src/udev.rs` |
| KMS device trait, property cache, `IN_FORMATS`, mode selection, atomic requests, connector and plane choice | `crates/shingle-native/src/kms.rs`, `kms/` (`drm.rs` is the real card, `fake.rs` the tests' card) |
| The lit output: startup test, flips, presentation, DPMS, pause and resume, failures | `crates/shingle-native/src/display.rs`; tests in `display/tests.rs` |
| DRM hotplug monitor (`DeviceMonitor`, udev) | `crates/shingle-native/src/hotplug.rs` |
| Configured output modes, output choice, connector diffing, requested modes | `crates/shingle-native/src/outputs.rs` |
| EDID make, model and serial | `crates/shingle-native/src/kms/edid.rs` |
| Copy scanout into dumb buffers | `crates/shingle-native/src/scanout.rs`, `scanout/copy.rs` |
| Page-flip state machine | `crates/shingle-native/src/pacing.rs` |
| Card selection, adapter matching, GPU renderer and its failure rules | `crates/shingle-native/src/gpu.rs` |
| Native event loop, pause and resume, `Host` | `crates/shingle-native/src/host.rs`; tests in `host/tests.rs` |
| Native test plan and tools (test machine only) | `docs/native-testing.md`, `tools/native_test/` |
| Output description, renaming, mode requests, configured placement | `crates/shingle/src/output.rs`, `output_management.rs`; tests in `output/tests.rs` |
| Output configuration and Wayfire's mode syntax | `crates/shingle-config/src/output.rs` |
| Real-client output-management checks (headless and nested) | `tools/output_test/run.py` |
| Host keyboard focus changes | `crates/shingle-nested/src/host_keyboard.rs` |
| xkb keyboard state, key names, keysyms, modifier masks | `crates/shingle-frontend/src/keyboard.rs` |
| Key repeat timer | `crates/shingle-frontend/src/keyboard/repeat.rs` |
| Evdev code to Wayfire key name table | `crates/shingle-frontend/src/keyboard/names.rs` |
| Scale layout and title filter | `crates/shingle-frontend/src/scale.rs` |
| Expo, vswitch transitions, touch gestures | `crates/shingle-frontend/src/expo.rs`, `workspace_transition.rs`, `extra_gestures.rs` |
| GPU device, pipelines, textures, gamma and effects | `crates/shingle-render/src/gpu.rs` |
| Composite pass and tile geometry | `crates/shingle-render/src/draw.rs` |
| Window and offscreen render targets | `crates/shingle-render/src/target.rs` |
| DMA-BUF device setup, modifier caps, fourcc table, import, foreign queue-family barriers | `crates/shingle-render/src/dmabuf.rs`; caps of every adapter: `examples/dmabuf_caps.rs`; device tests `tests/dmabuf_import.rs` |
| GPU test namespace with a private `/dev` (render node and NVIDIA nodes only) | `tools/gpu_test/run.py` |
| Native config model and XKB generation | `crates/shingle-config/src/lib.rs` |
| Wayfire INI import | `crates/shingle-config/src/wayfire.rs` |
| i3 import | `crates/shingle-config/src/i3.rs` |

## Design rules

- The tiling tree in `shingle-core` is the source of truth for layout and is what sessions persist. Temporary visual geometry (Scale thumbnails, Expo, vswitch slides, window rotation) must never be written back into it.
- Scale's snapshot selection lives in `shingle`, while its interactive state and wgpu thumbnail transforms live in the frontend (`shingle-frontend`).
- `shingle` and `shingle-frontend` never depend on winit; only the backend crates and `shingle-bin` link it (`cargo tree -p shingle` lists no winit).
- `shingle-frontend` never depends on winit, DRM or libinput. A host reaches it only through `Frontend`'s public API, `InputEvent` and the `Host` trait, which nested and native implement.
- Native code never touches a device in a unit test: libseat, libinput, the DRM card and the GPU renderer sit behind `Session`, `InputSource`, `KmsDevice` and `FrameRenderer`, and only the test machine runs the real implementations ([native-testing.md](native-testing.md)).
- Wayfire-facing names stay as they are: `WAYFIRE_SOCKET`, IPC method and event names, and INI section names.
- Outside xdg request dispatch, code reaches a toplevel only through `Role::toplevel` and `Role::toplevel_mut`, and per-shell behaviour (configure, close, xdg-only protocols) only through `ToplevelHandle`, so X11 windows can share the role (docs/design/xwayland.md).
- Every process Shingle starts gets its environment from `Compositor::client_environment`. Shingle never changes its own environment for its children.
