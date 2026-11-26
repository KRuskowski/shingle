# Development

## Requirements

- Rust 1.92 or newer (edition 2024).
- Development headers for Wayland, xkbcommon and PAM, and for the native backend libinput, libseat, udev, libdrm and GBM. On Debian: `libwayland-dev libxkbcommon-dev libpam0g-dev libinput-dev libseat-dev libudev-dev libdrm-dev libgbm-dev`.
- A Vulkan-capable GPU and driver for wgpu.

## Build, test and lint

```sh
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

To be sure that no test reaches a GPU, input or VT device of the host, run the suite in a namespace whose `/dev` has none, with lavapipe as the only Vulkan driver: `bwrap --bind / / --dev /dev env VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/lvp_icd.json cargo test --workspace --no-fail-fast`. The three reference-image tests in crates/shingle-render/tests/offscreen_pixels.rs then fail, because their references were rendered on a hardware GPU; everything else passes, and the udmabuf tests in crates/shingle-render/tests/dmabuf_import.rs skip.

To run a GPU test on the render node without exposing anything else, use tools/gpu_test/run.py: it runs one command in a user, mount, network and PID namespace whose `/dev` holds only the render node, NVIDIA's compute nodes, `udmabuf` and the basic nodes, and fails if any process opens another device. For example `cargo test -p shingle-render --test dmabuf_import --no-run`, then `tools/gpu_test/run.py -- target/debug/deps/dmabuf_import-HASH --include-ignored`. `cargo run -p shingle-render --example dmabuf_caps` prints which formats and modifiers each Vulkan adapter can import. After `cargo build -p shingle-bin` and `cargo build -p shingle --example wl_probe`, `tools/gpu_test/run.py --checks offscreen nested baseline` runs the real-client checks of `shingle --backend offscreen` in that namespace: shm clients and inversion in captures, nested Shingle inside it, and what GPU clients do without linux-dmabuf; `--captures DIR` keeps the settled captures. DB-4's linux-dmabuf checks are the groups `dmabuf browsers xwayland nested-gpu nodmabuf weston` (tools/gpu_test/dmabuf_checks.py): they run `wayland-info`, `vkcube`, the gears, `kitty`, Firefox and Chromium (each with a throwaway profile in the namespace's private HOME, without network), `glxgears` through Xwayland, nested Shingle on NVIDIA, `weston-simple-dmabuf-*` and `mpv`, one at a time, and judge each by captures and by Shingle's buffer trace. For these groups run.py also exposes `/dev/nvidia-modeset`, which NVIDIA's Vulkan WSI needs for a swapchain; the card node stays hidden, so NVIDIA's EGL clients fall back to Mesa on `wl_shm` there and are only recorded. DB-6's group `shm` runs `wl_probe --damage`, which damages 10×10 pixels of a 1000×1000 buffer and must upload 400 bytes for it by the trace, and records the CPU time offscreen Shingle spends on 600 full-output frames of `wl_probe --animate`; `Captures.settled(..., stable=True)` waits for two identical captures, so that the captures of `offscreen`, `nested`, `buffers` and `shm` saved with `--captures` compare pixel for pixel between builds. DB-5's syncobj checks are the groups `syncobj` (after `cargo build -p shingle --example syncobj_probe`: one toplevel stalled on an acquire point nobody signals while another keeps drawing) and `nosyncobj` (`dmabuf`, `xwayland` and `nested-gpu` again with `SHINGLE_SYNCOBJ=0`); `dmabuf`, `xwayland` and `nested-gpu` also check that their clients commit with syncobj points. `SHINGLE_TRACE_BUFFERS=1` makes any Shingle log each client buffer's path (`dmabuf import`, `dmabuf cached`, `shm upload` with the bytes copied from the pool, `blocked implicit`, `blocked syncobj`, `syncobj acquire ready`, `syncobj release`, `release`), `SHINGLE_DMABUF=0` turns linux-dmabuf off and `SHINGLE_SYNCOBJ=0` turns off linux-drm-syncobj alone. The device-backed syncobj test (`linux_drm_syncobj::timeline::tests`) opens `/dev/dri/renderD128` on the host, never a card node, and skips without it.

After `cargo build -p shingle-bin` and `cargo build -p shingle --example wl_probe`, `tools/session_test/run.py` checks that a slow disk does not stall Shingle: in the output harness's namespace it runs nested Shingle on lavapipe inside a headless one, with `XDG_STATE_HOME` on a FUSE mount (tools/session_test/slowfs.py, python3-fuse) whose fsync sleeps `--delay` seconds (1.5 by default), changes the layout over IPC every 100 ms while a session write is stuck, and requires every reply within 50 ms, few coalesced writes, a clean SIGTERM exit and the last layout in the session file. It needs a readable and writable `/dev/fuse`; the mount lives only in the namespace. `tools/session_test/run.py --checks round-trip` instead runs session round trips without FUSE (tools/session_test/round_trip.py): five `wl_probe` clients in nested Shingle inside a headless one are arranged in a nested, unevenly weighted split with two floating views on two workspaces, and the layouts and views must be identical after a SIGKILL and restart and after a SIGTERM and restart. With `--master-binary PATH`, a `shingle` built from `master` (for example from `git archive master` in a directory of its own, built with its own target directory) saves the same layout as a version 1 session, which the build under test must restore unchanged and save back as version 2. `--checks slow round-trip` runs both.

`.cargo/config.toml` limits Cargo to two jobs. Check `/proc/loadavg` before large builds, and run builds and test suites one after another rather than in parallel.

`wayland_toplevels_look_unchanged` (crates/shingle/src/wayland_pins.rs) compares what xdg toplevels look like from outside (IPC replies and events, configures, surface snapshots and sessions) with crates/shingle/testdata/wayland_pins.txt byte for byte. If a change is meant to alter that, rerun it with `SHINGLE_BLESS_PINS=1` and review the diff of the file.

## Formatting

`rustfmt.toml` sets 80 columns and 2-space indentation:

```sh
cargo fmt --all
```

rustfmt leaves an entire expression untouched if it cannot fit one of its lines in 80 columns. After formatting, check `rg -n '.{81,}' crates --type rust`. Fix any hit by hand, with a `use` for long protocol paths, a helper for long `matches!` guards, or `concat!` for long strings, then format again.

Comments go on their own line above the code and use `//` or `///`, never `/* */`.

