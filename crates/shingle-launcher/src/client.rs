//! The Wayland side: a full-output overlay layer surface with the
//! keyboard, the pointer, buffers and the event loop
//! (docs/design/launcher.md, sections 2 and 10).

use std::{
  io::{BufRead, BufReader, Write},
  os::unix::net::{UnixListener, UnixStream},
  path::{Path, PathBuf},
  time::{Duration, Instant},
};

use calloop::{
  EventLoop, Interest, LoopHandle, Mode, PostAction, RegistrationToken,
  channel::{self, Event as ChannelEvent},
  generic::Generic,
  signals::{Signal, Signals},
  timer::{TimeoutAction, Timer},
};
use calloop_wayland_source::WaylandSource;
use serde_json::{Value, json};
use shingle_launcher::{
  index::{Index, SearchPaths, save_cache, scan},
  launch::{client_environment, command_line, spawn},
  model::{Button, Effect, Key, Model, Modifiers, Row, Target},
  render::Renderer,
  shm::{ShmBuffer, premultiplied},
  theme::Theme,
  view::{self, Frame, Rect},
};
use wayland_client::{
  Connection, Dispatch, Proxy, QueueHandle, WEnum,
  globals::{GlobalListContents, registry_queue_init},
  protocol::{
    wl_buffer, wl_callback, wl_compositor, wl_keyboard, wl_pointer,
    wl_registry, wl_seat, wl_shm, wl_shm_pool, wl_surface,
  },
};
use wayland_protocols::wp::cursor_shape::v1::client::{
  wp_cursor_shape_device_v1::{self, WpCursorShapeDeviceV1},
  wp_cursor_shape_manager_v1::WpCursorShapeManagerV1,
};
use wayland_protocols_wlr::layer_shell::v1::client::{
  zwlr_layer_shell_v1::{self, ZwlrLayerShellV1},
  zwlr_layer_surface_v1::{self, ZwlrLayerSurfaceV1},
};
use xkbcommon::xkb;

/// evdev codes of the pointer buttons.
const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;
/// Pixels of a wheel step, and how many rows a step scrolls.
const AXIS_STEP: f64 = 15.0;

/// A size or position in logical pixels.
type Point = (f32, f32);

/// How the launcher was started.
pub struct Options {
  /// Print one JSON line per frame and per launch.
  pub trace: bool,
  /// Print how long the first frame took.
  pub timing: bool,
  /// When `main` began.
  pub started: Instant,
  /// Where the index cache is written after a rescan.
  pub index_cache: PathBuf,
  /// What the background rescan reads.
  pub search_paths: SearchPaths,
  /// The terminal for terminal entries.
  pub terminal: Option<String>,
}

/// What ended the run.
#[derive(Debug, PartialEq, Eq)]
enum Exit {
  Closed,
  Launched,
  Failed(String),
}

struct Keyboard {
  proxy: wl_keyboard::WlKeyboard,
  state: Option<xkb::State>,
  rate: i32,
  delay: i32,
  repeat: Option<(u32, RegistrationToken)>,
}

struct Pointer {
  proxy: wl_pointer::WlPointer,
  shape: Option<WpCursorShapeDeviceV1>,
  at: (f64, f64),
  scroll: f64,
}

struct State {
  connection: Connection,
  handle: LoopHandle<'static, State>,
  shm: wl_shm::WlShm,
  _seat: Option<wl_seat::WlSeat>,
  cursor_shapes: Option<WpCursorShapeManagerV1>,
  surface: wl_surface::WlSurface,
  layer: ZwlrLayerSurfaceV1,
  xkb: xkb::Context,
  keyboard: Option<Keyboard>,
  pointer: Option<Pointer>,
  size: Option<(u32, u32)>,
  scale: i32,
  buffers: Vec<ShmBuffer>,
  frame_pending: bool,
  dirty: bool,
  model: Model,
  theme: Theme,
  renderer: Renderer,
  frame: Option<Frame>,
  options: Options,
  first_frame: Option<Duration>,
  rescan_started: bool,
  exit: Option<Exit>,
}

