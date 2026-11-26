//! The window manager's side of the X connection, behind a trait so tests
//! can record requests instead of talking to a server.

use std::{
  fmt,
  os::{
    fd::{AsFd, BorrowedFd},
    unix::net::UnixStream,
  },
};

use x11rb::{
  COPY_FROM_PARENT, CURRENT_TIME, NONE,
  connection::{Connection, RequestConnection},
  protocol::{
    Event,
    composite::{self, ConnectionExt as _, Redirect},
    render::{ConnectionExt as _, CreatePictureAux, PictType, Pictforminfo},
    res::{ClientIdMask, ClientIdSpec, ConnectionExt as _},
    xfixes::{ClientDisconnectFlags, ConnectionExt as _, SelectionEventMask},
    xproto::{
      CONFIGURE_NOTIFY_EVENT, ChangeWindowAttributesAux, ClientMessageEvent,
      ConfigureNotifyEvent, ConfigureWindowAux, ConnectionExt as _,
      CreateGCAux, CreateWindowAux, EventMask, ImageFormat, InputFocus,
      PropMode, SELECTION_NOTIFY_EVENT, SelectionNotifyEvent, StackMode,
      Window, WindowClass,
    },
  },
  rust_connection::{DefaultStream, RustConnection},
  wrapper::ConnectionExt as _,
};

use super::{cursor::CursorImage, props::Property, window::Geometry};

/// How many 32-bit items of a property are read at most: 256 KiB.
const PROPERTY_LIMIT: u32 = 1 << 16;

/// How many 32-bit items of a selection property are read at most, as
/// wlroots reads them: all of it.
const SELECTION_PROPERTY_LIMIT: u32 = 0x1fff_ffff;

/// Why the window manager could not talk to the X server.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum XwmError {
  /// The connection setup failed.
  Connect(String),
  /// Reading or writing the connection failed, or it closed.
  Connection(String),
  /// The server refused a request the window manager depends on, for
  /// example because another window manager selected substructure
  /// redirection on the root.
  Request(String),
  /// The server lacks an extension the window manager needs.
  MissingExtension(&'static str),
}

impl fmt::Display for XwmError {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Self::Connect(error) => write!(f, "X connection setup failed: {error}"),
      Self::Connection(error) => write!(f, "X connection failed: {error}"),
      Self::Request(error) => write!(f, "X request failed: {error}"),
      Self::MissingExtension(name) => {
        write!(f, "the X server lacks the {name} extension")
      }
    }
  }
}

impl std::error::Error for XwmError {}

/// Maps any x11rb error to [`XwmError::Connection`].
fn connection_error(error: impl fmt::Display) -> XwmError {
  XwmError::Connection(error.to_string())
}

/// What the window manager asks of the X server. Requests without a reply
/// are queued until [`XConnection::flush`]; those with one, and the
/// `checked` ones, wait for it.
pub trait XConnection {
  /// The root window of screen 0.
  fn root(&self) -> Window;

  /// A new resource ID.
  ///
  /// # Errors
  ///
  /// When the connection has run out of IDs or failed.
  fn generate_id(&mut self) -> Result<u32, XwmError>;

