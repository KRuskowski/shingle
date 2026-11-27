# Lock screen design

Status: built, 2026-11-27, in workspace `shingle-lock-screen`, with the open questions decided on the operator's behalf ([Decisions](#decisions)); the operator reviews them. Follow-ups, 2026-11-27, in workspace `shingle-lock-followups`: Caps Lock and the keyboard layout drawn in the input, a blurred capture on outputs added while locked, the GPU opened while the captures wait, and every swaylock-effects option checked against its source (decisions 17 to 24). What was asked: make `shingle-lock` the operator's lock screen in place of their swaylock-effects setup, with a blurred screenshot of every output that never lets the real screen flash through, a little movement in the blur, only the blur, vignette and clock at rest, an outlined password input that fades in on typing, and the operator's swaylock config carried over.

Sources: Shingle on branch `shingle-lock-screen`, cut from `master` at `aca9bd9`; Wayfire at `669bbe36` in /home/karl/dev/workspaces/shingle-input-gaps/wayfire (cited as `wayfire:PATH:LINE`); the ext-session-lock-v1 protocol text; swaylock's and swaylock-effects' option sets and config format as their manuals describe them; for the follow-ups, `main.c`, `render.c` and `swaylock.1.scd` of jirutka/swaylock-effects and `main.c`, `render.c` and `seat.c` of swaywm/swaylock, both at their `master` on 2026-11-27 (cited as `swaylock-effects:FILE` and `swaylock:FILE`). The operator's own swaylock config was not available on this host and is not copied anywhere; tests use invented values.

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
| 10 | Caps Lock and layouts | Small labels inside the input while typing: the layout's name on a plate at the left, `Caps Lock` at the right; `indicator-caps-lock` turns the input into its Caps Lock colours. |
| 11 | Outputs added while locked | Their own capture if they had one (found by `wl_output` name), else the capture closest in shape, scaled to cover, never a client. |
| 12 | Time to lock | The GPU opens while the captures wait for their frames; a capture not answered in 1 s is dropped. |

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
3. On a thread, the GPU (`shingle_render::GpuContext` on the default adapter) and the font (`fc-match` for `font`, else the bundled Inter, crates/shingle-lock/src/font.rs); meanwhile, with `screenshot`, every `wl_output` is captured with wlr-screencopy, without the cursor, within 1 s (crates/shingle-lock/src/capture.rs), each with its `wl_output` name.
4. Once both are done, each capture is uploaded and blurred once (crates/shingle-lock/src/gpu.rs, `Gpu::image`): `effect-blur=RxT` is T passes of a separable Gaussian with sigma R/2 over R texels; a radius above 16 blurs a copy a power of two smaller. The captures are kept until the lock ends.
5. `ext_session_lock_manager_v1.lock`, one lock surface per output, and frames.

Each frame (`Target::draw`) draws the background, the blurred capture moved by the drift or the `color`, then `effect-vignette=B:F` as swaylock-effects computes it (`B + F * 16 * x * y * (1 - x) * (1 - y)`, crates/shingle-lock/src/lock.wgsl, `fs_background`), then the clock and the input as premultiplied overlays rasterized on the CPU with ab_glyph (crates/shingle-lock/src/ui.rs). The target is `Bgra8Unorm`, so its bytes read back are the `XRGB8888` the `wl_shm` buffer needs; the read-back is the one copy per frame.

**Which capture an output shows** (crates/shingle-lock/src/main.rs, `pick_capture`): its own, by its global, or by its `wl_output` name when it was turned off and on again and came back under a new global; otherwise, for an output added while locked or one whose capture was not answered in time, the capture whose aspect ratio is closest to its own, the larger on a tie, then the first output's. A capture of another shape covers the output with its aspect ratio kept and the overflow cut evenly (crates/shingle-lock/src/gpu.rs, `cover_scale`). Without any capture the output shows `color`. Every capture was taken before the lock request, so none shows a client drawn later, and the compositor never draws one under the lock anyway.