/// Runs the launcher until it launches something or is closed. `model`,
/// `renderer` and `theme` are ready; `connection` is the compositor.
pub fn run(
  connection: Connection,
  model: Model,
  renderer: Renderer,
  theme: Theme,
  options: Options,
  listener: Option<UnixListener>,
) -> Result<(), String> {
  let (globals, queue) = registry_queue_init::<State>(&connection)
    .map_err(|error| format!("Wayland registry: {error}"))?;
  let qh = queue.handle();
  let compositor: wl_compositor::WlCompositor = globals
    .bind(&qh, 4..=6, ())
    .map_err(|error| format!("wl_compositor: {error}"))?;
  let shm: wl_shm::WlShm = globals
    .bind(&qh, 1..=1, ())
    .map_err(|error| format!("wl_shm: {error}"))?;
  let layer_shell: ZwlrLayerShellV1 = globals
    .bind(&qh, 1..=4, ())
    .map_err(|_| "the compositor has no zwlr_layer_shell_v1".to_owned())?;
  let seat: Option<wl_seat::WlSeat> = globals.bind(&qh, 1..=7, ()).ok();
  let cursor_shapes = globals.bind(&qh, 1..=1, ()).ok();
  let surface = compositor.create_surface(&qh, ());
  let layer = layer_shell.get_layer_surface(
    &surface,
    None,
    zwlr_layer_shell_v1::Layer::Overlay,
    "shingle-launcher".to_owned(),
    &qh,
    (),
  );
  use zwlr_layer_surface_v1::Anchor;
  layer.set_anchor(Anchor::Top | Anchor::Bottom | Anchor::Left | Anchor::Right);
  layer.set_exclusive_zone(-1);
  layer.set_size(0, 0);
  layer.set_keyboard_interactivity(
    zwlr_layer_surface_v1::KeyboardInteractivity::Exclusive,
  );
  surface.commit();

  let mut event_loop = EventLoop::<State>::try_new()
    .map_err(|error| format!("event loop: {error}"))?;
  let handle = event_loop.handle();
  WaylandSource::new(connection.clone(), queue)
    .insert(handle.clone())
    .map_err(|error| format!("Wayland source: {error}"))?;
  let signals =
    Signals::new(&[Signal::SIGINT, Signal::SIGTERM, Signal::SIGHUP])
      .map_err(|error| format!("signals: {error}"))?;
  handle
    .insert_source(signals, |_, _, state| {
      state.exit.get_or_insert(Exit::Closed);
    })
    .map_err(|error| format!("signals: {error}"))?;
  if let Some(listener) = listener {
    listener
      .set_nonblocking(true)
      .map_err(|error| format!("instance socket: {error}"))?;
    handle
      .insert_source(
        Generic::new(listener, Interest::READ, Mode::Level),
        |_, listener, state| {
          while let Ok((stream, _)) = listener.accept() {
            if read_command(stream).as_deref() == Some("close") {
              state.trace(json!({"event": "close", "by": "second instance"}));
              state.exit.get_or_insert(Exit::Closed);
            }
          }
          Ok(PostAction::Continue)
        },
      )
      .map_err(|error| format!("instance socket: {error}"))?;
  }

  let mut state = State {
    connection,
    handle,
    shm,
    _seat: seat,
    cursor_shapes,
    surface,
    layer,
    xkb: xkb::Context::new(xkb::CONTEXT_NO_FLAGS),
    keyboard: None,
    pointer: None,
    size: None,
    scale: 1,
    buffers: Vec::new(),
    frame_pending: false,
    dirty: true,
    model,
    theme,
    renderer,
    frame: None,
    options,
    first_frame: None,
    rescan_started: false,
    exit: None,
  };
  while state.exit.is_none() {
    event_loop
      .dispatch(None, &mut state)
      .map_err(|error| format!("event loop: {error}"))?;
    if state.dirty && state.exit.is_none() {
      state.draw(&qh);
    }
  }
  state.layer.destroy();
  state.surface.destroy();
  let _ = state.connection.flush();
  match state.exit {
    Some(Exit::Failed(message)) => Err(message),
    _ => Ok(()),
  }
}

