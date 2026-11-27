# Lock screen design

Status: built, 2026-11-27, in workspace `shingle-lock-screen`, with the open questions decided on the operator's behalf ([Decisions](#decisions)); the operator reviews them. What was asked: make `shingle-lock` the operator's lock screen in place of their swaylock-effects setup, with a blurred screenshot of every output that never lets the real screen flash through, a little movement in the blur, only the blur, vignette and clock at rest, an outlined password input that fades in on typing, and the operator's swaylock config carried over.

Sources: Shingle on branch `shingle-lock-screen`, cut from `master` at `aca9bd9`; Wayfire at `669bbe36` in /home/karl/dev/workspaces/shingle-input-gaps/wayfire (cited as `wayfire:PATH:LINE`); the ext-session-lock-v1 protocol text; swaylock's and swaylock-effects' option sets and config format as their manuals describe them. The operator's own swaylock config was not available on this host and is not copied anywhere; tests use invented values.

## Summary

| # | Topic | Decision |
| --- | --- | --- |
| 1 | When clients disappear | From the moment a lock client asks for the lock (`ext_session_lock_manager_v1.lock`) until it unlocks, also after it crashed, no output draws any client. Each output is black until the active lock's surface for it has committed a full opaque frame of its size, and then shows only that surface over black. |
| 2 | Locks Shingle starts | The lock binding and `idle.lock_seconds` enter a new state, `Starting`: input to clients stops at once, but the outputs still show them, so that the lock client can capture them, until the client asks for the lock or 5 s pass (then `Pending`, black). |
| 3 | Native modesets | While the lock hides the clients, a modeset (DPMS on, the seat's resume after a VT switch, a hotplug) never shows a frame drawn before the lock: it shows a black dumb buffer until a frame drawn under the lock has flipped. |
| 4 | Screenshots | `shingle-lock` captures every output with wlr-screencopy before it asks for the lock, blurs each capture once on the GPU, and only then locks. |
| 5 | Drift | The blurred capture moves on a slow Lissajous path (12 px, 30 s per cycle) with a half-percent breathing zoom and a 2.5 % moving shimmer, drawn on the GPU at `lock.drift.fps` (8) and only when the compositor's frame callback says the output was drawn, so nothing is drawn while an output is off. |
| 6 | Input | An outlined rounded input, `indicator-radius` high and six times as wide, with one dot per character, fades in on the first key and out `lock.input_timeout` (5 s) after the last; swaylock's ring, inside, line and text colours per state become its border, background, inner edge and dots or message. |
| 7 | Options | swaylock's config format and command line, read by `shingle-lock` in this order: defaults, the swaylock config, Shingle's YAML `lock.options`, the command line. Unknown and unsupported options warn and are ignored. |
| 8 | Default lock command | `lock.command`, default `shingle-lock`, run with `SHINGLE_CONFIG_FILE` naming Shingle's config; `SHINGLE_LOCK` still overrides it. A Wayfire import replaces a bare `swaylock` in `[command]` and `[autostart]` with `shingle-lock` and warns; `/usr/bin/swaylock` keeps swaylock. |
| 9 | Verification | `tools/gpu_test/run.py --checks lock` records every frame the compositor presents (`SHINGLE_RECORD_FRAMES`) and checks each one. |

## 1. Wayfire's behaviour

Wayfire's session-lock plugin (wayfire:plugins/protocols/session-lock.cpp) is the reference for what a lock means; Shingle is stricter where it lets the desktop through.

- **Lock request.** A new lock is in state `LOCKING` (wayfire:plugins/protocols/session-lock.cpp:355). Nothing hides the outputs then: they keep drawing the desktop until every output has a lock surface (wayfire:plugins/protocols/session-lock.cpp:318-323) or a 1 s timer fires (wayfire:plugins/protocols/session-lock.cpp:350-353). Only `lock_all` inhibits the outputs and shows the lock surfaces (wayfire:plugins/protocols/session-lock.cpp:398-428). With swaylock this is a window of up to a second, or until swaylock draws, in which the desktop shows after the lock was asked for; on a suspend that follows at once, the desktop is what the screen shows on resume. Shingle hides the clients from the request on (decision 1).
- **What a locked output shows.** The lock surface node goes into the `LOCK` layer (wayfire:plugins/protocols/session-lock.cpp:104-118), above everything. An output without a lock surface gets a `lock_crashed_node` with an empty text on a black background (wayfire:plugins/protocols/session-lock.cpp:404-411, 166-188). `set_inhibited` itself only cancels plugins (wayfire:src/output/output.cpp:339-346); what hides the desktop is the lock layer on top. A lock surface that is not opaque would show the desktop under it; Shingle draws black under it.
- **Output added while locked.** It gets the black crashed node at once, and its lock surface when the client makes one (wayfire:plugins/protocols/session-lock.cpp:368-377). Shingle: black until the new surface's first full frame.
- **Output removed.** Its state is dropped; the session stays locked (wayfire:plugins/protocols/session-lock.cpp:379-383). Shingle already did the same (crates/shingle/src/shell.rs, `lock_output_removed`).
- **Output resized.** The lock surface is configured to the new size (wayfire:plugins/protocols/session-lock.cpp:260-273), as Shingle's `reconfigure_lock_surfaces` does.
- **Lock client crash.** The lock goes to `ZOMBIE`, every output shows the crashed node (a black background with a "💥", wayfire:plugins/protocols/session-lock.cpp:190-193, 333-347) and the session stays locked; a new lock client may take over and unlock (wayfire:plugins/protocols/session-lock.cpp:497-510, 541-547). Shingle: `Failed`, black without the glyph, and the lock binding starts a new client while the outputs stay black.
- **DPMS and VT switches.** The plugin does nothing for them; wlroots draws a new frame of the scene when an output is enabled again. Shingle's native backend re-enables an output by a modeset that shows its current buffer before the next frame, which is why it needs decision 3.

## 2. Lock states

crates/shingle-idle/src/lib.rs, `LockState`:

```text
Unlocked --request_lock/lock timeout-------------> Starting
Unlocked, Starting, Failed --client_lock_requested-> Pending
Starting --lock_start_timeout---------------------> Pending
Pending  --lock_surface_ready---------------------> Locked
Locked   --unlock---------------------------------> Unlocked
Pending, Locked --lock_client_failed--------------> Failed
```

`LockState::hides_clients` is true for `Pending`, `Locked` and `Failed`. `Starting` blocks input like `Pending` but draws normally. `request_lock` in `Failed` starts a new client and stays `Failed`, so the outputs never show the desktop between two lock clients. The DPMS timeout does not fire while a lock is starting, pending or failed.

## 3. The no-flash guarantee

**Compositor.** crates/shingle/src/state.rs, `visible_surfaces_on`: while `hides_clients` holds, an output's surfaces are only the active lock's surface for that output (crates/shingle/src/shell.rs, `shown_lock_surface`), and only once it has committed a full frame: a buffer of the output's size that covers it opaquely (`lock_surface_committed` sets the role's `shown`). Surfaces of a lock that is gone, of another client, or not yet shown are not drawn. Layer surfaces and the cursor are hidden the same way.

