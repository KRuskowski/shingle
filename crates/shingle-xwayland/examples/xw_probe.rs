//! An X11 probe client for Shingle's XWayland tests: the X counterpart of
//! the in-process Wayland test client (docs/design/xwayland.md, section 7).
//!
//! It creates one window with the class, title, type, parent, size hints,
//! input model, override-redirect flag and initial `_NET_WM_STATE` given
//! on the command line, maps it, and then follows commands on stdin, one
//! per line, while it reports what happens to the window as JSON lines on
//! stdout.
//!
//! It runs only inside tools/xwayland_test/run.py's namespace, which sets
//! `SHINGLE_XWAYLAND_TEST_NAMESPACE=1`, so that it never opens a window
//! on the host's X displays.
//!
//! ```text
//! xw_probe [--class INSTANCE,CLASS] [--title TITLE]
//!     [--type normal|dialog|utility|menu|splash|dnd] [--transient-for ID]
//!     [--size WxH] [--position X,Y] [--min WxH] [--max WxH]
//!     [--no-input] [--take-focus] [--no-delete] [--override-redirect]
//!     [--state fullscreen,maximized,hidden] [--color RRGGBB]
//!     [--no-background] [--no-map] [--no-decorations]
//!     [--xdnd-source TEXT [--drag-icon]] [--xdnd-target]
//! ```
//!
//! `--no-decorations` sets `_MOTIF_WM_HINTS` to ask for no decorations,
//! as a client that draws its own frame does.
//!
//! `--xdnd-source TEXT` makes the window an Xdnd version 5 drag source
//! (XW-6): a press of the first button and a motion of more than three
//! pixels take `XdndSelection`, and from then on every motion looks for
//! the `XdndAware` child of the root under the pointer and sends it
//! `XdndEnter` (with `UTF8_STRING`), `XdndPosition` and `XdndLeave`; the
//! release sends `XdndDrop` to a window that accepted, or `XdndLeave`.
//! Conversions of `XdndSelection` to `UTF8_STRING` or `TEXT` get TEXT.
//! With `--drag-icon` an override-redirect window of type
//! `_NET_WM_WINDOW_TYPE_DND` follows the pointer during the drag, 8
//! pixels below and to the right of it. `--xdnd-target` makes the window
//! `XdndAware`: it accepts drags that offer `UTF8_STRING` with
//! `XdndActionCopy`, converts `XdndSelection` on the drop and answers
//! with `XdndFinished`.
//!
//! Commands: `map`, `unmap`, `destroy`, `exit`, `title TEXT`,
//! `configure X Y W H`, `state add|remove|toggle NAME`, `activate`,
//! `iconify`, `moveresize CODE`, `close`, `geometry`, `focus` (reports
//! the X input focus), `pointer` (reports where the server has the
//! pointer, and the root child under it), `grab_focus` (sets the X focus
//! to the window itself, as a client stealing it would), `cursor`
//! (reports the cursor image the server shows, through XFixes),
//! `echo TEXT`.
//!
//! Events: `window`, `mapped`, `unmapped`, `configure`, `focus_in`,
//! `focus_out`, `delete`, `take_focus`, `net_wm_state`, `wm_state`,
//! `geometry`, `focus`, `pointer`, `cursor`, `echo`, `destroyed` and
//! `error`; as a drag source `drag_started`, `drag_target` (window),
//! `drag_status` (accepted, action), `drag_dropped`, `drag_cancelled`,
//! `drag_send` (target) and `drag_finished` (performed, action); as a
//! drop target `drag_enter` (source, types), `drag_position` (x, y),
//! `drag_leave`, `drag_drop` and `dropped` (text).

use std::{
  cell::RefCell,
  io::{BufRead, Write},
  sync::mpsc,
  time::Duration,
};

use x11rb::{
  CURRENT_TIME, NONE,
  connection::Connection,
  protocol::{
    Event,
    xfixes::ConnectionExt as _,
    xproto::{
      AtomEnum, ClientMessageEvent, ConfigureWindowAux, ConnectionExt,
      CreateWindowAux, EventMask, InputFocus, PropMode, SELECTION_NOTIFY_EVENT,
      SelectionNotifyEvent, SelectionRequestEvent, Window, WindowClass,
    },
  },
  rust_connection::RustConnection,
  wrapper::ConnectionExt as _,
};

/// The variable the test harness sets inside its namespace.
const NAMESPACE_VARIABLE: &str = "SHINGLE_XWAYLAND_TEST_NAMESPACE";

/// The window to create.
#[derive(Debug)]
struct Options {
  instance: String,
  class: String,
  title: String,
  window_type: Option<String>,
  transient_for: Option<u32>,
  size: (u16, u16),
  position: Option<(i16, i16)>,
  min: Option<(u32, u32)>,
  max: Option<(u32, u32)>,
  input: bool,
  take_focus: bool,
  delete: bool,
  override_redirect: bool,
  states: Vec<String>,
  /// The background as `0xRRGGBB` on the default TrueColor visual; white
  /// without it.
  color: Option<u32>,
  background: bool,
  map: bool,
  /// Asks for no decorations through `_MOTIF_WM_HINTS`.
  no_decorations: bool,
  /// Makes the window an Xdnd source of this text.
  xdnd_source: Option<String>,
  /// Shows a drag icon during the drag.
  drag_icon: bool,
  /// Makes the window an Xdnd target.
  xdnd_target: bool,
}

