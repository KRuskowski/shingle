# Native backend test plan

The steps here verify the parts of the native backend that need a seat, real input devices, a VT or a display. They run only on the dedicated test machine ([design, Layer 2](design/native-backend.md#layer-2-dedicated-test-machine)), never on a host whose display, input or VTs belong to someone's session: starting `shingle` from a VT login takes that VT, and the uinput tool injects input into whatever session is active. Unit tests through fakes cover the logic behind every step ([status.md](status.md#automated)); this plan checks that the real libseat and libinput behave the way the fakes assume.

Record each result in [status.md](status.md#with-real-clients), with the date, the commit, the libseat backend and the devices used, separately from the unit tests.

## NB-6: seat and input, no display

NB-6 has no KMS output yet. While `shingle` runs on a VT, the seat daemon has put that VT into graphics mode and nothing is drawn, so the screen keeps whatever it showed and typing on it reaches Shingle, not the console. Watch Shingle over SSH: `~/.local/state/shingle/native.log` (or `$XDG_STATE_HOME/shingle/native.log`) receives every message the native backend writes, and IPC answers on the socket given to `--ipc`. Ctrl+Alt+F2 on the physical keyboard, or `sudo chvt 2` over SSH, always switches away, and `kill -TERM` over SSH always ends Shingle.

### Step 0: prepare the test machine

1. Install the build dependencies from [development.md](development.md#requirements), plus `seatd`, `wev`, `python3-evdev`, `wayland-utils` and `procps`.
2. Mark the machine, so that tools/native_test/uinput.py agrees to create devices there and nowhere else:

   ```sh
   sudo touch /etc/shingle-native-test-machine
   ```

3. Build at the commit under test and put the binary on `PATH`:

   ```sh
   git -C ~/shingle checkout <commit>
   cargo build --manifest-path ~/shingle/Cargo.toml --bin shingle
   sudo install -m 755 ~/shingle/target/debug/shingle /usr/local/bin/shingle
   ```

4. Give the test user a test config without session restore or XWayland, so every run starts the same way:

   ```sh
   mkdir -p ~/.config/shingle
   printf 'session: {restore: false}\nxwayland: {mode: false}\n' \
     > ~/.config/shingle/test.yaml
   ```

5. In every step, "start Shingle on tty1" means: log in on tty1 as the test user (a VT login gets a logind session with `XDG_SEAT=seat0` and `XDG_RUNTIME_DIR`), then run

   ```sh
   rm -f ~/.local/state/shingle/native.log /tmp/wev.log /tmp/shingle-exit
   shingle --config ~/.config/shingle/test.yaml \
     --ipc "$XDG_RUNTIME_DIR/shingle-ipc" \
     --spawn sh -c 'exec wev > /tmp/wev.log 2>&1' \
     2> /tmp/shingle-stderr; echo "exit=$?" > /tmp/shingle-exit
   ```

   and continue over SSH as the same user, with

   ```sh
   export IPC="/run/user/$(id -u)/shingle-ipc"
   alias ipc='python3 ~/shingle/tools/native_test/ipc.py "$IPC"'
   alias uinput='sudo python3 ~/shingle/tools/native_test/uinput.py'
   ```

   `wev` is the test client: it maps one window, which Shingle tiles over the whole output and focuses, and it logs every keyboard, pointer and touch event with its `time`.

### Step 1: a VT login selects native, under logind and under seatd

Acceptance: "`shingle` started from tty1 selects the native backend and runs under seatd or logind; inside a Wayland session it still selects nested."

1. Make sure seatd is stopped (`sudo systemctl stop seatd`), then start Shingle on tty1.
2. Over SSH:

   ```sh
   cat /tmp/shingle-stderr ~/.local/state/shingle/native.log
   ipc input/list-devices
   ls -l /proc/$(pgrep -n -u "$USER" -x shingle)/fd | grep -E 'dri|input|tty'
   ```

   Expected: stderr starts with `shingle: using the native backend (none of WAYLAND_DISPLAY, WAYLAND_SOCKET or DISPLAY is set)`. The log has `seat seat0 enabled (logind backend)` and one `input device eventN (NAME) added as seat devices ...` line per keyboard, mouse and touchpad. `input/list-devices` lists those devices with their real names and numeric `vendor` and `product`, `"enabled": true`, one entry per capability (a keyboard with a touchpad appears as a `keyboard` and a `pointer` with the same name). The descriptor list shows `/dev/input/event*` nodes and no `/dev/dri` node.
3. End Shingle with `kill -TERM $(pgrep -n -u "$USER" -x shingle)`.
4. Start seatd for the test user's group and repeat with libseat's seatd backend:

   ```sh
   sudo systemctl start seatd
   # On tty1:
   LIBSEAT_BACKEND=seatd shingle --config ~/.config/shingle/test.yaml \
     --ipc "$XDG_RUNTIME_DIR/shingle-ipc" \
     --spawn sh -c 'exec wev > /tmp/wev.log 2>&1' \
     2> /tmp/shingle-stderr; echo "exit=$?" > /tmp/shingle-exit
   ```

   Expected: as in 2, with `seat seat0 enabled (seatd backend)`. If the user may not use seatd, the log or stderr says `no seat: libseat could not open one (...)` and Shingle exits with status 1 before creating `$XDG_RUNTIME_DIR/wayland-*`.
5. Nested selection: in a graphical session on the test machine, if one is installed (otherwise the unit tests in crates/shingle-bin/src/backend.rs cover it), run `shingle --config ~/.config/shingle/test.yaml` from a terminal. Expected: `shingle: using the nested backend (WAYLAND_DISPLAY is set)` and a Shingle window.

### Step 2: SSH logins are refused

Acceptance: "Refuses to start over SSH with the 'no seat' message."

1. With nothing running on tty1, from another machine:

   ```sh
   ssh test@test-machine 'shingle; echo "status=$?"; ls "$XDG_RUNTIME_DIR"'
   ```

   Expected, exactly:

   ```
   shingle: no seat: started over SSH (SSH_CONNECTION is set and XDG_SEAT is not); run from a VT login or start seatd
   shingle: the native backend was selected because none of WAYLAND_DISPLAY, WAYLAND_SOCKET or DISPLAY is set; inside a Wayland or X11 session shingle runs nested, and --headless needs no seat.
   status=1
   ```

   and no `wayland-*` socket in the listing.
2. `ssh test@test-machine 'shingle --backend native; echo "status=$?"'` gives the same first line, the second line ending in `was requested on the command line; ...`, and `status=1`.
3. Past the SSH check, libseat itself must refuse (under logind an SSH session has no seat): `ssh test@test-machine 'XDG_SEAT=seat0 shingle; echo "status=$?"'`. Expected within 5 s: `shingle: no seat: libseat could not open one (...)` or `shingle: no seat: the seat was not enabled within 5 s, so this session is not on the active VT; ...`, and `status=1`. Run this only while tty1 shows a text console, and stop seatd first (`sudo systemctl stop seatd`), since seatd may hand the seat to any client in its group.

### Step 3: uinput keyboard, mouse and touch events reach the client with libinput timestamps

Acceptance: "uinput keyboard, mouse and touch events reach a test client with libinput timestamps."

1. Start Shingle on tty1.
2. Over SSH:

   ```sh
   uinput key KEY_A
   uinput motion 10 5
   uinput button BTN_LEFT
   uinput wheel -1
   uinput touch 0.5 0.5
   grep -E 'wl_keyboard\] key|wl_pointer\] (motion|button|axis)|wl_touch\] (down|up)' /tmp/wev.log
   ```

   Each `uinput` command prints lines such as `KEY_A (30) pressed at 1234567.891 ms`, the `CLOCK_MONOTONIC` time right after the event was written, which is the clock libinput stamps events with.

   Expected in wev's log: a `wl_keyboard` key event with `key: 30` and state pressed, then released; a pointer `motion`; a `button` event for 272 (`BTN_LEFT`) pressed and released; an `axis` event (vertical, wheel source); and a `wl_touch` `down` and `up`. Each event's `time` equals the printed time of the matching `uinput` line, truncated to whole milliseconds, within 2 ms. A difference of a frame or more (16 ms) means the compositor's clock was used instead of libinput's.
3. The touch `down` position is the middle of the 1280x720 output, about (640, 360) in wev's window coordinates.

### Step 4: hotplugged devices appear in and leave `input/list-devices`

Acceptance: "Hotplugged uinput devices appear in and disappear from `input/list-devices`."

1. Start Shingle on tty1.
2. Over SSH:

   ```sh
   uinput device keyboard --seconds 10 &
   sleep 3; ipc input/list-devices
   wait; sleep 1; ipc input/list-devices
   ```

   Expected: the first listing has a `keyboard` named `shingle-test keyboard` with `"vendor": 4660`, `"product": 22136` and `"enabled": true`, with an `id` higher than every device from startup; the log says `input device eventN (shingle-test keyboard) added as seat devices M`. The second listing no longer has it, and the log says `input device eventN removed (seat devices M)`.
3. Repeat with `uinput device mouse` (`type` `pointer`) and `uinput device touch` (`type` `touch`).
4. A device plugged in again gets a new `id`; ids are never reused.

### Step 5: `configure-device` disables a real device and releases its held keys

Acceptance: "`configure-device` disables a real device and releases its held keys."

1. Start Shingle on tty1.
2. Over SSH, hold Shift on a virtual keyboard and find its id:

   ```sh
   uinput --settle 2 key KEY_LEFTSHIFT --hold 20 &
   sleep 4; ipc input/list-devices
   ```

   wev's log shows `key: 42` pressed and a `modifiers` event with Shift depressed.
3. Disable it, with ID being the `id` of `shingle-test keyboard`:

   ```sh
   ipc input/configure-device '{"id": ID, "enabled": false}'
   ipc input/list-devices
   ```

   Expected: the reply is `{"result": "ok"}`; at once, wev's log shows `key: 42` released and a `modifiers` event with nothing depressed; the listing shows `"enabled": false` for it. When the `uinput` command releases Shift 20 s after it started, wev shows nothing more.
4. While it is disabled, `uinput key KEY_B` from a second virtual keyboard still reaches wev, which shows that only the disabled device is affected. Physical keys on the disabled device (if a real keyboard was disabled instead) produce nothing.
5. `ipc input/configure-device '{"id": ID, "enabled": true}'` re-enables it; wev's next `modifiers` event shows nothing depressed.
6. Repeat 2 to 5 with the test machine's real keyboard, holding a key physically while it is disabled, to check libinput's send-events mode on real hardware: with the keyboard disabled, typing on it reaches neither wev nor Shingle's bindings.

### Step 6: switching VT away releases held input, switching back restores it

Acceptance: "`chvt` away releases held input and `chvt` back restores it."

1. Start Shingle on tty1.
2. Over SSH:

   ```sh
   uinput --settle 2 key KEY_A --hold 15 &
   sleep 4; ipc input/list-devices > /tmp/before.json
   sudo chvt 2; sleep 2
   tail -n 5 ~/.local/state/shingle/native.log
   ```

   Expected: wev's log shows `key: 30` pressed, then released once the switch happens; the log says `seat disabled; paused`; `fgconsole` (with sudo) prints `2`; Shingle and wev keep running.
3. Switch back and check:

   ```sh
   sudo chvt 1; sleep 2
   tail -n 5 ~/.local/state/shingle/native.log
   ipc input/list-devices > /tmp/after.json; diff /tmp/before.json /tmp/after.json
   uinput key KEY_B
   ```

   Expected: the log says `seat enabled; resumed`; the device listing is unchanged (same ids and enabled states), apart from the uinput keyboard if it was removed in the meantime; `KEY_B` (48) reaches wev with a libinput time as in step 3; the release of `KEY_A` at the end of its 15 s does not reach wev.
4. The Ctrl+Alt+Fn switch: `uinput key KEY_LEFTCTRL KEY_LEFTALT KEY_F2`. Expected: `fgconsole` prints `2`, wev's log has `key: 29` and `key: 56` but no `key: 60` (F2 is not passed on), and the log says `seat disabled; paused`. `sudo chvt 1` resumes as in 3. Also press Ctrl+Alt+F2 on the physical keyboard, then Ctrl+Alt+F1 on tty2's console, and check the same.
5. A device disabled over IPC before the switch (step 5) is still disabled after it.

### Step 7: SIGTERM exits with status 0 and tty1 is usable again

Acceptance: "SIGTERM exits 0 and tty1 is usable again."

1. Start Shingle on tty1.
2. Over SSH:

   ```sh
   pid=$(pgrep -n -u "$USER" -x shingle); echo "$pid"
   kill -TERM "$pid"; sleep 1; cat /tmp/shingle-exit
   ls -l ~/.local/state/shingle/session.json
   tail -n 2 ~/.local/state/shingle/native.log
   ```

   Check that `$pid` is a single number greater than 1 before running `kill`. Expected: `exit=0`; the session file was written just now; the log ends with `native backend stopped`.
3. On the physical screen and keyboard: tty1 shows the shell prompt again and typing `echo ok` and Enter prints `ok`. Ctrl+Alt+F2 and Ctrl+Alt+F1 switch VT as usual.
4. Repeat with `kill -INT`.
5. Not an NB-6 acceptance item, but cheap to check here: `kill -KILL "$pid"` instead of SIGTERM. The seat daemon must restore tty1 (prompt and typing work, Ctrl+Alt+Fn works); `/tmp/shingle-exit` says `exit=137`.

### Step 8: libseat backends without VT switching

Not an acceptance item, but the refusal is only unit-tested.

1. As root on tty1 with seatd stopped: `LIBSEAT_BACKEND=builtin shingle --config ~/.config/shingle/test.yaml`. Expected: `shingle: the libseat builtin backend cannot switch VT, so there would be no way back to a text console; start seatd or log in on a VT, or pass --allow-builtin-seat to use it anyway`, status 1, and tty1 still a text console. (If Debian's libseat is built without the builtin backend, libseat fails instead, with `no seat: libseat could not open one (...)`.)
2. With `--allow-builtin-seat` it starts; end it with SIGTERM over SSH.

## NB-7: KMS display with copy scanout

From NB-7 on, Shingle lights the monitor: it opens the boot GPU's card through the seat, sets the monitor's preferred mode with atomic KMS and shows every frame by copying it into a dumb buffer. The screen therefore shows Shingle while it runs on tty1, and the steps below look at the screen as well as at the log over SSH. Steps 1 to 8 still apply unchanged, except that `ls -l /proc/PID/fd` now also lists one `/dev/dri/cardN` node.

Unit tests through a fake card cover the logic behind every step ([design, NB-7 as built](design/native-backend.md#nb-7-as-built)); these steps check that real drivers behave the way the fake assumes.

Since NB-8, outputs move to zero-copy scanout when they can (steps 36 to 42). Steps 10 to 15 were written for copy scanout: run them with `SHINGLE_SCANOUT=copy` in Shingle's environment to check the copy path, and once more without it, in which case the log has `output NAME: zero-copy scanout, modifier M` after the output's line and presented frames carry `"flags": 15` instead of 7.

### Step 9: prepare the test machine for NB-7

1. In addition to step 0, install `foot`, `grim`, `wlopm` and `wayland-utils`, and connect one monitor to the GPU that drives the boot console (the one whose `/sys/class/drm/cardN/device/boot_vga` is `1`).
2. Build and install the presentation probe next to `shingle`:

   ```sh
   cargo build --manifest-path ~/shingle/Cargo.toml -p shingle-native \
     --example present_probe
   sudo install -m 755 ~/shingle/target/debug/examples/present_probe \
     /usr/local/bin/present_probe
   ```

3. In the NB-7 steps, "start Shingle on tty1 with CLIENT" means: log in on tty1 as the test user and run

   ```sh
   rm -f ~/.local/state/shingle/native.log /tmp/shingle-exit
   SHINGLE_TRACE_FLIPS=1 shingle --config ~/.config/shingle/test.yaml \
     --ipc "$XDG_RUNTIME_DIR/shingle-ipc" --spawn CLIENT \
     > /tmp/shingle-stdout 2> /tmp/shingle-stderr
   echo "exit=$?" > /tmp/shingle-exit
   ```

   and continue over SSH with the variables of step 0, plus

   ```sh
   export XDG_RUNTIME_DIR="/run/user/$(id -u)"
   export WAYLAND_DISPLAY="$(sed -n 's/^WAYLAND_DISPLAY=//p' /tmp/shingle-stdout)"
   ```

   `SHINGLE_TRACE_FLIPS=1` adds a `CONN: page flip seq=S at SEC.NSEC s` line to the log for every frame shown, with `CONN` the connector that showed it (before MO-7 the line had no connector).

### Step 10: a client is visible on the display

Acceptance: "A client is visible on the test machine's display, checked with `grim` through screencopy."

1. Start Shingle on tty1 with `foot`.
2. Over SSH:

   ```sh
   grep -v 'page flip' ~/.local/state/shingle/native.log
   ls -l /proc/$(pgrep -n -u "$USER" -x shingle)/fd | grep dri
   grim /tmp/shot.png && file /tmp/shot.png
   wayland-info | grep -A4 "interface: 'wl_output'"
   ```

   Expected: the log has `using /dev/dri/cardN` (the boot VGA card), `output NAME: WxH at R Hz` with the monitor's connector name (such as `HDMI-A-1` or `DP-1`) and preferred mode, `rendering on ADAPTER`, where ADAPTER is the GPU on that card's PCI slot, and, with `SHINGLE_SCANOUT=copy`, `output NAME: copy scanout, because SHINGLE_SCANOUT=copy forces copy scanout`. (Before NB-8 the output's line ended in `, copy scanout` and the adapter's in `; zero-copy scanout is not built yet, so frames are copied`.) The descriptors include that `/dev/dri/cardN`. `grim` writes a PNG of the mode's size showing the foot window, tiled over the whole output. `wayland-info` reports the output with the connector name, the mode's size and refresh rate, and the monitor's physical size.
3. On the physical screen: the same foot window, sharp, at the monitor's native resolution, and its cursor follows typing on the test machine's keyboard. The mouse pointer is drawn and moves.
4. Run this step with one monitor connected. (In NB-7 a second monitor stayed dark; since MO-7 every connected monitor that a CRTC is free for is lit, which steps 25 to 35 check.)
5. End Shingle with `kill -TERM`; expected `exit=0` and the text console back on the screen.

### Step 11: presentation times are the page-flip times, and frames are paced by flips

Acceptance: "`wp_presentation` timestamps match page-flip times within 1 ms" and "Frames are paced by flips: no frame callback before the previous flip completes."

1. Start Shingle on tty1 with `sh -c 'exec present_probe 600 > /tmp/probe.out'`. The probe draws a new grey shade on every frame callback, 600 frames, then exits.
2. After 20 s, over SSH:

   ```sh
   python3 ~/shingle/tools/native_test/present_check.py \
     ~/.local/state/shingle/native.log /tmp/probe.out; echo "status=$?"
   ```

   Expected: `status=0` and a summary with `"presented": 600`, `"discarded": 0`, `"max_difference_ns"` below 1000000 (it should be 0, since the feedback carries the flip's own timestamp) and `"median_frame_interval_ms"` equal to one refresh period rounded (16 or 17 at 60 Hz, 7 at 144 Hz). A median of two periods means the copy path did not finish each frame within one refresh; record the mode and GPU. Any `callback at ... is no flip time` or `callback before its commit` line fails the pacing item.
3. In the same probe output, every `presented` line has `"flags": 7` (vsync, hardware clock, hardware completion) and `"refresh"` equal to `1000000000000 / refresh_mHz` for the mode in the log.

### Step 12: DPMS off and on through wlr-output-power-management

Acceptance: "DPMS off and on works through `wlr-output-power-management`."

1. Start Shingle on tty1 with `sh -c 'exec present_probe 100000 > /tmp/probe.out'`.
2. Over SSH:

   ```sh
   wlopm; wlopm --off '*'; sleep 3; wlopm
   tail -n 3 ~/.local/state/shingle/native.log | grep -v 'page flip'
   wc -l < /tmp/probe.out; sleep 2; wc -l < /tmp/probe.out
   wlopm --on '*'; sleep 2; wlopm
   tail -n 3 ~/.local/state/shingle/native.log | grep -v 'page flip'
   ```

   Expected: `wlopm` first lists the output as `on`, then `off`. The monitor goes dark and reports no signal or standby within a few seconds, and the log says `output CONN off` (`display off` before MO-7), with `CONN` the connector. While it is off the probe's output does not grow (no frame callbacks without flips). After `--on` the picture returns within about a second, without a restart, the log says `output CONN on`, `wlopm` lists `on` and the probe's output grows again.
3. Moving the mouse or pressing a key while the display is off also turns it on, as Wayfire's idle handling does.

### Step 13: a VT switch away and back resumes with a fresh modeset

Acceptance: "A VT switch away and back resumes with a fresh modeset."

1. Start Shingle on tty1 with `sh -c 'exec present_probe 100000 > /tmp/probe.out'`, so a frame is in flight at almost any moment (the "VT switch while rendering" row).
2. Over SSH:

   ```sh
   sudo chvt 2; sleep 2
   grep -v 'page flip' ~/.local/state/shingle/native.log | tail -n 2
   wc -l < /tmp/probe.out; sleep 2; wc -l < /tmp/probe.out
   sudo chvt 1; sleep 2
   grep -v 'page flip' ~/.local/state/shingle/native.log | tail -n 3
   grim /tmp/after.png
   ```

   Expected: tty2's text console appears on the monitor at its console resolution; the log says `seat disabled; paused`; the probe's output stops growing. After `chvt 1` Shingle's picture is back at the monitor's preferred mode within about a second, the log says `seat enabled; resumed` with no modeset error, the probe's output grows again and `grim` shows the probe's window.
3. Repeat with Ctrl+Alt+F2 and Ctrl+Alt+F1 on the physical keyboard, five times in quick succession. Shingle keeps running and shows its picture after the last switch back.
4. On tty2, change the console's mode before switching back if the machine allows it (for example `sudo fbset -g 1024 768 1024 768 32`, where fbdev permits it), then `sudo chvt 1`: Shingle's mode is restored, which shows the resume modesets rather than reusing the old state.

### Step 14: SIGTERM, SIGINT and `kill -9` leave a usable tty1

Acceptance: "`kill -9` leaves a usable tty1."

1. Start Shingle on tty1 with `foot`. Over SSH, check `pid=$(pgrep -n -u "$USER" -x shingle)` is a single number above 1, then `kill -KILL "$pid"`.
2. Expected: within a second the monitor shows tty1's text console again (the kernel's fbdev console takes over when the last DRM master closes the card), the shell prompt accepts `echo ok`, and Ctrl+Alt+F2 and Ctrl+Alt+F1 switch VT. `/tmp/shingle-exit` says `exit=137`.
3. Repeat with `kill -TERM` and `kill -INT`: `exit=0`, the log ends with `native backend stopped` and the console is back as in 2.

### Step 15: failure-table rows that the test machine can produce

Acceptance: "Every row of the failure table that can be produced on the test machine is exercised and recorded." Each start below is from a tty1 login, with the command of step 9 unless it says otherwise; check over SSH that no `$XDG_RUNTIME_DIR/wayland-*` socket was created by a failed start.

1. **No DRM device, or `SHINGLE_DRM_DEVICE` invalid.** Run `SHINGLE_DRM_DEVICE=/dev/dri/card9 shingle --config ~/.config/shingle/test.yaml; echo "status=$?"`. Expected: `shingle: no DRM device: tried /dev/dri/card9 (...)` with libseat's error, `status=1`, no socket and tty1 still a text console. Then `SHINGLE_DRM_DEVICE=/dev/dri/cardN` with the real card starts normally.
2. **No Vulkan adapter.** Run `VK_ICD_FILENAMES=/nonexistent.json shingle --config ~/.config/shingle/test.yaml; echo "status=$?"`. Expected: `shingle: no Vulkan adapter: ...`, `status=1`, no socket, text console back.
3. **No connected output at startup.** Unplug the monitor, log in on tty1 blind and start Shingle with `foot`. Over SSH: the log says `no connected output; nothing is shown until one is`, `ipc window-rules/list-views` lists the foot window and SIGTERM is not needed yet. Plug the monitor in, then `sudo chvt 2; sleep 1; sudo chvt 1`: the log names the output and the picture appears. Since NB-9 the picture appears as soon as the monitor is plugged in; that is step 17. Record whether the monitor shows the console or stays dark while unplugged-then-plugged before the switch.
4. **Output unplugged.** With Shingle running and lit, `sudo chvt 2`, unplug the monitor, `sudo chvt 1`: the log says `no connected output`, Shingle and foot keep running (IPC answers). Plug it in, `sudo chvt 2; sudo chvt 1`: the picture is back with the window in place.
5. **GPU device lost or hung.** Only where the driver offers a reset trigger: on i915, `echo 1 | sudo tee /sys/kernel/debug/dri/N/i915_wedged`; on amdgpu, `sudo cat /sys/kernel/debug/dri/N/amdgpu_gpu_recover`, with N the card's minor. Expected: the log says `the GPU device was lost (...)` or `the GPU did not finish a frame within 100 ms` three times, then `recreating the GPU context`, and the picture continues with every window's content. Triggering it three times within 60 s ends Shingle with `the GPU failed 3 times within 60 s; saving the session and exiting`, `exit=2` and a fresh `session.json`. If the driver has no such trigger, record that the row was not producible.
6. **Panic and SIGKILL.** SIGKILL is step 14. A panic cannot be produced from outside; it is covered by the panic hook's log and the same kernel and seat cleanup as SIGKILL.

The other rows cannot be produced on a healthy machine and are unit-tested only: no atomic support (every current KMS driver has it), another DRM master (libseat grants master only to the active session), every mode rejected by `TEST_ONLY` (no real monitor rejects all its modes; the fallback order is unit-tested), and a page flip that never completes (would need a broken driver; the timeout, the recovery modeset and the exit after three are unit-tested).

## NB-9: hotplug and output configuration

From NB-9 on, a udev monitor for the card reports DRM hotplug, so plugging and unplugging monitors takes effect without a VT switch. In NB-9 Shingle lit one output: the one it lit while that stays connected, else a connector named in the configuration, else an internal panel, else the first connected one, never one whose mode is `off`. Since MO-7 it lights every connected monitor in that order as far as the card's CRTCs allow; where a step below has A and B connected at once, both are lit unless the step says otherwise, and the multi-monitor behaviour itself is steps 25 to 35. The mode comes from the output's `mode` option, chosen as Wayfire chooses it. Unit tests through the fake card and a fake monitor cover the logic ([design, NB-9 as built](design/native-backend.md#nb-9-as-built)); these steps check that real connectors, EDIDs, udev events and drivers behave the way the fakes assume.

### Step 16: prepare the test machine for NB-9

1. In addition to steps 0 and 9, install `wlr-randr`, `edid-decode` and `udev` (for `udevadm`), and have two monitors at hand, A and B, ideally with different preferred modes (for example a 1920x1080 and a 2560x1440 monitor), both connectable to the boot VGA card. Note their connector names from `ls /sys/class/drm/` once both are plugged in, as `CONN_A` and `CONN_B` below (such as `HDMI-A-1` and `DP-1`).
2. Write the NB-9 test configs:

   ```sh
   cd ~/.config/shingle
   printf 'session: {restore: false}\nxwayland: {mode: false}\n' > nb9.yaml
   cp nb9.yaml nb9-explicit.yaml
   printf 'outputs:\n  - {name: CONN_A, mode: "1280x720@60"}\n' \
     >> nb9-explicit.yaml
   ```

   replacing `CONN_A` with monitor A's connector name and `1280x720@60` with a mode monitor A lists but does not prefer (`edid-decode /sys/class/drm/card*-CONN_A/edid` lists them).
3. In the NB-9 steps, "start Shingle on tty1 with CONFIG" means step 9's command with `--config ~/.config/shingle/CONFIG` and `--spawn foot`. Over SSH, also watch the udev events next to Shingle's log:

   ```sh
   udevadm monitor --udev --subsystem-match=drm &
   tail -f ~/.local/state/shingle/native.log | grep -v 'page flip' &
   ```

### Step 17: a monitor plugged in later is lit at once, with its EDID

Closes NB-7's open point that a monitor plugged in after the start was only seen after a VT switch (step 15.3).

1. Unplug both monitors, log in on tty1 blind and start Shingle with `nb9.yaml`. Over SSH: the log has `no connected output; nothing is shown until one is` and no line about a hotplug monitor (a line `no DRM hotplug monitor (...)` fails this step), and `ipc window-rules/list-views` lists the foot window.
2. Plug in monitor A. Expected within a second, without any VT switch: udevadm shows a `change` event for `/devices/.../drm/cardN` (with `HOTPLUG=1` in `udevadm info`), the log says `CONN_A connected` and `output CONN_A: WxH at R Hz, copy scanout` with A's preferred mode, and the screen shows foot tiled over the whole output.
3. Over SSH:

   ```sh
   wlr-randr
   edid-decode /sys/class/drm/card*-CONN_A/edid | \
     grep -E 'Manufacturer|Display Product (Name|Serial)|Serial Number'
   ipc window-rules/list-outputs
   ```

   Expected: `wlr-randr` lists one head `CONN_A`, whose `Make` is the manufacturer's name from hwdata's `pnp.ids` (for example `Dell Inc.` for `DEL`) or the three-letter id if hwdata is not installed, `Model` the display product name (or `0xPPPP`), `Serial` the product serial (or `0xSSSSSSSS`), and the description `MAKE MODEL SERIAL (CONN_A)`, matching edid-decode. Its modes are exactly the ones edid-decode and `/sys/class/drm/card*-CONN_A/modes` list, with the preferred mode marked `preferred, current`. `list-outputs` names `CONN_A`, and the foot window's `output-name` in `list-views` is `CONN_A`: the core output was renamed.

### Step 18: unplugging and replugging re-probes and picks a new mode

Acceptance: "On the test machine, unplugging and replugging a monitor triggers a re-probe and a new mode."

1. With Shingle running from step 17 on monitor A, note `ipc window-rules/list-views`. Unplug A. Expected: a udev `change` event, `CONN_A disconnected` and `no connected output; nothing is shown until one is` in the log, Shingle and foot keep running (IPC answers, `kill -0` on foot's PID succeeds), and `list-views` still has foot with the same geometry.
2. Plug monitor B into the same connector (or, with one monitor, plug A back in). Expected: `CONN_A connected` (or `offers other modes` if the connector stayed connected across the swap), `output CONN_A: ...` with B's preferred mode, the picture back on B at that mode, and `wlr-randr` showing B's EDID and modes. foot is resized to the new output, and `list-views` shows it filling the new size.
3. Repeat the unplug and replug ten times quickly (within a second each). Shingle keeps running, the last state is lit, and the log has no `modeset failed`.

### Step 19: with every monitor unplugged, clients live and the picture returns with the tiling tree intact

Acceptance: "Disconnecting all outputs keeps Shingle and its clients alive, and reconnecting restores the picture with the tiling tree intact."

1. Start Shingle on tty1 with `nb9.yaml` and monitor A, open two more windows (`ipc stipc/run '{"cmd": "foot"}'` twice) and move one to another workspace with its keybinding, so the tree has a split and two workspaces. Save `ipc simple-tile/get-layout '{"wset-index": 1, "workspace": {"x": 0, "y": 0}}'` and `ipc window-rules/list-views` to files.
2. Unplug A. For a minute: foot windows keep running (type into one over SSH with `ipc stipc/feed_key` and check it echoes nothing to the screen but keeps its process), `list-views` and `get-layout` are unchanged, and the log shows no frame drawn (`grep -c 'page flip'` stops growing).
3. Plug A in again. Expected: the picture returns within a second at A's mode, `get-layout` and `list-views` are byte for byte the files of 1 (the same mode gives the same geometry), and typing reaches the focused foot.
4. Start again with only A connected and windows as in 1, then plug in B (both are lit now) and unplug A: A's windows join B's workspace set under the default `output_layout.unplug: return` (`list-views` shows them on `CONN_B`, each keeping its workspace, tiled ones appended to B's tree), and `wlr-randr` and `wayland-info` list only `CONN_B`. On a card with one usable CRTC, B is lit only once A is unplugged (`output CONN_B: ...` then).

### Step 20: the configured mode follows Wayfire

Acceptance: "Mode-selection tests cover Wayfire's heuristics" is unit-tested; this step checks the configuration end to end.

1. Start Shingle on tty1 with `nb9-explicit.yaml` and monitor A. Expected: `output CONN_A: 1280x720 at 60.000 Hz` (the configured mode, not the preferred one) and `wlr-randr` marks it `current`.
2. With Shingle running, edit `nb9-explicit.yaml` to each of the values below in turn (the config reloads by itself) and check the log and `wlr-randr` after each: `highres` lights the largest mode, `highrr` the largest among the fastest, a size A does not offer (`mode: "1234x567"`) lights the preferred mode as Wayfire falls back to `auto`, a size with an unlisted refresh lights the listed refresh closest to it, and `off` turns A off (`no connected output` if B is not connected), leaving B lit if it is connected; `wlr-randr` then lists A as a disabled head (`Enabled: no`).
3. Repeat 1 with the same option in a Wayfire file, `printf '[core]\nxwayland = false\n[output:CONN_A]\nmode = 1280x720@60\n[output:CONN_B]\nmode = off\n' > ~/.config/shingle/nb9.ini`, started with `--config ~/.config/shingle/nb9.ini`: the same mode is lit and B stays dark, which shows each `[output:NAME]` section kept apart.

### Step 21: wlr-randr changes the mode, with `TEST_ONLY` first

Acceptance: "`wlr-randr` (or an equivalent client) can change the mode."

1. Start Shingle on tty1 with `nb9.yaml` and monitor A, with `present_probe 100000` as the client so that a flip is pending at almost any moment.
2. Over SSH, pick a non-current mode from `wlr-randr` and run `wlr-randr --output CONN_A --mode WxH@R --dryrun; echo "status=$?"`. Expected: `status=0`, nothing changes on the screen and the log has no `output CONN_A:` line.
3. `wlr-randr --output CONN_A --mode WxH@R; echo "status=$?"`. Expected: `status=0`, the monitor switches to that mode within a second, the log says `output CONN_A: WxH at R Hz, copy scanout`, `wlr-randr` marks it `current`, the probe's window fills the new size and present_check.py on the probe's output (step 11) still finds every presentation time equal to a flip time.
4. `wlr-randr --output CONN_A --custom-mode 1234x567; echo "status=$?"`: a mode the monitor does not list fails (`status` non-zero, `failed to apply configuration`), the log says `output mode Custom { ... }: the output does not offer that mode`, and the mode is unchanged. `wlr-randr --output CONN_A --off` also fails, since Shingle does not turn its only output off (use wlopm, step 12).
5. Switch VT away and back (`sudo chvt 2; sudo chvt 1`): the mode set in 3 is kept.

### Step 22: hotplug while away and during a VT switch

1. With Shingle lit on A, `sudo chvt 2`, unplug A, plug in B, `sudo chvt 1`. Expected: hotplug events while away are dropped (no `connected` line until the resume), the resume lights B, and `wlr-randr` lists `CONN_B`.
2. While on tty1, plug and unplug B during a busy `present_probe` run: no `modeset failed` or `page flip failed` lines, since a hotplug waits for the pending flip.

## DMA-BUF clients on the native output (DB-4)

These steps check `zwp_linux_dmabuf_v1` ([design/dmabuf.md](design/dmabuf.md#db-4-as-built)) where the GPU namespace on the development host cannot: with the card node present, so that NVIDIA's EGL clients can run too. Install `vulkan-tools`, `mesa-utils-bin`, `wayland-utils` and `kitty`.

### Step 23: GPU clients present DMA-BUFs, across a VT switch and a GPU loss

1. Start Shingle on tty1 as in step 9 with `SHINGLE_TRACE_BUFFERS=1` in its environment. The log has `linux-dmabuf version 5 on render node M:m, N format and modifier pairs`, and `wayland-info` over SSH (with Shingle's `WAYLAND_DISPLAY` and `XDG_RUNTIME_DIR`) lists `zwp_linux_dmabuf_v1` version 5 with that main device and N pairs.
2. Start `vkcube --wsi wayland`, then `es2gears_wayland` and `kitty`, each with `ipc stipc/run`. Expected: each draws on the monitor, and for each the log has one `dmabuf import` per buffer followed by `dmabuf cached` lines and no `shm upload` for its surface. Record which EGL vendor `es2gears_wayland -info` reports.
3. While `vkcube` runs, switch VT away and back (`sudo chvt 2; sleep 5; sudo chvt 1`). Expected: the cube keeps turning after the resume without new `dmabuf import` lines, and no client exits.
4. With the clients running, trigger a GPU loss as in step 15's row 5 where the driver allows it. Expected: `recreating the GPU context`, then `dmabuf import` lines again for the buffers on screen (imported again from their kept fds), and the clients keep drawing. Record if the driver has no trigger.
5. Repeat 2 with `SHINGLE_DMABUF=0`: the log says `SHINGLE_DMABUF=0 forces wl_shm`, `wayland-info` has no `zwp_linux_dmabuf_v1` and the clients fall back to `wl_shm` (`shm upload` lines) or fail as DB-2's baseline recorded.

## Explicit sync on the native output (DB-5)

These steps check `wp_linux_drm_syncobj_manager_v1` ([design/dmabuf.md](design/dmabuf.md#db-5-as-built)) where the GPU namespace cannot: with NVIDIA's EGL clients, which need the card node, and on the native backend. Install the packages of step 23 and `weston` and `firefox-esr`.

### Step 24: GPU clients commit with syncobj points

1. Start Shingle on tty1 as in step 23, with `SHINGLE_TRACE_BUFFERS=1`. The log has `linux-drm-syncobj version 1 on render node M:m` after the linux-dmabuf line, and `wayland-info` lists `wp_linux_drm_syncobj_manager_v1` version 1.
2. Start `vkcube --wsi wayland`, `es2gears_wayland -info`, `eglgears_wayland`, `kitty` and `firefox-esr` with a throwaway `--profile`, one at a time, each with `ipc stipc/run`. Expected for each: it draws, and its trace has `syncobj acquire ready` or `blocked syncobj N µs` lines and `syncobj release` lines, no `blocked implicit`. Record how many commits waited (`blocked syncobj`) and their times, and which EGL vendor `es2gears_wayland -info` reports (NVIDIA expected).
3. While `vkcube` runs, switch VT away and back (`sudo chvt 2; sleep 5; sudo chvt 1`). Expected: the cube keeps turning, and no client exits or stops with a commit stuck in `blocked syncobj`.
4. Repeat 2 with `SHINGLE_SYNCOBJ=0`: the log says `SHINGLE_SYNCOBJ=0 forces implicit sync`, `wayland-info` has no syncobj global, the clients draw, and the trace has no `syncobj` lines. Record whether the NVIDIA EGL clients' frame rate differs from 2, since without the protocol they wait for their own GPU before every commit.
5. With `weston-simple-dmabuf-feedback` (which needs the card node the namespace hides), record the tranche it prints.

## MO-7: several monitors

From MO-7 on, native lights every connected monitor that is not configured `off`, each on a CRTC of its own, in priority order (lit ones, then those named in `outputs`, internal panels, then card order) as far as the card's CRTCs and primary planes allow, with one `TEST_ONLY` modeset of all of them before one real modeset; each monitor flips on its own CRTC at its own refresh rate. Connected monitors that are not lit are listed as disabled heads in wlr-output-management. Unit tests through the fake card cover the matching, the combined modesets and their fallback, flips routed by CRTC, hotplug of a second and a third monitor, configurations across heads, DPMS per output and the absolute input mapping ([design, MO-7 as built](design/multi-output.md#mo-7-as-built)); these steps check them with real monitors, connectors and drivers.

### Step 25: prepare the test machine for MO-7

1. In addition to steps 0, 9 and 16, install `drm-info` and `wlopm`, and have monitors A and B at hand, ideally with different preferred refresh rates (for example 60 and 144 Hz), and a third, C, if the GPU has three outputs. Connect them to the boot VGA card and note their connector names as `CONN_A`, `CONN_B` and `CONN_C`. `drm_info | grep -E '^(├|└)───(Connector|CRTC)|Possible CRTCs'` lists the CRTCs and which CRTCs each connector's encoders can use; record the number of CRTCs.
2. Write the MO-7 test configs, one per unplug policy:

   ```sh
   cd ~/.config/shingle
   for policy in return move park; do
     printf 'session: {restore: false}\nxwayland: {mode: false}\n' \
       > "mo7-$policy.yaml"
     printf 'output_layout: {unplug: %s}\n' "$policy" >> "mo7-$policy.yaml"
   done
   ```

3. In the MO-7 steps, "start Shingle on tty1 with CONFIG" means step 9's command with `SHINGLE_TRACE_FLIPS=1`, `--config ~/.config/shingle/CONFIG` and `--spawn foot`. Over SSH, also use step 16's `udevadm monitor` and the log `tail`, and

   ```sh
   view() { ipc window-rules/list-views | python3 -c 'import json, sys
   for v in json.load(sys.stdin):
     print(v["id"], v["app-id"], v["output-name"], v["geometry"])'; }
   outputs() { ipc window-rules/list-outputs | python3 -c 'import json, sys
   for o in json.load(sys.stdin):
     print(o["id"], o["name"], o["geometry"])'; }
   ```

### Step 26: two and three monitors are lit at once with their preferred modes

1. With A and B connected, start Shingle on tty1 with `mo7-return.yaml`. Expected: the log has `output CONN_A: ...` and `output CONN_B: ...`, each with that monitor's preferred mode, and no `stays dark` line; both screens show Shingle's background, foot on one of them, and the pointer crosses from one screen to the other when moved past their shared edge.
2. Over SSH, `wlr-randr` lists both heads as `Enabled: yes` with their preferred modes `current`; `outputs` lists both, the first at 0,0 and the second at the first's width, 0 (automatic placement); `wayland-info | grep -A3 wl_output` shows two outputs with those names.
3. `grep -c 'page flip' ~/.local/state/shingle/native.log` grows while foot's cursor blinks, and `grep 'CONN_B: page flip' ~/.local/state/shingle/native.log | tail -3` shows that B's flips are logged under its own name, with B's own vblank sequence.
4. With C connected too (and a CRTC for it), repeat: three `output` lines, three heads, three screens with a picture. Record the GPU, the driver and the number of CRTCs.

### Step 27: more monitors than CRTCs, and `--outputs`

1. If the card has fewer usable CRTCs than the monitors at hand (step 25.1), connect them all and start Shingle with `mo7-return.yaml`. Expected: the lowest-priority monitors (last in card order, unless named in `outputs`) are dark, the log has `every CRTC that can drive CONN_C is in use; CONN_C stays dark`, and `wlr-randr` lists that head as `Enabled: no`. Unplug a lit monitor: the dark one is lit within a second (`output CONN_C: ...`). If the card has a CRTC for every connector, record that the case is not producible on it.
2. On any card: with A and B connected, start Shingle with `mo7-return.yaml` and `--outputs 1` added. Expected: only the first is lit, the log has `CONN_B stays dark: --outputs allows 1 lit outputs`, `wlr-randr` lists `CONN_B` as `Enabled: no`, and `wlr-randr --output CONN_B --on` lights it (status 0, B shows the background, `outputs` lists two).

### Step 28: plugging a second monitor in and out while clients run, under each unplug policy

1. Start Shingle on tty1 with `mo7-return.yaml`, A and B connected. Over SSH, start a second foot with `ipc stipc/run '{"cmd": "foot"}'`, find its view id and B's output id with `view` and `outputs`, and move it to B with `ipc window-rules/configure-view '{"id": VIEW, "output_id": OUT_B}'`. Expected: `view` shows it on `CONN_B`, and it is on B's screen.
2. Unplug B. Expected within a second: `CONN_B disconnected` in the log, B's foot on A (`view` shows it on `CONN_A`, on A's current workspace), `wlr-randr` lists only A, and both foot processes keep running.
3. Plug B in again. Expected: `output CONN_B: ...`, and the foot is back on B (`view` shows `CONN_B`) in its old place in B's tree.
4. Repeat 1 to 3 with `mo7-move.yaml`: in 2 the foot moves to A as before, but in 3 it stays on A and B shows an empty workspace set.
5. Repeat 1 to 3 with `mo7-park.yaml`: in 2 the foot is hidden (`view` shows it with `output-name` `null` or an `output-id` of -1, nothing on A's screen) but its process runs; in 3 it is back on B.
6. With `mo7-return.yaml`, plug and unplug B ten times quickly while `present_probe 100000` runs on A: Shingle keeps running, A's probe output keeps growing, and the log has no `modeset failed` or `page flip of CONN_A failed`.

### Step 29: a monitor moved to another connector gets its windows back by its EDID identity

Needs two free connectors for B (for example DisplayPort and HDMI) and a monitor B whose EDID has a serial number (`edid-decode /sys/class/drm/card*-CONN_B/edid | grep -i serial`).

1. Start Shingle with `mo7-return.yaml`, A on `CONN_A` and B on `CONN_B`, with a foot moved to B as in step 28.1.
2. Unplug B from `CONN_B`: the foot joins A.
3. Plug B into the other connector, `CONN_C`. Expected: `output CONN_C: ...`, and the foot is back on B, now named `CONN_C` (`view`), because the parked workspace set is found by make, model and serial. `wlr-randr` lists `CONN_C` with B's make, model and serial.

### Step 30: a dock disconnected and reconnected

Needs a dock or MST hub with two monitors (B and C) behind one cable; otherwise record that the step could not be run.

1. Start Shingle with `mo7-return.yaml`, the internal panel or A on the GPU directly and B and C on the dock, with a foot on each of B and C (step 28.1).
2. Pull the dock's cable. Expected: both `disconnected` lines, both foot windows on the remaining monitor, no `modeset failed`.
3. Plug the dock in again. Expected: B and C lit again (the dock may renumber its connectors; the windows follow their monitors' EDID identities as in step 29), each foot back on its monitor.

### Step 31: mode, position and disable changes through wlr-randr with two heads

1. Start Shingle with `mo7-return.yaml`, A and B connected, and `present_probe 100000` as the client, so that flips are pending at almost any moment.
2. `wlr-randr --output CONN_B --mode WxH@R --dryrun; echo "status=$?"` with a mode B lists but is not current: `status=0`, nothing changes and the log has no new `output CONN_B:` line.
3. `wlr-randr --output CONN_B --mode WxH@R; echo "status=$?"`: `status=0`, B switches mode within a second, the log says `output CONN_B: WxH at R Hz` (followed by its scanout line since NB-8), and A's picture does not flicker or drop out.
4. Both at once: `wlr-randr --output CONN_A --mode W1xH1 --output CONN_B --mode W2xH2; echo "status=$?"`: `status=0` and both `output` lines appear together (one combined modeset).
5. `wlr-randr --output CONN_B --pos 0,-1080` (B above A, using B's height): `outputs` shows B at 0,-1080, and the pointer crosses at the top edge of A.
6. `wlr-randr --output CONN_B --off`: B goes dark, `wlr-randr` lists it `Enabled: no`, windows on B join A, and A keeps its probe running. `wlr-randr --output CONN_B --on`: B is lit again and its windows come back. Unplugging and replugging B now leaves it as it is (`--off` and `--on` hold until Shingle restarts, as in Wayfire).
7. `wlr-randr --output CONN_A --off --output CONN_B --off; echo "status=$?"` fails (`status` non-zero): the last output is never turned off.

### Step 32: flips and presentation per CRTC at different refresh rates

1. With A and B at different refresh rates (step 25.1), start Shingle on tty1 with `mo7-return.yaml` and `sh -c 'exec present_probe 600 > /tmp/probe-a.out'` as the client. Over SSH, start a second probe with `ipc stipc/run '{"cmd": "exec present_probe 600 > /tmp/probe-b.out"}'` and move it to B at once with `configure-view` as in step 28.1.
2. After 20 s:

   ```sh
   cd ~/shingle/tools/native_test
   python3 present_check.py --output CONN_A \
     ~/.local/state/shingle/native.log /tmp/probe-a.out; echo "a=$?"
   python3 present_check.py --output CONN_B \
     ~/.local/state/shingle/native.log /tmp/probe-b.out; echo "b=$?"
   ```

   Expected: `a=0` and `b=0`. Each summary's `"median_frame_interval_ms"` is one refresh period of that probe's own monitor (for example 16 or 17 at 60 Hz and 7 at 144 Hz), and its `presented` lines carry that monitor's `"refresh"`. B's probe may report failures for the first frames, drawn on A before the move; record their number.

### Step 33: DPMS per output

1. Start Shingle with `mo7-return.yaml`, A and B connected, and `present_probe 100000` on A.
2. `wlopm; wlopm --off CONN_B; sleep 3; wlopm`: B goes dark, `wlopm` lists `CONN_A on` and `CONN_B off`, the log says `output CONN_B off`, and `wc -l < /tmp/probe.out` keeps growing (A keeps flipping).
3. `wlopm --on CONN_B`: B's picture returns within a second and the log says `output CONN_B on`.
4. `wlopm --off '*'`: both go dark and the probe output stops growing; moving the mouse turns both on again.

### Step 34: a VT switch away and back with two outputs lit

1. Start Shingle with `mo7-return.yaml`, A and B connected, a probe on each (step 32.1, with `100000` frames).
2. `sudo chvt 2; sleep 2; sudo chvt 1`. Expected: `seat disabled; paused`, then `seat enabled; resumed`; both monitors show Shingle again and both probe outputs grow again; the log has no `modeset after resume failed`.
3. `sudo chvt 2`, unplug B, `sudo chvt 1`: only A is lit, B's probe joins A. `sudo chvt 2`, plug B in, `sudo chvt 1`: B is lit again and its probe goes back to it.
4. Repeat 2 with Ctrl+Alt+F2 and Ctrl+Alt+F1 on the physical keyboard, five times quickly: Shingle keeps running, and both monitors show it after the last switch back.

### Step 35: absolute input mapped to an output

1. Add a mapping for the uinput touchscreen to `mo7-return.yaml`:

   ```sh
   printf 'input_devices:\n  "shingle-test touch": {output: CONN_B}\n' \
     >> ~/.config/shingle/mo7-return.yaml
   ```

   and start Shingle with it, A and B connected and `sh -c 'exec wev > /tmp/wev.log 2>&1'` as the client; move wev's window to B (step 28.1).
2. `uinput touch 0.5 0.5`. Expected: wev's log has a `wl_touch` `down` in the middle of its window on B (half of B's width and height in window coordinates), not on A.
3. Without the mapping (remove the two lines and restart), `uinput touch 0.5 0.5` touches the middle of the whole layout's bounding box: with A and B side by side and equally wide and high, the point on the edge between them.

## NB-8: zero-copy scanout

From NB-8 on, each lit monitor starts on copy scanout as before and, before its next frame, moves to zero-copy scanout: three GBM buffers with a modifier that the primary plane's `IN_FORMATS`, the Vulkan renderer and GBM (with one plane) all accept, imported into the renderer and drawn into directly, shown after a `TEST_ONLY` and a blocking flip. Whatever fails keeps copy scanout and logs why; `SHINGLE_SCANOUT=copy` forces copy scanout. Unit tests through the fake card, its fake GBM and a fake renderer cover the negotiation, the switch, every fallback and the override ([design, NB-8 as built](design/native-backend.md#nb-8-as-built)); these steps check that real drivers, GBM and Vulkan behave the way the fakes assume, and the acceptance item "on the test machine, zero-copy is selected on at least one of i915, amdgpu or nvidia-drm, and `wp_presentation` reports `ZERO_COPY`".

### Step 36: prepare the test machine for NB-8

1. In addition to steps 0 and 9, install `drm-info` and `vulkan-tools`. Connect one monitor to the boot VGA card. On NVIDIA, check that nvidia-drm runs with kernel modesetting, without which there is no atomic KMS at all: `cat /sys/module/nvidia_drm/parameters/modeset` must print `Y`.
2. Record what the card and the Vulkan driver offer, from a text console or over SSH while no Shingle runs (`drm_info` opens the card node, which is fine on the test machine):

   ```sh
   drm_info > /tmp/drm_info.txt
   cargo run --manifest-path ~/shingle/Cargo.toml -p shingle-render \
     --example dmabuf_caps > /tmp/dmabuf_caps.txt
   ```

   In /tmp/drm_info.txt, find the planes whose `type` is `Primary` and, under their `IN_FORMATS`, the modifiers that list `XR24` (XRGB8888). Note them and the `render` modifiers `dmabuf_caps` lists for `XRGB8888` on the card's adapter. The modifiers in both lists are the candidates; if none is in both, the expected result of step 37 is copy scanout with the reason `no XRGB8888 modifier is both scanned out by the primary plane (...) and rendered to by the renderer (...)`, which is recorded as the driver's result.
3. "Start Shingle on tty1 with CLIENT" means step 9's command, with `SHINGLE_TRACE_FLIPS=1`.

### Step 37: zero-copy is selected, and the picture is right

1. Start Shingle on tty1 with `foot`.
2. Over SSH:

   ```sh
   grep -E 'rendering on|output .*: |scanout' ~/.local/state/shingle/native.log
   grim /tmp/zero-copy.png && file /tmp/zero-copy.png
   ```

   Expected: `rendering on ADAPTER`, `output CONN: WxH at R Hz`, then `output CONN: zero-copy scanout, modifier M`, with M one of the candidates of step 36.2 (for example `INTEL 0x0100000000000002` for Y tiling, an `AMD 0x02...` modifier, or `NVIDIA 0x030000000060601x` block linear), and no `copy scanout, because` line. `grim` shows the foot window as in step 10.
3. On the physical screen: the same picture as in step 10, with the right colours (foot's background and text as configured, not with red and blue swapped), no tiling garbage, stripes or shear, and no tearing while typing. The first frame after the switch is black, then foot appears. Record any difference from step 10's copy scanout.
4. With `ls -l /proc/$(pgrep -n -u "$USER" -x shingle)/fd | grep -c dmabuf`, count the DMA-BUF descriptors: 3 per zero-copy output, besides any of client buffers.
5. Record the driver (`i915`, `xe`, `amdgpu` or `nvidia-drm`, from `readlink /sys/class/drm/cardN/device/driver`), the kernel, Mesa or the NVIDIA driver version, the modifier chosen and the mode. Repeat 1 to 4 on every GPU family the test machine has; the acceptance item needs one of i915, amdgpu or nvidia-drm to reach zero-copy.

### Step 38: `wp_presentation` reports `ZERO_COPY`, and frames keep up

1. Start Shingle on tty1 with `sh -c 'exec present_probe 600 > /tmp/probe.out'`.
2. After 20 s, over SSH:

   ```sh
   python3 ~/shingle/tools/native_test/present_check.py --scanout zero-copy \
     ~/.local/state/shingle/native.log /tmp/probe.out; echo "status=$?"
   grep -c '"flags":15' /tmp/probe.out
   ```

   Expected: `status=0`; the summary has `"presented": 600`, `"zero_copy": 600`, `"max_difference_ns"` below 1000000 and `"median_frame_interval_ms"` of one refresh period; every `presented` line has `"flags": 15` (vsync, hardware clock, hardware completion and zero copy), so the count is 600.
3. Compare the copy path: repeat 1 and 2 with `SHINGLE_SCANOUT=copy` in Shingle's environment and `--scanout copy` for the check. Expected: `status=0`, `"zero_copy": 0` and `"flags": 7`. In both runs, record Shingle's CPU time for the 600 frames, `ps -o times= -p $(pgrep -n -u "$USER" -x shingle)` read right before and after the probe (the difference), and the median frame interval. Zero-copy should need clearly less CPU time, since it drops the readback and the copy of every frame (about 8 MB at 1080p); record the numbers without claims beyond them.

### Step 39: `SHINGLE_SCANOUT=copy` and the other fallbacks name their reason

1. Start Shingle on tty1 with `foot` and `SHINGLE_SCANOUT=copy`. Expected: `output CONN: copy scanout, because SHINGLE_SCANOUT=copy forces copy scanout`, no `zero-copy scanout` line, and the picture of step 10.
2. `SHINGLE_SCANOUT=bogus`: the log has `SHINGLE_SCANOUT=bogus is neither copy nor auto; trying zero-copy`, then the zero-copy line of step 37.
3. A renderer that is not the card's GPU: with only lavapipe as the Vulkan driver, `VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/lvp_icd.json` in Shingle's environment, the log has `no Vulkan adapter on PCI SLOT; copy scanout renders on any` and `output CONN: copy scanout, because the renderer's adapter is not the card's GPU (PCI SLOT)`, and the picture of step 10 drawn by lavapipe.
4. On a card whose plane and renderer share no modifier (step 36.2), the reason names both lists; record it. If the test machine has no such card, record that this row was only unit-tested.

### Step 40: zero-copy across VT switches, DPMS, mode changes and hotplug

1. Start Shingle on tty1 with `sh -c 'exec present_probe 100000 > /tmp/probe.out'` and check step 37.2's zero-copy line.
2. `sudo chvt 2; sleep 2; sudo chvt 1`. Expected: `seat disabled; paused`, `seat enabled; resumed`, the probe's picture back, no new `zero-copy scanout` or `copy scanout` line (the GBM buffers are kept across the switch), and `tail -5 /tmp/probe.out` shows `"flags":15` again.
3. `wlopm --off CONN; sleep 3; wlopm --on CONN`: the picture returns, still zero-copy (`"flags":15`).
4. `wlr-randr --output CONN --mode WxH@R` with another listed mode: the log has `output CONN: WxH at R Hz` and a new `output CONN: zero-copy scanout, modifier M` line for the new mode, and the picture is right at the new size. `wlr-randr --output CONN --preferred` (or the old mode) goes back the same way.
5. Unplug and replug the monitor: after `output CONN: ...`, a new zero-copy line follows, and the probe presents with `"flags":15`.
6. With two monitors (step 25), start with `mo7-return.yaml`: each output has its own zero-copy line, and step 32's two probes pass `present_check.py --output CONN_A --scanout zero-copy` and `--output CONN_B --scanout zero-copy`.

### Step 41: a GPU loss with zero-copy scanout

1. Start Shingle on tty1 with `sh -c 'exec present_probe 100000 > /tmp/probe.out'` on zero-copy, and trigger a GPU reset as in step 15's row for a lost GPU, where the driver allows it.
2. Expected: `recreating the GPU context`, and the picture returns without a `back to copy scanout` line: the new renderer imports the same GBM buffers again, and the probe presents with `"flags":15` again. If the new device cannot import them, the log says `output CONN: back to copy scanout, because ...` and frames continue on copy scanout (`"flags":7`); record which happened. Record if the driver has no trigger.

### Step 42: failure-table rows of zero-copy that the test machine can produce

1. Note for each GPU in steps 37 to 41 whether the driver accepted the blocking flip from the dumb buffers to a GBM buffer with another modifier without a modeset (step 37 passing shows it did). If a driver refuses it, the log has `output CONN: copy scanout, because the card refuses a flip to a GBM buffer: ...`; record the error, since the design assumes plane updates may change the modifier.
2. A refused flip during zero-copy, if a driver produces one, shows `the page flip of CONN failed: ...` followed by `output CONN: back to copy scanout, because the page flip to a GBM buffer failed: ...`, and the picture continues on copy scanout. Record it if seen; it cannot be forced from outside.
3. End Shingle with `kill -TERM`; expected `exit=0` and the text console back, as in step 14, now with GBM buffers freed (no `drm` errors in `sudo dmesg | tail`).

## WG-5: touchpad workspace swipes

Nested Shingle gets no touchpad swipes from its host (winit reports none on Wayland), so vswipe with a real touchpad is checked here, on a laptop or with a USB touchpad that libinput lists as a touchpad. The arithmetic and the frontend are covered by unit tests and by tools/gpu_test/run.py's `vswipe` group through `stipc/feed_gesture` ([status.md](status.md#with-real-clients)).

### Step 43: four-finger swipes move the workspace wall

1. Put a Wayfire file beside the test config, `~/.config/shingle/vswipe.ini`, with `[core]` `plugins = vswipe vswitch simple-tile`, `vwidth = 3`, `vheight = 1` and `xwayland = false`, and start Shingle on tty1 with `--config ~/.config/shingle/vswipe.ini --ipc /tmp/shingle-ipc --spawn foot`.
2. After each swipe below, read the current workspace over SSH with `tools/native_test/ipc.py /tmp/shingle-ipc window-rules/get-focused-output` (its `workspace`).
3. Swipe four fingers to the left by about half the touchpad and lift them. Expected: the wall follows the fingers, with a 32-pixel dark gap between the workspaces, and slides on to workspace 2 when they lift; `workspace` is `"x": 1, "y": 0`.
4. A short swipe, under a third of the screen, slowly. Expected: the wall slides back and the workspace stays. The same short swipe flicked fast moves on.
5. Swipe right twice from workspace 1. Expected: the wall resists like a rubber band at the left edge and stays on workspace 1.
6. Swipe three fingers. Expected: the wall does not move, and a client that uses swipes (for example Firefox's history swipe, if it takes three fingers) gets it.
7. Record the touchpad, libinput's version and whether the motion felt like Wayfire's on the same machine, if Wayfire is installed there.

## NB-10: cursor plane, VRR, tearing and fences

From NB-10 on, each lit monitor shows the cursor on its CRTC's cursor plane where it has one, so moving the pointer commits only the cursor plane and draws no frame; sets `VRR_ENABLED` where the output's `vrr` asks for it and the monitor is VRR-capable; flips frames of clients that ask for tearing (`wp_tearing_control_v1`) with `PAGE_FLIP_ASYNC` where the driver takes it; and hands each zero-copy frame's render fence to the primary plane as `IN_FENCE_FD` instead of waiting for the GPU. Each falls back where the card lacks or refuses it, with a logged reason, and `SHINGLE_CURSOR_PLANE=0` (or `WLR_NO_HARDWARE_CURSORS=1`), `SHINGLE_ASYNC_FLIPS=0` and `SHINGLE_IN_FENCE=0` force each fallback. Unit tests through the fake card cover every atomic request and fallback ([design, NB-10 as built](design/native-backend.md#nb-10-as-built)); these steps check the acceptance items "the cursor moves without full-frame renders (frame counter); VRR toggles `VRR_ENABLED` and the monitor reports it; a tearing client gets async flips" and "each feature falls back cleanly where unsupported, verified by forcing each fallback on the test machine".

### Step 44: prepare the test machine for NB-10

1. In addition to steps 0, 9 and 36, install `drm-info`, `vulkan-tools` and `wlr-randr`. Have a VRR-capable monitor at hand (FreeSync or G-Sync Compatible, with adaptive sync turned on in its own menu) and note whether its menu or on-screen display shows the current refresh rate. `drm_info | grep -E 'vrr_capable|VRR_ENABLED|"type": (Cursor|Primary)|IN_FENCE_FD'` and `drm_info | grep -i -A1 'ASYNC_PAGE_FLIP\|CURSOR_WIDTH'` list what the card offers; record whether each CRTC has a cursor plane, the cursor size, `DRM_CAP_ATOMIC_ASYNC_PAGE_FLIP` and whether the monitor's connector says `vrr_capable` 1.
2. Rebuild `present_probe` (step 9) at the commit under test; `present_probe --tearing` asks for tearing.
3. Write the NB-10 configs:

   ```sh
   cd ~/.config/shingle
   printf 'session: {restore: false}\nxwayland: {mode: false}\n' \
     > nb10.yaml
   cp nb10.yaml nb10-vrr.yaml
   printf 'output: {vrr: true}\n' >> nb10-vrr.yaml
   ```

4. "Start Shingle on tty1 with CONFIG and CLIENT" means step 9's command with `SHINGLE_TRACE_FLIPS=1`, `--config ~/.config/shingle/CONFIG` and `--spawn CLIENT`. With `SHINGLE_TRACE_FLIPS=1` the log has `CONN: page flip seq=S at T s` for every frame shown, ending in ` async` for an async flip, and `CONN: cursor plane flip` for every flip of the cursor plane alone.

### Step 45: the cursor moves on its plane without drawing frames

1. Start Shingle on tty1 with `nb10.yaml` and `foot`, and leave foot idle (no cursor blink: `printf '\e[?12l'` in it, or `cursor.blink: no` in foot's config).
2. Over SSH: `grep -E 'cursor (is|plane)' ~/.local/state/shingle/native.log`. Expected: `output CONN: the cursor is on the cursor plane`.
3. Count frames while the pointer moves, with the physical mouse moved in circles for 5 s between the two counts, or with ten motions from uinput:

   ```sh
   log=~/.local/state/shingle/native.log
   frames() { grep -c 'page flip seq' "$log"; }
   cursor() { grep -c 'cursor plane flip' "$log"; }
   before=$(frames); moves=$(cursor)
   for i in $(seq 10); do uinput motion 20 10; done
   echo "frames $(( $(frames) - before )) cursor $(( $(cursor) - moves ))"
   ```

   Expected: `frames 0` (or 1 or 2 where moving onto foot changed its cursor shape), and `cursor` about one per motion. On the screen the pointer follows the mouse smoothly, with no trail and no second pointer, and over foot it becomes foot's text cursor.
4. The forced fallback: restart with `SHINGLE_CURSOR_PLANE=0` in Shingle's environment and repeat 2 and 3. Expected: the log starts with `SHINGLE_CURSOR_PLANE=0 turns cursor planes off` and says `output CONN: the cursor is drawn, because SHINGLE_CURSOR_PLANE=0 turns cursor planes off`; `frames` now grows by about one per motion and `cursor` by none; the pointer looks and moves the same. Repeat with `WLR_NO_HARDWARE_CURSORS=1` instead: the same, with that variable named.
5. Record the driver, the cursor size from step 44.1 and whether the pointer's hotspot is right: the tip of the arrow, not its middle, clicks foot's scroll bar edge.

### Step 46: what the cursor plane cannot show is drawn into the frame

1. Start Shingle on tty1 with `nb10.yaml` and `foot`, as in step 45, and check the plane line of step 45.2.
2. `ipc invert/toggle`. Expected: `output CONN: the cursor is drawn, because colours are inverted`, the picture inverted with an inverted pointer, and no moment with two pointers or none. `ipc invert/toggle` again: `the cursor is on the cursor plane` and the normal picture.
3. `wlr-randr --output CONN --scale 2`. Expected: `the cursor is drawn, because the cursor's buffer scale 1 is not the output's scale 2`, and the pointer at twice its size; `--scale 1` brings the plane back.
4. With two monitors (step 25), start with `nb10.yaml`, move the pointer across the shared edge in both directions: each output logs its own plane line, the pointer appears on one monitor at a time without lingering on the other, and `cursor plane flip` lines name the monitor the pointer is on.
5. A refused plane, if a driver produces one: the log has `output CONN: the cursor is drawn into the frame, because the cursor plane refused the cursor: ...` and the pointer keeps moving, drawn into the frames. Record the error; it cannot be forced from outside.

### Step 47: VRR follows the output's `vrr`, and the monitor reports it

1. With the VRR-capable monitor connected, start Shingle on tty1 with `nb10-vrr.yaml` and `sh -c 'exec present_probe 600 > /tmp/probe.out'`.
2. Over SSH:

   ```sh
   grep 'variable refresh' ~/.local/state/shingle/native.log
   drm_info | grep VRR_ENABLED
   wlr-randr | grep -i 'adaptive'
   python3 ~/shingle/tools/native_test/present_check.py --vrr \
     ~/.local/state/shingle/native.log /tmp/probe.out; echo "status=$?"
   ```

   Expected: `output CONN: variable refresh rate on`, `VRR_ENABLED` 1 on Shingle's CRTC, `Adaptive Sync: enabled`, `status=0` with `"variable_refresh": 600` (every frame has a refresh of 0). The monitor's display reports adaptive sync active (FreeSync or G-Sync Compatible in its on-screen display); record what it shows.
3. Toggle it with a config reload: remove the `output: {vrr: true}` line from nb10-vrr.yaml (Shingle reloads it within its reload delay). Expected: one modeset (the picture may blink), `VRR_ENABLED` 0, `Adaptive Sync: disabled`, the monitor reports a fixed rate, and a new probe run passes `present_check.py` without `--vrr` with the mode's `refresh`. Add the line back: on again.
4. With a monitor that is not VRR-capable (or with adaptive sync turned off in the monitor's menu, which some monitors report as `vrr_capable` 0), the log says `output CONN: variable refresh rate stays off, because the monitor or the driver is not VRR-capable (vrr_capable is not 1)` and `VRR_ENABLED` stays 0. Record which.
5. Optionally, with VRR on, run a client whose frame rate is below the refresh (for example `vkcube` limited by a CPU load) and record whether the monitor's reported rate follows it.

### Step 48: a tearing client gets async flips

1. Start Shingle on tty1 with `nb10.yaml` and `sh -c 'exec present_probe --tearing 600 > /tmp/probe.out'`.
2. After 20 s, over SSH:

   ```sh
   log=~/.local/state/shingle/native.log
   grep -c 'page flip.* async' "$log"
   python3 ~/shingle/tools/native_test/present_check.py --tearing \
     "$log" /tmp/probe.out; echo "status=$?"
   ```

   Expected on a driver with `DRM_CAP_ATOMIC_ASYNC_PAGE_FLIP` (step 44.1): about 600 async lines, `status=0` with `"tearing": 600` and flags 6 (hardware clock and completion, no vsync) in every `presented` line, and a `median_frame_interval_ms` below one refresh period when the copy path or the GPU allows it. With copy scanout (`SHINGLE_SCANOUT=copy`) the same; record both medians.
3. Real clients: `ipc stipc/run '{"cmd": "vkcube --wsi wayland --present_mode 0"}'` (immediate mode, which Mesa's and NVIDIA's Vulkan WSI present through `wp_tearing_control_v1` when offered): the trace shows async flips while it runs, and vkcube's frame rate is not capped at the refresh rate. Record the frame rate and whether tearing is visible.
4. Forced fallback: restart with `SHINGLE_ASYNC_FLIPS=0`: the log starts with `SHINGLE_ASYNC_FLIPS=0 keeps every page flip synchronized`, no async lines, and `present_check.py` passes without `--tearing` (flags 7).
5. On a driver without the capability the log says once `a client asks for tearing, but the driver has no atomic async page flips; its frames wait for the vblank` and the run passes without `--tearing`. A driver that advertises the capability but refuses a flip logs `output CONN: frames wait for the vblank again, because the card refused an async page flip: ...`, and the frame and later ones flip in sync; record the error.

### Step 49: zero-copy frames hand the plane their render fence

1. On a card that reached zero-copy in step 37, start Shingle on tty1 with `nb10.yaml` and `sh -c 'exec present_probe 600 > /tmp/probe.out'`.
2. Over SSH: `grep -E 'wait for the GPU|IN_FENCE_FD' ~/.local/state/shingle/native.log` prints nothing (no fallback), and step 38's check passes: `present_check.py --scanout zero-copy` with `status=0` and a median of one refresh period. On the screen: no tearing, stripes or half-drawn frames, which would mean the plane did not wait for the fence.
3. Forced fallback: restart with `SHINGLE_IN_FENCE=0`: the log starts with `SHINGLE_IN_FENCE=0 makes the renderer wait for the GPU before each flip`, and the same check passes. In both runs record Shingle's CPU time for the 600 frames (`ps -o times=`, as in step 38) and the median interval; the fence should leave the frames' timing unchanged while the render loop no longer blocks on the GPU.
4. If a driver refuses the fence, the log says `frames wait for the GPU before their flip again, because the card refused IN_FENCE_FD: ...` and the frames keep coming. Record it. If the renderer cannot export fences, the log says `zero-copy frames wait for the GPU before their flip, because ...` at the start; record the reason.

### Step 50: the cursor plane and VRR across VT switches, DPMS and hotplug

1. Start Shingle on tty1 with `nb10-vrr.yaml` and `foot` on the VRR-capable monitor, with the pointer over foot.
2. `sudo chvt 2; sleep 2; sudo chvt 1`. Expected: the pointer is back where it was, on the plane (moving it adds `cursor plane flip` lines and no frames), and `VRR_ENABLED` is still 1.
3. `wlopm --off CONN; sleep 3; wlopm --on CONN`: the same.
4. Unplug and replug the monitor: after `output CONN: ...` the log has `variable refresh rate on` again and the pointer is on the plane.
5. With two monitors, `wlr-randr --output CONN_B --off` while the pointer is on B: the pointer appears on A, and `drm_info` shows B's cursor plane with no framebuffer (`FB_ID` 0).

### Step 51: end and record

1. End Shingle with `kill -TERM`: `exit=0` and the text console back, with no cursor left on the console (the kernel's console turns the planes off) and no `drm` errors in `sudo dmesg | tail`.
2. Record for each GPU family: the cursor plane's use and size, async flips, VRR and the monitor's report, fences, and every fallback line seen.

## WG-9: per-device input options

Since WG-9 a device takes the libinput options of the `[input:NAME]` section (YAML `input_devices.NAME`) that names it by its udev `ID_PATH`, `ID_SERIAL`, `LIBINPUT_DEVICE_GROUP` or `DEVPATH`, or else by its name, in place of `[input]`'s, and a touch screen the `calibration` of its `[input-device:NAME]` section, as Wayfire does ([design, WG-9 as built](design/wayfire-gaps.md#wg-9-as-built)). Unit tests through the fake libinput device cover the matching and the settings; this step checks them on real devices. It needs a mouse and a touchpad, ideally two mice, and a touch screen for the calibration if there is one.

### Step 52: each device gets its own section's options

1. Start Shingle once with the test config and read the names and properties: `tools/native_test/ipc.py /tmp/shingle-ipc input/list-devices` lists each device's `name` with `input-section` `input`, and `udevadm info /dev/input/eventN | grep -E 'ID_PATH|ID_SERIAL|LIBINPUT_DEVICE_GROUP'` gives the properties. Stop Shingle.
2. Write `~/.config/shingle/devices.ini` with `[core]` `plugins = simple-tile` and `xwayland = false`, `[input]` `mouse_cursor_speed = -0.8` and `tap_to_click = true`, `[input:MOUSE]` `mouse_cursor_speed = 0.8` and `mouse_accel_profile = flat`, where MOUSE is the first mouse's name, `[input:PATH]` `tap_to_click = false`, where PATH is the touchpad's `ID_PATH`, and, with a touch screen, `[input-device:SCREEN]` `calibration = -1 0 1 0 1 0`, which mirrors it left to right. Start Shingle on tty1 with `--config ~/.config/shingle/devices.ini --ipc /tmp/shingle-ipc --spawn foot`.
3. Over SSH: `grep 'options from' ~/.local/state/shingle/native.log` shows `options from [input:MOUSE]` for the first mouse, `[input:PATH]` for the touchpad and `[input]` for everything else; `input/list-devices` reports the same as `input-section`, and `input-device:SCREEN` as the touch screen's `input-device-section`.
4. `sudo libinput list-devices` while Shingle runs is not reliable (it opens its own context), so check by hand: the first mouse moves the pointer fast and without acceleration, a second mouse slowly; tapping the touchpad does not click, while its physical button does.
5. Touch the left edge of the touch screen: the pointer or touch point appears at the right edge.
6. Unplug and replug the first mouse: the log shows `options from [input:MOUSE]` again and it is fast again.
7. Record the devices, their properties and whether each behaved as configured.

## Results

Record results in [status.md](status.md#with-real-clients). None of the steps above has been run yet: the test machine does not exist so far.