  /// Whether the server has the extension `name`.
  ///
  /// # Errors
  ///
  /// When the query fails.
  fn has_extension(&mut self, name: &'static str) -> Result<bool, XwmError>;

  /// Interns every name in one pipelined batch and returns the atoms in
  /// the same order.
  ///
  /// # Errors
  ///
  /// When a request or reply fails.
  fn intern_atoms(&mut self, names: &[&str]) -> Result<Vec<u32>, XwmError>;

  /// Asks the server to treat this client like one that has gone when it
  /// decides whether to exit with `-terminate` (XFixes 6
  /// `SetClientDisconnectMode(Terminate)`). Without it, a lazily started
  /// Xwayland would never exit, since the window manager stays
  /// connected. Returns `false` when the server's XFixes is older than 6.
  ///
  /// # Errors
  ///
  /// When the version query fails.
  fn terminate_without_clients(&mut self) -> Result<bool, XwmError>;

  /// Composite `RedirectSubwindows(window, Manual)`, checked.
  ///
  /// # Errors
  ///
  /// When the server refuses it.
  fn redirect_subwindows(&mut self, window: Window) -> Result<(), XwmError>;

  /// Sets the event mask of `window`, checked, so a second window
  /// manager on the root fails here.
  ///
  /// # Errors
  ///
  /// When the server refuses it.
  fn select_events(
    &mut self,
    window: Window,
    mask: EventMask,
  ) -> Result<(), XwmError>;

  /// Creates an unmapped `InputOutput` child `id` of `parent` with the
  /// given geometry.
  ///
  /// # Errors
  ///
  /// When the request cannot be queued.
  fn create_window(
    &mut self,
    id: Window,
    parent: Window,
    geometry: (i16, i16, u16, u16),
  ) -> Result<(), XwmError>;

  /// Creates an unmapped `InputOnly` child `id` of `parent` with the given
  /// geometry, which takes pointer input but is never drawn: the drag and
  /// drop proxy.
  ///
  /// # Errors
  ///
  /// When the request cannot be queued.
  fn create_input_window(
    &mut self,
    id: Window,
    parent: Window,
    geometry: (i16, i16, u16, u16),
  ) -> Result<(), XwmError>;

  /// Unmaps `window`.
  ///
  /// # Errors
  ///
  /// When the request cannot be queued.
  fn unmap_window(&mut self, window: Window) -> Result<(), XwmError>;

  /// Makes `owner` the owner of `selection`, at `CurrentTime`.
  ///
  /// # Errors
  ///
  /// When the request cannot be queued.
  fn set_selection_owner(
    &mut self,
    owner: Window,
    selection: u32,
  ) -> Result<(), XwmError>;

  /// Replaces a property with 32-bit items.
  ///
  /// # Errors
  ///
  /// When the request cannot be queued.
  fn change_property32(
    &mut self,
    window: Window,
    property: u32,
    kind: u32,
    data: &[u32],
  ) -> Result<(), XwmError>;

  /// Replaces a property with 8-bit items.
  ///
  /// # Errors
  ///
  /// When the request cannot be queued.
  fn change_property8(
    &mut self,
    window: Window,
    property: u32,
    kind: u32,
    data: &[u8],
  ) -> Result<(), XwmError>;

  /// Sets the event mask of a client window, unchecked, since the window
  /// may already be gone.
  ///
  /// # Errors
  ///
  /// When the request cannot be queued.
  fn watch_window(
    &mut self,
    window: Window,
    mask: EventMask,
  ) -> Result<(), XwmError>;

  /// Reads `properties` of `window` in one pipelined batch, in the same
  /// order. A property that is not set, or cannot be read because the
  /// window is gone, is `None`.
  ///
  /// # Errors
  ///
  /// When the connection fails.
  fn get_properties(
    &mut self,
    window: Window,
    properties: &[u32],
  ) -> Result<Vec<Option<Property>>, XwmError>;

  /// The PID of the client that created `window`, from XRes
  /// `QueryClientIds` with `LocalClientPID`, as wlroots'
  /// `read_surface_client_id` asks. `None` without XRes or a PID.
  ///
  /// # Errors
  ///
  /// When the connection fails.
  fn client_pid(&mut self, window: Window) -> Result<Option<u32>, XwmError>;

  /// Maps `window`.
  ///
  /// # Errors
  ///
  /// When the request cannot be queued.
  fn map_window(&mut self, window: Window) -> Result<(), XwmError>;

  /// `ConfigureWindow` with position, size and a border of 0.
  ///
  /// # Errors
  ///
  /// When the request cannot be queued.
  fn configure_window(
    &mut self,
    window: Window,
    geometry: Geometry,
  ) -> Result<(), XwmError>;

  /// Sends `window` a synthetic `ConfigureNotify` with `geometry`, as
  /// ICCCM 4.1.5 asks when a configure leaves the size unchanged.
  ///
  /// # Errors
  ///
  /// When the request cannot be queued.
  fn send_configure_notify(
    &mut self,
    window: Window,
    geometry: Geometry,
  ) -> Result<(), XwmError>;

  /// Sends `window` a format-32 `ClientMessage` of type `kind`, with no
  /// event mask, so it goes to the window's creator.
  ///
  /// # Errors
  ///
  /// When the request cannot be queued.
  fn send_client_message(
    &mut self,
    window: Window,
    kind: u32,
    data: [u32; 5],
  ) -> Result<(), XwmError>;

  /// Disconnects the client that created `window`.
  ///
  /// # Errors
  ///
  /// When the request cannot be queued.
  fn kill_client(&mut self, window: Window) -> Result<(), XwmError>;

  /// `SetInputFocus(window, revert to PointerRoot, CurrentTime)`; `window`
  /// may be `None` (0). Returns the request's sequence number, which the
  /// window manager compares with `FocusIn` events.
  ///
  /// # Errors
  ///
  /// When the request cannot be queued.
  fn set_input_focus(&mut self, window: Window) -> Result<u16, XwmError>;

  /// Restacks `window` just above `sibling`, or above all its siblings
  /// without one (`ConfigureWindow` with `StackMode::Above`).
  ///
  /// # Errors
  ///
  /// When the request cannot be queued.
  fn stack_above(
    &mut self,
    window: Window,
    sibling: Option<Window>,
  ) -> Result<(), XwmError>;

  /// Makes `image` the root window's cursor through Render
  /// `CreateCursor`, as wlroots' `xwm_set_cursor` does.
  ///
  /// # Errors
  ///
  /// When the server has no ARGB32 picture format, or a request fails.
  fn set_root_cursor(&mut self, image: &CursorImage) -> Result<(), XwmError>;

  /// Destroys a window the window manager created.
  ///
  /// # Errors
  ///
  /// When the request cannot be queued.
  fn destroy_window(&mut self, window: Window) -> Result<(), XwmError>;

  /// XFixes `SelectSelectionInput` on `window` for `selection`: owner
  /// changes, and the owner's window or client going away.
  ///
  /// # Errors
  ///
  /// When the request cannot be queued.
  fn select_selection_input(
    &mut self,
    window: Window,
    selection: u32,
  ) -> Result<(), XwmError>;

  /// `ConvertSelection`: asks the owner of `selection` to store it as
  /// `target` in `property` of `requestor`.
  ///
  /// # Errors
  ///
  /// When the request cannot be queued.
  fn convert_selection(
    &mut self,
    requestor: Window,
    selection: u32,
    target: u32,
    property: u32,
    time: u32,
  ) -> Result<(), XwmError>;

  /// Reads `property` of `window` whole and deletes it, as a selection
  /// requestor does. `None` when it is not set or the window is gone.
  ///
  /// # Errors
  ///
  /// When the connection fails.
  fn take_property(
    &mut self,
    window: Window,
    property: u32,
  ) -> Result<Option<Property>, XwmError>;

  /// The names of `atoms`, in one pipelined batch; `None` for an atom the
  /// server does not know.
  ///
  /// # Errors
  ///
  /// When the connection fails.
  fn atom_names(
    &mut self,
    atoms: &[u32],
  ) -> Result<Vec<Option<String>>, XwmError>;

  /// Sends `requestor` a `SelectionNotify`: `property` holds the
  /// converted `target`, or is `None` (0) when the conversion failed.
  ///
  /// # Errors
  ///
  /// When the request cannot be queued.
  fn send_selection_notify(
    &mut self,
    requestor: Window,
    selection: u32,
    target: u32,
    property: u32,
    time: u32,
  ) -> Result<(), XwmError>;

  /// Sends every queued request.
  ///
  /// # Errors
  ///
  /// When writing fails.
  fn flush(&mut self) -> Result<(), XwmError>;

  /// The next event or error that has arrived, without waiting.
  ///
  /// # Errors
  ///
  /// When reading fails or the connection closed.
  fn poll_for_event(&mut self) -> Result<Option<Event>, XwmError>;
}

/// The production connection: x11rb's pure-Rust `RustConnection` over the
/// compositor end of Xwayland's `-wm` socket pair.
pub struct X11Connection {
  connection: RustConnection,
  root: Window,
  has_res: Option<bool>,
  /// The root cursor set last, freed when it is replaced.
  cursor: Option<u32>,
}

impl X11Connection {
  /// Performs the X connection setup on `stream` and uses screen 0.
  ///
  /// # Errors
  ///
  /// [`XwmError::Connect`] when the setup fails.
  pub fn connect(stream: UnixStream) -> Result<Self, XwmError> {
    let (stream, _) = DefaultStream::from_unix_stream(stream)
      .map_err(|error| XwmError::Connect(error.to_string()))?;
    let connection = RustConnection::connect_to_stream(stream, 0)
      .map_err(|error| XwmError::Connect(error.to_string()))?;
    let root = connection.setup().roots[0].root;
    Ok(Self {
      connection,
      root,
      has_res: None,
      cursor: None,
    })
  }
}

impl AsFd for X11Connection {
  fn as_fd(&self) -> BorrowedFd<'_> {
    self.connection.stream().as_fd()
  }
}