impl Default for Options {
  fn default() -> Self {
    Self {
      instance: "xw_probe".into(),
      class: "XwProbe".into(),
      title: "xw_probe".into(),
      window_type: None,
      transient_for: None,
      size: (320, 240),
      position: None,
      min: None,
      max: None,
      input: true,
      take_focus: false,
      delete: true,
      override_redirect: false,
      states: Vec::new(),
      color: None,
      background: true,
      map: true,
      no_decorations: false,
      xdnd_source: None,
      drag_icon: false,
      xdnd_target: false,
    }
  }
}

/// Parses `WxH` or `X,Y`.
fn pair<T: std::str::FromStr>(text: &str, separator: char) -> Option<(T, T)> {
  let (first, second) = text.split_once(separator)?;
  Some((first.parse().ok()?, second.parse().ok()?))
}

/// Parses the command line.
fn parse(arguments: &[String]) -> Result<Options, String> {
  let mut options = Options::default();
  let mut arguments = arguments.iter();
  while let Some(argument) = arguments.next() {
    let mut value = || {
      arguments
        .next()
        .cloned()
        .ok_or_else(|| format!("{argument} needs a value"))
    };
    let bad = |what: &str| format!("bad {what} for {argument}");
    match argument.as_str() {
      "--class" => {
        let value = value()?;
        let (instance, class) =
          value.split_once(',').ok_or_else(|| bad("INSTANCE,CLASS"))?;
        options.instance = instance.into();
        options.class = class.into();
      }
      "--title" => options.title = value()?,
      "--type" => options.window_type = Some(value()?),
      "--transient-for" => {
        let value = value()?;
        let id = value.trim_start_matches("0x");
        options.transient_for = Some(
          u32::from_str_radix(
            id,
            if value.starts_with("0x") { 16 } else { 10 },
          )
          .map_err(|_| bad("window id"))?,
        );
      }
      "--size" => options.size = pair(&value()?, 'x').ok_or(bad("WxH"))?,
      "--position" => {
        options.position = Some(pair(&value()?, ',').ok_or(bad("X,Y"))?);
      }
      "--min" => options.min = Some(pair(&value()?, 'x').ok_or(bad("WxH"))?),
      "--max" => options.max = Some(pair(&value()?, 'x').ok_or(bad("WxH"))?),
      "--no-input" => options.input = false,
      "--take-focus" => options.take_focus = true,
      "--no-delete" => options.delete = false,
      "--override-redirect" => options.override_redirect = true,
      "--state" => {
        options.states =
          value()?.split(',').map(str::to_owned).collect::<Vec<_>>();
      }
      "--color" => {
        options.color =
          Some(u32::from_str_radix(&value()?, 16).map_err(|_| bad("RRGGBB"))?);
      }
      "--no-background" => options.background = false,
      "--no-map" => options.map = false,
      "--no-decorations" => options.no_decorations = true,
      "--xdnd-source" => options.xdnd_source = Some(value()?),
      "--drag-icon" => options.drag_icon = true,
      "--xdnd-target" => options.xdnd_target = true,
      _ => return Err(format!("unknown option {argument}")),
    }
  }
  Ok(options)
}

/// Quotes `text` as a JSON string.
fn quote(text: &str) -> String {
  let mut quoted = String::from("\"");
  for character in text.chars() {
    match character {
      '"' => quoted.push_str("\\\""),
      '\\' => quoted.push_str("\\\\"),
      character if (character as u32) < 0x20 => {
        quoted.push_str(&format!("\\u{:04x}", character as u32));
      }
      character => quoted.push(character),
    }
  }
  quoted.push('"');
  quoted
}

/// Prints one JSON line: `{"event": NAME, FIELDS}`.
fn report(name: &str, fields: &[(&str, String)]) {
  let mut line = format!("{{\"event\": {}", quote(name));
  for (key, value) in fields {
    line.push_str(&format!(", {}: {value}", quote(key)));
  }
  line.push('}');
  let mut stdout = std::io::stdout().lock();
  let _ = writeln!(stdout, "{line}");
  let _ = stdout.flush();
}

/// Interns `name`.
fn atom(connection: &RustConnection, name: &str) -> Result<u32, String> {
  connection
    .intern_atom(false, name.as_bytes())
    .map_err(|error| error.to_string())?
    .reply()
    .map(|reply| reply.atom)
    .map_err(|error| error.to_string())
}

/// The Xdnd version the probe speaks.
const XDND_VERSION: u32 = 5;

/// How far the pointer moves with the button held before a drag starts.
const DRAG_THRESHOLD: i32 = 3;

/// The atoms of Xdnd and the conversions it needs.
struct XdndAtoms {
  selection: u32,
  aware: u32,
  enter: u32,
  position: u32,
  status: u32,
  leave: u32,
  drop: u32,
  finished: u32,
  copy: u32,
  utf8: u32,
  text: u32,
  targets: u32,
  property: u32,
}

impl XdndAtoms {
  fn new(connection: &RustConnection) -> Result<Self, String> {
    Ok(Self {
      selection: atom(connection, "XdndSelection")?,
      aware: atom(connection, "XdndAware")?,
      enter: atom(connection, "XdndEnter")?,
      position: atom(connection, "XdndPosition")?,
      status: atom(connection, "XdndStatus")?,
      leave: atom(connection, "XdndLeave")?,
      drop: atom(connection, "XdndDrop")?,
      finished: atom(connection, "XdndFinished")?,
      copy: atom(connection, "XdndActionCopy")?,
      utf8: atom(connection, "UTF8_STRING")?,
      text: atom(connection, "TEXT")?,
      targets: atom(connection, "TARGETS")?,
      property: atom(connection, "XW_PROBE_DROP")?,
    })
  }
}

