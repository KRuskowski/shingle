mod shm;
mod username;

use std::{
    collections::HashMap,
    fs, io,
    panic::{AssertUnwindSafe, catch_unwind},
    path::{Path, PathBuf},
    process::ExitCode,
    thread,
};

use calloop::{
    EventLoop,
    channel::{self, Event as ChannelEvent, Sender},
    signals::{Signal, Signals},
};
use calloop_wayland_source::WaylandSource;
use shm::{BufferData, ShmBuffer};
use thiserror::Error;
use tilefire_auth::{Authenticator, Password};
use tilefire_render::{OffscreenRenderer, RenderRect, RenderTile};
use wayland_client::{
    Connection, Dispatch, Proxy, QueueHandle, WEnum,
    globals::{
        BindError, GlobalError, GlobalList, GlobalListContents,
        registry_queue_init,
    },
    protocol::{
        wl_buffer, wl_compositor, wl_keyboard, wl_output, wl_region,
        wl_registry, wl_seat, wl_shm, wl_shm_pool, wl_surface,
    },
};
use wayland_protocols::ext::session_lock::v1::client::{
    ext_session_lock_manager_v1::ExtSessionLockManagerV1,
    ext_session_lock_surface_v1::ExtSessionLockSurfaceV1,
    ext_session_lock_v1::ExtSessionLockV1,
};
use xkbcommon::xkb;
use zeroize::{Zeroize, Zeroizing};

const MAX_PASSWORD_BYTES: usize = 4096;

struct Options {
    background_color: [f64; 4],
    show_indicator: bool,
}

enum CliAction {
    Run(Options),
    Help,
    Version,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            background_color: [0.012, 0.016, 0.028, 1.0],
            show_indicator: true,
        }
    }
}

impl Options {
    fn parse() -> Result<CliAction, LockError> {
        let arguments = std::env::args_os().skip(1).collect::<Vec<_>>();
        let config = Self::config_path(&arguments)?.or_else(default_config);
        Self::parse_arguments(arguments, config)
    }

    #[cfg(test)]
    fn parse_from(
        arguments: impl IntoIterator<Item = std::ffi::OsString>,
    ) -> Result<CliAction, LockError> {
        let arguments = arguments.into_iter().collect::<Vec<_>>();
        let config = Self::config_path(&arguments)?;
        Self::parse_arguments(arguments, config)
    }

    fn parse_arguments(
        arguments: Vec<std::ffi::OsString>,
        config: Option<PathBuf>,
    ) -> Result<CliAction, LockError> {
        for argument in &arguments {
            match argument.to_str() {
                Some("-h" | "--help") => return Ok(CliAction::Help),
                Some("-v" | "--version") => return Ok(CliAction::Version),
                _ => {}
            }
        }
        let mut options = Self::default();
        if let Some(path) = config {
            options.apply_config(&path)?;
        }
        options.apply_arguments(arguments)?;
        Ok(CliAction::Run(options))
    }

    fn config_path(
        arguments: &[std::ffi::OsString],
    ) -> Result<Option<PathBuf>, LockError> {
        let mut path = None;
        let mut arguments = arguments.iter();
        while let Some(argument) = arguments.next() {
            match argument.to_str() {
                Some("-C" | "--config") => {
                    let value = arguments.next().ok_or_else(|| {
                        LockError::Message(format!(
                            "{} needs a config path",
                            argument.to_string_lossy()
                        ))
                    })?;
                    if value.is_empty() {
                        return Err(LockError::Message(
                            "config path cannot be empty".into(),
                        ));
                    }
                    path = Some(PathBuf::from(value));
                }
                Some(argument) => {
                    if let Some(value) = argument.strip_prefix("--config=") {
                        if value.is_empty() {
                            return Err(LockError::Message(
                                "config path cannot be empty".into(),
                            ));
                        }
                        path = Some(PathBuf::from(value));
                    }
                }
                None => {
                    return Err(LockError::Message(
                        "command-line arguments must be valid UTF-8".into(),
                    ));
                }
            }
        }
        Ok(path)
    }

    fn apply_config(&mut self, path: &Path) -> Result<(), LockError> {
        let text = fs::read_to_string(path).map_err(|error| {
            LockError::Message(format!(
                "failed to read lock config {}: {error}",
                path.display()
            ))
        })?;
        for (index, original) in text.lines().enumerate() {
            let line = original.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (name, value) = line
                .split_once('=')
                .map(|(name, value)| (name.trim(), Some(value.trim())))
                .unwrap_or_else(|| {
                    let mut parts = line.splitn(2, char::is_whitespace);
                    (
                        parts.next().unwrap_or_default(),
                        parts.next().map(str::trim),
                    )
                });
            let name = name.trim().trim_start_matches("--");
            match (name, value) {
                ("color", Some(value)) => {
                    self.background_color =
                        parse_color(value).map_err(|error| {
                            LockError::Message(format!(
                                "{}:{}: {error}",
                                path.display(),
                                index + 1
                            ))
                        })?
                }
                ("no-unlock-indicator", None | Some("")) => {
                    self.show_indicator = false
                }
                _ => {
                    return Err(LockError::Message(format!(
                        "{}:{}: unknown or malformed option {name:?}",
                        path.display(),
                        index + 1
                    )));
                }
            }
        }
        Ok(())
    }