impl XConnection for X11Connection {
  fn root(&self) -> Window {
    self.root
  }

  fn generate_id(&mut self) -> Result<u32, XwmError> {
    self.connection.generate_id().map_err(connection_error)
  }

  fn has_extension(&mut self, name: &'static str) -> Result<bool, XwmError> {
    self
      .connection
      .extension_information(name)
      .map(|information| information.is_some())
      .map_err(connection_error)
  }

  fn intern_atoms(&mut self, names: &[&str]) -> Result<Vec<u32>, XwmError> {
    let cookies = names
      .iter()
      .map(|name| self.connection.intern_atom(false, name.as_bytes()))
      .collect::<Result<Vec<_>, _>>()
      .map_err(connection_error)?;
    cookies
      .into_iter()
      .map(|cookie| {
        cookie
          .reply()
          .map(|reply| reply.atom)
          .map_err(|error| XwmError::Request(error.to_string()))
      })
      .collect()
  }

  fn terminate_without_clients(&mut self) -> Result<bool, XwmError> {
    let version = self
      .connection
      .xfixes_query_version(6, 0)
      .map_err(connection_error)?
      .reply()
      .map_err(|error| XwmError::Request(error.to_string()))?;
    if version.major_version < 6 {
      return Ok(false);
    }
    self
      .connection
      .xfixes_set_client_disconnect_mode(ClientDisconnectFlags::TERMINATE)
      .map_err(connection_error)?;
    Ok(true)
  }

