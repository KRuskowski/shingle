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

   `SHINGLE_TRACE_FLIPS=1` adds a `page flip seq=S at SEC.NSEC s` line to the log for every frame shown.

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

   Expected: the log has `using /dev/dri/cardN` (the boot VGA card), `output NAME: WxH at R Hz, copy scanout` with the monitor's connector name (such as `HDMI-A-1` or `DP-1`) and preferred mode, and `rendering on ADAPTER; zero-copy scanout is not built yet, so frames are copied`, where ADAPTER is the GPU on that card's PCI slot. The descriptors include that `/dev/dri/cardN`. `grim` writes a PNG of the mode's size showing the foot window, tiled over the whole output. `wayland-info` reports the output with the connector name, the mode's size and refresh rate, and the monitor's physical size.
3. On the physical screen: the same foot window, sharp, at the monitor's native resolution, and its cursor follows typing on the test machine's keyboard. The mouse pointer is drawn and moves.
4. If a second monitor is connected, it is dark (Shingle lights one output in NB-7) and the log names only the first.
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

   Expected: `wlopm` first lists the output as `on`, then `off`. The monitor goes dark and reports no signal or standby within a few seconds, and the log says `display off`. While it is off the probe's output does not grow (no frame callbacks without flips). After `--on` the picture returns within about a second, without a restart, the log says `display on`, `wlopm` lists `on` and the probe's output grows again.
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
3. **No connected output at startup.** Unplug the monitor, log in on tty1 blind and start Shingle with `foot`. Over SSH: the log says `no connected output; nothing is shown until one is`, `ipc window-rules/list-views` lists the foot window and SIGTERM is not needed yet. Plug the monitor in, then `sudo chvt 2; sleep 1; sudo chvt 1`: the log names the output and the picture appears (hotplug without a VT switch is NB-9). Record whether the monitor shows the console or stays dark while unplugged-then-plugged before the switch.
4. **Output unplugged.** With Shingle running and lit, `sudo chvt 2`, unplug the monitor, `sudo chvt 1`: the log says `no connected output`, Shingle and foot keep running (IPC answers). Plug it in, `sudo chvt 2; sudo chvt 1`: the picture is back with the window in place.
5. **GPU device lost or hung.** Only where the driver offers a reset trigger: on i915, `echo 1 | sudo tee /sys/kernel/debug/dri/N/i915_wedged`; on amdgpu, `sudo cat /sys/kernel/debug/dri/N/amdgpu_gpu_recover`, with N the card's minor. Expected: the log says `the GPU device was lost (...)` or `the GPU did not finish a frame within 100 ms` three times, then `recreating the GPU context`, and the picture continues with every window's content. Triggering it three times within 60 s ends Shingle with `the GPU failed 3 times within 60 s; saving the session and exiting`, `exit=2` and a fresh `session.json`. If the driver has no such trigger, record that the row was not producible.
6. **Panic and SIGKILL.** SIGKILL is step 14. A panic cannot be produced from outside; it is covered by the panic hook's log and the same kernel and seat cleanup as SIGKILL.

The other rows cannot be produced on a healthy machine and are unit-tested only: no atomic support (every current KMS driver has it), another DRM master (libseat grants master only to the active session), every mode rejected by `TEST_ONLY` (no real monitor rejects all its modes; the fallback order is unit-tested), and a page flip that never completes (would need a broken driver; the timeout, the recovery modeset and the exit after three are unit-tested).

## Results

Record results in [status.md](status.md#with-real-clients). None of the steps above has been run yet: the test machine does not exist so far.
