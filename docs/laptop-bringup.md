# Laptop bring-up

The first real hardware for the native backend is a laptop. This is the order to work through it so that the most informative checks come first; each line points at the full step in [native-testing.md](native-testing.md), which has the exact commands and expected results. Record every result in the [Results](native-testing.md#results) format there.

tools/native_test/bringup.py drives the laptop from the development host over SSH, and tools/native_test/start_tty1.py (installed on the laptop as `shingle-tty1`) starts Shingle at the laptop's tty1 login; [The bring-up driver](#the-bring-up-driver) below describes both. In the lists, LAPTOP is what `ssh LAPTOP` reaches.

## Before the first boot into Shingle

1. Install the distribution, preferably Debian trixie like the development host, so package names in [development.md](development.md#requirements) match.
2. Enable SSH from the development host with a key in the operator's agent (bringup.py runs ssh in batch mode and never asks for or stores a password), and give the laptop a name in `~/.ssh/config`. From here on steps can be driven from the development host.
3. Note the hardware: `tools/native_test/bringup.py report LAPTOP` collects `lspci -nnk`, the DRM cards with their drivers and `boot_vga`, every connector's status and first mode, `/dev/dri`, `libinput list-devices` (and `/proc/bus/input/devices`, which needs no group), `drm_info -j` for every card, the kernel, the Mesa, libdrm, libinput, libseat and NVIDIA versions, Vulkan's devices, and whether logind or seatd runs. It writes target/bringup/LAPTOP/report.md and drm_info.json and prints, per card, the zero-copy path ([step 37](native-testing.md#step-37-zero-copy-is-selected-and-the-picture-is-right)) and cursor plane ([step 45](native-testing.md#step-45-the-cursor-moves-on-its-plane-without-drawing-frames)) the GPU should get: from the primary plane's XR24 modifiers, the cursor planes and size, async page flips and `vrr_capable` that drm_info reports, or from the GPU family's usual behaviour where drm_info gave nothing (not installed, or the user is not yet in the `video` group). Run it again after step 4 for the full picture.
4. Do [step 0](native-testing.md#step-0-prepare-the-test-machine): `tools/native_test/bringup.py check LAPTOP` lists each item (packages, the `/etc/shingle-native-test-machine` marker, a build on `PATH`, the test config, the groups, the sudo rule for `chvt`, `fgconsole` and uinput.py, a getty on tty1) and prints the exact command for every one that is missing, to run on the laptop; it runs no sudo itself. Then `tools/native_test/bringup.py deploy LAPTOP` installs the build, and `check` again should end with `step 0 is complete`.
5. Keep a way back: a second VT with a plain login (Ctrl+Alt+F2), and SSH. Shingle never puts the VT in graphics mode itself, and the seat daemon restores it after a crash ([failure handling](design/native-backend.md#6-failure-handling)).

## First session: does it run at all

6. [Step 1](native-testing.md#step-1-a-vt-login-selects-native-under-logind-and-under-seatd) and [step 10](native-testing.md#step-10-a-client-is-visible-on-the-display): start from tty1 with `shingle-tty1 --step 10`; a client is visible on the internal panel. `bringup.py step 10 LAPTOP` checks the log, the card and `grim`, and `bringup.py collect LAPTOP` keeps `native.log` from this first run whatever happens (shingle-tty1 also keeps every earlier run's log under `~/.local/state/shingle/bringup/previous`).
7. [Step 14](native-testing.md#step-14-sigterm-sigint-and-kill--9-leave-a-usable-tty1): SIGTERM over SSH, and `kill -9`, both leave tty1 usable: `bringup.py step 14 LAPTOP --signal TERM`, then a new start and `bringup.py step 14 LAPTOP` (SIGKILL).
8. [Step 13](native-testing.md#step-13-a-vt-switch-away-and-back-resumes-with-a-fresh-modeset): `bringup.py step 13 LAPTOP` switches with `chvt` and back; then Ctrl+Alt+F2 and back by hand.
9. [Step 3](native-testing.md#step-3-uinput-keyboard-mouse-and-touch-events-reach-the-client-with-libinput-timestamps): `bringup.py step 3 LAPTOP` compares uinput's timestamps with wev's; then the built-in keyboard and touchpad reach `wev` by hand.

## Second session: daily use on the laptop

10. [Step 37](native-testing.md#step-37-zero-copy-is-selected-and-the-picture-is-right) and [step 38](native-testing.md#step-38-wp_presentation-reports-zero_copy-and-frames-keep-up): which scanout path the laptop's GPU gets, compared with what `report` expected, and whether frames keep up (`bringup.py step 38 LAPTOP` runs present_check.py on the probe's output).
11. [Step 45](native-testing.md#step-45-the-cursor-moves-on-its-plane-without-drawing-frames): the hardware cursor; `bringup.py step 45 LAPTOP` counts frames and cursor plane flips for ten uinput motions.
12. [Step 43](native-testing.md#step-43-four-finger-swipes-move-the-workspace-wall): touchpad swipes, and tap-to-click and natural scrolling from the config.
13. [Step 23](native-testing.md#step-23-gpu-clients-present-dma-bufs-across-a-vt-switch-and-a-gpu-loss): GPU clients (the browser, `mpv`, `vkcube`), which on this host could only be checked partly.
14. An external monitor: [step 17](native-testing.md#step-17-a-monitor-plugged-in-later-is-lit-at-once-with-its-edid), [step 26](native-testing.md#step-26-two-and-three-monitors-are-lit-at-once-with-their-preferred-modes) and [step 28](native-testing.md#step-28-plugging-a-second-monitor-in-and-out-while-clients-run-under-each-unplug-policy).
15. The real Wayfire config: start Shingle with the operator's own `~/.config/wayfire.ini`, wf-panel and the usual applications, and use it for a while.

## Laptop-only checks not in native-testing.md yet

16. **Suspend and resume.** With Shingle running and clients open, `systemctl suspend`, then wake the laptop. Expected: the picture comes back without a VT switch, input works, clients are alive, and `native.log` shows what happened (a session pause and resume, a hotplug re-probe, or nothing). Repeat by closing the lid, if logind suspends on lid close. If the picture does not come back, that is a bug to fix next: record the log.
17. **Lid switch with an external monitor.** Close the lid with an external monitor attached and logind set not to suspend (`HandleLidSwitchDocked=ignore`). Shingle has no lid handling yet; record whether the internal panel stays lit and what Wayfire would do (it turns the panel off through its output configuration only if configured to).
18. **Brightness keys.** Shingle has no backlight control of its own, as in Wayfire. Bind the keys to a command, for example `commands: [{binding: brightnessup, command: brightnessctl set +5%}, {binding: brightnessdown, command: brightnessctl set 5%-}]`, and check they work.
19. **Battery and idle.** Leave it idle: the configured idle timeout blanks the panel (DPMS through [step 12](native-testing.md#step-12-dpms-off-and-on-through-wlr-output-power-management)), and input wakes it.

Everything found here goes into a fix workspace with a regression test through the fakes, the same as the other native bugs.

## The bring-up driver

On the development host, from the checkout:

```sh
tools/native_test/bringup.py report LAPTOP
tools/native_test/bringup.py check LAPTOP
tools/native_test/bringup.py deploy LAPTOP
# What to do for step 10:
tools/native_test/bringup.py step 10
# Drive it while Shingle runs on tty1:
tools/native_test/bringup.py step 10 LAPTOP
tools/native_test/bringup.py collect LAPTOP
```

At the laptop's tty1 login:

```sh
shingle-tty1 --step 10
```

- **Safety.** Every subcommand refuses localhost and this host: the name given, the host name `ssh -G` resolves it to (before connecting), this host's names and addresses, and a target whose `/etc/machine-id` or hostname is this host's. `deploy` and the driven steps also refuse a target without `/etc/shingle-native-test-machine`. `shingle-tty1` refuses without the marker, over SSH, inside a graphical session, off a VT, and while the last run's Shingle still runs.
- **deploy** builds `shingle` and the `present_probe` example here with `cargo build --release --jobs 2` (refusing while the 1-minute load is above 12), reads the highest `GLIBC_` symbol version and the `NEEDED` libraries of both binaries, and compares them with the laptop's glibc, architecture and `ldconfig -p`. If they fit, it copies the binaries, ipc.py, uinput.py, present_check.py and start_tty1.py to `~/shingle-bringup` on the laptop, links `shingle`, `present_probe` and `shingle-tty1` into `~/.local/bin` and records the commit in `~/shingle-bringup/DEPLOYED.json`. If they do not fit, or with `--build-on-target`, it pushes the commit to `~/shingle` on the laptop with git over the same SSH connection and builds it there (a dirty checkout is refused, since only a commit can be pushed); the laptop then needs the build packages and a Rust toolchain, which `check` lists as optional.
- **shingle-tty1** runs `shingle --backend native` with the config, client and environment of a step (`--step N`; `--config`, `--client wev|foot|probe|probe-tearing|true|none`, `--frames`, `--env VAR=VALUE`, `--trace flips,buffers`, `--outputs` and `-- PROGRAM ARGS` override it), with `SHINGLE_TRACE_FLIPS=1`, the IPC socket at `$XDG_RUNTIME_DIR/shingle-ipc` and the native log at `~/.local/state/shingle/native.log`. The run's other files (stdout with `WAYLAND_DISPLAY`, stderr, wev.log or probe.out, the PID and its start time, run.json and `exit=N`) are in `~/.local/state/shingle/bringup/current` instead of the `/tmp` files native-testing.md names; the previous run and its log move to `bringup/previous/TIMESTAMP` first. `--dry-run` prints the command without starting it.
- **step N** prints the step's text from native-testing.md after the `shingle-tty1` command for it. With LAPTOP it drives what SSH can while Shingle runs: steps 1 to 7, 10 to 14, 37, 38, 45, 48, 49 and 51 (IPC calls, uinput events and `chvt` through `sudo -n`, DPMS through wlopm, grim, present_check.py on the probe's output, and SIGTERM, SIGINT or SIGKILL with `--signal`; `--stop` ends Shingle where the step says so). Signals go only to the PID shingle-tty1 recorded, after the same remote command has checked that it still is `shingle` with the recorded start time. The result is printed and appended to target/bringup/LAPTOP/results.md as PASS, FAIL, INFO and MANUAL lines, MANUAL being what needs eyes on the screen or hands on the keyboard; screenshots and fetched logs go beside it.
- **collect** copies native.log and the current run's files to target/bringup/LAPTOP/runs/STARTED, runs present_check.py on every probe output (with `--scanout`, `--tearing`, `--vrr` and `--output` as given; `--tearing` also when the run used `probe-tearing`) and the log checks of tools/native_test/bringup_logs.py (backend, seat and libseat backend, card, renderer, outputs with their scanout and cursor lines, flips, and every panic or failure line), and writes summary.md there.