/// Where a drag the probe makes or receives is.
#[derive(Default)]
struct DragState {
  /// Where the first button went down, while it is held.
  pressed: Option<(i32, i32)>,
  /// A drag from the probe is running.
  dragging: bool,
  /// The `XdndAware` window under the pointer.
  target: Option<Window>,
  /// Whether it accepted the drag in its last `XdndStatus`.
  accepted: bool,
  /// The drag icon, once created.
  icon: Option<Window>,
  /// As a target: the source of the drag over the probe.
  source: Option<Window>,
}

/// The probe: its connection, window and atoms.
struct Probe {
  connection: RustConnection,
  root: Window,
  window: Window,
  wm_protocols: u32,
  wm_delete_window: u32,
  wm_take_focus: u32,
  net_wm_state: u32,
  wm_state: u32,
  xdnd: XdndAtoms,
  /// The text to drag, for an Xdnd source.
  drag_text: Option<String>,
  drag_icon: bool,
  drag: RefCell<DragState>,
}

impl Probe {
  /// Creates the window `options` describe, without mapping it.
  fn create(options: &Options) -> Result<Self, String> {
    let (connection, screen) =
      x11rb::connect(None).map_err(|error| error.to_string())?;
    let screen = &connection.setup().roots[screen];
    let root = screen.root;
    let white = screen.white_pixel;
    let window = connection.generate_id().map_err(|e| e.to_string())?;
    let mut mask = EventMask::STRUCTURE_NOTIFY
      | EventMask::FOCUS_CHANGE
      | EventMask::PROPERTY_CHANGE
      | EventMask::EXPOSURE;
    if options.xdnd_source.is_some() {
      mask |= EventMask::BUTTON_PRESS
        | EventMask::BUTTON_RELEASE
        | EventMask::BUTTON_MOTION;
    }
    let mut aux = CreateWindowAux::new()
      .event_mask(mask)
      .override_redirect(u32::from(options.override_redirect));
    if options.background {
      aux = aux.background_pixel(options.color.unwrap_or(white));
    }
    let (x, y) = options.position.unwrap_or((0, 0));
    connection
      .create_window(
        x11rb::COPY_DEPTH_FROM_PARENT,
        window,
        root,
        x,
        y,
        options.size.0,
        options.size.1,
        0,
        WindowClass::INPUT_OUTPUT,
        x11rb::COPY_FROM_PARENT,
        &aux,
      )
      .map_err(|error| error.to_string())?;
    let probe = Self {
      wm_protocols: atom(&connection, "WM_PROTOCOLS")?,
      wm_delete_window: atom(&connection, "WM_DELETE_WINDOW")?,
      wm_take_focus: atom(&connection, "WM_TAKE_FOCUS")?,
      net_wm_state: atom(&connection, "_NET_WM_STATE")?,
      wm_state: atom(&connection, "WM_STATE")?,
      xdnd: XdndAtoms::new(&connection)?,
      drag_text: options.xdnd_source.clone(),
      drag_icon: options.drag_icon,
      drag: RefCell::default(),
      connection,
      root,
      window,
    };
    probe.set_properties(options)?;
    if options.xdnd_target {
      probe
        .connection
        .change_property32(
          PropMode::REPLACE,
          window,
          probe.xdnd.aware,
          AtomEnum::ATOM,
          &[XDND_VERSION],
        )
        .map_err(|error| error.to_string())?;
    }
    Ok(probe)
  }

  /// Sets the ICCCM and EWMH properties `options` ask for.
  fn set_properties(&self, options: &Options) -> Result<(), String> {
    let connection = &self.connection;
    let window = self.window;
    let error = |error: x11rb::errors::ConnectionError| error.to_string();
    let class = format!("{}\0{}\0", options.instance, options.class);
    connection
      .change_property8(
        PropMode::REPLACE,
        window,
        AtomEnum::WM_CLASS,
        AtomEnum::STRING,
        class.as_bytes(),
      )
      .map_err(error)?;
    self.set_title(&options.title)?;
    let mut protocols = Vec::new();
    if options.delete {
      protocols.push(self.wm_delete_window);
    }
    if options.take_focus {
      protocols.push(self.wm_take_focus);
    }
    connection
      .change_property32(
        PropMode::REPLACE,
        window,
        self.wm_protocols,
        AtomEnum::ATOM,
        &protocols,
      )
      .map_err(error)?;
    // WM_HINTS with InputHint.
    connection
      .change_property32(
        PropMode::REPLACE,
        window,
        AtomEnum::WM_HINTS,
        AtomEnum::WM_HINTS,
        &[1, u32::from(options.input), 0, 0, 0, 0, 0, 0, 0],
      )
      .map_err(error)?;
    let mut hints = [0_u32; 18];
    if options.position.is_some() {
      // USPosition.
      hints[0] |= 1;
      hints[1] = options.position.unwrap_or_default().0 as u32;
      hints[2] = options.position.unwrap_or_default().1 as u32;
    }
    if let Some((width, height)) = options.min {
      hints[0] |= 16;
      hints[5] = width;
      hints[6] = height;
    }
    if let Some((width, height)) = options.max {
      hints[0] |= 32;
      hints[7] = width;
      hints[8] = height;
    }
    connection
      .change_property32(
        PropMode::REPLACE,
        window,
        AtomEnum::WM_NORMAL_HINTS,
        AtomEnum::WM_SIZE_HINTS,
        &hints,
      )
      .map_err(error)?;
    if let Some(kind) = &options.window_type {
      let name = format!("_NET_WM_WINDOW_TYPE_{}", kind.to_uppercase());
      let value = atom(connection, &name)?;
      let property = atom(connection, "_NET_WM_WINDOW_TYPE")?;
      connection
        .change_property32(
          PropMode::REPLACE,
          window,
          property,
          AtomEnum::ATOM,
          &[value],
        )
        .map_err(error)?;
    }
    if let Some(parent) = options.transient_for {
      connection
        .change_property32(
          PropMode::REPLACE,
          window,
          AtomEnum::WM_TRANSIENT_FOR,
          AtomEnum::WINDOW,
          &[parent],
        )
        .map_err(error)?;
    }
    if !options.states.is_empty() {
      let states = options
        .states
        .iter()
        .flat_map(|name| state_atoms(connection, name))
        .collect::<Vec<_>>();
      connection
        .change_property32(
          PropMode::REPLACE,
          window,
          self.net_wm_state,
          AtomEnum::ATOM,
          &states,
        )
        .map_err(error)?;
    }
    if options.no_decorations {
      // Flags MWM_HINTS_DECORATIONS, and no decorations.
      let hints = atom(connection, "_MOTIF_WM_HINTS")?;
      connection
        .change_property32(
          PropMode::REPLACE,
          window,
          hints,
          hints,
          &[2, 0, 0, 0, 0],
        )
        .map_err(error)?;
    }
    let pid = atom(connection, "_NET_WM_PID")?;
    connection
      .change_property32(
        PropMode::REPLACE,
        window,
        pid,
        AtomEnum::CARDINAL,
        &[std::process::id()],
      )
      .map_err(error)?;
    Ok(())
  }