**Frontend.** crates/shingle-frontend/src/frontend/lock.rs, `draw_lock_scene`: while `Compositor::lock_hides_clients` holds, the frontend does not build its normal scene at all. It draws those lock surfaces over black, and nothing else it draws: no Scale, Expo, switcher, workspace slide or swipe, drag, frame, label, closing or minimize animation, cursor or `background_color`. Geometry animations are dropped, so nothing drawn before the lock is animated from after it. Screencopy during the lock answers from the same scene. A lock state change asks every output for a frame.

**Native backend.** crates/shingle-native/src/display.rs, `set_lock_hides_clients`: each lit output remembers whether its buffer on screen may show a client (`shows_clients`, set by every flip from whether the lock hid them). While the lock hides them, `modeset` and `modeset_one` show a black dumb buffer instead of such a buffer. The host tells the display every turn (crates/shingle-native/src/host.rs, `follow_lock`).

Each case:

| Case | What the outputs show |
| --- | --- |
| A client asks for the lock | Black from the next frame on; the lock surface once it has a full frame. |
| The lock binding or timeout | The desktop until the started client asks (at most 5 s), with input cut off; then as above. |
| A lock surface's first frame is not opaque or not full size | Black, the session stays pending. |
| DPMS on | Native: the black buffer if the frame on screen predates the lock, then the lock. Offscreen and nested draw a new frame. |
| VT switch back (seat resume) | Native: the same as DPMS on. |
| An output added or turned on while locked | Black until its new lock surface has a full frame; native buffers of a new mode start black. |
| The lock client crashes | Black on every output (`Failed`) until a new lock client's surfaces. |
| Unlock | The normal scene on the next frame. |