## Gloomy

WCM uses the Gloomy UI library from https://github.com/KRuskowski/gloomy, pinned to a commit in the root `Cargo.toml`. To change Gloomy, commit and push there, then bump the `rev` in its own Shingle commit.

To work on both at once, point Cargo at a local checkout without editing `Cargo.toml`:

```sh
cargo build --config 'patch."https://github.com/KRuskowski/gloomy".gloomy-core.path="../gloomy/crates/gloomy-core"' \
  --config 'patch."https://github.com/KRuskowski/gloomy".gloomy-app.path="../gloomy/crates/gloomy-app"'
```

## Running

```sh
cargo run --bin shingle -- --config data/shingle.yaml --spawn foot
cargo run --bin shingle -- --help
```

Inside a Wayland or X11 session this selects the nested backend and opens Shingle in a window, prints its private `WAYLAND_DISPLAY` and passes it to `--spawn` children. Every child Shingle starts gets the client environment (crates/shingle-session/src/environment.rs): Shingle's own `WAYLAND_DISPLAY`, its own `DISPLAY` and, with `--ipc`, `WAYFIRE_SOCKET`; the host's `DISPLAY`, `WAYLAND_SOCKET` and `WAYFIRE_SOCKET` never get through. With the default `xwayland.mode: lazy`, Shingle binds the next free X display in `/tmp/.X11-unix` when it starts (`:2` beside the host session's `:0` and `:1`). `--headless` runs without a window. `--backend native`, and a start with none of `WAYLAND_DISPLAY`, `WAYLAND_SOCKET` and `DISPLAY` set, runs the native backend: it opens the seat with libseat and the input devices with libinput, so it is only ever run on the native test machine ([native-testing.md](native-testing.md)); from an SSH login without `XDG_SEAT` it refuses with "no seat" and status 1. Scripts and agents on a development host always pass `--headless` or `--backend nested`, so that a missing display variable can never select native. Bad options and unknown backends exit with status 2. The selection rules are in crates/shingle-bin/src/backend.rs and the [README](../README.md#running). Locking runs `shingle-lock` from `PATH`; set `SHINGLE_LOCK` to use another executable.

When testing nested from a session whose `XDG_RUNTIME_DIR` is replaced by a private one, set `WAYLAND_DISPLAY` to the host socket's absolute path so the window still reaches the host, and set `XDG_CONFIG_HOME`, `XDG_STATE_HOME` and `HOME` to private directories too. Outside the XWayland harness, also set `xwayland: {mode: false}` in the test config, so that nothing is bound in the host's `/tmp`.

## Real-client tests

Unit tests do not cover protocol behaviour end to end. For any protocol, IPC or session change, also test with real clients:

- Never stop or replace the host Wayfire session.
- Use a private `XDG_RUNTIME_DIR`, Wayland socket and IPC socket for each run.
- Exercise real protocol calls and check the resulting state changes; a successful build or import proves nothing.
- A harness that signals processes runs in a PID namespace (`unshare --pid --fork --mount-proc`) and checks every PID before signalling it: never signal a PID taken from a reply without checking it is positive, greater than 1 and the expected process, because `kill(-1)` reaches every process of the user.
- X11 clients run only inside `tools/xwayland_test/run.py`'s namespace, never against the host's X displays. Anything that needs the GPU's render node runs inside `tools/gpu_test/run.py`'s namespace, never with the host's card node, input devices or TTYs.
- Afterwards remove temporary fixture sources, binaries outside `target/`, sockets and logs. Leave `target/` itself in place.
- Record the result in [status.md](status.md), separately from what was only unit-tested.

## Behavioural reference

Wayfire's source (https://github.com/WayfireWM/wayfire) is the reference for compatible behaviour. Keep a checkout next to this repository, at `../wayfire`.