  /// Sets `WM_NAME` and `_NET_WM_NAME`.
  fn set_title(&self, title: &str) -> Result<(), String> {
    let connection = &self.connection;
    let error = |error: x11rb::errors::ConnectionError| error.to_string();
    connection
      .change_property8(
        PropMode::REPLACE,
        self.window,
        AtomEnum::WM_NAME,
        AtomEnum::STRING,
        title.as_bytes(),
      )
      .map_err(error)?;
    let net_wm_name = atom(connection, "_NET_WM_NAME")?;
    let utf8 = atom(connection, "UTF8_STRING")?;
    connection
      .change_property8(
        PropMode::REPLACE,
        self.window,
        net_wm_name,
        utf8,
        title.as_bytes(),
      )
      .map_err(error)?;
    Ok(())
  }

  /// Sends an EWMH client message about the window to the root, as
  /// EWMH asks.
  fn root_message(&self, kind: &str, data: [u32; 5]) -> Result<(), String> {
    let kind = atom(&self.connection, kind)?;
    let event = ClientMessageEvent::new(32, self.window, kind, data);
    self
      .connection
      .send_event(
        false,
        self.root,
        EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY,
        event,
      )
      .map(drop)
      .map_err(|error| error.to_string())
  }

  /// Runs one stdin command; returns `false` to exit.
  fn command(&self, line: &str) -> Result<bool, String> {
    let connection = &self.connection;
    let error = |error: x11rb::errors::ConnectionError| error.to_string();
    let mut words = line.split_whitespace();
    let Some(name) = words.next() else {
      return Ok(true);
    };
    let rest = line[name.len()..].trim();
    match name {
      "map" => drop(connection.map_window(self.window).map_err(error)?),
      "unmap" => drop(connection.unmap_window(self.window).map_err(error)?),
      "destroy" => {
        connection.destroy_window(self.window).map_err(error)?;
        connection.flush().map_err(|error| error.to_string())?;
        report("destroyed", &[]);
        return Ok(false);
      }
      "exit" => return Ok(false),
      "title" => self.set_title(rest)?,
      "configure" => {
        let values = words
          .map(str::parse::<i32>)
          .collect::<Result<Vec<_>, _>>()
          .map_err(|_| "configure X Y W H".to_string())?;
        let [x, y, width, height] = values[..] else {
          return Err("configure X Y W H".into());
        };
        let aux = ConfigureWindowAux::new()
          .x(x)
          .y(y)
          .width(width as u32)
          .height(height as u32);
        connection
          .configure_window(self.window, &aux)
          .map_err(error)?;
      }
      "state" => {
        let action = match words.next() {
          Some("remove") => 0,
          Some("add") => 1,
          Some("toggle") => 2,
          _ => return Err("state add|remove|toggle NAME".into()),
        };
        let atoms = state_atoms(connection, words.next().unwrap_or(""));
        let first = atoms.first().copied().unwrap_or(NONE);
        let second = atoms.get(1).copied().unwrap_or(NONE);
        self.root_message("_NET_WM_STATE", [action, first, second, 1, 0])?;
      }
      "activate" => {
        self.root_message("_NET_ACTIVE_WINDOW", [1, CURRENT_TIME, 0, 0, 0])?;
      }
      // IconicState.
      "iconify" => self.root_message("WM_CHANGE_STATE", [3, 0, 0, 0, 0])?,
      "moveresize" => {
        let code = words
          .next()
          .and_then(|code| code.parse().ok())
          .ok_or("moveresize CODE")?;
        self.root_message("_NET_WM_MOVERESIZE", [0, 0, code, 1, 1])?;
      }
      "close" => {
        self.root_message("_NET_CLOSE_WINDOW", [CURRENT_TIME, 1, 0, 0, 0])?;
      }
      "focus" => {
        let focus = connection
          .get_input_focus()
          .map_err(error)?
          .reply()
          .map_err(|error| error.to_string())?;
        report(
          "focus",
          &[
            ("window", focus.focus.to_string()),
            ("own", (focus.focus == self.window).to_string()),
          ],
        );
      }
      "pointer" => {
        let pointer = connection
          .query_pointer(self.root)
          .map_err(error)?
          .reply()
          .map_err(|error| error.to_string())?;
        report(
          "pointer",
          &[
            ("x", pointer.root_x.to_string()),
            ("y", pointer.root_y.to_string()),
            ("child", pointer.child.to_string()),
            ("own", (pointer.child == self.window).to_string()),
          ],
        );
      }
      "grab_focus" => drop(
        connection
          .set_input_focus(InputFocus::POINTER_ROOT, self.window, CURRENT_TIME)
          .map_err(error)?,
      ),
      "cursor" => {
        connection
          .xfixes_query_version(4, 0)
          .map_err(error)?
          .reply()
          .map_err(|error| error.to_string())?;
        let image = connection
          .xfixes_get_cursor_image()
          .map_err(error)?
          .reply()
          .map_err(|error| error.to_string())?;
        report(
          "cursor",
          &[
            ("width", image.width.to_string()),
            ("height", image.height.to_string()),
            ("xhot", image.xhot.to_string()),
            ("yhot", image.yhot.to_string()),
            ("serial", image.cursor_serial.to_string()),
          ],
        );
      }
      "geometry" => {
        let geometry = connection
          .get_geometry(self.window)
          .map_err(error)?
          .reply()
          .map_err(|error| error.to_string())?;
        let absolute = connection
          .translate_coordinates(self.window, self.root, 0, 0)
          .map_err(error)?
          .reply()
          .map_err(|error| error.to_string())?;
        report(
          "geometry",
          &[
            ("x", absolute.dst_x.to_string()),
            ("y", absolute.dst_y.to_string()),
            ("width", geometry.width.to_string()),
            ("height", geometry.height.to_string()),
          ],
        );
      }
      "echo" => report("echo", &[("text", quote(rest))]),
      _ => return Err(format!("unknown command {name}")),
    }
    connection.flush().map_err(|error| error.to_string())?;
    Ok(true)
  }