/// The first line a second instance wrote.
fn read_command(stream: UnixStream) -> Option<String> {
  stream
    .set_read_timeout(Some(Duration::from_millis(200)))
    .ok()?;
  let mut line = String::new();
  BufReader::new(stream).read_line(&mut line).ok()?;
  Some(line.trim().to_owned())
}

/// Asks the launcher listening on `path` to close; returns whether one
/// answered.
pub fn close_running(path: &Path) -> bool {
  match UnixStream::connect(path) {
    Ok(mut stream) => stream.write_all(b"close\n").is_ok(),
    Err(_) => false,
  }
}

impl State {
  fn trace(&self, value: Value) {
    if self.options.trace {
      println!("{value}");
    }
  }

  fn fail(&mut self, message: impl Into<String>) {
    self.exit.get_or_insert(Exit::Failed(message.into()));
  }

  /// The output's logical size, the panel's size and its origin on the
  /// output.
  fn geometry(&self) -> Option<(Point, Point, Point)> {
    let (width, height) = self.size?;
    let output = (width as f32, height as f32);
    Some((
      output,
      view::panel_size(&self.theme, output),
      view::panel_origin(&self.theme, output),
    ))
  }

  fn draw(&mut self, qh: &QueueHandle<Self>) {
    if self.frame_pending {
      return;
    }
    let Some((output, panel, origin)) = self.geometry() else {
      return;
    };
    let scale = self.scale.max(1) as u32;
    let width = output.0 as u32 * scale;
    let height = output.1 as u32 * scale;
    if self
      .buffers
      .first()
      .is_some_and(|buffer| (buffer.width, buffer.height) != (width, height))
    {
      self.buffers.clear();
    }
    while self.buffers.len() < 2 {
      let index = self.buffers.len();
      match ShmBuffer::new(&self.shm, qh, width, height, index) {
        Ok(buffer) => self.buffers.push(buffer),
        Err(error) => return self.fail(format!("shared memory: {error}")),
      }
    }
    let Some(index) = self.buffers.iter().position(|buffer| !buffer.busy)
    else {
      return;
    };
    self.model.page = view::page_size(&self.theme, panel);
    let mut frame = {
      let measure = self.renderer.measurer();
      view::build(&self.model, &self.theme, panel, &measure)
    };
    self.renderer.resize(
      panel.0 as u32 * scale,
      panel.1 as u32 * scale,
      scale as f32,
    );
    let buffer = &mut self.buffers[index];
    let fresh = buffer.fresh;
    if fresh {
      buffer.fill(premultiplied(self.theme.backdrop));
      buffer.fresh = false;
    }
    let left = origin.0 as u32 * scale;
    let top = origin.1 as u32 * scale;
    let drawn =
      self
        .renderer
        .draw(&mut frame, self.theme.panel, |row, bytes| {
          buffer.write_row(left, top + row as u32, bytes)
        });
    if let Err(error) = drawn {
      return self.fail(format!("drawing: {error}"));
    }
    let buffer = &mut self.buffers[index];
    buffer.busy = true;
    self.surface.set_buffer_scale(self.scale.max(1));
    self.surface.attach(Some(&buffer.buffer), 0, 0);
    if fresh {
      self
        .surface
        .damage_buffer(0, 0, width as i32, height as i32);
    } else {
      self.surface.damage_buffer(
        left as i32,
        top as i32,
        (panel.0 as u32 * scale) as i32,
        (panel.1 as u32 * scale) as i32,
      );
    }
    self.surface.frame(qh, ());
    self.frame_pending = true;
    self.surface.commit();
    self.dirty = false;
    self.frame = Some(frame);
    if self.first_frame.is_none() {
      let elapsed = self.options.started.elapsed();
      self.first_frame = Some(elapsed);
      if self.options.timing {
        eprintln!(
          "shingle-launcher: first frame after {:.1} ms on {}",
          elapsed.as_secs_f64() * 1000.0,
          self.renderer.adapter()
        );
      }
      self.start_rescan();
    }
    if self.options.trace {
      self.trace(self.describe(origin));
    }
  }

