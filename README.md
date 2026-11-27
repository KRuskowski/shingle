# Shingle

Shingle is a tiling Wayland compositor written in Rust, built around Wayfire's tiling behaviour and compatible with Wayfire's configuration and IPC. Windows live in a persistent tiling tree rather than as independent positions, so the exact layout survives logout and login. Rendering uses wgpu.

It reads a WCM-written `wayfire.ini` and behaves like Wayfire with it: simple-tile, vswitch, wsets, oswitch, expo, scale, the switcher, animate, window rules, decorations and wf-shell's protocol, with Wayfire's IPC. Xwayland windows, several monitors, client DMA-BUFs with explicit sync, screen sharing through xdg-desktop-portal-wlr, `shingle-launcher` (an application launcher edited in place) and `shingle-lock` (a blurred lock screen that reads swaylock's config and never shows the desktop before an unlock) are built in.

Shingle runs **nested** inside another Wayland or X11 session, offscreen or headless today. The native backend (libseat, libinput, atomic KMS with planes, VRR and zero-copy scanout) is built and unit-tested but has not run on hardware yet, so it is not yet a replacement for Wayfire on a real seat. See [docs/status.md](docs/status.md).

## Quick start

```sh
cargo build --workspace
cargo run --bin shingle -- --config data/shingle.yaml --spawn foot
```

Started inside a Wayland or X11 session, this opens Shingle in a window without touching the host compositor. Build requirements and the test workflow are in [docs/development.md](docs/development.md).

## Running

There is one binary, `shingle`, which picks its backend the way Wayfire does:

| Backend | Selected when |
| --- | --- |
| nested, a window in the host session | `WAYLAND_DISPLAY`, `WAYLAND_SOCKET` or `DISPLAY` is set |
| native, libseat, libinput and KMS | none of them is set, as on a TTY. It takes the seat, its input devices and one monitor (atomic KMS with copy scanout; not yet run on hardware); from an SSH login it refuses with "no seat" and exits with status 1 |
| headless, no display or input devices | `--headless` |
| offscreen, headless but drawing with the GPU into an offscreen target that only screencopy shows, for GPU clients in tests | `--backend offscreen` |

`--backend nested|native|headless|offscreen` overrides the choice, and so does the `SHINGLE_BACKEND` environment variable with the same values; the command line wins over the variable. Other options are `--config PATH`, `--ipc PATH` (Wayfire IPC, exported to clients as `WAYFIRE_SOCKET`), `--outputs N` (headless, offscreen and nested, which opens a window for each: `HEADLESS-1` to `HEADLESS-N`, else `WLR_HEADLESS_OUTPUTS`, for nested `WLR_WL_OUTPUTS`; native lights at most N of the connected monitors) and `--spawn PROGRAM [ARG]...`, which starts a client; `shingle --help` lists them all. The separate `shingle-nested` binary is gone: run `shingle`, or `shingle --backend nested` to be explicit.

Every program Shingle starts gets Wayfire's variables as well as Shingle's sockets: `XDG_CURRENT_DESKTOP=wayfire` when Shingle was started without one, so xdg-desktop-portal reads Wayfire's `wayfire-portals.conf`, `XCURSOR_THEME` and `XCURSOR_SIZE` from `[input] cursor_theme` and `cursor_size`, and `WAYFIRE_CONFIG_FILE` when the configuration is a Wayfire file ([docs/features.md](docs/features.md#client-environment)).

### Starting from a display manager

data/shingle.desktop is a Wayland session file for display managers such as GDM, SDDM or greetd's greeters, like Wayfire's `wayfire.desktop`. It runs `shingle` with no options, which from a display manager selects the native backend and reads the default configuration (`~/.config/shingle/shingle.yaml`). Nothing installs it; to offer Shingle at the login screen, install the binaries where the display manager's `PATH` finds them and copy the file into a `wayland-sessions` directory it reads, for example:

```sh
cargo build --release
sudo install -m 755 target/release/shingle target/release/shingle-lock \
  target/release/shingle-launcher /usr/local/bin/
sudo install -D -m 644 data/shingle.desktop \
  /usr/local/share/wayland-sessions/shingle.desktop
```

Use `/usr/share/wayland-sessions` instead if the display manager does not search `/usr/local/share` (SDDM, for example, only reads `/usr/share/wayland-sessions` by default). To start with a `wayfire.ini`, change `Exec` to `shingle --config /home/USER/.config/wayfire.ini`. The session sets `XDG_CURRENT_DESKTOP` to `Shingle:Wayfire:wlroots` from `DesktopNames`, which Shingle keeps: xdg-desktop-portal then uses `wayfire-portals.conf` when Wayfire is installed, and otherwise the portals that name `wlroots` as their desktop, such as xdg-desktop-portal-wlr. `desktop-file-validate` reports `DesktopNames` as an unknown key, as it does for Wayfire's file; display managers read it. The native backend has not run on hardware yet ([docs/status.md](docs/status.md)).

## Configuration

Shingle reads `$XDG_CONFIG_HOME/shingle/shingle.yaml`, or `~/.config/shingle/shingle.yaml` when `XDG_CONFIG_HOME` is unset. [data/shingle.yaml](data/shingle.yaml) lists the available settings and default shortcuts.

YAML is the native format, and misspelled options are rejected. Wayfire and i3 configuration can be imported: files ending in `.ini`, or starting with an INI section, are read as Wayfire configuration, and `.conf` and `.i3` files as i3 configuration. Not every Wayfire or i3 option is supported yet. A `wayfire.ini` is read as Wayfire reads it, including the one WCM writes: a value Shingle cannot use keeps its default and is reported with its line, so the file still loads; `shingle-wcm --check` lists those warnings.

WCM is a graphical configuration editor that can also validate and convert files:

```sh
cargo run -p shingle-wcm -- --config data/shingle.yaml
cargo run -p shingle-wcm -- --config data/shingle.yaml --check
cargo run -p shingle-wcm -- \
  --config wayfire.ini --output shingle.yaml --export-yaml
```

### Locking

`shingle-lock` locks the session; the lock binding (`super+f8`) and `idle.lock_seconds` run it. It blurs a screenshot of every output and shows the time; typing brings a slim white password input. It reads your swaylock config (`~/.config/swaylock/config`, swaylock's and swaylock-effects' options) and the `lock` section of Shingle's config, which adds the background's drift and `style: swaylock` for swaylock's coloured indicator. A Wayfire config that runs `swaylock` runs `shingle-lock` instead; write `/usr/bin/swaylock` to keep swaylock. With swayidle, use `swayidle -w ... before-sleep 'shingle-lock -f'` so that a suspend waits until the lock is up. PAM uses the `shingle` service (data/pam/shingle). Details are in [docs/design/lock-screen.md](docs/design/lock-screen.md#6-switching-from-swaylock).

## Scripting

Shingle serves Wayfire-compatible IPC and exports `WAYFIRE_SOCKET` to the programs it starts, so Wayfire scripts work unchanged. Those programs get Shingle's own `DISPLAY` and never inherit the host session's `DISPLAY`, `WAYLAND_SOCKET` or `WAYFIRE_SOCKET`. See [docs/ipc.md](docs/ipc.md).

## Upgrading from Tilefire

Shingle was called Tilefire. Existing setups keep working:

| Tilefire | Shingle | Fallback |
| --- | --- | --- |
| `~/.config/tilefire/tilefire.yaml` | `~/.config/shingle/shingle.yaml` | Old file is used while the new one does not exist |
| `~/.local/state/tilefire/session.json` | `~/.local/state/shingle/session.json` | Old session is restored and saved in place while the new one does not exist |
| PAM service `tilefire` | PAM service `shingle` ([data/pam/shingle](data/pam/shingle)) | `tilefire` is used while only `/etc/pam.d/tilefire` is installed |
| `TILEFIRE_LOCK`, `TILEFIRE_PAM_SERVICE` | `SHINGLE_LOCK`, `SHINGLE_PAM_SERVICE` | Old variable is read when the new one is unset |

Move the files and install the new PAM service to finish migrating.

## Documentation

- [docs/features.md](docs/features.md): overviews, effects, input and protocol behaviour, with configuration examples.
- [docs/ipc.md](docs/ipc.md): Wayfire IPC methods and events.
- [docs/status.md](docs/status.md): what works, what does not, and how it was verified.
- [docs/roadmap.md](docs/roadmap.md): planned work in priority order.
- [docs/development.md](docs/development.md): building, testing, formatting and real-client test rules.
- [docs/architecture.md](docs/architecture.md): crates and where things live.

## License

MIT.