  fn redirect_subwindows(&mut self, window: Window) -> Result<(), XwmError> {
    self
      .connection
      .composite_redirect_subwindows(window, Redirect::MANUAL)
      .map_err(connection_error)?
      .check()
      .map_err(|error| XwmError::Request(error.to_string()))
  }

  fn select_events(
    &mut self,
    window: Window,
    mask: EventMask,
  ) -> Result<(), XwmError> {
    self
      .connection
      .change_window_attributes(
        window,
        &ChangeWindowAttributesAux::new().event_mask(mask),
      )
      .map_err(connection_error)?
      .check()
      .map_err(|error| XwmError::Request(error.to_string()))
  }

  fn create_window(
    &mut self,
    id: Window,
    parent: Window,
    (x, y, width, height): (i16, i16, u16, u16),
  ) -> Result<(), XwmError> {
    self
      .connection
      .create_window(
        COPY_FROM_PARENT as u8,
        id,
        parent,
        x,
        y,
        width,
        height,
        0,
        WindowClass::INPUT_OUTPUT,
        COPY_FROM_PARENT,
        &CreateWindowAux::new(),
      )
      .map(drop)
      .map_err(connection_error)
  }

  fn create_input_window(
    &mut self,
    id: Window,
    parent: Window,
    (x, y, width, height): (i16, i16, u16, u16),
  ) -> Result<(), XwmError> {
    self
      .connection
      .create_window(
        0,
        id,
        parent,
        x,
        y,
        width,
        height,
        0,
        WindowClass::INPUT_ONLY,
        COPY_FROM_PARENT,
        &CreateWindowAux::new(),
      )
      .map(drop)
      .map_err(connection_error)
  }

  fn unmap_window(&mut self, window: Window) -> Result<(), XwmError> {
    self
      .connection
      .unmap_window(window)
      .map(drop)
      .map_err(connection_error)
  }

  fn set_selection_owner(
    &mut self,
    owner: Window,
    selection: u32,
  ) -> Result<(), XwmError> {
    self
      .connection
      .set_selection_owner(owner, selection, CURRENT_TIME)
      .map(drop)
      .map_err(connection_error)
  }

