# Multi-output design

Status: accepted, 2026-11-26, with the proposed defaults adopted on the operator's behalf ([Decisions](#decisions)). MO-1 to MO-5 are built ([MO-1 as built](#mo-1-as-built), [MO-2 as built](#mo-2-as-built), [MO-3 as built](#mo-3-as-built), [MO-4 as built](#mo-4-as-built), [MO-5 as built](#mo-5-as-built)), with the deviations listed there; the sections below are corrected where the code differs. Roadmap item "Multi-output" ([roadmap.md](../roadmap.md#5-multi-output)). It extends the native backend's output work ([native-backend.md](native-backend.md#5-outputs), [NB-9 as built](native-backend.md#nb-9-as-built)), which lights exactly one connector.

Shingle drives one output. The core window model already holds any number of outputs, each with its own workspaces and tiling trees, but everything above it assumes one: `CompositorState` has a single output rectangle, scale, transform, description and `wl_output` global, IPC reports output id 1 for everything, the frontend draws one scene, nested opens one window and native lights one CRTC. This document decides how several outputs are lit and used at once, as in Wayfire: the output layout and positions, workspace sets and tiling trees per output, focus and pointer movement between outputs, moving views between outputs, unplugging and replugging, sessions, rendering and frame pacing in every backend, the client protocols that name outputs, Xwayland's root, headless outputs created over IPC, and the Wayfire IPC methods and events that involve outputs. It is the brief for the implementation workspaces in [section 12](#12-implementation-plan).

Line numbers refer to branch `shingle-multi-output-design`, cut from `master` at `e64cdbc`.

Sources and dates (all read on 2026-11-26):

- **Wayfire** was read at `/home/karl/dev/wayfire`, commit `b36615a7` (2026-02-22), the same commit the other designs use. Paths below are relative to that checkout.
- **wlroots** was read on master `56f15f8` (2026-11-21), cloned from gitlab.freedesktop.org. Wayfire builds on wlroots, so its output layout, DRM CRTC allocation and screencopy are the behaviour Shingle matches.
- **Xwayland** was read from `hw/xwayland/xwayland-output.c` on the xserver master branch, fetched from gitlab.freedesktop.org.
- **Shingle** was read on this branch: `crates/shingle-core`, `crates/shingle`, `crates/shingle-frontend`, `crates/shingle-nested`, `crates/shingle-native`, `crates/shingle-bin` and `crates/shingle-config`.

## Summary of decisions

| # | Topic | Decision |
| --- | --- | --- |
| 1 | Output model | `CompositorState` keeps a map of outputs keyed by name, each with its rectangle, scale, transform, description, IPC id, `wl_output` global and per-output protocol state. Positions follow Wayfire's `[output:NAME] position`, which gains `auto` (the default) with wlroots' auto-placement. Outputs may overlap or leave gaps, with a warning, as in Wayfire. |
| 2 | Workspace sets | The core separates workspace sets from outputs, as Wayfire does: a workspace set owns a workspace grid with one tiling tree per workspace and is attached to at most one output. Each output shows one set; a set without an output is parked. Sets have Wayfire's indices (lowest free, from 1). |
| 3 | Focus and pointer | One active (focused) output, changed as in Wayfire: by the first button press or a touch down on another output, by focusing a view, by `oswitch` and by IPC, never by pointer motion alone. The pointer moves freely across the layout and is clamped to the nearest point on an output. Keyboard focus follows the active output's current workspace. |
| 4 | Moving views | `move_view_to_output` semantics from Wayfire: same workspace, tiled views join the target workspace's tree, floating geometry scales with the output size and is clamped, fullscreen and snapped views are re-applied on the target. Exposed through `oswitch` bindings, `window-rules/configure-view` with `output_id`, `wsets/send-view-to-wset`, `start_on_output` rules, `xdg_toplevel.set_fullscreen(output)` and drags. |
| 5 | Unplug and replug | Configurable with `output_layout.unplug`: `move` (Wayfire's core: views join the active output), `park` (Wayfire's preserve-output: the set waits, hidden, for its output) and `return` (the default: views join the active output and go back to their trees when the output returns, matched by EDID make, model and serial, else by name). The last output is never removed; it stays described without frames, as NB-9 does today. |
| 6 | Sessions | Snapshot version 2 stores workspace sets with the name and EDID identity of their output and the windows that are away from their set. A start restores each set to its output when that output exists, and otherwise treats it as unplugged under `output_layout.unplug`. Version 1 files still load. |
| 7 | Rendering | One frontend, one scene per output, drawn into one render target per output and clipped to that output, as Wayfire clips each output's views. Frame callbacks are sent by every output a surface is visible on (the first one consumes them); presentation feedback, FIFO and commit-timing follow the surface's primary output, chosen as wlroots' scene does. Nested opens one host window per output; native lights one CRTC per output with wlroots' CRTC matching, combined `TEST_ONLY` modesets and page flips per CRTC; headless and offscreen take `--outputs N`. |
| 8 | Protocols | One `wl_output` global per output with xdg-output, `wl_surface.enter`/`leave` by overlap, a lock surface per output, layer surfaces on their requested or the active output and closed when it goes, screencopy in output-local coordinates scaled to physical pixels, output management with every head in one configuration, power and gamma per output, ext-workspace groups per output and foreign-toplevel `output_enter` for the view's output. |
| 9 | Xwayland | Nothing to drive: Xwayland sizes its root from the outputs' xdg-output geometry (union with origin 0,0) and creates a RandR output per `wl_output`. Shingle keeps layout coordinates as configured, like Wayfire, and warns when an output has a negative position, which X cannot reach. |
| 10 | IPC | Real output ids (monotonic, never reused, a replugged output gets a new one) and wset indices in every reply; six new methods (`wayfire/create-headless-output`, `wayfire/destroy-headless-output`, `stipc/create_wayland_output`, `stipc/destroy_wayland_output`, `wsets/set-output-wset`, `wsets/send-view-to-wset`) and six new events (`output-added`, `output-removed`, `output-gain-focus`, `output-wset-changed`, `view-set-output`, `view-wset-changed`), and Wayfire's payloads for `output-layout-changed` and `wset-workspace-changed`, which Shingle sends in a shorter form today. |
| 11 | Testing | Everything except DRM is verified on this host: headless Shingle with several outputs inside the existing harness namespaces, real clients (`wlr-randr`, `wayland-info`, `grim`, `xrandr`, `shingle-lock`, the probe), nested Shingle with two windows as a client of headless Shingle, and the offscreen backend in the GPU namespace. CRTC allocation, hotplug of a second monitor and flips on two CRTCs need the test machine. |
| 12 | Plan | Seven ordered workspaces, MO-1 to MO-7: core workspace sets and sessions, the compositor's output registry, per-output client protocols, focus and moving views, the frontend's per-output scenes with the offscreen backend, nested windows, and native multi-CRTC. Mirroring is a later, optional MO-8. |

## What exists today

- **Core.** `Shingle` (crates/shingle-core/src/core.rs:225) maps `OutputId` (core.rs:15) to an `OutputState` (core.rs:204) with a workarea, the current workspace and one `TileTree` per workspace. Windows are located by `WorkspaceRef { output, workspace }` (core.rs:79). `add_output` (core.rs:254) and `rename_output` (core.rs:298, NB-9) exist; there is no removal and no way to move all windows of an output elsewhere except the merge inside `rename_output`. `drop_window` (core.rs:589) already moves a window across outputs.
- **Sessions.** `SessionSnapshot` (crates/shingle-core/src/session.rs:30) is a list of `OutputSnapshot`s (session.rs:44) keyed by output name, at `SNAPSHOT_VERSION` 1 (session.rs:18). A session test already round-trips two outputs (session.rs:616).
- **Compositor state.** `CompositorState` (crates/shingle/src/state.rs:161-180) holds one `output: Rect`, `output_scale`, `output_transform`, `output_id`, `output_info`, `output_globals`, `output_power` and `output_management`, and `outputs: HashMap<ObjectId, WlOutput>` (state.rs:197) is the bound `wl_output` resources of that one global. At creation every saved output that is not `HEADLESS-1` is merged into it (state.rs:298-313), so no window is lost on an output that is not shown. About two hundred lines in `crates/shingle/src` read `state.output`, `state.output_id` or `state.output_info` directly, the most in lib.rs (39), shell.rs (38), state.rs (29) and ipc.rs (26).
- **Public API.** `OUTPUT_NAME` is `HEADLESS-1` (crates/shingle/src/lib.rs:148). `Compositor::output` (lib.rs:1214), `set_output` (lib.rs:2921), `set_output_info` (lib.rs:2973, renaming and replacing the global when the name changes, output.rs:300), `frame` and `frame_presented` (lib.rs:1304, lib.rs:1324, every visible surface gets its callback), `Event::OutputChanged` (lib.rs:846) and `Event::CaptureRequested` (lib.rs:911) all mean the one output. `OutputInfo` (crates/shingle/src/output.rs:64) and the configured `Placements` (output.rs:223) are already per name.
- **Protocols.** Layer surfaces ignore the output in `get_layer_surface` (crates/shingle/src/layer_shell.rs:384) and shrink the one workarea (layer_shell.rs:173, 270). A second lock surface is refused as a duplicate (crates/shingle/src/shell.rs:2505), although `shingle-lock` already creates one per `wl_output` (crates/shingle-lock/src/main.rs:334). Screencopy captures the one output rectangle and intersects regions with it in layout coordinates (crates/shingle/src/screencopy.rs:355-369). `wl_surface.enter` is sent for every output the client bound (state.rs:1492). ext-workspace (crates/shingle/src/ext_workspace.rs:76-83), foreign-toplevel (crates/shingle/src/foreign_toplevel.rs:49) and xdg-output (crates/shingle/src/xdg_output.rs:24) track the one output.
- **IPC.** `output_json` and `wset_json` (crates/shingle/src/ipc.rs:705, 716) report id 1 and index 1, views report `output-id` 1 (ipc.rs:791), and `configure-view` refuses any other output id (ipc.rs:2090). `output-layout-changed` carries `[output_json]` and `wset-workspace-changed` only `wset` (ipc.rs:680-687), where Wayfire sends the layout configuration and old and new workspaces (plugins/ipc-rules/ipc-events.hpp:296-331, 469-481).
- **Frontend.** `Frontend` (crates/shingle-frontend/src/frontend.rs:74) has one pointer position, zoom, Scale, Expo and workspace transition, and `render` (crates/shingle-frontend/src/frontend/scene.rs:280) draws everything into one `RenderTarget` (crates/shingle-frontend/src/render_target.rs:14). `Host::request_redraw` (crates/shingle-frontend/src/host.rs:19) has no output.
- **Nested.** One winit window (`WindowHost`, crates/shingle-nested/src/lib.rs:175), sized by `initial_window_size` (lib.rs:228); closing it quits (lib.rs:436).
- **Native.** `Display` (crates/shingle-native/src/display.rs:278) drives one `Lit` output (display.rs:167) with one `FrameScheduler` (crates/shingle-native/src/pacing.rs:53). Each lit output's modeset turns every other CRTC and plane off (`Card::teardown`, crates/shingle-native/src/kms/output.rs:219). `outputs::choose` (crates/shingle-native/src/outputs.rs:86) picks one connector.
- **Headless and offscreen.** `shingle --headless` paces frames for one output by its refresh rate (crates/shingle-bin/src/headless.rs:99); the offscreen backend draws one `OffscreenOutput` (crates/shingle-bin/src/offscreen.rs:233).
- **Configuration.** `ShingleConfig::outputs` keeps one entry per `[output:NAME]` (NB-9), but `OutputConfig` (crates/shingle-config/src/output.rs:237) has only a fixed `x`, `y` defaulting to 0,0 (output.rs:251), and the importer sets them only when `position` is a pair (crates/shingle-config/src/wayfire.rs:696). Two outputs without positions would both sit at the origin.

## 1. Output model and layout

### What Wayfire does

- An output's position is `[output:NAME] position`, default `auto` (metadata/output.xml:8). Fixed-position outputs are added to the wlroots layout first, then automatic ones (src/core/output-layout.cpp:1710-1745). wlroots places automatic outputs left to right in the order they were added, starting at the right edge of the rightmost fixed output and at its `y`, or at 0,0 when there is none (wlroots types/wlr_output_layout.c, `output_layout_reconfigure`).
- An output's layout size is its mode divided by its scale, with width and height swapped for 90° and 270° transforms (`calculate_geometry_from_state`, output-layout.cpp:1452-1467).
- Overlapping and disjoint fixed outputs are only warned about (`test_configuration`, output-layout.cpp:1578-1618): "pointer will not be movable between them".
- Output ids are Wayfire object ids, a global counter shared with views (src/core/object.cpp:72-73), so a re-added output gets a new id. IPC finds outputs by id (plugins/ipc/wayfire/plugins/ipc/ipc-helpers.hpp:128).
- The first output added is focused; later ones are not, unless the focused output was the NOOP output (output-layout.cpp:660-667).
- When every real output is gone, Wayfire adds a 1280x720 `NOOP-1` headless output to hold views (`ensure_noop_output`, output-layout.cpp:1281) and removes it a second after a real output returns (output-layout.cpp:1773).

### Decision

**State.** `CompositorState`'s single-output fields become `outputs: Outputs`, a map from output name to `OutputState` in the order outputs were added, plus `active_output: String`. Each `OutputState` holds:

- the `OutputInfo` (name, make, model, serial, physical size, subpixel, modes, refresh), the logical `Rect` in the layout, scale and transform;
- the IPC id (`u32`, from a counter starting at 1 that is never reset or reused);
- the attached workspace set ([section 2](#2-workspace-sets-and-tiling-trees));
- the `wl_output` global and its generation, the bound `wl_output` and xdg-output resources, the output-management head, power and gamma controls, and the ext-workspace group;
- the layer surfaces on it and its exclusive zones, from which its workarea follows;
- its kind: `Backend` (a connector, nested window or the headless backend's outputs) or `Virtual` (created by `wayfire/create-headless-output`).

The per-output code that exists (output.rs, output_management.rs, output_power.rs, gamma_control.rs, xdg_output.rs, ext_workspace.rs) takes an output name instead of reading the one output. `state.output` and friends are replaced by helpers `state.output(name)` and `state.active()`; the roughly two hundred call sites ([What exists today](#what-exists-today)) are moved in MO-2 mostly mechanically, choosing in each case whether the code means the window's own output, the active output or the whole layout.

**Public API.** A backend adds and removes outputs and the frontend names the output it draws or presents:

```rust
// shingle, as built in MO-2 (see MO-2 as built)
pub struct OutputHandle(String);        // the output's name
Compositor::add_output(&mut self, info: OutputInfo, physical: (u32, u32))
  -> Result<OutputHandle, Error>;
Compositor::remove_output(&mut self, output: &OutputHandle)
  -> Result<(), Error>;
Compositor::describe_output(&mut self, output: &OutputHandle, OutputInfo);
Compositor::set_output_size(&mut self, output: &OutputHandle, (u32, u32))
  -> Result<(), Error>;
Compositor::outputs(&self) -> Vec<OutputView>;  // name, rect, scale, ...
Compositor::output_at(&self, x: f64, y: f64) -> Option<OutputHandle>;
Compositor::active_output(&self) -> OutputHandle;
Compositor::primary_output(&self) -> OutputHandle;  // the first added
Compositor::frame_presented(&mut self, &OutputHandle, PresentInfo);
Compositor::visible_surfaces_on(&self, &OutputHandle)
  -> Vec<SurfaceSnapshot>;
Compositor::restore_outputs(&mut self);
Compositor::take_output_requests(&mut self) -> Vec<OutputRequest>;
pub enum Event { OutputAdded { output }, OutputRemoved { output },
  OutputChanged { output, rect }, .. }
// later: Event::OutputFocused (MO-4), CaptureRequested's output (MO-3,
// MO-5), CreateHostOutput and DestroyHostOutput (MO-6)
```

`Compositor::new` still creates `HEADLESS-1`, so embedders and tests that never add an output keep working; a backend that knows its outputs at start renames it to the first one before clients connect (the NB-9 rename, `output::rename`, output.rs:300) and adds the others. `set_output_info` with a new name is a rename only before clients have connected; afterwards a backend removes the old output and adds the new one, which runs the unplug rules of [section 5](#5-unplug-and-replug).

**Positions.** `OutputConfig` gains `position: auto | {x, y}`. YAML accepts `position: auto` and `position: {x: 1920, y: 0}`; the old `x` and `y` keys still mean a fixed position, so existing files keep their meaning. The default is `auto`. The Wayfire importer maps `position = auto` (or no `position`) to `auto` and `X,Y` to fixed; the i3 importer maps sway's `pos X Y` to fixed. For one output nothing changes, since `auto` with no fixed outputs is 0,0. The layout is a pure function `layout(outputs in add order, configured positions, sizes) -> rects` in shingle-config or shingle-core, ported from wlroots' `output_layout_reconfigure`: fixed outputs where configured, automatic ones in add order from the right edge of the rightmost fixed output, at its `y`. It runs whenever an output is added, removed, resized, rescaled or rotated, or the config is reloaded, and an output whose rectangle changed sends `wl_output.geometry`, xdg-output `logical_position` and `done` and an `output-layout-changed` event. Overlap and gaps between fixed outputs are logged, not refused.

**No outputs.** The last output is never removed. When a backend reports that its last output is gone (native with every monitor unplugged, nested with its last window closed is a quit instead), the output stays described with its last size, draws nothing and sends no frame callbacks, exactly as NB-9 handles it today (crates/shingle-native/src/display.rs, "No output" in [NB-9 as built](native-backend.md#nb-9-as-built)). A NOOP output is not needed: Wayfire adds one because a wlroots view needs some output, while Shingle's windows live in workspace sets that keep their trees without one. When a new output arrives while only such a dark output remains, the new output is added and the dark one is then removed under the unplug rules, so its windows join the new output (and return if their own monitor comes back).

**IPC ids.** Ids come from one counter, starting at 1, never reused while Shingle runs, so a replugged monitor gets a new id as in Wayfire. Output names are unique among live outputs.

### Rejected alternatives

- **Keeping the output fields and adding a list of "extra" outputs.** Every per-output protocol would have two code paths. Rejected.
- **Normalising the layout so that its top-left corner is 0,0.** It would help Xwayland ([section 9](#9-xwayland)), but clients, IPC users and `wlr-randr` would see positions other than the configured ones, and Wayfire does not do it. Rejected; Shingle warns instead.
- **Refusing overlapping or disjoint configurations.** Wayfire accepts them, and a layout is often disjoint while monitors are still being plugged in. Rejected.

## 2. Workspace sets and tiling trees

### What Wayfire does

- Every output shows exactly one workspace set (`wset`), a grid of workspaces with its own views; a set can be attached to at most one output, or to none (src/output/workspace-impl.cpp:323, `attach_to_output`). The tile plugin keeps one tiling tree per workspace of each set (plugins/tile/tile-wset.hpp:54-160) and sizes the roots from the attached output's workarea, or from the set's last output geometry while it is detached (tile-wset.hpp:136-160).
- Sets have an index, chosen as the lowest free index from 1 (`choose_lowest_free_wset_id`, workspace-impl.cpp:792). Each new output gets a new set.
- The `wsets` plugin (plugins/single_plugins/wsets.cpp) binds set N to the active output (`wsets_bindings`) and sends the focused view to set N (`send_window_bindings`), and offers `wsets/set-output-wset` and `wsets/send-view-to-wset` over IPC (wsets.cpp:34-35, 71-98). Selecting a set that another output shows gives that output a new empty set (`select_workspace`, wsets.cpp:222-252); empty unattached sets are destroyed (`cleanup_wsets`, wsets.cpp:158). A label "Workspace set N" is shown for `label_duration` (wsets.cpp:175-200). The plugin is not in Wayfire's default plugin list (metadata/core.xml:10).
- The workspace grid size is `core/vwidth` and `core/vheight`, the same for every set.

### Decision

The core takes Wayfire's model: workspace sets are separate from outputs. `shingle-core` gets

```rust
pub struct WsetId(pub u32);             // Wayfire's wset index, from 1
pub struct OutputIdentity { pub make: String, pub model: String,
  pub serial: Option<String> }
// WorkspaceRef stays { output, workspace }; WindowInfo gains
// wset: WsetId and away_from: Option<WsetId> (see MO-1 as built).

Shingle::create_wset(&mut self, workarea: Rect) -> Result<WsetId>;
Shingle::attach_wset(&mut self, wset: WsetId, output: Option<OutputId>,
  identity: Option<OutputIdentity>) -> Result<()>;
Shingle::wset_of(&self, output: &OutputId) -> Option<WsetId>;
Shingle::wsets(&self) -> impl Iterator<Item = (WsetId, &WsetInfo)>;
Shingle::set_wset_workarea(&mut self, wset: WsetId, workarea: Rect)
  -> Result<()>;
Shingle::move_window_to_wset(&mut self, window: WindowId, wset: WsetId,
  workspace: WorkspaceId) -> Result<Vec<WindowId>>;
Shingle::transfer_wset(&mut self, from: WsetId, to: WsetId,
  remember: bool) -> Result<Vec<WindowId>>;
Shingle::return_windows(&mut self, wset: WsetId) -> Result<Vec<WindowId>>;
Shingle::destroy_wset(&mut self, wset: WsetId) -> Result<()>;
Shingle::find_parked_wset(&self, name: &OutputId,
  identity: Option<&OutputIdentity>) -> Option<WsetId>;
```

`OutputState` (core.rs:204) becomes `WsetState { workarea, current_workspace, workspaces, output: Option<OutputId>, home: Option<OutputId>, identity: Option<OutputIdentity>, away }` (`home` is `None` only for a set created parked that was never shown). `home` is the name of the output the set was last attached to and `identity` that output's EDID make, model and serial; both are kept while the set is parked, so a returning monitor finds its set. The existing output-keyed calls (`current_workspace(&OutputId)`, `tree(&OutputId, ..)`, `attach_window(&OutputId, ..)`) stay as thin wrappers that resolve the output's attached set, so most of `shingle` reads as before; only code that must work on parked sets uses `WsetId`.

- **Allocation.** A new output first looks for a parked set to take ([section 5](#5-unplug-and-replug)); otherwise it gets a new set with the lowest free index, as Wayfire does.
- **Trees.** Each set keeps one `TileTree` per workspace, laid out in the attached output's workarea; a parked set keeps its last workarea, so its trees keep their geometry and are laid out again when it is attached to an output of another size.
- **The grid** stays global (`workspace_width`, `workspace_height`).
- **wsets plugin.** MO-4 implements `wsets/set-output-wset` and `wsets/send-view-to-wset` and imports `[wsets] wsets_bindings` and `send_window_bindings` (`wset_N` and `send_to_wset_N` entries) with Wayfire's rules: a set shown elsewhere is taken from that output, which gets a new empty set; an unattached empty set is destroyed, unless a return record ([section 5](#5-unplug-and-replug)) still refers to it. The "Workspace set N" label is drawn by the frontend like Scale's titles, top-left on the output, for `label_duration` (default 2 s).

### Rejected alternatives

- **Keeping workspaces keyed by output name.** It is what the core does now, and it matches Wayfire as long as each output shows its own set. But a parked set and the `wsets` plugin both need a set without an output or on another output, and naming a set after an output it no longer shows makes sessions and IPC misleading. Rejected; the output-keyed calls stay as wrappers.
- **Per-set grid sizes.** Wayfire has none. Rejected.

## 3. Focus, pointer and input

### What Wayfire does

- The seat has one active output (`seat_t::focus_output`, src/core/seat/seat.cpp:49-70). Changing it refocuses: keyboard focus goes to the topmost view of that output's current workspace, and `output-gain-focus` is emitted.
- The active output changes on the first pressed button, to the output under the cursor (src/core/seat/pointer.cpp:217-225), on a touch down (src/core/seat/touch.cpp:223), on a tablet tool event (src/core/seat/tablet.cpp:326), when a view is focused (its output, src/core/window-manager.cpp:164), through `oswitch` (plugins/single_plugins/oswitch.cpp:145-153), during a move drag across outputs (plugins/common/move-drag-interface.cpp:562) and a vswipe (plugins/single_plugins/vswipe.cpp:207). Pointer motion alone does not change it.
- When the active output goes, the next output in layout order becomes active (output-layout.cpp:691-697, `get_next_output` at 1915).
- `oswitch` (metadata/oswitch.xml, oswitch.cpp) has `next_output` (default `<super> KEY_O`), `next_output_with_win` (`<super> <shift> KEY_O`), `prev_output`, `prev_output_with_win`, and `left_output`, `right_output`, `above_output`, `below_output`. Next and previous wrap in layout order (oswitch.cpp:36-54); a direction picks the output whose centre lies that way with at least 20 % overlap on the other axis and the best overlap-to-distance score (oswitch.cpp:56-136). Switching warps the pointer into the target output (`ensure_pointer`, oswitch.cpp:138-154).
- The cursor is a `wlr_cursor` on the output layout: relative motion is clamped to the nearest point of any output. Absolute devices (touch, tablets, the nested pointer) are mapped to `[input-device:NAME] output`, by default the output the device names itself (the nested window's output), else the whole layout (`map_to_output`, src/core/seat/seat.cpp:678-702).
- Bindings, Scale, Expo, vswitch and the other per-output plugins act on the active output; IPC activators accept `output_id` or `output-id` and default to the active output (plugins/ipc/wayfire/plugins/ipc/ipc-activator.hpp:86-100).

### Decision

Shingle does the same:

- `state.active_output` is set by the events above (button press, touch down, view focus including xdg-activation and IPC `focus-view`, `oswitch`, drags across outputs, IPC with an explicit output) and by nothing else. A change emits `Event::OutputFocused` and the IPC event `output-gain-focus`, and refocuses the active output's current workspace through the existing focus code (`focused_window`, crates/shingle/src/input.rs:1681). Layer surfaces with keyboard interactivity, lock surfaces and grabs keep their current precedence.
- Workspace bindings, `vswitch` IPC without `output-id`, Scale, Expo, Show Desktop, `wm-actions` without a view and new windows use the active output.
- The pointer position stays in layout coordinates. Relative motion is clamped to the closest point on any output (wlroots' `wlr_output_layout_closest_point`), so the pointer crosses between touching outputs and cannot leave the layout. Pointer constraints keep working per surface.
- Absolute input carries the output it maps to: `InputEvent::PointerMotionAbsolute` and `Touch` gain an `output: Option<OutputHandle>`, with the position relative to that output. Nested passes its window's output; native maps libinput's normalised coordinates to `input_devices.NAME.output` when configured (imported from `[input-device:NAME] output`), else to the layout's bounding box, as wlroots does. Native lights one output until MO-7 and passes `None` until then ([MO-4 as built](#mo-4-as-built)).
- `oswitch` is a new config section with Wayfire's eight activators and defaults, imported from `[oswitch]`; the direction scoring is a pure function next to the layout function. Switching warps the pointer to the target output's centre unless it is already inside, as `ensure_pointer` does.
- Each output draws its own cursor image when the pointer is inside it; native gets a cursor plane per CRTC in NB-10.

### Rejected alternatives

- **Focus follows the pointer between outputs.** Moving the mouse to read another screen would move keyboard focus and the target of every workspace binding. Wayfire changes outputs only on a click. Rejected; it could become an option later.

## 4. Moving views between outputs

### What Wayfire does

`wf::move_view_to_output` (src/core/core.cpp:618-711) with `RECONFIGURE` and `SAME_WORKSPACE`, which `oswitch` and the unplug transfer use:

- The view keeps its workspace coordinates on the new set (clamped to the grid; core.cpp:676-683).
- A fullscreen view is made fullscreen on the new output and a tiled-edges (snapped) view is tiled again there; a floating view keeps its position scaled by the ratio of the output sizes and is clamped into the new workarea (core.cpp:644-698). Dialogs move with their parent (core.cpp:700-708); moving a dialog alone is not allowed (core.cpp:625), so `oswitch` moves the topmost parent (oswitch.cpp:160).
- The view is focused if the new output is the active one (core.cpp:667-670), and `view-set-output` and `view-wset-changed` are emitted.
- The tile plugin detaches a tiled view from the old set's tree before the move and attaches it to the new set's tree after it (plugins/tile/tile-plugin.cpp:502-538), so it stays tiled but its place in the tree is new.

The callers are `oswitch` `*_with_win`, `window-rules/configure-view` with `output_id` (plugins/ipc-rules/ipc-rules.cpp:291-300; reconfiguring unless a geometry is given), `wsets/send-view-to-wset`, the window rule `start_on_output NAME` (connector name, or make and model with an optional serial; plugins/window-rules/view-action-interface.cpp:526-570), `xdg_toplevel.set_fullscreen(output)` (src/core/window-manager.cpp:259), and the move plugin when a drag ends on another output. A new view maps on its parent's output, else on the output it already has, else on the active output (`adjust_view_output_on_map`, src/view/view-impl.cpp:423-440).

### Decision

`Shingle::move_window_to_wset` does the tree part (detach from the source tree, append to the target workspace's tree with its leaf state, children with their parent), and `output::move_window_to_output(state, window, output, geometry)` in `shingle` (crates/shingle/src/output/moving.rs; the design named it `shell::move_window_to_output` with a `reconfigure` flag, see [MO-4 as built](#mo-4-as-built)) does Wayfire's geometry part: floating geometry scaled by the size ratio relative to the output origin and clamped into the target workarea, fullscreen and snapped states re-applied, `wl_surface.enter`/`leave` and foreign-toplevel `output_enter`/`output_leave` updated, `view-set-output` and `view-wset-changed` sent, and focus moved when the target is active. Every caller listed above uses it, and IPC `configure-view` stops refusing other output ids. Shingle's drag drop across outputs (`drop_window`, core.rs:589) already crosses sets in the core; MO-4 makes the frontend's drag change the active output while it crosses, as Wayfire's move plugin does, and drop into the target output's tree or floating layer. New windows follow `adjust_view_output_on_map`: parent's set, else a pending `set_fullscreen` output or `start_on_output` rule, else the active output.

### Rejected alternatives

- **Keeping a tiled window's position in the tree** (inserting it at the same path in the target tree). The target tree has another shape, and Wayfire appends. Rejected.

## 5. Unplug and replug

### What Wayfire does

- **Without `preserve-output`**, the default: removing an output emits `output-pre-remove`, moves focus to the next output and moves every view of the removed output's set to the active output with `RECONFIGURE | SAME_WORKSPACE` (`transfer_views`, output-layout.cpp:252-298, called at 704). Views that are not in a set are given the new output, except layer-shell surfaces (`VIEW_ROLE_DESKTOP_ENVIRONMENT`), which are closed (output-layout.cpp:285-295). The emptied set goes away with the output. A replugged output gets a new, empty set; nothing returns.
- **With `preserve-output`** (plugins/single_plugins/preserve-output.cpp; not in the default plugin list): on `output-pre-remove` the output's set is detached and kept under the identifier `make|model|serial`, the output gets a dummy empty set, so nothing is transferred and the views are hidden. When an output with the same identifier is added, its saved set is attached again, unless the set was meanwhile attached to another output (`wsets` plugin), and the output is focused again if it was focused when it went and `last_output_focus_timeout` (default 10 s) has not passed. Headless outputs (NOOP, IPC-created) are skipped and keep the default behaviour.
- **Disabling** an output through wlr-output-management or `mode = off` is the same removal (`apply_configuration` with `OUTPUT_IMAGE_SOURCE_NONE`, output-layout.cpp:1700-1708), and outputs are disabled before others are enabled because of CRTC limits (output-layout.cpp:1658-1669).

### Decision

A new option `output_layout.unplug` chooses what happens to a removed output's set, for unplugged monitors, disabled heads, `mode: off` on reload, closed nested windows and destroyed IPC outputs:

- **`move`** is Wayfire's default exactly: every window of the set moves to the active output's set as in [section 4](#4-moving-views-between-outputs) (same workspace, tiled windows appended), the set is destroyed, and nothing comes back.
- **`park`** is Wayfire's `preserve-output` exactly: the set is detached with its trees, its windows stay unmapped from every output (hidden, but alive and in the session), and the set is attached again when its output returns. Imported as `park` when Wayfire's `core/plugins` lists `preserve-output`, together with `[preserve-output] last_output_focus_timeout`.
- **`return`**, the default, combines both: windows move to the active output as with `move`, so nothing becomes unreachable, but the set is kept parked with a copy of its trees, and each moved window remembers that it is away from it. When the output returns, every window still away from that set (not closed, and not moved by the user since) is detached from where it is and put back into the set's trees at its old place, and the set is attached to the output again. Windows that were closed or moved meanwhile are pruned from the copy, as a restored session prunes missing windows.

Details that apply to all three:

- **Matching a returning output.** A parked set is taken by a new output with the same EDID identity (make, model and serial, as preserve-output's `make_output_identifier`), provided the identity has a serial or is unique among parked sets; otherwise by the same output name (connector). Identity first means a monitor moved from `DP-1` to `DP-2` or a dock that renumbers connectors gets its windows back; the name fallback covers monitors without an EDID serial, two identical monitors and nested and headless outputs.
- **Away records.** `WindowRecord` gains `away_from: Option<WsetId>`, set when an unplug moves the window and only if not already set, so a window moved twice (its monitor went, then the one it moved to) still returns to its first set. Any move by the user or a client clears it. A parked set under `return` holds the tree copy as `Vec<WorkspaceSnapshot>`, reusing the session format.
- **Focus.** The active output, if removed, passes to the next output in add order. A returning output is focused again if it was active when it went, within `last_output_focus_timeout`.
- **Layer surfaces** on a removed output are closed (`zwlr_layer_surface_v1.closed`), as in Wayfire; panels re-create theirs on the new `wl_output`.
- **Lock surfaces** on a removed output are destroyed with it; the session stays locked.
- **Override-redirect X windows and other windows without a set** move to the active output.
- **IPC-created headless outputs** always use `move`, as preserve-output skips headless outputs.
- **Last output.** The last output is not removed ([section 1](#1-output-model-and-layout)), so its set never needs a target.
- **Events.** A removal sends `output-removed` after the transfer, `view-set-output` and `view-wset-changed` for each moved window and `output-wset-changed` for an output whose set changed; a return sends `output-added`, then the same window events.

### Rejected alternatives

- **`move` as the default, as Wayfire.** Docking and undocking a laptop would pile every window onto the internal panel and leave it there. Proposed default `return` instead; this is [open question 1](#open-questions).
- **`park` as the default.** Windows on an unplugged monitor would be unreachable, possibly for days, which breaks Shingle's rule that no window is lost on an output that is not shown (state.rs:302-304). Rejected as default, kept as an option.
- **Keeping a window in two trees** (its home set and the one it moved to). The core's invariant is one leaf per window record (core.rs:220-224); a tree copy for the return is simpler and is exactly the session restore path. Rejected.

## 6. Sessions

### Decision

- **Format.** `SNAPSHOT_VERSION` becomes 2. `OutputSnapshot` becomes `WsetSnapshot { wset: WsetId, output: Option<OutputId>, home: Option<OutputId>, identity: Option<OutputIdentity>, workarea, current_workspace, workspaces, away: Option<Vec<WorkspaceSnapshot>> }`, `WindowSnapshot` gains `away_from: Option<WsetId>` and the snapshot `active_output: Option<OutputId>`. `output` records which output showed the set, so that a snapshot restores to exactly the model it was taken from. A version 1 file loads as one set per saved output, with `home` the output name and no identity, so every saved session keeps working; files are always written as version 2.
- **Saving** stores every set, attached or parked, so a session saved while undocked still knows where windows go when the monitor returns, and the active output's name, so a start focuses it again.
- **Restoring.** The restored sets are attached when outputs exist, not when the `Compositor` is created: `Compositor::new` currently merges every other saved output into `HEADLESS-1` at once (state.rs:298-313). That merge moves to `Frontend::start_clients` (crates/shingle-frontend/src/frontend.rs:256), which runs after the backend has added its outputs. There each saved set is matched to an output as in [section 5](#5-unplug-and-replug); an output without a saved set gets a new one, and saved sets without an output are handled by `output_layout.unplug` as if the output had just been unplugged (merged under `move`, parked under `park`, merged with away records under `return`). A native session restored in nested therefore shows its windows in the nested window and, under `return`, gives them back to their monitors on the next native start.
- **Placeholders** keep working per set: saved applications relaunch and reconcile into the set and workspace they were saved in, including parked sets.

### Rejected alternatives

- **One session file per output configuration** (as kanshi profiles). Windows are one list whatever is plugged in, and the away records already give each window its place. Rejected.

## 7. Rendering and frame pacing

### What Wayfire does

- Every output has its own render manager and frame loop (src/output/render-manager.cpp), repainting on its own `frame` event with its own damage. Each output renders the scene clipped to its own layout rectangle: a set's views appear only on its output (`output_node_t::limit_region` and `gen_render_instances`, src/core/scene.cpp:306-320, 508-525), unless `workarounds/remove_output_limits` is set.
- A surface gets `frame_done` from each output that rendered it (src/view/wlr-surface-node.cpp:217-236, `on_frame_done` at 240); since frame callbacks are one-shot, the first output to present consumes them. wlroots' scene chooses one primary output per buffer, the one with the largest share above 10 % of its visible area (types/scene/wlr_scene.c:496-570), for presentation feedback and scanout; Wayfire's surfaces enter and leave outputs as their visible region changes (wlr-surface-node.cpp:440-480).
- Nested Wayfire is wlroots' Wayland backend: one host toplevel per output, `WLR_WL_OUTPUTS` outputs at start (wlroots backend/backend.c:193) and more through `stipc/create_wayland_output`, which only works when nested (plugins/ipc/stipc.cpp:371-386); `stipc/destroy_wayland_output` destroys one by name (stipc.cpp:388-410).
- `wayfire/create-headless-output` adds a wlroots headless output of the given size to a headless backend it creates on first use, and `wayfire/destroy-headless-output` destroys only outputs created that way (plugins/ipc-rules/ipc-utility-methods.hpp:66-119). They work on every backend.
- On DRM, wlroots allocates CRTCs for all enabled connectors at once with a matching that maximises enabled outputs and keeps each enabled output's CRTC (`realloc_crtcs`, backend/drm/drm.c:1407, and `match_connectors_with_crtcs`, backend/drm/util.c:239), and each output page-flips on its own CRTC.

### Decision: the frontend

- **One scene per output.** `Frontend::render(host, output, target)` builds the scene of one output, in that output's logical coordinates, from `Compositor::visible_surfaces(output)`: the windows of its attached set, its layer surfaces, override-redirect X windows and the drag icon that intersect it, and the cursor if inside. Windows are clipped to their own output, as in Wayfire; `workarounds/remove_output_limits` is imported as `behavior.remove_output_limits` to turn that off. A window being dragged is drawn on every output it crosses.
- **Per-output frontend state.** Zoom, the workspace transition and damage are per output; Scale, Expo and the switchers run on one output at a time, the active one when they start, which is what Wayfire's per-output plugins do when started from a binding ([open question 7](#open-questions)).
- **Host seam.** `Host::request_redraw(output)` names the output, and the host keeps one `RenderTarget` (render_target.rs:14) per output on the shared `GpuContext` (NB-1). Damage marks every output a changed surface intersects. As built, `Host::warp_pointer` names its output too, with the position relative to it ([MO-5 as built](#mo-5-as-built)).
- **Frame callbacks.** `Compositor::frame_presented(output, info)` releases frame callbacks of surfaces visible on that output; a surface on two outputs gets its callback from whichever presents first, as in Wayfire. Hidden surfaces keep their callbacks, as now.
- **Primary output.** Each surface has a primary output, chosen as wlroots' scene does: the output with the largest overlap above 10 % of the surface's visible area. Its frames deliver `wp_presentation` feedback (with `sync_output` that output's `wl_output`), release FIFO barriers and commit-timing deadlines, and set the refresh rate a surface's timing uses. A surface visible nowhere keeps the last primary output's timing.
- **Captures.** `Event::CaptureRequested` carries its output; the frontend draws that output's scene for it.

### Decision: nested

Nested opens one host window per output, as wlroots' Wayland backend does. `WindowHost` becomes a map from winit `WindowId` to an output, a window and its renderer surface; winit 0.30 supports several windows in one event loop. Each window's size sets its output's size, each window's input is in its output's coordinates (`output` set on absolute events), and each window presents and calls `frame_presented` for its own output. The number of windows at start is `--outputs N` (default 1), or `WLR_WL_OUTPUTS` when the option is absent, for scripts written for Wayfire. `stipc/create_wayland_output` opens another window (`Event::CreateHostOutput`) and `stipc/destroy_wayland_output` closes one. Closing a window removes its output under `output_layout.unplug`; closing the last one quits, as now. Outputs are named `HEADLESS-1`, `HEADLESS-2`, and so on, so existing configs and sessions that name `HEADLESS-1` keep working ([open question 2](#open-questions) asks whether to use Wayfire's `WL-N`).

### Decision: native (extends NB-9)

- **Display.** `Display`'s single `lit: Option<Lit>` becomes `lit: Vec<Lit>`, ordered as outputs were added, each `Lit` with its connector, CRTC, primary plane, mode, blob, scanout buffers, `FrameScheduler` and presentation `Sequence`. This is the per-connector model [section 5 of the native design](native-backend.md#5-outputs) planned and NB-9 deferred.
- **Which connectors are lit.** Every connected connector that is not configured `off`, in priority order: outputs already lit, then connectors named in `outputs` in config order, internal panels, then card order (NB-9's `choose`, outputs.rs:86, generalised to a list). If there are more connectors than CRTCs, the lowest-priority ones stay dark and are listed in the log and in output management as disabled heads.
- **CRTCs.** A port of wlroots' `match_connectors_with_crtcs` (backend/drm/util.c:124-260) as a pure function over `possible_crtcs`: maximise the lit connectors, prefer each connector's current CRTC, and never move the CRTC of a lit output; if that is impossible, keep the old configuration, as `realloc_crtcs` does (drm.c:1483-1500).
- **Modesets.** All lit outputs are set in one atomic request with `TEST_ONLY` first and then one blocking `ALLOW_MODESET` commit, with every unused CRTC and plane turned off in the same request; `Card::teardown` (kms/output.rs:219) therefore turns off only what no lit output uses. If the combined test fails, the lowest-priority new output is dropped and the test repeated, so a bandwidth limit dims one monitor rather than all; mode changes through output management test the whole new configuration the same way. Disabling heads comes before enabling others, as in Wayfire.
- **Flips.** Each output flips on its own CRTC with `NONBLOCK | PAGE_FLIP_EVENT`; `PageFlipEvent::crtc` routes the event to its `Lit`, and `Display::dispatch` returns `(output, PresentInfo)` pairs that the host passes to `frame_presented`. A modeset or hotplug waits until no flip is pending on any CRTC, as NB-9 waits for the one. The host renders each output whose scheduler is ready and that has damage, one after another on the one wgpu device.
- **Hotplug** diffs connectors as NB-9 does and adds or removes compositor outputs instead of relighting the one; DPMS and wlr-output-power are per output, and idle DPMS turns all of them off.
- **Later NB items.** NB-8's GBM scanout and NB-10's cursor plane, VRR and tearing are per `Lit` already in their designs; they need no structural change.

### Decision: headless and offscreen

- `shingle --headless` and `--backend offscreen` take `--outputs N` (default 1; `WLR_HEADLESS_OUTPUTS` when the option is absent), named `HEADLESS-1` to `HEADLESS-N`, each paced by its own timer at its refresh rate (headless.rs:99, offscreen.rs:233) and sized and placed by its `[output:HEADLESS-N]` entry.
- **IPC headless outputs** (`wayfire/create-headless-output` with `width` and `height`) are `Virtual` outputs owned by the compositor and work in every backend, as in Wayfire. They continue the `HEADLESS-N` numbering (never reused), are paced by a frontend timer at 60 Hz that `Frontend::next_deadline` reports, and are drawn into an offscreen target only when a screencopy asks for them, since nobody can see them otherwise; frame callbacks are sent at the refresh rate either way. In the headless backend they are paced like its own outputs. `wayfire/destroy-headless-output` (by `output` name or `output-id`) refuses outputs it did not create, with Wayfire's error text.

### Rejected alternatives

- **One nested window showing every output side by side.** Outputs could not have their own size, scale, transform or pacing, and it is not what Wayfire does. Rejected.
- **One frontend per output.** Windows, drags, Scale and sessions span outputs, and the frontend owns the `Compositor`. Rejected.
- **Separate modeset commits per output.** A configuration that passes output by output can fail as a whole (shared bandwidth, CRTC routing), and the kernel tests it as a whole anyway. Rejected; page flips stay per output.
- **Drawing windows on every output they overlap.** It differs from Wayfire, makes a tiled window on one monitor bleed onto the next, and needs a window to be in two outputs' scenes. Rejected as default; imported as `remove_output_limits`.

## 8. Client protocols per output

- **`wl_output`.** One global per output, version 4, with geometry, mode, scale, name and description per output; the generation logic and the 5 s retired-global delay of NB-9 (output.rs:34-38, 254-298) apply per output. Removing an output disables its global, sends `leave` for every surface on it and finishes its xdg-outputs, heads, power and gamma controls and workspace groups, as `output::rename` does now (output.rs:300-345).
- **xdg-output** reports each output's logical position and size. Xwayland depends on it ([section 9](#9-xwayland)).
- **`wl_surface.enter` and `leave`** follow the outputs each surface is visible on, as Wayfire's visibility tracking does, recomputed once per turn, instead of entering every bound output on every commit. Since windows are clipped to their own output, a toplevel is on its set's output while that output shows its workspace; popups and subsurfaces follow their parent ([MO-3 as built](#mo-3-as-built)).
- **Preferred scale.** `wp_fractional_scale_v1.preferred_scale` and `wl_surface.preferred_buffer_scale` use the scale of the output the window belongs to (its set's output), as Wayfire uses the view's output; layer and lock surfaces use their output's scale; X11 windows are unaffected (XW-7).
- **Layer shell.** `get_layer_surface` places the surface on the requested output, else on the active output (src/view/layer-shell/layer-shell.cpp:433-441). Anchors, exclusive zones and the workarea are per output; a set's trees use its output's workarea. A layer surface whose output goes is sent `closed`.
- **Session lock.** One lock surface per output, as the protocol requires and `shingle-lock` already does; `duplicate_output` is raised only for a second surface on the same output (fixing shell.rs:2505). An output without a lock surface, including one added while locked, shows the background colour and receives no input until its surface arrives. Removing an output destroys its lock surface's role but keeps the session locked. `locked` is sent once every output has a lock surface whose buffer covers it opaquely at its size; today that check covers the one output (shell.rs:680-692).
- **Screencopy.** `capture_output` captures that output; `capture_output_region` takes the region in the output's logical coordinates, clipped to it, and the buffer is in physical pixels, scaled and transformed as wlroots does (types/wlr_screencopy_v1.c:576-595). Before MO-3 Shingle intersected the region with the output in layout coordinates and sized the buffer in logical pixels, which is only right for one output at 0,0 with scale 1. The CPU composite and the frontend capture both work per output.
- **Output management.** One head per output with its mode list, position, scale, transform and enabled state; disabled heads are listed too (connected native connectors not lit). A configuration must name every head, else it fails, as Wayfire's `test_configuration` requires (output-layout.cpp:1580-1583). Enabling and disabling heads and changing positions now work; disabling every head fails, as turning the only output off does today. Mode changes for native outputs become one `OutputRequest` per configuration with the requested state of every head, tested and applied together by the backend; nested and headless apply them themselves. After an applied configuration, as with Wayfire's `is_externally_managed` (output-layout.cpp:1268-1272), config reloads stop overriding those outputs until the next start.
- **Output power and gamma** are per output; idle DPMS turns all outputs off and wakes all of them.
- **ext-workspace.** One workspace group per output, holding the workspaces of its attached set; a set change re-creates the group's workspaces, ids keep carrying the output name (ext_workspace.rs:76-78).
- **Foreign toplevel** (wlr and ext) sends `output_enter` for the output a window belongs to, and `output_leave` when it moves or the output goes; `set_fullscreen(output)` fullscreens on that output.
- **`xdg_toplevel.set_fullscreen(output)`** moves the window to that output first ([section 4](#4-moving-views-between-outputs)).
- **wp-pointer-warp** and pointer constraints work in layout coordinates; `Event::PointerWarp` names the output under the warped position so the host can move its own cursor (nested: the window of that output).
- **Presentation time** sends `sync_output` for the primary output ([section 7](#7-rendering-and-frame-pacing)).

## 9. Xwayland

### What Xwayland does

Xwayland creates one RandR output and CRTC per `wl_output` and positions it from xdg-output's logical position (`apply_output_change`, hw/xwayland/xwayland-output.c:710-767). The root window's size is computed in `output_get_new_size` (xwayland-output.c:165-184) as the largest `logical_x + width` and `logical_y + height` over all outputs, taking for each output the larger of its logical size and its mode size, and set by `update_screen_size` (xwayland-output.c:221-240). The root always starts at 0,0; an output at a negative position therefore lies partly or wholly outside the root, where X windows cannot be placed. wlroots' XWM does not size the root itself, and Wayfire does nothing extra.

### Decision

- Shingle drives nothing: correct, per-output xdg-output geometry is what Xwayland needs, and MO-3 checks it with `xrandr --listmonitors` and `xdpyinfo` in the XWayland harness.
- The XWM configures managed X windows at their layout coordinates, as now; windows of an output at a negative position cannot be shown correctly, so Shingle logs a warning when the layout has a negative coordinate and Xwayland is enabled ([open question 5](#open-questions)).
- Override-redirect windows belong to the output under their top-left corner, else the active output; unmanaged windows of a removed output move to the active one.
- `_NET_WORKAREA` is not set, as today; X clients see the outputs through RandR.
- XW-7 (zero-scaling HiDPI for X11) depends on per-output scales and can follow MO-3.

## 10. IPC

Wayfire's names and payloads, from plugins/ipc-rules (`output_to_json` and `wset_to_json`, plugins/ipc-rules/ipc-rules-common.hpp:16-34 and 216-235; `view_to_json`'s `output-id`, `output-name` and `wset-index`, ipc-rules-common.hpp:191-202) and the plugins named:

| Method or event | Today | After |
| --- | --- | --- |
| `window-rules/list-outputs`, `output-info` (`id`), `get-focused-output` | one output, id 1 | every output with its id, layout geometry, workarea, `wset-index` and current workspace; `get-focused-output` the active output |
| `window-rules/list-wsets`, `wset-info` (`id` is the index) | one set, index 1 | every set, attached or parked; a parked set has `output-id` -1 and `output-name` "" |
| `window-rules/view-info`, `list-views` and every view payload | `output-id` 1 | the window's output id and name (-1 and "null" while its set is parked) and `wset-index` |
| `window-rules/configure-view` with `output_id` | refused unless 1 | moves the view ([section 4](#4-moving-views-between-outputs)), reconfiguring unless a geometry is given |
| `vswitch/set-workspace` (`output-id`), `vswitch/send-view`, activators (`output_id`, `output-id`) | one output | that output, default the active output |
| `simple-tile/get-layout`, `set-layout` (`wset-index`) | index 1 | any set |
| `wayfire/create-headless-output` (`width`, `height`) | missing | new; replies `ok` with `output` |
| `wayfire/destroy-headless-output` (`output` or `output-id`) | missing | new; only outputs it created |
| `stipc/create_wayland_output`, `stipc/destroy_wayland_output` (`output`) | missing | new; nested only, otherwise Wayfire's error "Wayfire is not running in nested wayland mode!" |
| `wsets/set-output-wset` (`output-id`, `wset-index`), `wsets/send-view-to-wset` (`view-id`, `wset-index`) | missing | new ([section 2](#2-workspace-sets-and-tiling-trees)) |
| `output-added`, `output-removed` (`output`) | missing | new |
| `output-gain-focus` (`output`) | missing | new |
| `output-wset-changed` (`new-wset`, `output`, `new-wset-data`, `output-data`) | missing | new |
| `view-set-output` (`output`, `view`), `view-wset-changed` (`old-wset`, `new-wset`, `view`) | missing | new |
| `output-layout-changed` | `configuration: [output_json]` | Wayfire's `configuration` array of every output with `name`, `output-id`, `source`, `depth`, `scale`, `vrr`, `transform`, `mirror-from`, `position` and `mode` (ipc-events.hpp:296-331) |
| `wset-workspace-changed` | `wset` | Wayfire's `previous-workspace`, `new-workspace`, `output`, `wset`, `output-data`, `wset-data` (ipc-events.hpp:469-481), per output |
| `plugin-activation-state-changed` | output 1 | the output the plugin runs on |

`list-methods` grows from 79 to 85 methods and the event list from 20 to 26; docs/ipc.md's counts change in the workspaces that add them. Changing the two existing event payloads to Wayfire's form is a compatibility fix: tools written for Wayfire read `new-workspace` and `configuration[].output-id`.

## 11. Testing

### Layer 1: unit tests, no devices

- **Core** (shingle-core): set allocation (lowest free index), attach and detach, `transfer_wset` with and without remembering, `return_windows` with closed, moved and twice-moved windows, the session format version 2 round trip and version 1 loading, restore matching by identity then name, and every invariant the snapshot validator checks.
- **Pure functions**: auto layout against wlroots' algorithm (fixed and automatic outputs, transforms, scales), closest-point clamping, `oswitch` direction scoring against Wayfire's examples, the primary-output rule, CRTC matching (shingle-native, ported test cases from wlroots' matching), connector priority with more connectors than CRTCs.
- **Compositor** (shingle, with the in-process client `crates/shingle/src/test_client.rs`): two outputs give two globals with their own names, positions and xdg-output geometry; enter and leave as a window moves; frame callbacks and presentation feedback per output; layer surfaces on a named and on the active output and `closed` on removal; two lock surfaces; screencopy region coordinates and buffer sizes with scale 2; output management with two heads, enable, disable and positions; each unplug policy; IPC replies and events against recorded Wayfire payloads. The Wayland pin transcript (crates/shingle/testdata/wayland_pins.txt) must stay unchanged for one output.
- **Frontend** (`FakeHost`, crates/shingle-frontend/src/frontend/tests.rs): render per output, redraw requests per output, active-output changes by click, `oswitch`, drags across outputs, Scale on the active output.
- **Native** (fake card with two connectors and two CRTCs, crates/shingle-native/src/kms/fake.rs): combined modesets with teardown of unused CRTCs only, a failing combined test dropping the lowest-priority output, flips routed by CRTC, hotplug of a second and a third monitor, unplugging one of two, mode changes per head.

### Layer 2: real clients on this host

All of it inside the namespaces the existing harnesses build (`unshare -U -r -m -n -p -f --mount-proc`, private `/tmp`, empty `/dev/dri` and `/dev/input`, the host's Wayland and X variables removed): `tools/output_test/run.py` gains a multi-output check set, and the XWayland and GPU harnesses gain a few checks each.

- **Headless with several outputs** (`--headless --outputs 2 --ipc`): `wlr-randr` lists two heads and can move, disable and re-enable one; `wayland-info` shows two `wl_output`s with names and xdg-output positions; `grim -o HEADLESS-2` and `grim -g` across both outputs through the CPU composite; the probe (`crates/shingle/examples/wl_probe.rs`, which gains `--report-outputs` for enter, leave and preferred scale and a `--layer OUTPUT` mode) sees enter and leave as IPC moves it; `shingle-lock` locks both outputs; the IPC methods and events of [section 10](#10-ipc) from a Python client, including `wayfire/create-headless-output`, removal of a created output under each `output_layout.unplug` policy, and a session saved with two outputs and restored with one and then two.
- **Xwayland** (tools/xwayland_test/run.py with two outputs): `xrandr --listmonitors` lists both at their positions, `xdpyinfo` reports the union size, and an X window moved to the second output maps there.
- **Offscreen in the GPU namespace** (tools/gpu_test/run.py, `--backend offscreen --outputs 2`): each output's frames are drawn and paced on their own, `grim` per output shows each output's own windows clipped at the boundary, and presentation feedback names the right `wl_output`.
- **Nested with two windows**: nested Shingle with `--outputs 2` runs as a client of a headless Shingle inside the namespace, as output_test already does for one window (on lavapipe); the headless host sees two toplevels, input fed to each through the host's `stipc` reaches the right nested output, `stipc/create_wayland_output` adds a third, and closing one window transfers its windows.

### Layer 3: the test machine

Steps added to [native-testing.md](../native-testing.md) by MO-7: two and three monitors lit at once with their preferred modes; more monitors than CRTCs; a second monitor plugged in and out while clients run, under each `output_layout.unplug` policy; a monitor moved to another connector returning its windows by EDID identity; a dock disconnect and reconnect; mode, position and disable changes through `wlr-randr` with two heads; flips and presentation per CRTC at different refresh rates (`tools/native_test/present_check.py`); DPMS per output; a VT switch away and back with two outputs lit.

### Rejected test strategies

- **Virtual DRM devices (vkms) or VMs.** The operator decided against VMs for native work ([native-backend.md decision 1](native-backend.md#decisions)), and vkms on this host would need its DRM. Rejected.
- **Testing nested outputs against the host's Wayfire session.** It would open windows on the operator's desktop. Rejected; nested runs only as a client of headless Shingle in the namespace.

## 12. Implementation plan

The workspaces are ordered by dependency; each can be finished and verified on its own. Every workspace also follows the repository rules: unit tests for new code, clippy clean, 80 columns, docs/status.md, docs/roadmap.md, docs/ipc.md and docs/features.md updated, real-client results recorded apart from unit tests, and signed commits. Until MO-5, the frontend, nested and native keep drawing one output (the first), through the new API; MO-2 to MO-4 are verified headless.

### MO-1 `shingle-wsets-core`: workspace sets, away records and sessions

Split workspace sets from outputs in `shingle-core` ([section 2](#2-workspace-sets-and-tiling-trees)): `WsetId`, `WsetState` with `home` and `identity`, attach and detach, lowest-free allocation, `move_window_to_wset`, `transfer_wset` with away records, `return_windows`, `destroy_wset`, and the output-keyed wrappers so `shingle` builds unchanged. Session snapshot version 2 with version 1 loading ([section 6](#6-sessions)). In `shingle-config`: `position: auto | {x, y}`, `output_layout.unplug`, the `oswitch` and `wsets` sections, `input_devices.NAME.output`, `workarounds.remove_output_limits`, and their Wayfire and i3 imports (including `preserve-output` in `core/plugins`); the pure layout, closest-point and direction functions.

Acceptance:

- Unit tests for everything listed under Layer 1 "Core" and "Pure functions" except CRTC matching.
- Every existing test passes unchanged, and the session round trip of [status.md](../status.md#with-real-clients) is repeated with nested Shingle with identical results.
- A version 1 session file from `master` restores in nested with every window in place.
- Verified on this host only; nothing needs the test machine.

### MO-1 as built

Built on branch `shingle-wsets-core`, 2026-11-26. The core (crates/shingle-core/src/core.rs, crates/shingle-core/src/wset.rs, crates/shingle-core/src/session.rs) holds `WsetState`s keyed by `WsetId`; which output shows a set is the set's `output`, and the output-keyed calls resolve it, so `shingle`, the frontend and the backends build unchanged apart from three readers of the snapshot's set list (`simple-tile/get-layout` and `set-layout`, the session launcher's tile order and one headless test). The configuration parts are in crates/shingle-config/src/output.rs (`OutputPosition`), crates/shingle-config/src/layout.rs (the pure functions) and crates/shingle-config/src/multi_output.rs (the new sections). Where the code differs from the sections above:

- **`WorkspaceRef` keeps its output.** The design made it `{ wset, workspace }`, but about twenty places in `shingle` read `location.output` and existing tests build it with `output`. It stays `{ output, workspace }`, where `output` is the output showing the set or, while the set is parked, its home (empty for a set never shown), and `WindowInfo` gains `wset` and `away_from`. `Shingle::window_location` returns the reference by value, since it is now computed.
- **Snapshots record which output shows a set.** `WsetSnapshot` has `output: Option<OutputId>` besides `home`, and a restore attaches each set to it. Without it a restored model would differ from the saved one, and until MO-2 moves the restore into `Frontend::start_clients`, the compositor's start-up merge (state.rs:298-313) needs restored sets on their outputs to work unchanged. MO-2 may detach them all on restore and match them to outputs as section 6 describes. `home` is `Option<OutputId>` because `create_wset` makes a set that was never shown.
- **Moves report what moved.** `move_window_to_wset` and `transfer_wset` return the windows that changed place, which MO-4 needs for `view-set-output` and `view-wset-changed`. A move to the place a window already has changes nothing.
- **Away records.** Any move by the user or a client to another workspace clears `away_from` (`drop_window`, `swap_windows` and `move_window_to_wset` across workspaces, and `detach_window` then `reattach_window`); a window that arrives in the set it is away from is home and its record ends. `destroy_wset` refuses a shown or non-empty set and clears the records that name it, so `away_from` always names an existing set other than the window's own; the restore checks the same. A returned window takes the leaf state it had in the copy, except `held`, which belongs to its current map. With `remember`, `transfer_wset` replaces an earlier copy only when the set still holds windows.
- **Finding a parked set.** `find_parked_wset` implements section 5's matching: identity first, when it has a serial or is unique among parked sets, else the home name. It also skips, in the name fallback, a parked set whose identity has a serial that differs from the new output's, so that another monitor plugged into the same connector does not take the first monitor's windows.
- **Config names.** `outputs` is already the list of `[output:NAME]` entries, so the unplug policy is `output_layout.unplug`, with preserve-output's `last_output_focus_timeout` beside it; Wayfire's `[workarounds]` options live in `behavior`, so `remove_output_limits` is `behavior.remove_output_limits`. A Wayfire file whose `[core] plugins` lacks `preserve-output` keeps the `return` default rather than importing Wayfire's `move`, since every `wayfire.ini` lists its plugins and decision 1 chose `return`. `OutputConfig` keeps `x` and `y` beside `position`, set from it, for the code that places one output; YAML takes either `position` or the old keys, not both, and writes only `position`. The i3 importer also reads `focus output`, `move container to output`, `input NAME map_to_output`, Shingle's own `wset N` and `move container to wset N` bindings and `$output_unplug`, `$last_output_focus_timeout`, `$remove_output_limits` and `$wsets_label_duration`.
- **Session saves.** Building the real-client round trip found two more ways a change reached the session file only at the next save or at exit: `simple-tile/set-layout` changing only weights or nesting, and `window-rules/configure-view` moving a window that already floated. Both now send `Event::LayoutChanged`, on which the frontend saves.

### MO-2 `shingle-output-registry`: several outputs in the compositor

Replace `CompositorState`'s single output with `Outputs` and `active_output` ([section 1](#1-output-model-and-layout)), add the public API (`add_output`, `remove_output`, `outputs`, `output_at`, `frame_presented` per output, the output events), one `wl_output` global and xdg-output per output, the auto layout, IPC ids, the unplug policies and the session restore in `Frontend::start_clients`. Headless gains `--outputs N`. IPC: real ids and indices in every reply, `list-outputs`, `list-wsets`, `get-focused-output`, `wayfire/create-headless-output` and `destroy-headless-output`, `output-added`, `output-removed`, `output-wset-changed`, and Wayfire's payloads for `output-layout-changed` and `wset-workspace-changed`.

Acceptance:

- Unit tests for the compositor items of Layer 1 that concern globals, xdg-output, layout, unplug policies and IPC.
- The Wayland pin transcript is unchanged; the IPC fixtures of [status.md](../status.md#with-real-clients) are unchanged apart from the two event payloads, whose new form is recorded.
- On this host, in the output_test namespace with `--headless --outputs 2`: `wayland-info` and `wlr-randr` list both outputs at their auto positions and at configured fixed ones; `wayfire/create-headless-output` adds a third that clients see, and destroying it moves its windows to the active output; `list-methods` returns 81.
- Nothing needs the test machine.

### MO-2 as built

Built on branch `shingle-output-registry`, 2026-11-26. The registry is crates/shingle/src/output/registry.rs (`Outputs`, `OutputState`, `OutputKind`), adding and removing outputs and the restore are crates/shingle/src/output/hotplug.rs, and globals, renames, the layout and claiming a parked set are in crates/shingle/src/output.rs. `CompositorState` holds `outputs` and `active_output`; code that meant the one output now reads `state.primary()` (the first output added), `state.active()` or `state.output_of_window(window)`. Headless takes `--outputs N` (crates/shingle-bin/src/headless.rs). Where the code differs from the sections above:

- **The calls without an output stay.** `Compositor::output`, `output_info`, `set_output_info`, `set_output`, the scale and transform calls, `map_output_point`, `visible_surfaces` and `allows_tearing` mean the first output added, which `Compositor::primary_output` names; every host draws only that one until MO-5. The per-output forms are `describe_output(&OutputHandle, OutputInfo)` (the design's `set_output_info` with an output), `set_output_size`, `visible_surfaces_on`, `frame_presented(&OutputHandle, PresentInfo)` and `frame_now(&OutputHandle)`; `frame()` presents every output that is not dark, so that the frontend's hosts also give frame callbacks to windows on outputs they do not draw, such as IPC outputs. `Compositor::configured_physical_size` gives a backend without modes the size an output's entry asks for. `OutputView` has `kind` and `dark` besides the fields listed.
- **Renames stay renames.** A new name in `set_output_info` renames the output at any time, as NB-9 does, instead of removing and adding it once clients have connected; native needs that until MO-7 adds and removes its outputs. When the new name or identity has a parked set, the output shows that set and the windows of the set it showed join it.
- **Placements.** `OutputPlacement` has `position: OutputPosition` instead of `x` and `y`, and `Config` gains `output_position` (the default entry's position; the position of `Config::output` is ignored) and `output_unplug`. A call to `set_output` that moves the first output fixes it there; one that keeps its position keeps it automatic.
- **Identities.** Only an output with its own description has an EDID identity: one whose make, model or serial differs from Shingle's own headless description. Nested and headless outputs all look alike, so with identities a new `HEADLESS-3` would have taken the parked set of `HEADLESS-2` as the only one with its identity; they are matched by name instead. IPC outputs have none.
- **Restoring a session.** `Compositor::with_core` parks every saved set, and each output takes its own as it is added (`HEADLESS-1` in `Compositor::new`, the others in `add_output` and renames), returning the windows that are away from it. `Compositor::restore_outputs` then handles the sets that no output took under `output_layout.unplug` and focuses the saved active output. It runs once, from `Frontend::start_clients`, from headless after its outputs, or else at the first `poll`. Under `return`, a native session restored in nested therefore shows its windows in the one output, and `describe_output` on the next native start brings them back. The core records the active output only when it changes, so that a session with one output is saved exactly as before (the pin transcript is unchanged).
- **Unplugging.** A set that nothing can return to (no windows, no copy, no window away from it) is destroyed under `move` and `return`, so closing and creating outputs leaves no empty parked sets behind; `park` keeps it. IPC outputs take parked sets by name like any other output, which the output harness uses to bring a restored window to a created `HEADLESS-3`.
- **What still means the first output.** Until MO-3 (which moved all of these but the cursor's drawing and input panels to their own outputs; see [MO-3 as built](#mo-3-as-built)): layer surfaces and their exclusive zones (every other output's workarea is its whole rectangle), lock surfaces, override-redirect X windows and the cursor are on the first output and drawn only there; `wl_surface.enter` names the window's output for toplevels and the first output for everything else; foreign-toplevel `output_enter` is sent for the first output's bindings; the ext-workspace group is the first output's; FIFO and commit-timing barriers are released by the first output's frames. Presentation feedback is per output already: a surface shown on another output waits for that output's frame instead of being discarded, and `sync_output` is the presenting output's `wl_output`.
- **Output management.** Every output has a head and a configuration applies to each head's own output, so `wlr-randr --output HEADLESS-2 --pos` works; disabling a head still fails (MO-3), and a backend mode change for more than one head in one configuration fails, since `OutputRequest` names no output yet (MO-7).
- **IPC limits until MO-4** (lifted there, see [MO-4 as built](#mo-4-as-built)). `vswitch/set-workspace` takes only the active output's id, `simple-tile/get-layout` and `set-layout` only the active output's set index, and `configure-view` only the window's own output id. `wset_to_json`'s `name` is the set's index as a string, as Wayfire names a set by its object id; it was the output name before. `output-layout-changed` is sent once per batch of events, as Wayfire sends one per configuration change.
- **Events.** `Event::OutputChanged` carries `output` and `rect`, `Event::WorkspaceChanged` gains `previous` and `output`, and `Event::OutputAdded` and `OutputRemoved` are new. `output-added`, `output-removed` and `output-wset-changed` are queued for IPC directly, as the X11 events are, so headless sends them too. Headless now drains events every turn: before, nothing drained them in headless, so IPC watchers there never got `view-mapped` or any other event built from them, and the event queue grew without bound.
- **Other backends.** `--outputs` is refused for every backend but headless until MO-5 (offscreen) and MO-6 (nested). Offscreen and native present only the first output; outputs created over IPC in them get no frames until MO-5, while nested gives them frames through `frame()`.

### MO-3 `shingle-output-protocols`: client protocols per output

[Section 8](#8-client-protocols-per-output): enter and leave by overlap, preferred scale per output, layer shell on its output with `closed` on removal, one lock surface per output, screencopy in output-local coordinates and physical pixels, output management with every head, power and gamma per output, ext-workspace groups and foreign-toplevel output events, frame callbacks and the primary output for presentation, FIFO and commit-timing. The probe's `--report-outputs` and `--layer` modes.

Acceptance:

- Unit tests for the protocol items of Layer 1.
- On this host, headless with two outputs: `grim -o` per output and `grim -g` across both; `wlr-randr --output HEADLESS-2 --off`, `--on` and `--pos`; the probe reports enter and leave; a layer probe on `HEADLESS-2` is closed when that output is destroyed; `shingle-lock` locks and unlocks both outputs; `xrandr --listmonitors` and `xdpyinfo` in the XWayland harness show both outputs and the union root.
- Nothing needs the test machine.

### MO-3 as built

Built on branch `shingle-output-protocols`, 2026-11-26. Which outputs a surface is on, its primary output and its preferred scales are crates/shingle/src/surface_outputs.rs, synced once per turn from `Compositor::poll`; layer surfaces are crates/shingle/src/layer_shell.rs, lock surfaces crates/shingle/src/shell.rs (`finish_lock`), screencopy crates/shingle/src/screencopy.rs (`CaptureGeometry`), turned-off outputs `DisabledOutput` in crates/shingle/src/output/registry.rs with their heads in crates/shingle/src/output_management.rs, and power, gamma, ext-workspace and foreign-toplevel their own modules. Where the code differs from the sections above:

- **Which outputs a surface is on.** A toplevel is on its set's output while that output shows its set, its workspace is current (or it is sticky) and it is not minimized, and on no other output, since windows are clipped to their own output (`behavior.remove_output_limits` waits for the frontend, MO-5). Layer and lock surfaces are on their output, unmanaged X windows on every output they overlap (drawn on the one under their top-left corner, else the active one), the cursor on the output under the pointer (else the first), and popups and subsurfaces on their parent's outputs rather than by their own overlap; input-method panels stay on the first output. A surface whose output goes leaves it in the same turn, and every surface entered every bound output on every commit before.
- **Preferred scales.** `wp_fractional_scale_v1.preferred_scale` and `wl_surface.preferred_buffer_scale` (which Shingle never sent before, for `wl_compositor` version 6) follow the output a surface belongs to and are sent only when they change; a surface that belongs to no output, such as a closed layer surface, keeps its last scale, and one without a role yet gets the first output's.
- **Primary output.** As section 7 says, the largest overlap above a tenth of the surface; when no output has that much, the last primary output stays while the surface is still on it, else the first output it is on. FIFO barriers and commit-timing deadlines of a surface are released by its primary output's frames and are waited for wherever the surface is visible; on an output that gets no frames (IPC outputs in offscreen and native, until MO-5) they wait as its frame callbacks do. Presentation feedback of a surface shown on two outputs comes from its primary output's frame.
- **Screencopy with frame capture.** `Event::CaptureRequested` carries the output and the region in its logical coordinates; the frontend answers in logical pixels and the compositor scales them into the client's physical buffer (nearest pixel), so the frontend's capture of a scale-2 output has logical detail until MO-5 draws at the output's scale. The frontend draws only the first output until MO-5, so captures of other outputs fail there; headless's CPU composite draws every output at its physical size. A region that misses its output fails.
- **Output management.** A configuration that disables a head removes the output under `output_layout.unplug`; the output is kept as a `DisabledOutput` (description, physical size, position, scale, transform and its place in the add order) whose heads stay, disabled, also for managers bound later, and send only their description, modes and `enabled`, as wlroots does. Enabling it adds the output again at its old place in the add order, so the first output stays first. Turning on or off an output whose backend lists modes (native) fails until MO-7, as does a configuration that combines that with a backend mode change. Decision 9 is built here: outputs an applied configuration placed keep their placement across config reloads until Shingle restarts.
- **Power.** Each output has its own power state. Turning the last lit output off is idle DPMS off, as turning the only output off was, and waking (input, or a client turning any output on while DPMS is off) turns every output on again. An output turned off draws nothing and gets no frames; there is no per-output backend event yet, so native's CRTCs follow only the idle DPMS state until MO-7. `Compositor::output_powered` tells a host.
- **Gamma.** One control per output with a ramp of its own; `Compositor::gamma_ramp` stays the first output's and `gamma_ramp_of` names the output.
- **ext-workspace.** One group per output per manager; when an output shows another set its group's workspaces are made again, and activating a workspace switches that group's output (`CompositorState::switch_workspace_on`), not the active one.
- **Session lock.** `locked` waits until every output that is not dark has a lock surface of the active lock's client at its size; lock surfaces of an output that goes become inert, and the session stays locked. The keyboard goes to the active output's lock surface first.
- **Pulled forward from MO-4.** Pointer input finds the surface under the pointer among those of the output there, which lock surfaces on the second output need; focus still changes outputs only as MO-2 left it.
- **Left for later.** `Event::PointerWarp` does not name an output yet (it does since MO-4), and the cursor is drawn by the frontend on the first output only (MO-5).
- **Fixed on the way.** Power and gamma controls of an output that went stayed alive after MO-2's `remove_output` (only renames failed them), and a lock surface's `ack_configure` recorded the output's size at the time of the ack instead of the size its configure sent, so a resize between the two could refuse a correct buffer.

### MO-4 `shingle-output-focus`: focus, pointer and moving views

[Sections 3 and 4](#3-focus-pointer-and-input): the active-output rules and `output-gain-focus`, pointer clamping, absolute input with an output, `oswitch` bindings, `shell::move_window_to_output` and its callers (`configure-view` with `output_id`, `start_on_output`, `set_fullscreen(output)`, foreign-toplevel fullscreen), new windows on the active output, `view-set-output` and `view-wset-changed`, `vswitch` and activators with `output-id`, `simple-tile` layouts by `wset-index`, the `wsets` IPC methods and bindings.

Acceptance:

- Unit tests for the focus, moving and wsets items of Layer 1 and the frontend's `oswitch` dispatch through `FakeHost`.
- On this host, headless with two outputs and real clients: `stipc/move_cursor` and `stipc/feed_button` on the second output make it active and focus its window; `stipc/feed_key` with `oswitch` keys moves focus and a window; `configure-view` with `output_id` moves a tiled, a floating and a fullscreen window with Wayfire's geometry; `wsets/set-output-wset` swaps sets between the two outputs; `list-methods` returns 83, then 85 with the nested methods of MO-6.
- Nothing needs the test machine.

### MO-4 as built

Built on branch `shingle-output-focus`, 2026-11-26. The active output is crates/shingle/src/output/focus.rs, moving windows crates/shingle/src/output/moving.rs, the wsets plugin crates/shingle/src/output/wsets.rs, the `oswitch` and `wsets` bindings crates/shingle/src/output/bindings.rs and the public calls crates/shingle/src/output_focus_api.rs (`Compositor::focus_output`, `switch_output`, `move_window_to_output`, `show_wset`, `send_window_to_wset`, `clamp_to_layout`, `input_device_output`, `output_binding`, `run_output_action`). Where the code differs from the sections above:

- **One move function with a geometry rule.** `move_window_to_output` and `move_window_to_wset` take a `MoveGeometry` instead of a `reconfigure` flag: `Scale` is Wayfire's `RECONFIGURE` (the position relative to the output scaled by the ratio of the outputs' sizes, then clamped into the target workarea, dialogs moved by their parent's offset), `Translate` keeps the position relative to the output, as a view moved without reconfiguring keeps its output-relative geometry in Wayfire (`wsets/send-view-to-wset`, `configure-view` with a geometry), and `Keep` keeps layout coordinates, for a floating window dropped where the pointer put it. Snapped windows are snapped again with the same edges in the target workarea; fullscreen, maximized and tiled windows follow their set's output and tree by themselves.
- **The workspace a window lands on.** Section 4 said "same workspace". `oswitch`, `configure-view` and `start_on_output` use `move_view_to_output` without `SAME_WORKSPACE` in Wayfire, so a view keeps its position relative to the current workspace; Shingle lands a moved window on the workspace with the same offset from the target set's current workspace as it had from its own set's, clamped into the grid, so a visible window stays visible. Unplug transfers keep the same workspace, as section 5 says (`transfer_wset`).
- **Unplug transfers reconfigure.** Before MO-4 a transfer moved windows only in the core, so a floating window of an unplugged output kept its layout coordinates and was out of sight on the output it joined. Transferred windows are now scaled into the target output as `transfer_views` moves them with `RECONFIGURE`, and every transfer and return sends `view-wset-changed` and, when the output changed, `view-set-output`, as section 5 asks.
- **Coordinates in IPC.** View geometries and `configure-view`'s `geometry` stay in layout coordinates, as Shingle has always reported them; Wayfire's are relative to the view's output. They agree on the first output at 0,0; changing them for the other outputs is left open.
- **Activators with an output.** Scale, Expo, show-desktop and the vswitch bindings act on the active output (decision 7), so `scale/toggle`, `scale/toggle_all`, `expo/toggle`, `wm-actions/toggle_showdesktop` and the vswitch activators with `output_id` or `output-id` first make that output active, which section 3 lists as "IPC with an explicit output". `vswitch/set-workspace` and `wsets/set-output-wset` act on the output they name without focusing it, as in Wayfire.
- **Bindings in the compositor.** The `oswitch` and `wsets` bindings are matched by the compositor (`Compositor::output_binding`), which the frontend asks for each key and headless runs for STIPC keys, so that the acceptance checks run against headless Shingle as other STIPC bindings do. The "Workspace set N" label is left to MO-5 with the other per-output drawing.
- **Which press focuses an output.** Every pointer button press focuses the output under the pointer, rather than only the first of several held buttons, and only the first touch point down focuses one.
- **The active output coming back.** Section 5's rule that an output active when it went is focused again within `last_output_focus_timeout` is built here, for every unplug policy, matched by EDID identity, else by name.
- **Dialogs.** A dialog maps in its parent's set and workspace. Shingle creates a window when the client sets its app id, which is usually before `set_parent`, so the dialog is moved to its parent's set when it maps, as Wayfire's `adjust_view_output_on_map` puts a view on its parent's output. Floating placement uses the window's own set's workarea.
- **Pointer warp.** `Event::PointerWarp` gained `output`, the output under the new position; `oswitch` warps the pointer to the target output's centre with it. Hosts still move their cursor on the first output only (MO-5, MO-6).
- **Native input.** Native lights one output until MO-7, so it passes absolute input without an output and keeps clamping relative motion to its lit output; `Compositor::input_device_output` gives MO-7 the configured `input_devices.NAME.output`. The compositor itself pulls every pointer position onto the layout (STIPC, virtual pointers, hosts).
- **Fixed on the way.** `vswitch/send-view`, `vswitch/set-workspace` with a view and the frontend's workspace moves put the window into the active output's set whatever set it was in; they now keep it in its own set. Floating placement and the workspace-change animation assumed the active output; placement uses the window's set and the animation only the first output's changes, the only output the frontend draws.

### MO-5 `shingle-frontend-outputs`: a scene per output

[Section 7, the frontend](#decision-the-frontend): `Frontend::render(host, output, target)`, per-output redraw, damage, zoom and workspace transitions, Scale and Expo on the active output, captures per output, drags drawn across outputs, the wsets label, clipping and `remove_output_limits`, IPC-created outputs drawn for captures. The offscreen backend gains `--outputs N` with a target and pacer per output.

Acceptance:

- Unit tests for the frontend items of Layer 1.
- On this host, in the GPU namespace: offscreen with two outputs at different refresh rates paces each on its own (frame counts per output over 5 s within 2 % of the rates), `grim` per output shows each output's own windows clipped at the boundary, presentation feedback names the right output, and a drag across the boundary moves the window.
- The DB-2 to DB-6 GPU checks still pass with one output.
- Nothing needs the test machine.

### MO-5 as built

Built on branch `shingle-frontend-outputs`, 2026-11-26. What the frontend keeps per output is crates/shingle-frontend/src/frontend/outputs.rs (`OutputScene`: zoom, the workspace slide, the "Workspace set N" label, the surfaces drawn last with their geometry and buffers, their geometry animations and the surfaces committed since the output's last frame), one output's scene is `draw_scene` in crates/shingle-frontend/src/frontend/scene.rs, and the offscreen backend's targets are `Outputs` in crates/shingle-bin/src/offscreen.rs. Where the code differs from the sections above:

- **The host seam.** `Frontend::render(host, output, target)` draws `output`, `Host::request_redraw(&OutputHandle)` asks for a frame of one output and `Host::warp_pointer(&OutputHandle, x, y)` names the output with the position relative to it, zoom applied. The frontend asks only for outputs the backend added; nested and native draw the compositor's first output until MO-6 and MO-7 and ignore requests for others (native has no other). Before, `warp_pointer` got output-relative coordinates that nested then mapped as layout coordinates, subtracting the output's origin a second time; it only worked because the first output sits at 0,0.
- **No damage regions.** Shingle still redraws whole outputs. A commit asks for frames of the outputs its surface's rectangle overlaps, and any other compositor event, and FIFO or commit-timing waits, of every output. Presenting an output's frame sends only that output's frame callbacks (`Compositor::frame_now`); `Compositor::frame` is no longer used by the frontend.
- **Shared and per-output state.** Textures stay shared, keyed by surface, and are dropped once no output drew their surface in its last frame. Opening, closing and minimizing animations stay global but are drawn on, and ask for frames of, the outputs their surfaces were drawn on; a closing window's animation remembers its output. Inversion stays global, as one `invert/toggle` and one binding; Wayfire's invert is per output, which can follow if wanted. The gamma ramp, scale, transform and tearing are the output's (`gamma_ramp_of`, `allows_tearing_on`); offscreen sets the one GPU's gamma table before each output's frame.
- **Clipping.** Each output draws its own surfaces (`visible_surfaces_on`), moved into its own coordinates; what lies outside its target is cut by the target, so a window is clipped to its own output without a clip rectangle. `behavior.remove_output_limits` is built in the compositor, not only in the frontend: windows of other outputs that overlap an output are listed for it after its own windows (`foreign_windows_on`), and they also enter it, as Wayfire's surfaces enter the outputs they are drawn on.
- **Drags.** The window being moved by a frontend drag is drawn above everything on every other output it overlaps (`Compositor::window_surfaces`), but enters no other output.
- **Scale and Expo.** They run on the output that was active when they started (`overview_output`), lay out in its rectangle and draw only there; the other outputs keep drawing their normal scenes. Another output becoming active, by IPC or a window taking focus, or their output going ends them at once, without the exit animation.
- **Workspace slides.** Each output slides on `Event::WorkspaceChanged` for its own set, from the event's `previous` workspace rather than one the frontend remembered, so an output that was never drawn still slides from the right place.
- **The wsets label.** The compositor emits `Event::WsetSelected { output, wset }` whenever `wsets/set-output-wset` or a `wset_N` binding selects a set on an output, also the one it shows already, and for an output that got a new empty set because another took its set, as Wayfire's `select_workspace` shows the overlay on both. The frontend draws "Workspace set N" at 10,10 on that output with Wayfire's 32-pixel font size and colours for `wsets.label_duration`, and `next_deadline` wakes the host when it ends.
- **IPC outputs.** Outputs created with `wayfire/create-headless-output` get frames from a 60 Hz frontend timer that `next_deadline` reports, as decision 8 asks. The host is never asked for frames of them; a capture of one asks for frames of every output the host draws, and the next one draws the IPC output's scene through that host target's `capture` and answers it. Captures of outputs that are gone, dark or turned off fail at once instead of waiting.
- **Captures stay logical.** The frontend still answers in logical pixels, which the compositor scales into the client's buffer (MO-3), so a capture of a scale-2 output has logical detail. Drawing captures at the output's physical size needs a change to `finish_capture` and is left for later.
- **STIPC pointer input.** `stipc/move_cursor` and `stipc/feed_button` moved the compositor's pointer directly, so frontend drags and pointer bindings never saw them, unlike STIPC keys. Wayfire feeds its STIPC pointer to every plugin, so a drag with `S-BTN_LEFT` moves a view there. `Compositor::set_stipc_pointer_events`, which the frontend turns on, makes them `Event::StipcPointerMotion` and `Event::StipcButton` (with Super as `Event::StipcKey`); the real-client drag check needs it. Headless keeps handling them in the compositor.
- **Offscreen.** `--outputs N` (else `WLR_HEADLESS_OUTPUTS`) adds `HEADLESS-2` to `HEADLESS-N` as headless does. Every lit output the backend added has a target, render state, pacer and frame counter of its own on one GPU context, is drawn at most once per its own refresh interval and at its own physical size, and presents with its own sequence numbers. Outputs turned off through wlr-output-power get no target and no frames.

### MO-6 `shingle-nested-outputs`: one window per output

[Section 7, nested](#decision-nested): several winit windows, `--outputs N` and `WLR_WL_OUTPUTS`, input per window with its output, presentation per window, `stipc/create_wayland_output` and `destroy_wayland_output`, closing a window as an unplug.

Acceptance:

- Unit tests for window-to-output mapping and the stipc methods.
- On this host, nested Shingle with `--outputs 2` as a client of headless Shingle in the namespace: two toplevels on the host, input through the host reaches the right output, a third window by `stipc/create_wayland_output`, and closing one returns its windows when it is created again under `return`.
- The nested smoke test of [status.md](../status.md#with-real-clients) is repeated with one window with identical results. The operator's interactive check on the desktop with two nested windows is recorded separately, under "needs operator".
- Nothing needs the test machine.

### MO-7 `shingle-native-multi-output`: one CRTC per output

[Section 7, native](#decision-native-extends-nb-9): `Display` with a list of lit outputs, connector priority, CRTC matching, combined modesets with teardown of unused CRTCs only and the fallback that drops outputs, flips and presentation per CRTC, hotplug adding and removing compositor outputs, DPMS and output power per output, output-management configurations across heads, and native-testing.md steps for Layer 3.

Acceptance:

- Unit tests on the fake card for every native item of Layer 1, and the NB-7 and NB-9 display and host tests passing unchanged for one connector.
- On the test machine: the Layer 3 steps.
- Built and unit-tested on this host; it never opens this host's DRM.

### MO-8 `shingle-output-mirror`: mirrored outputs (later, optional)

Wayfire's `mode = mirror OUTPUT`, which NB-9 imports as `off`: draw the source output's frame scaled into the mirror output and list the mirror as disabled in the layout, as Wayfire's fourth step of `apply_configuration` does (output-layout.cpp:1748-1760). Only if the operator wants it ([open question 4](#open-questions)).

MO-1 comes first. MO-2 needs MO-1, MO-3 and MO-4 need MO-2 (they touch the same modules and are best done in that order), MO-5 needs MO-3 and MO-4, MO-6 and MO-7 need MO-5 but not each other. MO-7 does not need NB-8 or NB-10, and NB-8 and NB-10 can land before or after it. XW-7 can start after MO-3.

## Decisions

The operator asked for the work to continue without stopping for questions, so on 2026-11-26 the proposed defaults below were adopted on the operator's behalf. Each can be revisited before the workspace that depends on it lands.

1. **Unplug:** `output_layout.unplug` defaults to `return`; `move` and `park` are available, and `park` is imported when a Wayfire config loads preserve-output.
2. **Nested output names** stay `HEADLESS-N`.
3. **Nested windows at start:** one, more with `--outputs N`; `WLR_WL_OUTPUTS` is read only when the option is absent.
4. **Mirroring** is left out (MO-8 later) and still imported as `off`.
5. **Negative positions with Xwayland:** kept as configured, with a warning, as in Wayfire.
6. **Focus between outputs** follows Wayfire's click rule; focus-follows-pointer is a later option.
7. **Scale and Expo** act on the active output only.
8. **IPC-created headless outputs** are drawn only when a screencopy asks, with frame callbacks at 60 Hz.
9. **After a client applies an output configuration,** config reloads stop overriding those outputs until restart, as in Wayfire.
10. **A locked output without a lock surface** shows the background colour.

## Open questions (answered above)

1. **Unplug default.** `output_layout.unplug: return` (windows join the active output and go back when their monitor returns), or Wayfire's default `move`, or `park` (Wayfire with preserve-output)? Proposed: `return`, with the other two available and `park` imported when a Wayfire config loads `preserve-output`.
2. **Nested output names.** Keep `HEADLESS-1`, `HEADLESS-2` for nested windows, as today, or use Wayfire's `WL-1`, `WL-2` and migrate saved sessions and `[output:HEADLESS-1]` entries? Proposed: keep `HEADLESS-N`, so existing configs, sessions and harnesses need no change.
3. **Nested outputs at start.** One window by default, more with `--outputs N` or `WLR_WL_OUTPUTS`? Proposed: yes, one by default, and the environment variable read only when the option is absent.
4. **Mirroring.** Is Wayfire's `mirror` mode wanted (MO-8), or does it stay imported as `off`? Proposed: leave it out of this item and keep importing it as `off`.
5. **Negative positions with Xwayland.** Keep configured positions as Wayfire does and warn, or shift the whole layout so that its top-left corner is 0,0 whenever Xwayland runs? Proposed: keep them and warn.
6. **Focus between outputs.** Wayfire's rule (the active output changes on a click, a touch, a focused view or `oswitch`, not on motion), or focus follows the pointer across outputs? Proposed: Wayfire's rule, with focus-follows-pointer left for a later option.
7. **Scale and Expo across outputs.** One output at a time (the active one), or independent instances on every output at once, as Wayfire's per-output plugins can be? Proposed: the active output only; per-output instances later if wanted.
8. **IPC headless outputs drawn only for captures.** Draw `wayfire/create-headless-output` outputs only when a screencopy asks, while still sending frame callbacks at 60 Hz? Proposed: yes; Wayfire renders them every frame, but nobody can see them, and screencopy and clients behave the same.
9. **Output-management priority over the config file.** After a client applies an output configuration, should config reloads stop changing those outputs until restart, as Wayfire's `is_externally_managed` does? Proposed: yes, as Wayfire.
10. **Lock with a missing lock surface.** An output without a lock surface shows the background colour; should it instead show the red "lock client crashed" fill some compositors use, or Shingle's lock-crash handling? Proposed: the background colour, since `shingle-lock` creates a surface per output and the existing crash handling stays as it is.
