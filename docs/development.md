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
- X11 clients run only inside `tools/xwayland_test/run.py`'s namespace, never against the host's X displays.
- Afterwards remove temporary fixture sources, binaries outside `target/`, sockets and logs. Leave `target/` itself in place.
- Record the result in [status.md](status.md), separately from what was only unit-tested.

## Behavioural reference

Wayfire's source (https://github.com/WayfireWM/wayfire) is the reference for compatible behaviour. Keep a checkout next to this repository, at `../wayfire`.