  /// Reports one event; returns `false` to exit.
  fn event(&self, event: Event) -> Result<bool, String> {
    match event {
      Event::MapNotify(event) if event.window == self.window => {
        report("mapped", &[]);
      }
      Event::UnmapNotify(event) if event.window == self.window => {
        report("unmapped", &[]);
      }
      Event::ConfigureNotify(event) if event.window == self.window => {
        report(
          "configure",
          &[
            ("x", event.x.to_string()),
            ("y", event.y.to_string()),
            ("width", event.width.to_string()),
            ("height", event.height.to_string()),
            ("synthetic", (event.response_type & 0x80 != 0).to_string()),
          ],
        );
      }
      Event::FocusIn(_) => report("focus_in", &[]),
      Event::FocusOut(_) => report("focus_out", &[]),
      Event::ClientMessage(event)
        if event.type_ == self.wm_protocols
          && event.data.as_data32()[0] == self.wm_delete_window =>
      {
        report("delete", &[]);
        return Ok(false);
      }
      Event::ClientMessage(event)
        if event.type_ == self.wm_protocols
          && event.data.as_data32()[0] == self.wm_take_focus =>
      {
        report("take_focus", &[]);
      }
      Event::PropertyNotify(event) if event.atom == self.net_wm_state => {
        let names = self.atom_names(self.net_wm_state)?;
        let list = names.iter().map(|name| quote(name)).collect::<Vec<_>>();
        report(
          "net_wm_state",
          &[("value", format!("[{}]", list.join(", ")))],
        );
      }
      Event::PropertyNotify(event) if event.atom == self.wm_state => {
        let state = self
          .connection
          .get_property(false, self.window, self.wm_state, self.wm_state, 0, 2)
          .map_err(|error| error.to_string())?
          .reply()
          .map_err(|error| error.to_string())?;
        let value = state
          .value32()
          .and_then(|mut values| values.next())
          .map_or("null".to_string(), |value| value.to_string());
        report("wm_state", &[("value", value)]);
      }
      Event::ButtonPress(event) if event.detail == 1 => {
        self.drag.borrow_mut().pressed =
          Some((event.root_x.into(), event.root_y.into()));
      }
      Event::MotionNotify(event) => {
        self.drag_motion(
          event.root_x.into(),
          event.root_y.into(),
          event.time,
        )?;
      }
      Event::ButtonRelease(event) if event.detail == 1 => {
        self.drag_release(event.time)?;
      }
      Event::ClientMessage(event) if event.format == 32 => {
        self.xdnd_message(&event)?;
      }
      Event::SelectionRequest(event) => self.selection_request(&event)?,
      Event::SelectionNotify(event) => self.selection_notify(&event)?,
      Event::Error(error) => {
        report("error", &[("text", quote(&format!("{error:?}")))]);
      }
      _ => {}
    }
    Ok(true)
  }

  /// Sends an Xdnd message of type `kind` to `window`.
  fn xdnd_send(
    &self,
    window: Window,
    kind: u32,
    data: [u32; 5],
  ) -> Result<(), String> {
    let event = ClientMessageEvent::new(32, window, kind, data);
    self
      .connection
      .send_event(false, window, EventMask::NO_EVENT, event)
      .map(drop)
      .map_err(|error| error.to_string())
  }