**Time to lock.** `shingle-lock` reports each step on standard error (`shingle-lock: timing: ...`) and the time to `ready`. Measured in the GPU harness's namespace (debug builds, the default adapter the NVIDIA RTX 3060, 640x400 and 1280x720 outputs), 2026-11-27:

| | Before | After |
| --- | --- | --- |
| Lock binding to every output showing the lock (`--checks lock`) | 0.294 s | 0.274 s, 0.275 s |
| Start to the lock request, offscreen, one output (three runs) | 228 to 253 ms | 198 to 232 ms |
| The same, headless | 244 to 304 ms | 197 to 273 ms |
| The same, nested Shingle with two windows in offscreen Shingle | 318 to 357 ms | 278 to 336 ms |
| Of that, the captures | 20 to 89 ms | hidden behind the GPU |
| Of that, the GPU (the Vulkan loader and driver about 80 to 100 ms, the device and pipelines the rest) | 157 to 302 ms | the same, now beside the captures |

No step took 2 s in any of these runs, nor with DPMS off (a capture of an output that is off fails at once). The only 2 s wait in the code was the capture timeout, which an output whose frames never come waits out in full; it is 1 s now, and such an output shows another output's capture. What is left is the GPU's opening, which is driver work rather than waiting.

**Pacing.** An output gets a frame only when its last frame callback is done and something changed: the drift's interval, a fade, the input, the clock's second. While an output is off Shingle sends no callback, so the drift stops there and resumes with the first callback after the wake; the configure of a new size is always answered at once.

**Input.** crates/shingle-lock/src/ui.rs, `Indicator`: typing shows `Input` with a dot per character and a key highlight (`key-hl-color`) on the border under the newest dot, with `separator-color` ends; Backspace shows the `bs-hl-color` highlight, or `Clear` once empty; Escape clears; Enter shows `Verify` while PAM runs, then `Wrong` with the count (`show-failed-attempts`) or unlocks. `Verify` never fades. Messages are drawn inside the input, as swaylock draws them inside its ring, so that the per-state text colours, black by default, stay readable on the per-state inside colours.

**Caps Lock and the keyboard layout.** crates/shingle-lock/src/ui.rs, `Labels` and `draw_labels`; read from the keyboard's xkb state at every keymap and modifiers event (crates/shingle-lock/src/main.rs, `KeyboardStatus`), as swaylock reads them: Caps Lock is the locked `Lock` modifier (swaylock:seat.c, `keyboard_modifiers`), the layout the first effective one and its name `xkb_keymap_layout_get_name`'s (swaylock:render.c). While the input shows `Input`, the layout's name sits on a small rounded `layout-bg-color` plate with a `layout-border-color` outline at the input's left, in `layout-text-color`, when the keymap has more than one layout or `show-keyboard-layout` is set, never with `hide-keyboard-layout`; and while Caps Lock is on, `text-caps-lock` (default `Caps Lock`) in `text-caps-lock-color` at its right, unless `disable-caps-lock-text`. Each is at most a third of the input wide, its text about 0.7 of the input's; the dots or the message take the middle. With `indicator-caps-lock` the input itself takes the `*-caps-lock-color` colours and the `caps-lock-key-hl-color` and `caps-lock-bs-hl-color` highlights while Caps Lock is on (swaylock:render.c, `set_color_for_state`). Neither label shows while the input is cleared, verifying or wrong, as in both swaylocks. A group change redraws the label; the Caps Lock key itself is a key press, which shows the input as any key does.

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
| `indicator-caps-lock`, `-l`, `disable-caps-lock-text`, `-L` | Caps Lock's colours on the input, and no Caps Lock label. |
| `ring-`, `inside-`, `line-`, `text-caps-lock-color`, `caps-lock-key-hl-color`, `caps-lock-bs-hl-color` | The input while Caps Lock is on, with `indicator-caps-lock`; the text colour is also the label's. |
| `show-keyboard-layout`, `-k`, `hide-keyboard-layout`, `-K` | The layout's label with only one layout, or never. |
| `layout-bg-color`, `layout-border-color`, `layout-text-color` | The layout label's plate, outline and text. |
| `text-clear`, `text-caps-lock`, `text-ver`, `text-wrong` | The texts of the cleared, Caps Lock, verifying and wrong states. |
| `indicator-idle-visible` | As `indicator`. |