The frame on screen when the lock is asked for stays until the next frame is presented, one refresh at most; a lock asked for while an output is off is covered by decision 3.

## 4. shingle-lock

crates/shingle-lock/src/main.rs runs in this order, so that nothing that can fail or take long happens after the lock is asked for:

1. `--daemonize` forks before anything else (crates/shingle-lock/src/daemon.rs); the parent waits on a pipe and exits 0 once the child reports the session locked, or 1. swaylock forks after locking, but by then `shingle-lock` has GPU driver threads that a fork would not copy.
2. Options (crates/shingle-lock/src/options.rs): defaults, then the swaylock config (`-C`, else `lock.swaylock_config`, else `$XDG_CONFIG_HOME/swaylock/config`, `~/.swaylock/config`, `/etc/swaylock/config`, unless `lock.read_swaylock_config` is false), then `lock.options` from Shingle's config (`--shingle-config`, else `SHINGLE_CONFIG_FILE`, else Shingle's default path), then the command line.
3. The GPU (`shingle_render::GpuContext` on the default adapter), the font (`fc-match` for `font`, else the bundled Inter, crates/shingle-lock/src/font.rs).
4. With `screenshot`: every `wl_output` captured with wlr-screencopy, without the cursor, within 2 s (crates/shingle-lock/src/capture.rs), and each capture uploaded and blurred once (crates/shingle-lock/src/gpu.rs, `Gpu::image`): `effect-blur=RxT` is T passes of a separable Gaussian with sigma R/2 over R texels; a radius above 16 blurs a copy a power of two smaller.
5. `ext_session_lock_manager_v1.lock`, one lock surface per output, and frames.

Each frame (`Target::draw`) draws the background, the blurred capture moved by the drift or the `color`, then `effect-vignette=B:F` as swaylock-effects computes it (`B + F * 16 * x * y * (1 - x) * (1 - y)`, crates/shingle-lock/src/lock.wgsl, `fs_background`), then the clock and the input as premultiplied overlays rasterized on the CPU with ab_glyph (crates/shingle-lock/src/ui.rs). The target is `Bgra8Unorm`, so its bytes read back are the `XRGB8888` the `wl_shm` buffer needs; the read-back is the one copy per frame. An output added after the lock has no capture and shows the colour.

**Pacing.** An output gets a frame only when its last frame callback is done and something changed: the drift's interval, a fade, the input, the clock's second. While an output is off Shingle sends no callback, so the drift stops there and resumes with the first callback after the wake; the configure of a new size is always answered at once.

**Input.** crates/shingle-lock/src/ui.rs, `Indicator`: typing shows `Input` with a dot per character and a key highlight (`key-hl-color`) on the border under the newest dot, with `separator-color` ends; Backspace shows the `bs-hl-color` highlight, or `Clear` once empty; Escape clears; Enter shows `Verify` while PAM runs, then `Wrong` with the count (`show-failed-attempts`) or unlocks. `Verify` never fades. Messages are drawn inside the input, as swaylock draws them inside its ring, so that the per-state text colours, black by default, stay readable on the per-state inside colours.

**Grace.** For `grace` seconds after `locked`, any key, pointer motion or button (unless `grace-no-mouse`) or touch (unless `grace-no-touch`) unlocks without a password.

**PAM** is unchanged: `shingle-auth` on a worker thread, the `shingle` service (data/pam/shingle) or `SHINGLE_PAM_SERVICE`, and `ZeroOnFree` for the password's memory.

`SHINGLE_LOCK_TRACE=1` logs every frame `shingle-lock` draws, which the GPU harness counts.

## 5. Options