    fn apply_arguments(
        &mut self,
        arguments: Vec<std::ffi::OsString>,
    ) -> Result<(), LockError> {
        let mut arguments = arguments.into_iter();
        while let Some(argument) = arguments.next() {
            let argument = argument.into_string().map_err(|_| {
                LockError::Message(
                    "command-line arguments must be valid UTF-8".into(),
                )
            })?;
            match argument.as_str() {
                "-h" | "--help" | "-v" | "--version" => {}
                "-u" | "--no-unlock-indicator" => self.show_indicator = false,
                "-c" | "--color" => {
                    let value = arguments.next().ok_or_else(|| {
                        LockError::Message(format!(
                            "{argument} needs an RRGGBB color"
                        ))
                    })?;
                    let value = value.into_string().map_err(|_| {
                        LockError::Message(
                            "the lock color must be valid UTF-8".into(),
                        )
                    })?;
                    self.background_color = parse_color(&value)?;
                }
                "-C" | "--config" => {
                    arguments.next();
                }
                _ => {
                    if let Some(value) = argument.strip_prefix("--color=") {
                        self.background_color = parse_color(value)?;
                    } else if !argument.starts_with("--config=") {
                        return Err(LockError::Message(format!(
                            "unknown argument {argument:?}; use --help for usage"
                        )));
                    }
                }
            }
        }
        Ok(())
    }
}

fn default_config() -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|path| !path.is_empty())
                .map(|home| PathBuf::from(home).join(".config"))
        })
        .map(|path| path.join("swaylock/config"))
        .into_iter()
        .chain([PathBuf::from("/etc/swaylock/config")])
        .find(|path| path.is_file())
}

fn parse_color(text: &str) -> Result<[f64; 4], LockError> {
    let text = text
        .trim()
        .strip_prefix('#')
        .or_else(|| text.trim().strip_prefix("0x"))
        .unwrap_or(text.trim());
    if text.len() != 6 && text.len() != 8 {
        return Err(LockError::Message(format!(
            "invalid color {text:?}; expected RRGGBB or RRGGBBAA"
        )));
    }
    let mut bytes = [0_u8; 4];
    bytes[3] = 255;
    for (index, byte) in bytes.iter_mut().enumerate().take(text.len() / 2) {
        *byte = u8::from_str_radix(&text[index * 2..index * 2 + 2], 16)
            .map_err(|_| {
                LockError::Message(format!(
                    "invalid color {text:?}; expected RRGGBB or RRGGBBAA"
                ))
            })?;
    }
    Ok([
        srgb_to_linear(bytes[0]),
        srgb_to_linear(bytes[1]),
        srgb_to_linear(bytes[2]),
        f64::from(bytes[3]) / 255.0,
    ])
}

fn srgb_to_linear(value: u8) -> f64 {
    let value = f64::from(value) / 255.0;
    if value <= 0.04045 {
        value / 12.92
    } else {
        ((value + 0.055) / 1.055).powf(2.4)
    }
}

fn print_help() {
    println!(
        "tilefire-lock {}\n\
         \n\
         Usage: tilefire-lock [OPTIONS]\n\
         \n\
         -c, --color <RRGGBB[AA]>  Set the lock-screen background color\n\
         -u, --no-unlock-indicator Hide the password and authentication indicator\n\
         -C, --config <PATH>       Read options from a swaylock-style config file\n\
         -h, --help                Print this help\n\
         -v, --version             Print the version\n\
         \n\
         Default config: $XDG_CONFIG_HOME/swaylock/config,\n\
         $HOME/.config/swaylock/config, or /etc/swaylock/config",
        env!("CARGO_PKG_VERSION")
    );
}