Recognised but not drawn, with a warning: `image`, `scaling`, `tiling`, `indicator-image`, the other effects (`effect-pixelate`, `effect-scale`, `effect-greyscale`, `effect-compose`, `effect-custom`) and `time-effects`, `fade-in`, the indicator position, `submit-on-touch`, `ready-fd`, `debug`, `trace`. Anything else warns as unknown. Each warns once for each time it is given. In YAML, which never warns, both are errors.

**Checked against the sources** (2026-11-27, crates/shingle-config/src/lock.rs, `every_swaylock_effects_option_is_known`): every entry of swaylock-effects' `long_options` (swaylock-effects:main.c) and swaylock's `ready-fd` (swaylock:main.c) is either set or warns as unsupported, and takes a value exactly where the source's entry has `required_argument`; `config`, `help` and `version` are the command line's. Found on the way: the Caps Lock and layout options are now drawn; swaylock-effects has `indicator-image`, `time-effects` and the four `text-*` texts, which warned as unknown before; `bs-hl-caps-lock-color` and `pass-char-keys`, which `shingle-lock` listed, are in neither source and now warn as unknown; `-t` is `trace` in swaylock-effects and `tiling` in swaylock (`shingle-lock` follows swaylock-effects, and both warn), `-T` is swaylock-effects' `tiling`, `-R` swaylock's `ready-fd`; swaylock-effects spells its option `screenshots` (`-S`), and `shingle-lock` takes `screenshot` too. Two defaults differ between the two: swaylock-effects' `indicator-radius` is 75 (swaylock-effects:main.c, `.radius = 75`), swaylock's 50, which `shingle-lock` keeps (decision 23), and the background is white in swaylock-effects, grey (`a3a3a3`) in swaylock. A typical swaylock-effects config (`screenshots`, `clock`, `indicator`, `effect-blur`, `effect-vignette`, the colours, `grace`, `fade-in`, `datestr`, `timestr`) is therefore taken whole except `fade-in`, which warns once.

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

Unit tests: the lock states (crates/shingle-idle/src/lib.rs); black until a full opaque frame, a crashed client, a lock the compositor starts (crates/shingle/src/lock_tests.rs); the lock scene hiding Scale until the lock's frame and after a crash, on the GPU (crates/shingle-frontend/src/frontend/tests/lock.rs); the frame recorder; the native black buffer on DPMS on, on resume and for a lock taken while the output was on (crates/shingle-native/src/display/lock_tests.rs); every swaylock option, every swaylock-effects option against its source, the Caps Lock, layout and text options and their rules, the command line, the YAML section and the Wayfire swap (crates/shingle-config/src/lock.rs, wayfire.rs); the option order, the input's states and fades, the layout and its pixels, the Caps Lock and layout labels and the Caps Lock colours, the keyboard status of an `us,de` keymap, the capture an output shows, the cover scale and a covering capture drawn on the GPU, the drift's speed and edges, the blur plan, a blurred frame with its vignette and overlays drawn on the GPU, the screencopy conversion, the clock and the font lookup (crates/shingle-lock/src).