  /// The frame as JSON, in output coordinates, for the harness.
  fn describe(&self, origin: (f32, f32)) -> Value {
    let model = &self.model;
    let shift = |rect: &Rect| {
      json!({
        "x": rect.x + origin.0, "y": rect.y + origin.1,
        "width": rect.width, "height": rect.height,
      })
    };
    let regions = self
      .frame
      .as_ref()
      .map(|frame| frame.regions.as_slice())
      .unwrap_or_default();
    let categories = regions
      .iter()
      .filter_map(|(rect, target)| match target {
        Target::Category(index) => Some(json!({
          "title": model.catalog.categories[*index].title,
          "count": model.catalog.categories[*index].entries.len(),
          "rect": shift(rect),
        })),
        _ => None,
      })
      .collect::<Vec<_>>();
    let all_rows = model.rows();
    let rows = regions
      .iter()
      .filter_map(|(rect, target)| match target {
        Target::Row(row) => Some(match all_rows[*row] {
          Row::Entry(position) => json!({
            "id": model.catalog.entries[position].id,
            "name": model.catalog.names[position],
            "favourite": model.catalog.is_favourite(position),
            "selected": *row == model.selected,
            "rect": shift(rect),
          }),
          Row::Run => json!({
            "id": null, "name": format!("Run: {}", model.query.trim()),
            "selected": *row == model.selected, "rect": shift(rect),
          }),
        }),
        _ => None,
      })
      .collect::<Vec<_>>();
    let menu = model.menu.as_ref().map(|menu| {
      regions
        .iter()
        .filter_map(|(rect, target)| match target {
          Target::MenuItem(item) => Some(json!({
            "label": menu.items[*item].1, "rect": shift(rect),
          })),
          _ => None,
        })
        .collect::<Vec<_>>()
    });
    let panel = self.geometry().map(|(_, size, _)| size).unwrap_or_default();
    json!({
      "event": "frame",
      "ms": self.options.started.elapsed().as_secs_f64() * 1000.0,
      "query": model.query,
      "prompt": model.prompt.as_ref().map(|prompt| json!({
        "label": prompt.kind.label(), "text": prompt.text,
      })),
      "category": model.catalog.categories[model.category].title,
      "categories": categories,
      "rows": rows,
      "menu": menu,
      "drag": model.drag.as_ref().map(|drag| drag.name.clone()),
      "status": model.status,
      "panel": {
        "x": origin.0, "y": origin.1, "width": panel.0, "height": panel.1,
      },
    })
  }

  /// Rescans the index in a thread after the first frame; a different
  /// result replaces the shown one and the cache.
  fn start_rescan(&mut self) {
    if self.rescan_started {
      return;
    }
    self.rescan_started = true;
    let (sender, receiver) = channel::channel::<Index>();
    let inserted = self.handle.insert_source(receiver, |event, _, state| {
      if let ChannelEvent::Msg(index) = event {
        state.trace(json!({"event": "index", "entries": index.entries.len()}));
        state.model.set_index(index);
        state.dirty = true;
      }
    });
    if inserted.is_err() {
      return;
    }
    let paths = self.options.search_paths.clone();
    let cache = self.options.index_cache.clone();
    let current = self.model.index().clone();
    let spawned =
      std::thread::Builder::new()
        .name("index".to_owned())
        .spawn(move || {
          let index = scan(&paths);
          if index != current {
            if let Err(error) = save_cache(&cache, &index) {
              eprintln!(
                "shingle-launcher: cannot write {}: {error}",
                cache.display()
              );
            }
            let _ = sender.send(index);
          }
        });
    if let Err(error) = spawned {
      eprintln!("shingle-launcher: cannot rescan: {error}");
    }
  }