  /// The `XdndAware` child of the root at root position (`x`, `y`),
  /// other than the probe's own windows.
  fn aware_window_at(&self, x: i32, y: i32) -> Result<Option<Window>, String> {
    let error = |error: x11rb::errors::ConnectionError| error.to_string();
    let reply_error = |error: x11rb::errors::ReplyError| error.to_string();
    let child = self
      .connection
      .translate_coordinates(self.root, self.root, x as i16, y as i16)
      .map_err(error)?
      .reply()
      .map_err(reply_error)?
      .child;
    let icon = self.drag.borrow().icon;
    if child == NONE || child == self.window || Some(child) == icon {
      return Ok(None);
    }
    let aware = self
      .connection
      .get_property(false, child, self.xdnd.aware, AtomEnum::ATOM, 0, 1)
      .map_err(error)?
      .reply()
      .map_err(reply_error)?;
    let version = aware.value32().and_then(|mut values| values.next());
    Ok(version.filter(|version| *version >= 3).map(|_| child))
  }

  /// A motion with the first button held: starts the drag past the
  /// threshold, follows the target and moves the icon.
  fn drag_motion(&self, x: i32, y: i32, time: u32) -> Result<(), String> {
    if self.drag_text.is_none() {
      return Ok(());
    }
    let error = |error: x11rb::errors::ConnectionError| error.to_string();
    let (pressed, dragging) = {
      let drag = self.drag.borrow();
      (drag.pressed, drag.dragging)
    };
    let Some((press_x, press_y)) = pressed else {
      return Ok(());
    };
    if !dragging {
      if (x - press_x).abs() <= DRAG_THRESHOLD
        && (y - press_y).abs() <= DRAG_THRESHOLD
      {
        return Ok(());
      }
      self
        .connection
        .set_selection_owner(self.window, self.xdnd.selection, time)
        .map_err(error)?;
      self.drag.borrow_mut().dragging = true;
      report("drag_started", &[]);
      if self.drag_icon {
        self.show_icon()?;
      }
    }
    if let Some(icon) = self.drag.borrow().icon {
      self
        .connection
        .configure_window(icon, &ConfigureWindowAux::new().x(x + 8).y(y + 8))
        .map_err(error)?;
    }
    let target = self.aware_window_at(x, y)?;
    let previous = self.drag.borrow().target;
    if previous != target {
      if let Some(previous) = previous {
        self.xdnd_send(previous, self.xdnd.leave, [self.window, 0, 0, 0, 0])?;
      }
      if let Some(target) = target {
        self.xdnd_send(
          target,
          self.xdnd.enter,
          [self.window, XDND_VERSION << 24, self.xdnd.utf8, 0, 0],
        )?;
      }
      let mut drag = self.drag.borrow_mut();
      drag.target = target;
      drag.accepted = false;
      report(
        "drag_target",
        &[("window", target.unwrap_or(NONE).to_string())],
      );
    }
    if let Some(target) = target {
      let packed = ((x.max(0) as u32) << 16) | (y.max(0) as u32 & 0xffff);
      self.xdnd_send(
        target,
        self.xdnd.position,
        [self.window, 0, packed, time, self.xdnd.copy],
      )?;
    }
    self.connection.flush().map_err(|error| error.to_string())
  }

  /// Maps the drag icon, creating it the first time.
  fn show_icon(&self) -> Result<(), String> {
    let error = |error: x11rb::errors::ConnectionError| error.to_string();
    let existing = self.drag.borrow().icon;
    let icon = match existing {
      Some(icon) => icon,
      None => {
        let icon = self.connection.generate_id().map_err(|e| e.to_string())?;
        let aux = CreateWindowAux::new()
          .override_redirect(1)
          .background_pixel(0x00ff00ff);
        self
          .connection
          .create_window(
            x11rb::COPY_DEPTH_FROM_PARENT,
            icon,
            self.root,
            0,
            0,
            32,
            32,
            0,
            WindowClass::INPUT_OUTPUT,
            x11rb::COPY_FROM_PARENT,
            &aux,
          )
          .map_err(error)?;
        let kind = atom(&self.connection, "_NET_WM_WINDOW_TYPE")?;
        let dnd = atom(&self.connection, "_NET_WM_WINDOW_TYPE_DND")?;
        self
          .connection
          .change_property32(
            PropMode::REPLACE,
            icon,
            kind,
            AtomEnum::ATOM,
            &[dnd],
          )
          .map_err(error)?;
        self.drag.borrow_mut().icon = Some(icon);
        report("drag_icon", &[("window", icon.to_string())]);
        icon
      }
    };
    self.connection.map_window(icon).map_err(error)?;
    Ok(())
  }

  /// The first button went up: drops on a target that accepted, or
  /// leaves it.
  fn drag_release(&self, time: u32) -> Result<(), String> {
    let (dragging, target, accepted, icon) = {
      let mut drag = self.drag.borrow_mut();
      drag.pressed = None;
      let dragging = std::mem::take(&mut drag.dragging);
      (dragging, drag.target.take(), drag.accepted, drag.icon)
    };
    if !dragging {
      return Ok(());
    }
    if let Some(icon) = icon {
      self
        .connection
        .unmap_window(icon)
        .map_err(|error| error.to_string())?;
    }
    match target {
      Some(target) if accepted => {
        self.xdnd_send(target, self.xdnd.drop, [self.window, 0, time, 0, 0])?;
        report("drag_dropped", &[("window", target.to_string())]);
      }
      Some(target) => {
        self.xdnd_send(target, self.xdnd.leave, [self.window, 0, 0, 0, 0])?;
        report("drag_cancelled", &[]);
      }
      None => report("drag_cancelled", &[]),
    }
    self.connection.flush().map_err(|error| error.to_string())
  }

