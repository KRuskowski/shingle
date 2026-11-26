//! The host side of the clipboard bridge: a Wayland connection of its own
//! to the host compositor, bound to its `ext_data_control_manager_v1` or,
//! without one, its `zwlr_data_control_manager_v1`.
//!
//! Nothing here waits for the host. [`HostClipboard::connect`] only asks
//! for the globals; the protocol is chosen once their list has arrived
//! ([`choose`]), and every turn reads what the host has sent without
//! blocking ([`HostClipboard::dispatch`]) and flushes what Shingle has to
//! say ([`HostClipboard::sync`]).

use std::{
  collections::HashMap,
  error::Error,
  io::ErrorKind,
  os::{fd::AsFd, unix::net::UnixStream},
  path::{Path, PathBuf},
};

use shingle::SelectionKind;
use wayland_client::{
  Connection, Dispatch, EventQueue, Proxy, QueueHandle,
  backend::{ObjectId, WaylandError},
  protocol::{
    wl_callback::{self, WlCallback},
    wl_registry::{self, WlRegistry},
    wl_seat::{self, WlSeat},
  },
};
use wayland_protocols::ext::data_control::v1::client::{
  ext_data_control_device_v1::{self, ExtDataControlDeviceV1},
  ext_data_control_manager_v1::{self, ExtDataControlManagerV1},
  ext_data_control_offer_v1::{self, ExtDataControlOfferV1},
  ext_data_control_source_v1::{self, ExtDataControlSourceV1},
};
use wayland_protocols_wlr::data_control::v1::client::{
  zwlr_data_control_device_v1::{self, ZwlrDataControlDeviceV1},
  zwlr_data_control_manager_v1::{self, ZwlrDataControlManagerV1},
  zwlr_data_control_offer_v1::{self, ZwlrDataControlOfferV1},
  zwlr_data_control_source_v1::{self, ZwlrDataControlSourceV1},
};

use super::{Bridge, HostEvent, HostSide, NestedSide};

/// The ext protocol's interface name.
const EXT_MANAGER: &str = "ext_data_control_manager_v1";
/// The wlr protocol's interface name.
const WLR_MANAGER: &str = "zwlr_data_control_manager_v1";

/// The host's socket: `WAYLAND_DISPLAY`, relative to `XDG_RUNTIME_DIR`
/// unless it is absolute, as libwayland finds it. `None` when the host is
/// not a Wayland session, or when only `WAYLAND_SOCKET` names it, since
/// that socket is the window's and can be used only once.
pub(crate) fn host_socket(
  env: impl Fn(&str) -> Option<String>,
) -> Option<PathBuf> {
  let display = env("WAYLAND_DISPLAY").filter(|value| !value.is_empty())?;
  let path = Path::new(&display);
  if path.is_absolute() {
    return Some(path.to_owned());
  }
  let runtime = env("XDG_RUNTIME_DIR").filter(|value| !value.is_empty())?;
  Some(Path::new(&runtime).join(path))
}

/// A global the host announced.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Global {
  /// Its name in the registry.
  pub(crate) name: u32,
  /// Its interface.
  pub(crate) interface: String,
  /// Its version.
  pub(crate) version: u32,
}

/// The data-control protocol to use.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Manager {
  /// `ext_data_control_manager_v1`, by its global name.
  Ext(u32),
  /// `zwlr_data_control_manager_v1`, by its global name and the version
  /// to bind.
  Wlr(u32, u32),
}

/// What to bind on the host.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Choice {
  /// The first seat's global name.
  pub(crate) seat: u32,
  /// The data-control manager.
  pub(crate) manager: Manager,
}

impl Choice {
  /// Whether the protocol has a primary selection: ext always, wlr from
  /// version 2.
  pub(crate) fn has_primary(self) -> bool {
    match self.manager {
      Manager::Ext(_) => true,
      Manager::Wlr(_, version) => version >= 2,
    }
  }
}

/// Picks the first seat and ext-data-control, else wlr-data-control at
/// version 2 at most; `None` without a seat or either protocol.
pub(crate) fn choose(globals: &[Global]) -> Option<Choice> {
  let find = |interface: &str| {
    globals
      .iter()
      .find(|global| global.interface == interface && global.version >= 1)
  };
  let seat = find(WlSeat::interface().name)?.name;
  let manager = match (find(EXT_MANAGER), find(WLR_MANAGER)) {
    (Some(ext), _) => Manager::Ext(ext.name),
    (None, Some(wlr)) => Manager::Wlr(wlr.name, wlr.version.min(2)),
    (None, None) => return None,
  };
  Some(Choice { seat, manager })
}

