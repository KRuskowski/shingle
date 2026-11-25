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
//!     [--no-background] [--no-map]
//! ```
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
//! `error`.

use std::{
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
      CreateWindowAux, EventMask, InputFocus, PropMode, Window, WindowClass,
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
    let mut aux = CreateWindowAux::new()
      .event_mask(
        EventMask::STRUCTURE_NOTIFY
          | EventMask::FOCUS_CHANGE
          | EventMask::PROPERTY_CHANGE
          | EventMask::EXPOSURE,
      )
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
      connection,
      root,
      window,
    };
    probe.set_properties(options)?;
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
      Event::Error(error) => {
        report("error", &[("text", quote(&format!("{error:?}")))]);
      }
      _ => {}
    }
    Ok(true)
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
    ]))
    .unwrap();
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