  /// The name of `atom`, or its number.
  fn atom_name(&self, atom: u32) -> String {
    self
      .connection
      .get_atom_name(atom)
      .ok()
      .and_then(|cookie| cookie.reply().ok())
      .map_or(atom.to_string(), |reply| {
        String::from_utf8_lossy(&reply.name).into_owned()
      })
  }

  /// An Xdnd message to the probe's window.
  fn xdnd_message(&self, event: &ClientMessageEvent) -> Result<(), String> {
    let data = event.data.as_data32();
    let xdnd = &self.xdnd;
    if event.type_ == xdnd.status {
      let accepted = data[1] & 1 != 0;
      let mut drag = self.drag.borrow_mut();
      if drag.target == Some(data[0]) {
        drag.accepted = accepted;
      }
      drop(drag);
      report(
        "drag_status",
        &[
          ("accepted", accepted.to_string()),
          ("action", quote(&self.atom_name(data[4]))),
        ],
      );
    } else if event.type_ == xdnd.finished {
      report(
        "drag_finished",
        &[
          ("performed", (data[1] & 1 != 0).to_string()),
          ("action", quote(&self.atom_name(data[2]))),
        ],
      );
    } else if event.type_ == xdnd.enter {
      let types = data[2..]
        .iter()
        .filter(|atom| **atom != NONE)
        .map(|atom| quote(&self.atom_name(*atom)))
        .collect::<Vec<_>>();
      self.drag.borrow_mut().source = Some(data[0]);
      report(
        "drag_enter",
        &[
          ("source", data[0].to_string()),
          ("version", (data[1] >> 24).to_string()),
          ("types", format!("[{}]", types.join(", "))),
        ],
      );
      self.drag.borrow_mut().accepted =
        data[1] & 1 != 0 || data[2..].contains(&xdnd.utf8);
    } else if event.type_ == xdnd.position {
      let accepted = self.drag.borrow().accepted;
      let flags = 2 | u32::from(accepted);
      let action = if accepted { xdnd.copy } else { NONE };
      self.xdnd_send(
        data[0],
        xdnd.status,
        [self.window, flags, 0, 0, action],
      )?;
      report(
        "drag_position",
        &[
          ("x", (data[2] >> 16).to_string()),
          ("y", (data[2] & 0xffff).to_string()),
        ],
      );
    } else if event.type_ == xdnd.leave {
      self.drag.borrow_mut().source = None;
      report("drag_leave", &[]);
    } else if event.type_ == xdnd.drop {
      self
        .connection
        .convert_selection(
          self.window,
          xdnd.selection,
          xdnd.utf8,
          xdnd.property,
          data[2],
        )
        .map_err(|error| error.to_string())?;
      report("drag_drop", &[]);
    }
    self.connection.flush().map_err(|error| error.to_string())
  }

  /// A conversion of `XdndSelection` from the probe as a drag source.
  fn selection_request(
    &self,
    event: &SelectionRequestEvent,
  ) -> Result<(), String> {
    let error = |error: x11rb::errors::ConnectionError| error.to_string();
    let xdnd = &self.xdnd;
    let mut property = event.property;
    let text = self.drag_text.as_deref().unwrap_or("");
    if event.selection != xdnd.selection || self.drag_text.is_none() {
      property = NONE;
    } else if event.target == xdnd.targets {
      self
        .connection
        .change_property32(
          PropMode::REPLACE,
          event.requestor,
          property,
          AtomEnum::ATOM,
          &[xdnd.targets, xdnd.utf8, xdnd.text],
        )
        .map_err(error)?;
    } else if event.target == xdnd.utf8 || event.target == xdnd.text {
      self
        .connection
        .change_property8(
          PropMode::REPLACE,
          event.requestor,
          property,
          event.target,
          text.as_bytes(),
        )
        .map_err(error)?;
      report(
        "drag_send",
        &[("target", quote(&self.atom_name(event.target)))],
      );
    } else {
      property = NONE;
    }
    let notify = SelectionNotifyEvent {
      response_type: SELECTION_NOTIFY_EVENT,
      sequence: 0,
      time: event.time,
      requestor: event.requestor,
      selection: event.selection,
      target: event.target,
      property,
    };
    self
      .connection
      .send_event(false, event.requestor, EventMask::NO_EVENT, notify)
      .map_err(error)?;
    self.connection.flush().map_err(|error| error.to_string())
  }

  /// The conversion of a drop on the probe as a target arrived: reports
  /// the text and finishes the drag.
  fn selection_notify(
    &self,
    event: &SelectionNotifyEvent,
  ) -> Result<(), String> {
    let xdnd = &self.xdnd;
    if event.selection != xdnd.selection {
      return Ok(());
    }
    let Some(source) = self.drag.borrow_mut().source.take() else {
      return Ok(());
    };
    let text = if event.property == NONE {
      None
    } else {
      let reply = self
        .connection
        .get_property(
          true,
          self.window,
          xdnd.property,
          AtomEnum::ANY,
          0,
          1 << 20,
        )
        .map_err(|error| error.to_string())?
        .reply()
        .map_err(|error| error.to_string())?;
      Some(String::from_utf8_lossy(&reply.value).into_owned())
    };
    report(
      "dropped",
      &[("text", text.as_deref().map_or("null".into(), quote))],
    );
    let performed = text.is_some();
    let action = if performed { xdnd.copy } else { NONE };
    self.xdnd_send(
      source,
      xdnd.finished,
      [self.window, u32::from(performed), action, 0, 0],
    )?;
    self.connection.flush().map_err(|error| error.to_string())
  }

