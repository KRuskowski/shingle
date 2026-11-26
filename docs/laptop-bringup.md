# Laptop bring-up

The first real hardware for the native backend is a laptop. This is the order to work through it so that the most informative checks come first; each line points at the full step in [native-testing.md](native-testing.md), which has the exact commands and expected results. Record every result in the Results section there.

## Before the first boot into Shingle

1. Install the distribution, preferably Debian trixie like the development host, so package names in [development.md](development.md#requirements) match.
2. Note the hardware: `lspci -nnk | grep -A3 -E 'VGA|3D|Display'`, `cat /sys/class/drm/card*/status`, `ls /dev/dri`, the touchpad from `libinput list-devices`, and `drm_info` (package `drm-info`) for every card. The GPU family decides which zero-copy path is expected (NB-8, step 37).
3. Enable SSH from the development host, so steps can be driven from there once Shingle runs on tty1.
4. Do [step 0](native-testing.md#step-0-prepare-the-test-machine): packages, the `/etc/shingle-native-test-machine` marker, a build on `PATH`, the test config.
5. Keep a way back: a second VT with a plain login (Ctrl+Alt+F2), and SSH. Shingle never puts the VT in graphics mode itself, and the seat daemon restores it after a crash ([failure handling](design/native-backend.md#6-failure-handling)).

## First session: does it run at all

6. [Step 1](native-testing.md#step-1-a-vt-login-selects-native-under-logind-and-under-seatd) and [step 10](native-testing.md#step-10-a-client-is-visible-on-the-display): start from tty1; a client is visible on the internal panel. Keep `~/.local/state/shingle/native.log` from this first run whatever happens.
7. [Step 14](native-testing.md#step-14-sigterm-sigint-and-kill--9-leave-a-usable-tty1): SIGTERM over SSH, and `kill -9`, both leave tty1 usable.
8. [Step 13](native-testing.md#step-13-a-vt-switch-away-and-back-resumes-with-a-fresh-modeset): Ctrl+Alt+F2 and back.
9. [Step 3](native-testing.md#step-3-uinput-keyboard-mouse-and-touch-events-reach-the-client-with-libinput-timestamps): the built-in keyboard and touchpad reach `wev`.

## Second session: daily use on the laptop

10. [Step 37](native-testing.md#step-37-zero-copy-is-selected-and-the-picture-is-right) and [step 38](native-testing.md#step-38-wp_presentation-reports-zero_copy-and-frames-keep-up): which scanout path the laptop's GPU gets, and whether frames keep up.
11. [Step 45](native-testing.md#step-45-the-cursor-moves-on-its-plane-without-drawing-frames): the hardware cursor.
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