  fn apply(&mut self, effect: Effect) {
    match effect {
      Effect::None => {}
      Effect::Redraw => self.dirty = true,
      Effect::Close => {
        self.trace(json!({"event": "close"}));
        self.exit.get_or_insert(Exit::Closed);
      }
      Effect::Launch(launch) => {
        let Some(line) =
          command_line(&launch, self.options.terminal.as_deref())
        else {
          self.model.status = "No terminal found for this entry".to_owned();
          self.dirty = true;
          return;
        };
        let environment = client_environment(|name| std::env::var_os(name));
        match spawn(&line, &environment) {
          Ok(pid) => {
            self.trace(json!({
              "event": "launch", "id": launch.id, "command": line, "pid": pid,
            }));
            self.exit.get_or_insert(Exit::Launched);
          }
          Err(error) => {
            self.model.status = format!("Cannot run {line}: {error}");
            self.dirty = true;
          }
        }
      }
    }
  }

  /// The pointer's position in panel coordinates and what it is over.
  fn pointer_target(&self, at: (f64, f64)) -> ((f32, f32), Target) {
    let Some((_, panel, origin)) = self.geometry() else {
      return ((0.0, 0.0), Target::Backdrop);
    };
    let point = (at.0 as f32 - origin.0, at.1 as f32 - origin.1);
    let target = self
      .frame
      .as_ref()
      .map(|frame| frame.hit(point, panel))
      .unwrap_or(Target::Backdrop);
    (point, target)
  }

  fn key(&mut self, code: u32, pressed: bool) {
    let Some(keyboard) = &self.keyboard else {
      return;
    };
    let Some(xkb_state) = &keyboard.state else {
      return;
    };
    let keycode = xkb::Keycode::new(code + 8);
    if !pressed {
      if let Some(keyboard) = &mut self.keyboard
        && keyboard.repeat.is_some_and(|(held, _)| held == code)
        && let Some((_, token)) = keyboard.repeat.take()
      {
        self.handle.remove(token);
      }
      return;
    }
    let modifiers = Modifiers {
      ctrl: xkb_state
        .mod_name_is_active(xkb::MOD_NAME_CTRL, xkb::STATE_MODS_EFFECTIVE),
      shift: xkb_state
        .mod_name_is_active(xkb::MOD_NAME_SHIFT, xkb::STATE_MODS_EFFECTIVE),
      alt: xkb_state
        .mod_name_is_active(xkb::MOD_NAME_ALT, xkb::STATE_MODS_EFFECTIVE),
    };
    let keysym = xkb_state.key_get_one_sym(keycode);
    let text = xkb_state.key_get_utf8(keycode);
    let repeats = xkb_state.get_keymap().key_repeats(keycode);
    if let Some(key) = translate(keysym, &text, modifiers) {
      let effect = self.model.key(key, modifiers);
      self.apply(effect);
    }
    if repeats {
      self.start_repeat(code);
    }
  }

  fn start_repeat(&mut self, code: u32) {
    let Some(keyboard) = &mut self.keyboard else {
      return;
    };
    if let Some((_, token)) = keyboard.repeat.take() {
      self.handle.remove(token);
    }
    if keyboard.rate <= 0 {
      return;
    }
    let delay = Duration::from_millis(keyboard.delay.max(0) as u64);
    let interval = Duration::from_millis((1000 / keyboard.rate.max(1)) as u64);
    let token = self.handle.insert_source(
      Timer::from_duration(delay),
      move |_, _, state| {
        state.repeat_key(code);
        TimeoutAction::ToDuration(interval)
      },
    );
    if let (Ok(token), Some(keyboard)) = (token, &mut self.keyboard) {
      keyboard.repeat = Some((code, token));
    }
  }

  fn repeat_key(&mut self, code: u32) {
    let Some(xkb_state) = self.keyboard.as_ref().and_then(|k| k.state.as_ref())
    else {
      return;
    };
    let keycode = xkb::Keycode::new(code + 8);
    let modifiers = Modifiers {
      ctrl: xkb_state
        .mod_name_is_active(xkb::MOD_NAME_CTRL, xkb::STATE_MODS_EFFECTIVE),
      shift: xkb_state
        .mod_name_is_active(xkb::MOD_NAME_SHIFT, xkb::STATE_MODS_EFFECTIVE),
      alt: xkb_state
        .mod_name_is_active(xkb::MOD_NAME_ALT, xkb::STATE_MODS_EFFECTIVE),
    };
    let keysym = xkb_state.key_get_one_sym(keycode);
    let text = xkb_state.key_get_utf8(keycode);
    if let Some(key) = translate(keysym, &text, modifiers) {
      let effect = self.model.key(key, modifiers);
      self.apply(effect);
    }
  }
}