/// One of Shingle's data-control sources on the host.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum HostSource {
  /// An ext-data-control source.
  Ext(ExtDataControlSourceV1),
  /// A wlr-data-control source.
  Wlr(ZwlrDataControlSourceV1),
}

/// A data-control offer from the host.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum HostOffer {
  /// An ext-data-control offer.
  Ext(ExtDataControlOfferV1),
  /// A wlr-data-control offer.
  Wlr(ZwlrDataControlOfferV1),
}

impl HostOffer {
  fn id(&self) -> ObjectId {
    match self {
      Self::Ext(offer) => offer.id(),
      Self::Wlr(offer) => offer.id(),
    }
  }
}

/// The bound protocol objects.
enum Protocol {
  Ext(ExtDataControlManagerV1, ExtDataControlDeviceV1),
  Wlr(ZwlrDataControlManagerV1, ZwlrDataControlDeviceV1),
}

/// The bound protocol and the queue its new objects go to: the
/// [`HostSide`] the bridge drives.
struct Bound {
  protocol: Protocol,
  queue: QueueHandle<State>,
}

impl HostSide for Bound {
  type Source = HostSource;
  type Offer = HostOffer;

  fn offer(
    &mut self,
    kind: SelectionKind,
    mime_types: &[String],
  ) -> HostSource {
    match &self.protocol {
      Protocol::Ext(manager, device) => {
        let source = manager.create_data_source(&self.queue, ());
        for mime_type in mime_types {
          source.offer(mime_type.clone());
        }
        match kind {
          SelectionKind::Clipboard => device.set_selection(Some(&source)),
          SelectionKind::Primary => device.set_primary_selection(Some(&source)),
        }
        HostSource::Ext(source)
      }
      Protocol::Wlr(manager, device) => {
        let source = manager.create_data_source(&self.queue, ());
        for mime_type in mime_types {
          source.offer(mime_type.clone());
        }
        match kind {
          SelectionKind::Clipboard => device.set_selection(Some(&source)),
          SelectionKind::Primary => device.set_primary_selection(Some(&source)),
        }
        HostSource::Wlr(source)
      }
    }
  }

  fn withdraw(&mut self, source: HostSource) {
    match source {
      HostSource::Ext(source) => source.destroy(),
      HostSource::Wlr(source) => source.destroy(),
    }
  }

  fn receive(
    &mut self,
    offer: &HostOffer,
    mime_type: String,
    fd: std::os::fd::OwnedFd,
  ) {
    // The request carries a duplicate of `fd`, which closes here.
    match offer {
      HostOffer::Ext(offer) => offer.receive(mime_type, fd.as_fd()),
      HostOffer::Wlr(offer) => offer.receive(mime_type, fd.as_fd()),
    }
  }

  fn discard(&mut self, offer: HostOffer) {
    match offer {
      HostOffer::Ext(offer) => offer.destroy(),
      HostOffer::Wlr(offer) => offer.destroy(),
    }
  }
}

/// What the host's events leave for [`HostClipboard`].
#[derive(Default)]
struct State {
  /// The globals announced so far.
  globals: Vec<Global>,
  /// The global list is complete.
  globals_done: bool,
  /// The MIME types of each offer not yet named as a selection.
  offer_types: HashMap<ObjectId, Vec<String>>,
  /// Events for the bridge, in order.
  events: Vec<HostEvent<HostSource, HostOffer>>,
  /// The device's seat went away.
  finished: bool,
}

impl State {
  /// Records the host's selection `kind`, with the MIME types its offer
  /// announced.
  fn selection(&mut self, kind: SelectionKind, offer: Option<HostOffer>) {
    let mime_types = offer
      .as_ref()
      .and_then(|offer| self.offer_types.remove(&offer.id()))
      .unwrap_or_default();
    self.events.push(HostEvent::Selection {
      kind,
      offer,
      mime_types,
    });
  }

  /// Records an offer's MIME type.
  fn offered(&mut self, offer: ObjectId, mime_type: String) {
    let types = self.offer_types.entry(offer).or_default();
    if !types.contains(&mime_type) {
      types.push(mime_type);
    }
  }
}

impl Dispatch<WlRegistry, ()> for State {
  fn event(
    state: &mut Self,
    _: &WlRegistry,
    event: wl_registry::Event,
    _: &(),
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
    if let wl_registry::Event::Global {
      name,
      interface,
      version,
    } = event
    {
      state.globals.push(Global {
        name,
        interface,
        version,
      });
    }
  }
}

impl Dispatch<WlCallback, ()> for State {
  fn event(
    state: &mut Self,
    _: &WlCallback,
    event: wl_callback::Event,
    _: &(),
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
    if let wl_callback::Event::Done { .. } = event {
      state.globals_done = true;
    }
  }
}

