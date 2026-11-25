# Development

## Requirements

- Rust 1.92 or newer (edition 2024).
- Development headers for Wayland, xkbcommon and PAM. On Debian: `libwayland-dev libxkbcommon-dev libpam0g-dev`.
- A Vulkan-capable GPU and driver for wgpu.

Without `libpam0g-dev`, every target that links `shingle-auth` (including `cargo test --workspace`) fails with `unable to find library -lpam`. If the headers cannot be installed, a symlink works around it:

```sh
mkdir -p /tmp/shingle-lib
ln -sf /usr/lib/x86_64-linux-gnu/libpam.so.0 /tmp/shingle-lib/libpam.so
export LIBRARY_PATH=/tmp/shingle-lib
```

## Build, test and lint

```sh
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

`.cargo/config.toml` limits Cargo to two jobs. Check `/proc/loadavg` before large builds, and run builds and test suites one after another rather than in parallel.

## Formatting

`rustfmt.toml` sets 80 columns and 2-space indentation. Format Shingle's crates with:

```sh
rustfmt --edition 2024 $(find crates -path '*/src/*' \( -name lib.rs -o -name main.rs \))
```

Do **not** run `cargo fmt --all`: it follows the path dependency from `shingle-wcm` into `third_party/gloomy` and reformats the vendored sources.

rustfmt leaves an entire expression untouched if it cannot fit one of its lines in 80 columns. After formatting, check `rg -n '.{81,}' crates --type rust`. Fix any hit by hand, with a `use` for long protocol paths, a helper for long `matches!` guards, or `concat!` for long strings, then format again.

Comments go on their own line above the code and use `//` or `///`, never `/* */`.

## Vendored Gloomy

`third_party/gloomy` is a copy of the Gloomy UI library used by WCM. It is excluded from the workspace and keeps its own formatting and warnings. Make changes upstream and copy them in, and keep any such update in its own commit.

## Running nested

```sh
cargo run -p shingle-nested -- --config data/shingle.yaml --spawn foot
```

This opens Shingle in a window inside the current session, prints its private `WAYLAND_DISPLAY` and passes it to `--spawn` children. Locking runs `shingle-lock` from `PATH`; set `SHINGLE_LOCK` to use another executable.

## Real-client tests

Unit tests do not cover protocol behaviour end to end. For any protocol, IPC or session change, also test with real clients:

- Never stop or replace the host Wayfire session.
- Use a private `XDG_RUNTIME_DIR`, Wayland socket and IPC socket for each run.
- Exercise real protocol calls and check the resulting state changes; a successful build or import proves nothing.
- Afterwards remove temporary fixture sources, binaries outside `target/`, sockets and logs. Leave `target/` itself in place.
- Record the result in [status.md](status.md), separately from what was only unit-tested.

## Behavioural reference

Wayfire's source (https://github.com/WayfireWM/wayfire) is the reference for compatible behaviour. Keep a checkout next to this repository, at `../wayfire`.