/// The launcher's key for `keysym` with `text` under `modifiers`.
fn translate(
  keysym: xkb::Keysym,
  text: &str,
  modifiers: Modifiers,
) -> Option<Key> {
  use xkb::keysyms;
  Some(match keysym.raw() {
    keysyms::KEY_Return | keysyms::KEY_KP_Enter => Key::Enter,
    keysyms::KEY_Escape => Key::Escape,
    keysyms::KEY_BackSpace => Key::Backspace,
    keysyms::KEY_Delete | keysyms::KEY_KP_Delete => Key::Delete,
    keysyms::KEY_Up | keysyms::KEY_KP_Up => Key::Up,
    keysyms::KEY_Down | keysyms::KEY_KP_Down => Key::Down,
    keysyms::KEY_Left | keysyms::KEY_KP_Left => Key::Left,
    keysyms::KEY_Right | keysyms::KEY_KP_Right => Key::Right,
    keysyms::KEY_Page_Up | keysyms::KEY_KP_Page_Up => Key::PageUp,
    keysyms::KEY_Page_Down | keysyms::KEY_KP_Page_Down => Key::PageDown,
    keysyms::KEY_Home | keysyms::KEY_KP_Home => Key::Home,
    keysyms::KEY_End | keysyms::KEY_KP_End => Key::End,
    keysyms::KEY_Tab | keysyms::KEY_ISO_Left_Tab => Key::Tab,
    keysyms::KEY_F2 => Key::F2,
    _ => {
      if modifiers.ctrl || modifiers.alt {
        let character = xkb::keysym_to_utf32(keysym);
        let character =
          char::from_u32(character).filter(|c| !c.is_control())?;
        return Some(Key::Char(character.to_lowercase().next()?));
      }
      let text = text
        .chars()
        .filter(|character| !character.is_control())
        .collect::<String>();
      if text.is_empty() {
        return None;
      }
      Key::Text(text)
    }
  })
}

impl Dispatch<ZwlrLayerSurfaceV1, ()> for State {
  fn event(
    state: &mut Self,
    proxy: &ZwlrLayerSurfaceV1,
    event: zwlr_layer_surface_v1::Event,
    _: &(),
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
    match event {
      zwlr_layer_surface_v1::Event::Configure {
        serial,
        width,
        height,
      } => {
        proxy.ack_configure(serial);
        state.trace(json!({
          "event": "configure", "width": width, "height": height,
        }));
        // A compositor may configure before it knows the output; the
        // surface is drawn once a configure has a size.
        if width > 0 && height > 0 {
          state.size = Some((width, height));
          state.dirty = true;
        }
      }
      zwlr_layer_surface_v1::Event::Closed => {
        state.exit.get_or_insert(Exit::Closed);
      }
      _ => {}
    }
  }
}

impl Dispatch<wl_surface::WlSurface, ()> for State {
  fn event(
    state: &mut Self,
    _: &wl_surface::WlSurface,
    event: wl_surface::Event,
    _: &(),
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
    if let wl_surface::Event::PreferredBufferScale { factor } = event
      && factor != state.scale
      && factor > 0
    {
      state.scale = factor;
      state.buffers.clear();
      state.dirty = true;
    }
  }
}

impl Dispatch<wl_callback::WlCallback, ()> for State {
  fn event(
    state: &mut Self,
    _: &wl_callback::WlCallback,
    event: wl_callback::Event,
    _: &(),
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
    if let wl_callback::Event::Done { .. } = event {
      state.frame_pending = false;
    }
  }
}