impl Dispatch<WlSeat, ()> for State {
  fn event(
    _: &mut Self,
    _: &WlSeat,
    _: wl_seat::Event,
    _: &(),
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
  }
}

/// The dispatch of one data-control protocol's objects; ext and wlr have
/// the same messages under different names.
macro_rules! dispatch_data_control {
  (
    $variant:ident,
    $manager:ident::$Manager:ident,
    $device:ident::$Device:ident,
    $offer:ident::$Offer:ident,
    $source:ident::$Source:ident $(,)?
  ) => {
    impl Dispatch<$Manager, ()> for State {
      fn event(
        _: &mut Self,
        _: &$Manager,
        _: $manager::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
      ) {
      }
    }

    impl Dispatch<$Device, ()> for State {
      fn event(
        state: &mut Self,
        _: &$Device,
        event: $device::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
      ) {
        match event {
          $device::Event::DataOffer { id } => {
            state.offer_types.insert(id.id(), Vec::new());
          }
          $device::Event::Selection { id } => {
            let offer = id.map(HostOffer::$variant);
            state.selection(SelectionKind::Clipboard, offer);
          }
          $device::Event::PrimarySelection { id } => {
            let offer = id.map(HostOffer::$variant);
            state.selection(SelectionKind::Primary, offer);
          }
          $device::Event::Finished => state.finished = true,
          _ => {}
        }
      }

      wayland_client::event_created_child!(State, $Device, [
        $device::EVT_DATA_OFFER_OPCODE => ($Offer, ()),
      ]);
    }

    impl Dispatch<$Offer, ()> for State {
      fn event(
        state: &mut Self,
        offer: &$Offer,
        event: $offer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
      ) {
        if let $offer::Event::Offer { mime_type } = event {
          state.offered(offer.id(), mime_type);
        }
      }
    }

    impl Dispatch<$Source, ()> for State {
      fn event(
        state: &mut Self,
        source: &$Source,
        event: $source::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
      ) {
        let source = HostSource::$variant(source.clone());
        match event {
          $source::Event::Send { mime_type, fd } => {
            state.events.push(HostEvent::Send {
              source,
              mime_type,
              fd,
            });
          }
          $source::Event::Cancelled => {
            state.events.push(HostEvent::Cancelled { source });
          }
          _ => {}
        }
      }
    }
  };
}

dispatch_data_control!(
  Ext,
  ext_data_control_manager_v1::ExtDataControlManagerV1,
  ext_data_control_device_v1::ExtDataControlDeviceV1,
  ext_data_control_offer_v1::ExtDataControlOfferV1,
  ext_data_control_source_v1::ExtDataControlSourceV1,
);
dispatch_data_control!(
  Wlr,
  zwlr_data_control_manager_v1::ZwlrDataControlManagerV1,
  zwlr_data_control_device_v1::ZwlrDataControlDeviceV1,
  zwlr_data_control_offer_v1::ZwlrDataControlOfferV1,
  zwlr_data_control_source_v1::ZwlrDataControlSourceV1,
);

/// Whether a connection error only means "not now".
fn would_block(error: &WaylandError) -> bool {
  match error {
    WaylandError::Io(error) => error.kind() == ErrorKind::WouldBlock,
    WaylandError::Protocol(_) => false,
  }
}

/// The bridge's connection to the host, and the bridge itself once the
/// host's protocol is known.
pub(crate) struct HostClipboard<Id> {
  connection: Connection,
  queue: EventQueue<State>,
  state: State,
  registry: WlRegistry,
  /// The selections nested wants to share.
  wanted: Vec<SelectionKind>,
  /// The bound protocol and the bridge, once chosen.
  bound: Option<(Bound, Bridge<HostSource, HostOffer, Id>)>,
  /// The host offers no data-control, or its device went: nothing more
  /// happens.
  idle: bool,
}

impl<Id: Clone + PartialEq> HostClipboard<Id> {
  /// Connects to the host compositor through `stream` and asks for its
  /// globals, to share selections `wanted` with it. Nothing waits for the
  /// host's answer.
  ///
  /// # Errors
  ///
  /// When the host cannot be talked to.
  pub(crate) fn connect(
    stream: UnixStream,
    wanted: Vec<SelectionKind>,
  ) -> Result<Self, Box<dyn Error>> {
    let connection = Connection::from_socket(stream)?;
    let queue = connection.new_event_queue();
    let handle = queue.handle();
    let display = connection.display();
    let registry = display.get_registry(&handle, ());
    display.sync(&handle, ());
    match connection.flush() {
      Err(error) if !would_block(&error) => return Err(error.into()),
      _ => {}
    }
    Ok(Self {
      connection,
      queue,
      state: State::default(),
      registry,
      wanted,
      bound: None,
      idle: false,
    })
  }