  fn change_property32(
    &mut self,
    window: Window,
    property: u32,
    kind: u32,
    data: &[u32],
  ) -> Result<(), XwmError> {
    self
      .connection
      .change_property32(PropMode::REPLACE, window, property, kind, data)
      .map(drop)
      .map_err(connection_error)
  }

  fn change_property8(
    &mut self,
    window: Window,
    property: u32,
    kind: u32,
    data: &[u8],
  ) -> Result<(), XwmError> {
    self
      .connection
      .change_property8(PropMode::REPLACE, window, property, kind, data)
      .map(drop)
      .map_err(connection_error)
  }

  fn watch_window(
    &mut self,
    window: Window,
    mask: EventMask,
  ) -> Result<(), XwmError> {
    self
      .connection
      .change_window_attributes(
        window,
        &ChangeWindowAttributesAux::new().event_mask(mask),
      )
      .map(drop)
      .map_err(connection_error)
  }

  fn get_properties(
    &mut self,
    window: Window,
    properties: &[u32],
  ) -> Result<Vec<Option<Property>>, XwmError> {
    let cookies = properties
      .iter()
      .map(|property| {
        self.connection.get_property(
          false,
          window,
          *property,
          x11rb::protocol::xproto::AtomEnum::ANY,
          0,
          PROPERTY_LIMIT,
        )
      })
      .collect::<Result<Vec<_>, _>>()
      .map_err(connection_error)?;
    let mut values = Vec::with_capacity(cookies.len());
    for cookie in cookies {
      match cookie.reply() {
        Ok(reply) if reply.type_ != NONE => values.push(Some(Property {
          kind: reply.type_,
          format: reply.format,
          value: reply.value,
        })),
        Ok(_) | Err(x11rb::errors::ReplyError::X11Error(_)) => {
          values.push(None);
        }
        Err(error) => return Err(connection_error(error)),
      }
    }
    Ok(values)
  }

  fn client_pid(&mut self, window: Window) -> Result<Option<u32>, XwmError> {
    let has_res = match self.has_res {
      Some(has_res) => has_res,
      None => {
        let has_res =
          self.has_extension(x11rb::protocol::res::X11_EXTENSION_NAME)?;
        self.has_res = Some(has_res);
        has_res
      }
    };
    if !has_res {
      return Ok(None);
    }
    let spec = ClientIdSpec {
      client: window,
      mask: ClientIdMask::LOCAL_CLIENT_PID,
    };
    let cookie = self
      .connection
      .res_query_client_ids(&[spec])
      .map_err(connection_error)?;
    match cookie.reply() {
      Ok(reply) => Ok(reply.ids.iter().find_map(|id| {
        (id.spec.mask & ClientIdMask::LOCAL_CLIENT_PID
          != ClientIdMask::from(0u32))
        .then(|| id.value.first().copied())
        .flatten()
      })),
      Err(x11rb::errors::ReplyError::X11Error(_)) => Ok(None),
      Err(error) => Err(connection_error(error)),
    }
  }

  fn map_window(&mut self, window: Window) -> Result<(), XwmError> {
    self
      .connection
      .map_window(window)
      .map(drop)
      .map_err(connection_error)
  }

  fn configure_window(
    &mut self,
    window: Window,
    geometry: Geometry,
  ) -> Result<(), XwmError> {
    let aux = ConfigureWindowAux::new()
      .x(geometry.x)
      .y(geometry.y)
      .width(geometry.width)
      .height(geometry.height)
      .border_width(0);
    self
      .connection
      .configure_window(window, &aux)
      .map(drop)
      .map_err(connection_error)
  }

  fn send_configure_notify(
    &mut self,
    window: Window,
    geometry: Geometry,
  ) -> Result<(), XwmError> {
    let event = ConfigureNotifyEvent {
      response_type: CONFIGURE_NOTIFY_EVENT,
      sequence: 0,
      event: window,
      window,
      above_sibling: NONE,
      x: clamp_i16(geometry.x),
      y: clamp_i16(geometry.y),
      width: clamp_u16(geometry.width),
      height: clamp_u16(geometry.height),
      border_width: 0,
      override_redirect: false,
    };
    self
      .connection
      .send_event(false, window, EventMask::STRUCTURE_NOTIFY, event)
      .map(drop)
      .map_err(connection_error)
  }