  /// The names of the atoms in the window's property `property`.
  fn atom_names(&self, property: u32) -> Result<Vec<String>, String> {
    let reply = self
      .connection
      .get_property(false, self.window, property, AtomEnum::ATOM, 0, 64)
      .map_err(|error| error.to_string())?
      .reply()
      .map_err(|error| error.to_string())?;
    let mut names = Vec::new();
    for atom in reply.value32().into_iter().flatten() {
      let name = self
        .connection
        .get_atom_name(atom)
        .map_err(|error| error.to_string())?
        .reply()
        .map_err(|error| error.to_string())?;
      names.push(String::from_utf8_lossy(&name.name).into_owned());
    }
    Ok(names)
  }
}

/// The `_NET_WM_STATE` atoms for a short state name; `maximized` is both
/// directions.
fn state_atoms(connection: &RustConnection, name: &str) -> Vec<u32> {
  let names: &[&str] = match name {
    "fullscreen" => &["_NET_WM_STATE_FULLSCREEN"],
    "maximized" => &[
      "_NET_WM_STATE_MAXIMIZED_VERT",
      "_NET_WM_STATE_MAXIMIZED_HORZ",
    ],
    "hidden" => &["_NET_WM_STATE_HIDDEN"],
    "modal" => &["_NET_WM_STATE_MODAL"],
    _ => &[],
  };
  names
    .iter()
    .filter_map(|name| atom(connection, name).ok())
    .collect()
}

/// Creates the window, then serves events and commands until told to
/// stop.
fn run(options: &Options) -> Result<(), String> {
  let probe = Probe::create(options)?;
  report("window", &[("id", probe.window.to_string())]);
  if options.map {
    probe
      .connection
      .map_window(probe.window)
      .map_err(|error| error.to_string())?;
  }
  probe
    .connection
    .flush()
    .map_err(|error| error.to_string())?;
  let (sender, commands) = mpsc::channel();
  std::thread::spawn(move || {
    for line in std::io::stdin().lock().lines() {
      let Ok(line) = line else {
        break;
      };
      if sender.send(line).is_err() {
        break;
      }
    }
  });
  loop {
    while let Some(event) = probe
      .connection
      .poll_for_event()
      .map_err(|error| error.to_string())?
    {
      if !probe.event(event)? {
        return Ok(());
      }
    }
    match commands.recv_timeout(Duration::from_millis(10)) {
      Ok(line) => match probe.command(&line) {
        Ok(true) => {}
        Ok(false) => return Ok(()),
        Err(error) => report("error", &[("text", quote(&error))]),
      },
      Err(mpsc::RecvTimeoutError::Timeout) => {}
      // stdin closed: keep serving events.
      Err(mpsc::RecvTimeoutError::Disconnected) => {
        std::thread::sleep(Duration::from_millis(10));
      }
    }
  }
}

fn main() {
  if std::env::var_os(NAMESPACE_VARIABLE).is_none_or(|value| value != "1") {
    eprintln!(
      "xw_probe: refusing to run outside tools/xwayland_test/run.py \
       ({NAMESPACE_VARIABLE}=1 is not set)"
    );
    std::process::exit(2);
  }
  let arguments = std::env::args().skip(1).collect::<Vec<_>>();
  let options = match parse(&arguments) {
    Ok(options) => options,
    Err(error) => {
      eprintln!("xw_probe: {error}");
      std::process::exit(2);
    }
  };
  if let Err(error) = run(&options) {
    report("error", &[("text", quote(&error))]);
    std::process::exit(1);
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn strings(arguments: &[&str]) -> Vec<String> {
    arguments
      .iter()
      .map(|argument| (*argument).into())
      .collect()
  }

  #[test]
  fn options_parse() {
    let options = parse(&strings(&[
      "--class",
      "probe,Probe",
      "--size",
      "300x200",
      "--min",
      "300x200",
      "--max",
      "300x200",
      "--position",
      "10,-5",
      "--type",
      "dialog",
      "--transient-for",
      "0x400001",
      "--no-delete",
      "--state",
      "fullscreen,hidden",
      "--color",
      "ff0000",
      "--no-decorations",
    ]))
    .unwrap();
    assert!(options.no_decorations);
    assert_eq!(options.color, Some(0xff0000));
    assert_eq!(options.class, "Probe");
    assert_eq!(options.instance, "probe");
    assert_eq!(options.size, (300, 200));
    assert_eq!(options.min, Some((300, 200)));
    assert_eq!(options.position, Some((10, -5)));
    assert_eq!(options.window_type.as_deref(), Some("dialog"));
    assert_eq!(options.transient_for, Some(0x400001));
    assert!(!options.delete);
    assert_eq!(options.states, ["fullscreen", "hidden"]);
    assert!(parse(&strings(&["--size", "big"])).is_err());
    assert!(parse(&strings(&["--bogus"])).is_err());
    assert!(parse(&strings(&["--title"])).is_err());
  }

  #[test]
  fn json_strings_are_escaped() {
    assert_eq!(quote("a\"b\\c\n"), "\"a\\\"b\\\\c\\u000a\"");
    assert_eq!(quote("ü"), "\"ü\"");
  }
}