  /// The selections shared, once the protocol is known: the wanted ones
  /// the host's protocol has.
  #[cfg(test)]
  pub(crate) fn shared(&self) -> Option<Vec<SelectionKind>> {
    self.bound.as_ref().map(|(_, bridge)| bridge.kinds())
  }

  /// Whether the bridge has stopped for good: the host offers no
  /// data-control, or its device went.
  pub(crate) fn idle(&self) -> bool {
    self.idle
  }

  /// Reads and applies what the host has sent, without waiting.
  ///
  /// # Errors
  ///
  /// When the connection to the host fails.
  pub(crate) fn dispatch<N: NestedSide<Id = Id>>(
    &mut self,
    nested: &mut N,
  ) -> Result<(), Box<dyn Error>> {
    if self.idle {
      return Ok(());
    }
    self.queue.dispatch_pending(&mut self.state)?;
    if let Some(guard) = self.queue.prepare_read() {
      match guard.read() {
        Err(error) if !would_block(&error) => return Err(error.into()),
        _ => {}
      }
    }
    self.queue.dispatch_pending(&mut self.state)?;
    if self.bound.is_none() && self.state.globals_done {
      self.bind();
    }
    let events = std::mem::take(&mut self.state.events);
    if let Some((bound, bridge)) = &mut self.bound {
      for event in events {
        bridge.host_event(bound, nested, event);
      }
      if self.state.finished {
        eprintln!(
          "shingle: the host's data-control device went; the clipboard is \
           no longer shared with the host"
        );
        bridge.clear(bound, nested);
        self.bound = None;
        self.idle = true;
      }
    }
    Ok(())
  }

  /// Binds the seat and data-control manager [`choose`] picks, or logs
  /// once that there are none and goes idle.
  fn bind(&mut self) {
    let Some(choice) = choose(&self.state.globals) else {
      eprintln!(
        "shingle: the host offers neither {EXT_MANAGER} nor {WLR_MANAGER}; \
         the clipboard is not shared with the host"
      );
      self.idle = true;
      return;
    };
    let handle = self.queue.handle();
    let seat = self
      .registry
      .bind::<WlSeat, _, _>(choice.seat, 1, &handle, ());
    let protocol = match choice.manager {
      Manager::Ext(name) => {
        let manager = self.registry.bind::<ExtDataControlManagerV1, _, _>(
          name,
          1,
          &handle,
          (),
        );
        let device = manager.get_data_device(&seat, &handle, ());
        Protocol::Ext(manager, device)
      }
      Manager::Wlr(name, version) => {
        let manager = self.registry.bind::<ZwlrDataControlManagerV1, _, _>(
          name,
          version,
          &handle,
          (),
        );
        let device = manager.get_data_device(&seat, &handle, ());
        Protocol::Wlr(manager, device)
      }
    };
    let kinds = self
      .wanted
      .iter()
      .copied()
      .filter(|&kind| kind == SelectionKind::Clipboard || choice.has_primary())
      .collect::<Vec<_>>();
    let name = match choice.manager {
      Manager::Ext(_) => EXT_MANAGER,
      Manager::Wlr(..) => WLR_MANAGER,
    };
    eprintln!("shingle: sharing {kinds:?} with the host through {name}");
    self.bound = Some((
      Bound {
        protocol,
        queue: handle,
      },
      Bridge::new(&kinds),
    ));
  }

  /// Offers nested's changed selections to the host, passes nested's
  /// reads of host selections on and flushes the connection, without
  /// waiting.
  ///
  /// # Errors
  ///
  /// When the connection to the host fails.
  pub(crate) fn sync<N: NestedSide<Id = Id>>(
    &mut self,
    nested: &mut N,
  ) -> Result<(), Box<dyn Error>> {
    if let Some((bound, bridge)) = &mut self.bound {
      bridge.sync(bound, nested);
    } else {
      // Reads of host sources cannot be made before the bridge installs
      // one, but drop any all the same.
      drop(nested.take_reads());
    }
    match self.connection.flush() {
      Err(error) if !would_block(&error) => Err(error.into()),
      _ => Ok(()),
    }
  }

  /// Lets go of the host's selections in nested, after the connection
  /// failed.
  pub(crate) fn abandon<N: NestedSide<Id = Id>>(&mut self, nested: &mut N) {
    if let Some((bound, bridge)) = &mut self.bound {
      bridge.clear(bound, nested);
    }
    self.bound = None;
    self.idle = true;
  }
}

#[cfg(test)]
mod tests;