  fn send_client_message(
    &mut self,
    window: Window,
    kind: u32,
    data: [u32; 5],
  ) -> Result<(), XwmError> {
    let event = ClientMessageEvent::new(32, window, kind, data);
    self
      .connection
      .send_event(false, window, EventMask::NO_EVENT, event)
      .map(drop)
      .map_err(connection_error)
  }

  fn kill_client(&mut self, window: Window) -> Result<(), XwmError> {
    self
      .connection
      .kill_client(window)
      .map(drop)
      .map_err(connection_error)
  }

  fn set_input_focus(&mut self, window: Window) -> Result<u16, XwmError> {
    self
      .connection
      .set_input_focus(InputFocus::POINTER_ROOT, window, CURRENT_TIME)
      .map(|cookie| cookie.sequence_number() as u16)
      .map_err(connection_error)
  }

  fn stack_above(
    &mut self,
    window: Window,
    sibling: Option<Window>,
  ) -> Result<(), XwmError> {
    let mut aux = ConfigureWindowAux::new().stack_mode(StackMode::ABOVE);
    if let Some(sibling) = sibling {
      aux = aux.sibling(sibling);
    }
    self
      .connection
      .configure_window(window, &aux)
      .map(drop)
      .map_err(connection_error)
  }

  fn set_root_cursor(&mut self, image: &CursorImage) -> Result<(), XwmError> {
    let request =
      |error: x11rb::errors::ReplyError| XwmError::Request(error.to_string());
    let formats = self
      .connection
      .render_query_pict_formats()
      .map_err(connection_error)?
      .reply()
      .map_err(request)?;
    let format = formats
      .formats
      .iter()
      .find(|format| is_argb32(format))
      .map(|format| format.id)
      .ok_or_else(|| XwmError::Request("no ARGB32 picture format".into()))?;
    let connection = &self.connection;
    let id = || connection.generate_id().map_err(connection_error);
    let (pixmap, picture, gc, cursor) = (id()?, id()?, id()?, id()?);
    connection
      .create_pixmap(32, pixmap, self.root, image.width, image.height)
      .map_err(connection_error)?;
    connection
      .render_create_picture(picture, pixmap, format, &CreatePictureAux::new())
      .map_err(connection_error)?;
    connection
      .create_gc(gc, pixmap, &CreateGCAux::new())
      .map_err(connection_error)?;
    connection
      .put_image(
        ImageFormat::Z_PIXMAP,
        pixmap,
        gc,
        image.width,
        image.height,
        0,
        0,
        0,
        32,
        &image.argb,
      )
      .map_err(connection_error)?;
    connection.free_gc(gc).map_err(connection_error)?;
    connection
      .render_create_cursor(cursor, picture, image.hotspot_x, image.hotspot_y)
      .map_err(connection_error)?;
    connection.free_pixmap(pixmap).map_err(connection_error)?;
    connection
      .render_free_picture(picture)
      .map_err(connection_error)?;
    connection
      .change_window_attributes(
        self.root,
        &ChangeWindowAttributesAux::new().cursor(cursor),
      )
      .map_err(connection_error)?;
    if let Some(old) = self.cursor.replace(cursor) {
      self.connection.free_cursor(old).map_err(connection_error)?;
    }
    Ok(())
  }

  fn destroy_window(&mut self, window: Window) -> Result<(), XwmError> {
    self
      .connection
      .destroy_window(window)
      .map(drop)
      .map_err(connection_error)
  }

  fn select_selection_input(
    &mut self,
    window: Window,
    selection: u32,
  ) -> Result<(), XwmError> {
    let mask = SelectionEventMask::SET_SELECTION_OWNER
      | SelectionEventMask::SELECTION_WINDOW_DESTROY
      | SelectionEventMask::SELECTION_CLIENT_CLOSE;
    self
      .connection
      .xfixes_select_selection_input(window, selection, mask)
      .map(drop)
      .map_err(connection_error)
  }