#[derive(Debug, Error)]
enum LockError {
    #[error("failed to connect to WAYLAND_DISPLAY: {0}")]
    Connect(#[from] wayland_client::ConnectError),
    #[error("failed to initialize the Wayland registry: {0}")]
    Globals(#[from] GlobalError),
    #[error("event loop failed: {0}")]
    EventLoop(#[from] calloop::Error),
    #[error("local system operation failed: {0}")]
    Io(#[from] io::Error),
    #[error("{0}")]
    Message(String),
}

enum AuthOutcome {
    Authenticated,
    Denied,
    WorkerFailed(String),
}

struct Seat {
    proxy: wl_seat::WlSeat,
}

struct Keyboard {
    proxy: wl_keyboard::WlKeyboard,
    xkb: Option<xkb::State>,
}

struct LockOutput {
    output: wl_output::WlOutput,
    surface: wl_surface::WlSurface,
    lock_surface: ExtSessionLockSurfaceV1,
    renderer: Option<OffscreenRenderer>,
    buffers: Vec<ShmBuffer>,
    size: Option<(u32, u32)>,
    next_buffer_id: u64,
    dirty: bool,
    committed: bool,
}

impl LockOutput {
    fn destroy(self) {
        self.lock_surface.destroy();
        self.surface.destroy();
        if self.output.version() >= 3 {
            self.output.release();
        }
    }
}

struct State {
    connection: Connection,
    _globals: GlobalList,
    _manager: ExtSessionLockManagerV1,
    lock: ExtSessionLockV1,
    compositor: wl_compositor::WlCompositor,
    shm: wl_shm::WlShm,
    outputs: HashMap<u32, LockOutput>,
    seats: HashMap<u32, Seat>,
    keyboards: HashMap<wayland_client::backend::ObjectId, Keyboard>,
    xkb_context: xkb::Context,
    background_color: [f64; 4],
    show_indicator: bool,
    authenticator: Authenticator,
    username: String,
    auth_sender: Sender<AuthOutcome>,
    auth_running: bool,
    auth_failed: bool,
    password: Zeroizing<String>,
    locked: bool,
    finished: bool,
    unlock_requested: bool,
    ready_reported: bool,
    failure: Option<String>,
}

impl State {
    fn fail(&mut self, message: impl Into<String>) {
        self.password.zeroize();
        if self.failure.is_none() {
            self.failure = Some(message.into());
        }
    }

    fn active(&self) -> bool {
        self.failure.is_none() && !self.finished && !self.unlock_requested
    }

    fn add_output(&mut self, name: u32, version: u32, qh: &QueueHandle<Self>) {
        if !self.active() || self.outputs.contains_key(&name) {
            return;
        }
        let output =
            self._globals.registry().bind(name, version.min(4), qh, ());
        let surface = self.compositor.create_surface(qh, ());
        let lock_surface =
            self.lock.get_lock_surface(&surface, &output, qh, name);
        self.outputs.insert(
            name,
            LockOutput {
                output,
                surface,
                lock_surface,
                renderer: None,
                buffers: Vec::new(),
                size: None,
                next_buffer_id: 0,
                dirty: false,
                committed: false,
            },
        );
    }

    fn remove_output(&mut self, name: u32) {
        if let Some(output) = self.outputs.remove(&name) {
            output.destroy();
            self.ready_reported =
                self.ready_reported && !self.outputs.is_empty();
        }
    }

    fn add_seat(&mut self, name: u32, version: u32, qh: &QueueHandle<Self>) {
        if !self.active() || self.seats.contains_key(&name) {
            return;
        }
        let proxy =
            self._globals
                .registry()
                .bind(name, version.min(7), qh, name);
        self.seats.insert(name, Seat { proxy });
    }

    fn remove_seat(&mut self, name: u32) {
        self.remove_keyboard(name);
        if let Some(seat) = self.seats.remove(&name)
            && seat.proxy.version() >= 5
        {
            seat.proxy.release();
        }
    }

    fn remove_keyboard(&mut self, seat_name: u32) {
        let ids = self
            .keyboards
            .iter()
            .filter_map(|(id, keyboard)| {
                (keyboard.proxy.data::<u32>() == Some(&seat_name))
                    .then_some(id.clone())
            })
            .collect::<Vec<_>>();
        for id in ids {
            if let Some(keyboard) = self.keyboards.remove(&id)
                && keyboard.proxy.version() >= 3
            {
                keyboard.proxy.release();
            }
        }
    }

    fn set_keyboard(
        &mut self,
        seat_name: u32,
        present: bool,
        qh: &QueueHandle<Self>,
    ) {
        self.remove_keyboard(seat_name);
        if !present || !self.active() {
            return;
        }
        let Some(seat) = self.seats.get(&seat_name) else {
            self.fail("keyboard capability arrived for an unknown seat");
            return;
        };
        let proxy = seat.proxy.get_keyboard(qh, seat_name);
        let id = proxy.id();
        self.keyboards.insert(id, Keyboard { proxy, xkb: None });
    }

    fn render_dirty(&mut self, qh: &QueueHandle<Self>) {
        let names = self.outputs.keys().copied().collect::<Vec<_>>();
        for name in names {
            if !self.active() {
                break;
            }
            if let Err(error) = self.render_output(name, qh) {
                self.fail(error);
                break;
            }
        }
        self.maybe_report_ready();
    }

    fn render_output(
        &mut self,
        name: u32,
        qh: &QueueHandle<Self>,
    ) -> Result<(), String> {
        let compositor = &self.compositor;
        let shm = &self.shm;
        let password_len = self.password.chars().count();
        let authenticating = self.auth_running;
        let auth_failed = self.auth_failed;
        let Some(output) = self.outputs.get_mut(&name) else {
            return Ok(());
        };
        if !output.dirty {
            return Ok(());
        }
        let Some((width, height)) = output.size else {
            return Ok(());
        };
        if output.renderer.is_none() {
            output.renderer = Some(
                pollster::block_on(OffscreenRenderer::new(width, height)).map_err(|error| {
                    format!("GPU initialization failed on output {name}: {error}")
                })?,
            );
        }
        if output.buffers.is_empty() {
            let mut buffers = Vec::with_capacity(2);
            for _ in 0..2 {
                let id = output.next_buffer_id;
                output.next_buffer_id = output.next_buffer_id.wrapping_add(1);
                buffers.push(
                    ShmBuffer::new(shm, qh, name, id, width, height).map_err(
                        |error| {
                            format!("shared-memory allocation failed: {error}")
                        },
                    )?,
                );
            }
            output.buffers = buffers;
        }
        let Some(index) = output.buffers.iter().position(|buffer| !buffer.busy)
        else {
            return Ok(());
        };
        let tiles = if self.show_indicator {
            feedback_tiles(
                width,
                height,
                password_len,
                authenticating,
                auth_failed,
            )
        } else {
            Vec::new()
        };
        let rgba = output
            .renderer
            .as_mut()
            .expect("renderer initialized")
            .render(&tiles, self.background_color)
            .map_err(|error| {
                format!("GPU rendering failed on output {name}: {error}")
            })?;
        output.buffers[index]
            .write_rgba(&rgba)
            .map_err(|error| format!("shared-memory write failed: {error}"))?;
        let region = compositor.create_region(qh, ());
        region.add(
            0,
            0,
            i32::try_from(width).map_err(|_| {
                "output width exceeds protocol limits".to_owned()
            })?,
            i32::try_from(height).map_err(|_| {
                "output height exceeds protocol limits".to_owned()
            })?,
        );
        output.surface.set_opaque_region(Some(&region));
        region.destroy();
        output.buffers[index].busy = true;
        output
            .surface
            .attach(Some(&output.buffers[index].buffer), 0, 0);
        output.surface.damage_buffer(
            0,
            0,
            i32::try_from(width).map_err(|_| {
                "output width exceeds protocol limits".to_owned()
            })?,
            i32::try_from(height).map_err(|_| {
                "output height exceeds protocol limits".to_owned()
            })?,
        );
        output.surface.commit();
        output.committed = true;
        output.dirty = false;
        Ok(())
    }

    fn mark_dirty(&mut self, qh: &QueueHandle<Self>) {
        for output in self.outputs.values_mut() {
            if output.size.is_some() {
                output.dirty = true;
            }
        }
        self.render_dirty(qh);
    }

    fn maybe_report_ready(&mut self) {
        if self.ready_reported
            || !self.locked
            || self.outputs.is_empty()
            || !self.outputs.values().all(|output| output.committed)
        {
            return;
        }
        println!(
            "tilefire-lock: ready; opaque lock surfaces committed on {} output(s)",
            self.outputs.len()
        );
        self.ready_reported = true;
    }

    fn process_key(
        &mut self,
        keysym: xkb::Keysym,
        text: String,
        qh: &QueueHandle<Self>,
    ) {
        if !self.active() || !self.locked || self.auth_running {
            return;
        }
        let changed = match keysym.raw() {
            xkb::keysyms::KEY_Return | xkb::keysyms::KEY_KP_Enter => {
                self.start_authentication(qh);
                return;
            }
            xkb::keysyms::KEY_BackSpace => self.password.pop().is_some(),
            xkb::keysyms::KEY_Escape | xkb::keysyms::KEY_Delete => {
                let changed = !self.password.is_empty();
                self.password.zeroize();
                changed
            }
            _ => {
                let previous_len = self.password.len();
                for character in
                    text.chars().filter(|character| !character.is_control())
                {
                    let mut encoded = [0_u8; 4];
                    let encoded = character.encode_utf8(&mut encoded);
                    if self.password.len() + encoded.len() > MAX_PASSWORD_BYTES
                    {
                        break;
                    }
                    self.password.push(character);
                }
                self.password.len() != previous_len
            }
        };
        if changed {
            self.auth_failed = false;
            self.mark_dirty(qh);
        }
    }

    fn start_authentication(&mut self, qh: &QueueHandle<Self>) {
        if !self.active()
            || !self.locked
            || self.auth_running
            || self.password.is_empty()
        {
            return;
        }
        self.auth_running = true;
        self.auth_failed = false;
        self.mark_dirty(qh);
        if !self.active() {
            return;
        }
        let mut password = std::mem::take(&mut self.password);
        let password = Password::new(std::mem::take(&mut *password));
        let authenticator = self.authenticator.clone();
        let username = self.username.clone();
        let sender = self.auth_sender.clone();
        let worker = thread::Builder::new()
            .name("tilefire-pam".to_owned())
            .spawn(move || {
                let outcome = match catch_unwind(AssertUnwindSafe(|| {
                    authenticator.authenticate(&username, password)
                })) {
                    Ok(Ok(())) => AuthOutcome::Authenticated,
                    Ok(Err(_)) => AuthOutcome::Denied,
                    Err(_) => AuthOutcome::WorkerFailed(
                        "authentication worker panicked; session remains locked".to_owned(),
                    ),
                };
                let _ = sender.send(outcome);
            });
        if let Err(error) = worker {
            self.fail(format!(
                "failed to start authentication worker; session remains locked: {error}"
            ));
            return;
        }
        eprintln!("tilefire-lock: authentication started");
    }

    fn handle_auth(&mut self, outcome: AuthOutcome, qh: &QueueHandle<Self>) {
        if !self.auth_running || !self.active() {
            return;
        }
        self.auth_running = false;
        match outcome {
            AuthOutcome::Authenticated => {
                if !self.locked || self.finished {
                    self.fail("authentication completed without an active session lock");
                    return;
                }
                self.password.zeroize();
                self.unlock_requested = true;
                self.lock.unlock_and_destroy();
                eprintln!(
                    "tilefire-lock: PAM authentication and account validation succeeded"
                );
            }
            AuthOutcome::Denied => {
                self.password.zeroize();
                self.auth_failed = true;
                eprintln!(
                    "tilefire-lock: authentication failed; session remains locked"
                );
                self.mark_dirty(qh);
            }
            AuthOutcome::WorkerFailed(error) => self.fail(error),
        }
    }
}

fn feedback_tiles(
    width: u32,
    height: u32,
    password_len: usize,
    authenticating: bool,
    auth_failed: bool,
) -> Vec<RenderTile<'static>> {
    let panel_width = width.min(560);
    let panel_height = height.min(260);
    let panel_x = (width - panel_width) / 2;
    let panel_y = (height - panel_height) / 2;
    let margin = (panel_width / 20).min(panel_height / 8);
    let field_width = panel_width.saturating_sub(margin * 2);
    let field_height = (panel_height / 4).max(1);
    let field_x = panel_x + margin;
    let field_y = panel_y + (panel_height.saturating_sub(field_height)) / 2;
    let mut tiles = vec![
        RenderTile::solid(
            RenderRect {
                x: panel_x as i32,
                y: panel_y as i32,
                width: panel_width,
                height: panel_height,
            },
            [0.055, 0.065, 0.095, 1.0],
        ),
        RenderTile::solid(
            RenderRect {
                x: field_x as i32,
                y: field_y as i32,
                width: field_width,
                height: field_height,
            },
            [0.015, 0.02, 0.035, 1.0],
        ),
    ];
    let count = password_len.min(32) as u32;
    if count > 0 && field_width > count {
        let spacing = field_width / (count + 1);
        let diameter = spacing.min(field_height / 2).min(14);
        if diameter > 0 {
            let color = if authenticating {
                [0.25, 0.55, 0.95, 1.0]
            } else if auth_failed {
                [0.95, 0.25, 0.25, 1.0]
            } else {
                [0.85, 0.88, 0.95, 1.0]
            };
            for index in 0..count {
                tiles.push(RenderTile::solid(
                    RenderRect {
                        x: (field_x
                            + spacing * (index + 1)
                            + (spacing - diameter) / 2)
                            as i32,
                        y: (field_y + (field_height - diameter) / 2) as i32,
                        width: diameter,
                        height: diameter,
                    },
                    color,
                ));
            }
        }
    }
    if auth_failed || authenticating {
        let color = if authenticating {
            [0.25, 0.55, 0.95, 1.0]
        } else {
            [0.95, 0.25, 0.25, 1.0]
        };
        tiles.push(RenderTile::solid(
            RenderRect {
                x: field_x as i32,
                y: (field_y + field_height + margin / 2) as i32,
                width: field_width,
                height: (panel_height / 64).max(1),
            },
            color,
        ));
    }
    tiles
}

fn required_global<T>(
    globals: &GlobalList,
    qh: &QueueHandle<State>,
    versions: std::ops::RangeInclusive<u32>,
    interface: &str,
) -> Result<T, LockError>
where
    T: Proxy + 'static,
    State: Dispatch<T, ()>,
{
    globals.bind(qh, versions, ()).map_err(|source| {
        let reason = match source {
            BindError::NotPresent => "is not advertised",
            BindError::UnsupportedVersion => "has an unsupported version",
        };
        LockError::Message(format!(
            "required Wayland global {interface} {reason}"
        ))
    })
}

fn run(options: Options) -> Result<(), LockError> {
    let username = username::current()?;
    let connection = Connection::connect_to_env()?;
    let (globals, event_queue) = registry_queue_init::<State>(&connection)?;
    let qh = event_queue.handle();
    let compositor = required_global(&globals, &qh, 4..=4, "wl_compositor")?;
    let shm = required_global(&globals, &qh, 1..=1, "wl_shm")?;
    let manager: ExtSessionLockManagerV1 =
        required_global(&globals, &qh, 1..=1, "ext_session_lock_manager_v1")?;
    let lock = manager.lock(&qh, ());
    let initial_globals = globals.contents().clone_list();
    if !initial_globals
        .iter()
        .any(|global| global.interface == "wl_output")
    {
        lock.destroy();
        return Err(LockError::Message(
            "no wl_output globals are available; lock request aborted"
                .to_owned(),
        ));
    }

    let mut event_loop = EventLoop::<State>::try_new()?;
    let signals = Signals::new(&[
        Signal::SIGINT,
        Signal::SIGTERM,
        Signal::SIGQUIT,
        Signal::SIGHUP,
    ])?;
    let (auth_sender, auth_receiver) = channel::channel();
    let mut state = State {
        connection: connection.clone(),
        _globals: globals,
        _manager: manager,
        lock,
        compositor,
        shm,
        outputs: HashMap::new(),
        seats: HashMap::new(),
        keyboards: HashMap::new(),
        xkb_context: xkb::Context::new(xkb::CONTEXT_NO_FLAGS),
        background_color: options.background_color,
        show_indicator: options.show_indicator,
        authenticator: Authenticator::from_env(),
        username,
        auth_sender,
        auth_running: false,
        auth_failed: false,
        password: Zeroizing::new(String::new()),
        locked: false,
        finished: false,
        unlock_requested: false,
        ready_reported: false,
        failure: None,
    };
    for global in initial_globals
        .iter()
        .filter(|global| global.interface == "wl_output")
    {
        state.add_output(global.name, global.version, &qh);
    }
    for global in initial_globals
        .iter()
        .filter(|global| global.interface == "wl_seat")
    {
        state.add_seat(global.name, global.version, &qh);
    }

    event_loop
        .handle()
        .insert_source(signals, |_, _, state| {
            state.fail("termination signal received; session remains locked");
        })
        .map_err(|error| {
            LockError::Message(format!("failed to register signals: {error}"))
        })?;
    event_loop
        .handle()
        .insert_source(auth_receiver, |event, _, state| match event {
            ChannelEvent::Msg(outcome) => state.handle_auth(outcome, &qh),
            ChannelEvent::Closed => {
                state.fail("authentication result channel closed unexpectedly")
            }
        })
        .map_err(|error| {
            LockError::Message(format!(
                "failed to register authentication channel: {error}"
            ))
        })?;
    WaylandSource::new(connection.clone(), event_queue)
        .insert(event_loop.handle())
        .map_err(|error| {
            LockError::Message(format!(
                "failed to register Wayland source: {error}"
            ))
        })?;

    eprintln!("tilefire-lock: session lock requested");
    while state.failure.is_none() && !state.unlock_requested {
        event_loop.dispatch(None, &mut state)?;
    }
    if let Some(failure) = state.failure {
        return Err(LockError::Message(failure));
    }
    state.connection.roundtrip().map_err(|error| {
        LockError::Message(format!("failed to confirm unlock request: {error}"))
    })?;
    println!("tilefire-lock: session unlocked");
    Ok(())
}

fn main() -> ExitCode {
    let options = match Options::parse() {
        Ok(CliAction::Run(options)) => options,
        Ok(CliAction::Help) => {
            print_help();
            return ExitCode::SUCCESS;
        }
        Ok(CliAction::Version) => {
            println!("tilefire-lock {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        Err(error) => {
            eprintln!("tilefire-lock: {error}");
            return ExitCode::FAILURE;
        }
    };
    match run(options) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("tilefire-lock: {error}");
            ExitCode::FAILURE
        }
    }
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        state: &mut Self,
        _proxy: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _data: &GlobalListContents,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if !state.active() {
            return;
        }
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } => match interface.as_str() {
                "wl_output" => state.add_output(name, version, qh),
                "wl_seat" => state.add_seat(name, version, qh),
                _ => {}
            },
            wl_registry::Event::GlobalRemove { name } => {
                if state.outputs.contains_key(&name) {
                    state.remove_output(name);
                } else if state.seats.contains_key(&name) {
                    state.remove_seat(name);
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<ExtSessionLockV1, ()> for State {
    fn event(
        state: &mut Self,
        proxy: &ExtSessionLockV1,
        event: <ExtSessionLockV1 as Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if !state.active() {
            return;
        }
        match event {
            wayland_protocols::ext::session_lock::v1::client::ext_session_lock_v1::Event::Locked => {
                state.locked = true;
                eprintln!("tilefire-lock: compositor reported the session locked");
                state.maybe_report_ready();
            }
            wayland_protocols::ext::session_lock::v1::client::ext_session_lock_v1::Event::Finished => {
                state.finished = true;
                if state.locked {
                    state.fail("compositor finished the session lock; no unlock was requested");
                } else {
                    proxy.destroy();
                    state.fail("compositor denied or finished the lock before it was acquired");
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<ExtSessionLockSurfaceV1, u32> for State {
    fn event(
        state: &mut Self,
        proxy: &ExtSessionLockSurfaceV1,
        event: <ExtSessionLockSurfaceV1 as Proxy>::Event,
        output_name: &u32,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if !state.active() {
            return;
        }
        if let wayland_protocols::ext::session_lock::v1::client::ext_session_lock_surface_v1::Event::Configure {
            serial,
            width,
            height,
        } = event
        {
            proxy.ack_configure(serial);
            let Some(output) = state.outputs.get_mut(output_name) else {
                return;
            };
            if width == 0 || height == 0 {
                output.size = None;
                output.renderer = None;
                output.buffers.clear();
                output.committed = false;
                return;
            }
            if output.size != Some((width, height)) {
                output.size = Some((width, height));
                output.renderer = None;
                output.buffers.clear();
            }
            output.committed = false;
            output.dirty = true;
            if let Err(error) = state.render_output(*output_name, qh) {
                state.fail(error);
                return;
            }
            state.maybe_report_ready();
        }
    }
}

impl Dispatch<wl_seat::WlSeat, u32> for State {
    fn event(
        state: &mut Self,
        _proxy: &wl_seat::WlSeat,
        event: wl_seat::Event,
        seat_name: &u32,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if !state.active() {
            return;
        }
        if let wl_seat::Event::Capabilities { capabilities } = event {
            match capabilities {
                WEnum::Value(capabilities) => {
                    state.set_keyboard(
                        *seat_name,
                        capabilities.contains(wl_seat::Capability::Keyboard),
                        qh,
                    );
                }
                WEnum::Unknown(_) => {
                    state.fail("compositor sent unknown seat capabilities")
                }
            }
        }
    }
}

impl Dispatch<wl_keyboard::WlKeyboard, u32> for State {
    fn event(
        state: &mut Self,
        proxy: &wl_keyboard::WlKeyboard,
        event: wl_keyboard::Event,
        _seat_name: &u32,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if !state.active() {
            return;
        }
        match event {
            wl_keyboard::Event::Keymap { format, fd, size } => {
                if !matches!(
                    format,
                    WEnum::Value(wl_keyboard::KeymapFormat::XkbV1)
                ) || size == 0
                {
                    state.fail("compositor provided an unsupported or empty keyboard keymap");
                    return;
                }
                // SAFETY: The compositor transfers ownership of a valid keymap fd and its exact
                // size; xkbcommon maps it read-only and takes ownership of the descriptor.
                let keymap = match unsafe {
                    xkb::Keymap::new_from_fd(
                        &state.xkb_context,
                        fd,
                        size as usize,
                        xkb::KEYMAP_FORMAT_TEXT_V1,
                        xkb::KEYMAP_COMPILE_NO_FLAGS,
                    )
                } {
                    Ok(Some(keymap)) => keymap,
                    Ok(None) => {
                        state.fail(
                            "failed to compile the compositor keyboard keymap",
                        );
                        return;
                    }
                    Err(error) => {
                        state.fail(format!(
                            "failed to read the keyboard keymap: {error}"
                        ));
                        return;
                    }
                };
                if let Some(keyboard) = state.keyboards.get_mut(&proxy.id()) {
                    keyboard.xkb = Some(xkb::State::new(&keymap));
                }
            }
            wl_keyboard::Event::Modifiers {
                mods_depressed,
                mods_latched,
                mods_locked,
                group,
                ..
            } => {
                if let Some(keyboard) = state.keyboards.get_mut(&proxy.id())
                    && let Some(xkb) = &mut keyboard.xkb
                {
                    xkb.update_mask(
                        mods_depressed,
                        mods_latched,
                        mods_locked,
                        0,
                        0,
                        group,
                    );
                }
            }
            wl_keyboard::Event::Key {
                key,
                state: key_state,
                ..
            } => {
                if !matches!(
                    key_state,
                    WEnum::Value(wl_keyboard::KeyState::Pressed)
                ) {
                    return;
                }
                let Some(keyboard) = state.keyboards.get(&proxy.id()) else {
                    state.fail("key event arrived for an unknown keyboard");
                    return;
                };
                let Some(xkb) = &keyboard.xkb else {
                    state.fail(
                        "key event arrived before a valid keyboard keymap",
                    );
                    return;
                };
                let keycode = xkb::Keycode::new(key.saturating_add(8));
                let keysym = xkb.key_get_one_sym(keycode);
                let text = xkb.key_get_utf8(keycode);
                state.process_key(keysym, text, qh);
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_buffer::WlBuffer, BufferData> for State {
    fn event(
        state: &mut Self,
        _proxy: &wl_buffer::WlBuffer,
        event: wl_buffer::Event,
        data: &BufferData,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if !state.active() || !matches!(event, wl_buffer::Event::Release) {
            return;
        }
        if let Some(output) = state.outputs.get_mut(&data.output)
            && let Some(buffer) = output
                .buffers
                .iter_mut()
                .find(|buffer| buffer.id == data.id)
        {
            buffer.busy = false;
        }
        if let Err(error) = state.render_output(data.output, qh) {
            state.fail(error);
        }
    }
}

macro_rules! noop_dispatch {
    ($($interface:ty),+ $(,)?) => {
        $(
            impl Dispatch<$interface, ()> for State {
                fn event(
                    _state: &mut Self,
                    _proxy: &$interface,
                    _event: <$interface as Proxy>::Event,
                    _data: &(),
                    _conn: &Connection,
                    _qh: &QueueHandle<Self>,
                ) {
                }
            }
        )+
    };
}

noop_dispatch!(
    wl_compositor::WlCompositor,
    wl_output::WlOutput,
    wl_region::WlRegion,
    wl_shm::WlShm,
    wl_shm_pool::WlShmPool,
    wl_surface::WlSurface,
    ExtSessionLockManagerV1,
);

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(arguments: &[&str]) -> CliAction {
        Options::parse_from(arguments.iter().map(std::ffi::OsString::from))
            .unwrap()
    }

    #[test]
    fn parses_swaylock_color_and_indicator_options() {
        let CliAction::Run(options) =
            parse(&["--color=ff8000", "--no-unlock-indicator"])
        else {
            panic!("expected runnable options");
        };
        assert!(!options.show_indicator);
        assert_eq!(options.background_color[0], 1.0);
        assert_eq!(options.background_color[2], 0.0);
        assert_eq!(options.background_color[3], 1.0);
        assert!(
            (options.background_color[1] - srgb_to_linear(128)).abs()
                < f64::EPSILON
        );

        let CliAction::Run(options) = parse(&["-c", "#11223380", "-u"]) else {
            panic!("expected runnable options");
        };
        assert_eq!(options.background_color[0], srgb_to_linear(0x11));
        assert_eq!(options.background_color[1], srgb_to_linear(0x22));
        assert_eq!(options.background_color[2], srgb_to_linear(0x33));
        assert_eq!(options.background_color[3], 128.0 / 255.0);
    }

    #[test]
    fn reads_config_and_applies_cli_overrides() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config");
        fs::write(&path, "# lock theme\ncolor=112233\nno-unlock-indicator\n")
            .unwrap();
        let path = path.to_string_lossy().into_owned();
        let CliAction::Run(options) = parse(&["-C", &path, "--color=ff8000"])
        else {
            panic!("expected runnable options");
        };
        assert!(!options.show_indicator);
        assert_eq!(options.background_color[0], 1.0);
        assert_eq!(options.background_color[1], srgb_to_linear(128));
        assert_eq!(options.background_color[2], 0.0);

        let second = directory.path().join("second");
        fs::write(&second, "color 334455\n").unwrap();
        let second = format!("--config={}", second.to_string_lossy());
        let CliAction::Run(options) = parse(&[&second]) else {
            panic!("expected runnable options");
        };
        assert!(options.show_indicator);
        assert_eq!(options.background_color[0], srgb_to_linear(0x33));
        assert_eq!(options.background_color[1], srgb_to_linear(0x44));
        assert_eq!(options.background_color[2], srgb_to_linear(0x55));
    }

    #[test]
    fn rejects_bad_config_without_blocking_help() {
        let directory = tempfile::tempdir().unwrap();
        let missing = directory
            .path()
            .join("missing")
            .to_string_lossy()
            .into_owned();
        assert!(matches!(
            parse(&["--config", &missing, "--help"]),
            CliAction::Help
        ));
        assert!(
            Options::parse_from([
                std::ffi::OsString::from("--config"),
                std::ffi::OsString::from(&missing),
            ])
            .is_err()
        );

        let path = directory.path().join("bad");
        fs::write(&path, "unknown-option=true\n").unwrap();
        let path = path.to_string_lossy().into_owned();
        assert!(
            Options::parse_from([
                std::ffi::OsString::from("--config"),
                std::ffi::OsString::from(path),
            ])
            .is_err()
        );
    }

    #[test]
    fn handles_informational_and_invalid_options() {
        assert!(matches!(parse(&["--help"]), CliAction::Help));
        assert!(matches!(parse(&["--version"]), CliAction::Version));
        for arguments in [
            &["--unknown"][..],
            &["--color"][..],
            &["--color=12345"][..],
            &["--color=gg0000"][..],
        ] {
            assert!(
                Options::parse_from(
                    arguments.iter().map(std::ffi::OsString::from)
                )
                .is_err()
            );
        }
    }
}