Real clients: `tools/gpu_test/run.py --checks lock` (tools/gpu_test/lock_checks.py) runs offscreen Shingle with two 640x400 outputs and `SHINGLE_RECORD_FRAMES` on a tmpfs, a `wl_probe --pattern` window and `shingle-lock` with an invented swaylock config, against a private PAM service in the namespace that accepts only an invented phrase. Every presented frame is checked: none while the lock hides the clients shows two of the pattern's neighbouring colours, pure, 8 pixels apart, and every one without a lock surface is black. Around that it checks the blurred capture, the drift's rate and movement, the input on typing, the verify and wrong states and the count, the fade-out, no frame and no drift while DPMS is off, the first frame after the wake, HEADLESS-2 turned off and on with `wlr-randr`, the unlock, a SIGKILLed client leaving the outputs black, the lock binding starting a new client then, which unlocks, another client's grace-period unlock after a second crash, and the lock binding capturing the clients while `Starting`, with every output showing the lock within 1 s of the key. With an `us,de` keymap and an invented config that sets `indicator-caps-lock` with a violet Caps Lock ring and a mint layout text, it also checks the layout's label on typing, Caps Lock's colours and label and their end, the other layout's name after `wayfire/set-keyboard-state`, HEADLESS-2 showing its own capture again after it came back, and an 800x400 output created over IPC while locked showing HEADLESS-1's capture, blurred and scaled to cover, in a `grim` capture (IPC outputs are not presented, so the recorder does not see them). Results are in [status.md](../status.md#with-real-clients).

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

17. **Caps Lock and the layout as small labels inside the input.** swaylock replaces the ring's text with `Caps Lock` and draws the layout in a box below the ring; inside the outlined input, the layout on the left and Caps Lock on the right keep the dots visible and add nothing outside the input, so the screen at rest stays the blur, vignette and clock. Rejected: a coloured edge alone for Caps Lock (`indicator-caps-lock` already gives that, and without it a colour says nothing to someone who does not know the config), and a box under the input as swaylock draws (a second element to fade).
18. **Labels only while typing, as swaylock.** Both swaylocks show them only in the input and backspace states; cleared, verifying and wrong keep their message alone. A Caps Lock or group change while the input is hidden does not show it; the Caps Lock key does, as any key.
19. **The Caps Lock text colour for the label in every case.** swaylock uses `text-caps-lock-color` for its text whether or not `indicator-caps-lock` is set; with it set, the dots take it too, as the rest of the Caps Lock colour set.
20. **An added output takes the capture closest in shape, scaled to cover.** The closest aspect ratio needs the least cut off; the larger wins a tie because it loses less detail when scaled; the first output's on a full tie, usually the main one. Covering rather than fitting: letterboxing would show bars of `color` beside the blur, and stretching distorts it. Rejected: the output beside the new one in the layout (the lock client does not know the layout), and the colour (the task).
21. **Captures kept for the whole lock, found again by name.** An output turned off and on, or unplugged and plugged back, comes back under a new `wl_output` global; its name (version 4) finds its own capture. The textures cost a few MB per output for the lock's lifetime.
22. **The GPU opened beside the captures, a 1 s capture timeout.** The captures wait for the compositor's next frame of each output (20 to 90 ms measured) while the process has nothing else to do; the GPU's opening (160 to 300 ms, mostly the Vulkan loader and driver) is the larger cost and now runs beside them. A drawn output answers within a frame or two and one that is off fails at once, so an output that has not answered after 1 s is not being drawn; waiting the old 2 s for it only delayed the lock, and decision 20 gives it a blurred background anyway.
23. **`indicator-radius` keeps swaylock's default of 50.** swaylock-effects defaults to 75; as the input's height that would make it 450 pixels wide by default. Configs that set it are unaffected.
24. **`-t` is swaylock-effects' `trace`.** The operator comes from swaylock-effects; both meanings are unsupported and warn under their own name.

## Open

- The native modeset rule needs the test machine: DPMS off, lock, DPMS on, and a VT switch while locked, with the camera on the screen.
- The operator's own config: check the warnings it prints and the look of its colours on the outlined input.
- The time to lock on the laptop's Intel GPU and with a release build; the measurements above are debug builds on the host's NVIDIA GPU in the harness.
- Shingle's frontend answers a capture only with a drawn frame, so a nested Shingle whose host window is not drawn still makes `shingle-lock` wait its 1 s for that output (not reproduced: nested Shingle in offscreen Shingle answered within 90 ms, also with the host's outputs off by DPMS).
- A lock asked for while an output is off and then a suspend: with swayidle use `-w` and `shingle-lock -f`.