  fn convert_selection(
    &mut self,
    requestor: Window,
    selection: u32,
    target: u32,
    property: u32,
    time: u32,
  ) -> Result<(), XwmError> {
    self
      .connection
      .convert_selection(requestor, selection, target, property, time)
      .map(drop)
      .map_err(connection_error)
  }

  fn take_property(
    &mut self,
    window: Window,
    property: u32,
  ) -> Result<Option<Property>, XwmError> {
    let cookie = self
      .connection
      .get_property(
        true,
        window,
        property,
        x11rb::protocol::xproto::AtomEnum::ANY,
        0,
        SELECTION_PROPERTY_LIMIT,
      )
      .map_err(connection_error)?;
    match cookie.reply() {
      Ok(reply) if reply.type_ != NONE => Ok(Some(Property {
        kind: reply.type_,
        format: reply.format,
        value: reply.value,
      })),
      Ok(_) | Err(x11rb::errors::ReplyError::X11Error(_)) => Ok(None),
      Err(error) => Err(connection_error(error)),
    }
  }

  fn atom_names(
    &mut self,
    atoms: &[u32],
  ) -> Result<Vec<Option<String>>, XwmError> {
    let cookies = atoms
      .iter()
      .map(|atom| self.connection.get_atom_name(*atom))
      .collect::<Result<Vec<_>, _>>()
      .map_err(connection_error)?;
    let mut names = Vec::with_capacity(cookies.len());
    for cookie in cookies {
      match cookie.reply() {
        Ok(reply) => {
          names.push(Some(String::from_utf8_lossy(&reply.name).into_owned()));
        }
        Err(x11rb::errors::ReplyError::X11Error(_)) => names.push(None),
        Err(error) => return Err(connection_error(error)),
      }
    }
    Ok(names)
  }

  fn send_selection_notify(
    &mut self,
    requestor: Window,
    selection: u32,
    target: u32,
    property: u32,
    time: u32,
  ) -> Result<(), XwmError> {
    let event = SelectionNotifyEvent {
      response_type: SELECTION_NOTIFY_EVENT,
      sequence: 0,
      time,
      requestor,
      selection,
      target,
      property,
    };
    self
      .connection
      .send_event(false, requestor, EventMask::NO_EVENT, event)
      .map(drop)
      .map_err(connection_error)
  }

  fn flush(&mut self) -> Result<(), XwmError> {
    self.connection.flush().map_err(connection_error)
  }

  fn poll_for_event(&mut self) -> Result<Option<Event>, XwmError> {
    self.connection.poll_for_event().map_err(connection_error)
  }
}

/// Whether a Render picture format is ARGB32 with the byte layout
/// [`CursorImage`] uses.
fn is_argb32(format: &Pictforminfo) -> bool {
  let direct = &format.direct;
  format.type_ == PictType::DIRECT
    && format.depth == 32
    && (direct.alpha_shift, direct.alpha_mask) == (24, 0xff)
    && (direct.red_shift, direct.red_mask) == (16, 0xff)
    && (direct.green_shift, direct.green_mask) == (8, 0xff)
    && (direct.blue_shift, direct.blue_mask) == (0, 0xff)
}

/// `value` clamped into an X coordinate.
fn clamp_i16(value: i32) -> i16 {
  value.clamp(i16::MIN.into(), i16::MAX.into()) as i16
}

/// `value` clamped into an X size.
fn clamp_u16(value: u32) -> u16 {
  value.min(u16::MAX.into()) as u16
}

/// The name Composite is registered under.
pub const COMPOSITE: &str = composite::X11_EXTENSION_NAME;

/// The name XFixes is registered under.
pub const XFIXES: &str = x11rb::protocol::xfixes::X11_EXTENSION_NAME;

/// `AtomEnum::WINDOW`, the type of window-valued properties.
pub const WINDOW_TYPE: u32 = 33;

/// `AtomEnum::ATOM`, the type of atom-valued properties.
pub const ATOM_TYPE: u32 = 4;

/// `AtomEnum::INTEGER`, the type of `TIMESTAMP` replies.
pub const INTEGER_TYPE: u32 = 19;

/// `AtomEnum::STRING`, a predefined atom and the Latin-1 text target.
pub const STRING_ATOM: u32 = 31;