impl Dispatch<wl_buffer::WlBuffer, usize> for State {
  fn event(
    state: &mut Self,
    _: &wl_buffer::WlBuffer,
    event: wl_buffer::Event,
    index: &usize,
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
    if let wl_buffer::Event::Release = event
      && let Some(buffer) = state.buffers.get_mut(*index)
    {
      buffer.busy = false;
    }
  }
}

impl Dispatch<wl_seat::WlSeat, ()> for State {
  fn event(
    state: &mut Self,
    seat: &wl_seat::WlSeat,
    event: wl_seat::Event,
    _: &(),
    _: &Connection,
    qh: &QueueHandle<Self>,
  ) {
    let wl_seat::Event::Capabilities {
      capabilities: WEnum::Value(capabilities),
    } = event
    else {
      return;
    };
    let keyboard = capabilities.contains(wl_seat::Capability::Keyboard);
    if keyboard && state.keyboard.is_none() {
      state.keyboard = Some(Keyboard {
        proxy: seat.get_keyboard(qh, ()),
        state: None,
        rate: 25,
        delay: 600,
        repeat: None,
      });
    } else if !keyboard
      && let Some(old) = state.keyboard.take()
      && old.proxy.version() >= 3
    {
      old.proxy.release();
    }
    let pointer = capabilities.contains(wl_seat::Capability::Pointer);
    if pointer && state.pointer.is_none() {
      let proxy = seat.get_pointer(qh, ());
      let shape = state
        .cursor_shapes
        .as_ref()
        .map(|manager| manager.get_pointer(&proxy, qh, ()));
      state.pointer = Some(Pointer {
        proxy,
        shape,
        at: (0.0, 0.0),
        scroll: 0.0,
      });
    } else if !pointer && let Some(old) = state.pointer.take() {
      if let Some(shape) = old.shape {
        shape.destroy();
      }
      if old.proxy.version() >= 3 {
        old.proxy.release();
      }
    }
  }
}

impl Dispatch<wl_keyboard::WlKeyboard, ()> for State {
  fn event(
    state: &mut Self,
    _: &wl_keyboard::WlKeyboard,
    event: wl_keyboard::Event,
    _: &(),
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
    match event {
      wl_keyboard::Event::Keymap { format, fd, size } => {
        if !matches!(format, WEnum::Value(wl_keyboard::KeymapFormat::XkbV1)) {
          return;
        }
        // SAFETY: the compositor hands over a keymap fd of `size` bytes;
        // xkbcommon maps it read-only.
        let keymap = unsafe {
          xkb::Keymap::new_from_fd(
            &state.xkb,
            fd,
            size as usize,
            xkb::KEYMAP_FORMAT_TEXT_V1,
            xkb::KEYMAP_COMPILE_NO_FLAGS,
          )
        };
        match keymap {
          Ok(Some(keymap)) => {
            if let Some(keyboard) = &mut state.keyboard {
              keyboard.state = Some(xkb::State::new(&keymap));
            }
          }
          _ => state.fail("cannot compile the compositor's keymap"),
        }
      }
      wl_keyboard::Event::Modifiers {
        mods_depressed,
        mods_latched,
        mods_locked,
        group,
        ..
      } => {
        if let Some(xkb_state) =
          state.keyboard.as_mut().and_then(|k| k.state.as_mut())
        {
          xkb_state.update_mask(
            mods_depressed,
            mods_latched,
            mods_locked,
            0,
            0,
            group,
          );
        }
      }
      wl_keyboard::Event::RepeatInfo { rate, delay } => {
        if let Some(keyboard) = &mut state.keyboard {
          keyboard.rate = rate;
          keyboard.delay = delay;
        }
      }
      wl_keyboard::Event::Key {
        key,
        state: WEnum::Value(key_state),
        ..
      } => {
        state.key(key, key_state == wl_keyboard::KeyState::Pressed);
      }
      wl_keyboard::Event::Leave { .. } => {
        if let Some((_, token)) =
          state.keyboard.as_mut().and_then(|k| k.repeat.take())
        {
          state.handle.remove(token);
        }
      }
      _ => {}
    }
  }
}