Every swaylock and swaylock-effects option is recognised (crates/shingle-config/src/lock.rs, `LockOptions::apply`); the same names work in the config, on the command line (`--name[=value]`, swaylock's short options, bundles such as `-fF`) and in YAML `lock.options`.

| Option | Effect |
| --- | --- |
| `daemonize`, `-f` | Return once the session is locked. |
| `show-failed-attempts`, `-F` | Count failed attempts in the input. |
| `clock` | Show `timestr` over `datestr` (strftime, local time; defaults `%T` and `%a, %x`) in `text-color` with a soft shadow. |
| `screenshot`, `-S` | Blurred captures as the background. |
| `effect-blur=RxT`, `effect-vignette=B:F` | As in swaylock-effects. |
| `color`, `-c` | The background without a capture; default `1e222e`, not swaylock's white. |
| `font`, `font-size` | The font family; the time is 2.4 times `font-size`, the date and messages `font-size`; automatic sizes follow `indicator-radius`. |
| `indicator` | Show the input at rest too. |
| `no-unlock-indicator`, `-u` | Never show the input. |
| `indicator-radius`, `indicator-thickness` | The input's height (it is six times as wide) and twice its border width. |
| `ring-*`, `inside-*`, `line-*`, `text-*-color` with `-clear`, `-ver`, `-wrong` | Border, background, inner edge and dots or text per state. |
| `key-hl-color`, `bs-hl-color`, `separator-color` | The highlights on the border and their ends. |
| `line-uses-inside`, `-n`, `line-uses-ring`, `-r` | The inner edge in the inside or ring colour. |
| `grace`, `grace-no-mouse`, `grace-no-touch` | As in swaylock-effects. |
| `ignore-empty-password`, `-e` | Enter without a password does nothing; otherwise it is tried. |

Recognised but not drawn, with a warning: `image`, `scaling`, `tiling`, keyboard layouts, the Caps Lock colours and texts, the other effects (`effect-pixelate`, `effect-scale`, `effect-greyscale`, `effect-compose`, `effect-custom`), `fade-in`, `indicator-idle-visible`, the indicator position, `submit-on-touch`, `ready-fd`, `debug`, `trace`. Anything else warns as unknown. In YAML, which never warns, both are errors.

The YAML section, with its defaults:

```yaml
lock:
  command: shingle-lock
  read_swaylock_config: true
  swaylock_config: null
  options: {}
  drift: {enabled: true, fps: 8, amplitude: 12, period: 30}
  input_timeout: 5
```

## 6. Switching from swaylock

- Install `shingle-lock` next to `shingle` (README) and keep `~/.config/swaylock/config` as it is: `shingle-lock` reads it and prints one warning per option it does not draw.
- With a `wayfire.ini` as Shingle's config: a `command_lock = swaylock …` binding and a `swayidle … swaylock …` autostart line now run `shingle-lock` with the same arguments, and Shingle's log names each replaced line. To keep swaylock, write `/usr/bin/swaylock` in them.
- With the YAML config: the lock binding (`keybindings.lock`, `super+f8`) and `idle.lock_seconds` run `lock.command`. For swayidle, run `swayidle -w timeout 300 'shingle-lock -f' before-sleep 'shingle-lock -f'`: with `-f` the command returns only once the session is locked, so swayidle's `-w` holds a suspend until the lock is up and the desktop is never what shows on resume.
- The drift and the input timeout are only in the YAML `lock` section; `lock.options` can override any swaylock option, `false` turning a flag off.

## 7. Verification

Unit tests: the lock states (crates/shingle-idle/src/lib.rs); black until a full opaque frame, a crashed client, a lock the compositor starts (crates/shingle/src/lock_tests.rs); the lock scene hiding Scale until the lock's frame and after a crash, on the GPU (crates/shingle-frontend/src/frontend/tests/lock.rs); the frame recorder; the native black buffer on DPMS on, on resume and for a lock taken while the output was on (crates/shingle-native/src/display/lock_tests.rs); every swaylock option, the command line, the YAML section and the Wayfire swap (crates/shingle-config/src/lock.rs, wayfire.rs); the option order, the input's states and fades, the layout and its pixels, the drift's speed and edges, the blur plan, a blurred frame with its vignette and overlays drawn on the GPU, the screencopy conversion, the clock and the font lookup (crates/shingle-lock/src).

Real clients: `tools/gpu_test/run.py --checks lock` (tools/gpu_test/lock_checks.py) runs offscreen Shingle with two 640x400 outputs and `SHINGLE_RECORD_FRAMES` on a tmpfs, a `wl_probe --pattern` window and `shingle-lock` with an invented swaylock config, against a private PAM service in the namespace that accepts only an invented phrase. Every presented frame is checked: none while the lock hides the clients shows two of the pattern's neighbouring colours, pure, 8 pixels apart, and every one without a lock surface is black. Around that it checks the blurred capture, the drift's rate and movement, the input on typing, the verify and wrong states and the count, the fade-out, no frame and no drift while DPMS is off, the first frame after the wake, HEADLESS-2 turned off and on with `wlr-randr`, the unlock, a SIGKILLed client leaving the outputs black, the lock binding starting a new client then, which unlocks, another client's grace-period unlock after a second crash, and the lock binding capturing the clients while `Starting`. Results are in [status.md](../status.md#with-real-clients).

Not verified here: the native black buffer on hardware (steps in [native-testing.md](../native-testing.md) once the test machine runs), and the look on the operator's monitors with their own config.

## Decisions

1. **No client from the lock request on, black until a full frame.** Wayfire shows the desktop for up to a second after the request and under a lock surface that is not opaque; the operator's complaint is exactly the desktop showing through. Shingle hides the clients from the request (Pending) and draws black until the lock surface's first full opaque frame. Rejected: drawing a translucent first frame over black (it would show a half-drawn lock).
2. **`Starting` for locks Shingle starts itself.** A screenshot needs the screen; if the lock binding or timeout went straight to `Pending`, `shingle-lock` would capture black. Input stops at once, the clients stay visible only until the client asks for the lock, and at most 5 s (`lock_start_timeout`), which is what swayidle and swaylock already do today. Rejected: serving the lock client's screencopy from a pre-lock frame kept by the frontend (a second scene path and a copy of every output's last frame), and passing screenshots to the client in files.
3. **Black dumb buffers for native modesets under the lock.** A modeset shows the CRTC's current buffer before the next frame; after a DPMS wake or VT switch that buffer may predate the lock. A black buffer per mode size, made when the lock starts hiding the clients and freed when it ends, costs nothing outside a lock. Rejected: turning the primary plane off for those modesets, which not every driver allows with an active CRTC.
4. **Screenshots before the lock request.** As swaylock-effects does; the compositor shows no client once the lock is pending, and blurring before the request keeps the first locked frame immediate.
5. **CPU read-back into `wl_shm`.** One GPU-drawn frame read back per drift frame (8 per second, about 8 MB per 1080p frame) is cheap next to the GPU work and needs no DMA-BUF allocation in the client; Shingle requires the lock buffer at the output's logical size, which rules out a smaller blurred buffer with a viewport. Revisit with linux-dmabuf if battery measurements on the laptop say so.
6. **Frame callbacks gate the drift.** No timer can tell that an output is off; the compositor's callbacks can. Rejected: wlr-output-power-management in the client, which says nothing about outputs the compositor does not draw for other reasons.
7. **The input's geometry from swaylock's indicator options.** `indicator-radius` is the input's height (six times as wide), `indicator-thickness` twice its border width (clamped to a quarter of the height), so a swaylock ring's proportions map to a box of similar weight.
8. **Swaylock's colour roles kept.** ring → border, inside → background, line → the border's inner edge, text → dots and messages, key and backspace highlights on the border with separator-coloured ends, so every `*-color` option of the operator's config has an effect.
9. **Messages inside the input.** As swaylock draws them inside the ring; outside, the per-state text colours (black by default) would be unreadable on a dark blurred background.
10. **Precedence: defaults, swaylock config, YAML, command line.** The swaylock config is the operator's existing one; YAML adds Shingle's own settings and may override; a command line wins as in swaylock. `lock.options` uses swaylock's names rather than new ones, so nothing needs translating.
11. **Default background colour.** A dark blue-black instead of swaylock's white, so that a lock without a config is not blinding at night.
12. **Wayfire imports swap bare `swaylock` only.** The task: keep the operator's command unless it is swaylock. A path to swaylock is the way to keep it.
13. **Daemonize before any thread.** swaylock forks after locking; with GPU driver threads that is unsafe, so `shingle-lock` forks first and the parent waits for the child's report.
14. **The crash screen is plain black.** Wayfire shows a large glyph; black is enough to show the session is locked, and a new lock client takes over.
15. **The lock binding after a crash.** Every binding but `always` commands is off while locked, so after a crash only a VT switch was left to start a new lock client. In `Failed` the lock binding (`keybindings.lock`) alone runs and starts `lock.command`; the outputs stay black until the new client shows its lock. Wayfire refuses plugin activation on inhibited outputs, so there a new client has to come from elsewhere.
16. **Frame recorder in the frontend.** `SHINGLE_RECORD_FRAMES` redraws each presented frame with the same tiles and writes it with its lock state and time; the GPU harness can then check every frame rather than sampling with `grim`.

## Open

- The native modeset rule needs the test machine: DPMS off, lock, DPMS on, and a VT switch while locked, with the camera on the screen.
- The operator's own config: check the warnings it prints and the look of its colours on the outlined input.
- Caps Lock indication and keyboard layouts are not drawn.
- A lock asked for while an output is off and then a suspend: with swayidle use `-w` and `shingle-lock -f`.