impl Dispatch<wl_pointer::WlPointer, ()> for State {
  fn event(
    state: &mut Self,
    _: &wl_pointer::WlPointer,
    event: wl_pointer::Event,
    _: &(),
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
    match event {
      wl_pointer::Event::Enter {
        serial,
        surface_x,
        surface_y,
        ..
      } => {
        if let Some(pointer) = &mut state.pointer {
          pointer.at = (surface_x, surface_y);
          if let Some(shape) = &pointer.shape {
            shape.set_shape(serial, wp_cursor_shape_device_v1::Shape::Default);
          }
        }
        state.pointer_moved();
      }
      wl_pointer::Event::Motion {
        surface_x,
        surface_y,
        ..
      } => {
        if let Some(pointer) = &mut state.pointer {
          pointer.at = (surface_x, surface_y);
        }
        state.pointer_moved();
      }
      wl_pointer::Event::Button {
        button,
        state: WEnum::Value(button_state),
        ..
      } => {
        let Some(at) = state.pointer.as_ref().map(|pointer| pointer.at) else {
          return;
        };
        let (point, target) = state.pointer_target(at);
        let button = match button {
          BTN_LEFT => Button::Left,
          BTN_RIGHT => Button::Right,
          _ => Button::Other,
        };
        let pressed = button_state == wl_pointer::ButtonState::Pressed;
        let effect = state.model.pointer_button(button, pressed, point, target);
        state.apply(effect);
      }
      wl_pointer::Event::Axis {
        axis: WEnum::Value(wl_pointer::Axis::VerticalScroll),
        value,
        ..
      } => {
        let Some(pointer) = &mut state.pointer else {
          return;
        };
        pointer.scroll += value;
        let rows = (pointer.scroll / AXIS_STEP).trunc();
        if rows != 0.0 {
          pointer.scroll -= rows * AXIS_STEP;
          let effect = state.model.scroll_by(rows as isize);
          state.apply(effect);
        }
      }
      _ => {}
    }
  }
}

impl State {
  fn pointer_moved(&mut self) {
    let Some(at) = self.pointer.as_ref().map(|pointer| pointer.at) else {
      return;
    };
    let (point, target) = self.pointer_target(at);
    let effect = self.model.pointer_motion(point, target);
    self.apply(effect);
  }
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
  fn event(
    _: &mut Self,
    _: &wl_registry::WlRegistry,
    _: wl_registry::Event,
    _: &GlobalListContents,
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
  }
}

macro_rules! ignore_events {
  ($($interface:ty),+ $(,)?) => {
    $(
      impl Dispatch<$interface, ()> for State {
        fn event(
          _: &mut Self,
          _: &$interface,
          _: <$interface as Proxy>::Event,
          _: &(),
          _: &Connection,
          _: &QueueHandle<Self>,
        ) {
        }
      }
    )+
  };
}

ignore_events!(
  wl_compositor::WlCompositor,
  wl_shm::WlShm,
  wl_shm_pool::WlShmPool,
  ZwlrLayerShellV1,
  WpCursorShapeManagerV1,
  WpCursorShapeDeviceV1,
);

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn keys_translate_with_and_without_ctrl() {
    let plain = Modifiers::NONE;
    let ctrl = Modifiers::CTRL;
    let sym = |raw| xkb::Keysym::new(raw);
    assert_eq!(
      translate(sym(xkb::keysyms::KEY_a), "a", plain),
      Some(Key::Text("a".into()))
    );
    assert_eq!(
      translate(sym(xkb::keysyms::KEY_D), "\u{4}", ctrl),
      Some(Key::Char('d'))
    );
    assert_eq!(
      translate(sym(xkb::keysyms::KEY_Return), "\r", plain),
      Some(Key::Enter)
    );
    assert_eq!(
      translate(sym(xkb::keysyms::KEY_ISO_Left_Tab), "", plain),
      Some(Key::Tab)
    );
    assert_eq!(translate(sym(xkb::keysyms::KEY_Shift_L), "", plain), None);
  }
}
