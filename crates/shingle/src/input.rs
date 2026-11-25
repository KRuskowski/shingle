use std::{
  collections::{BTreeSet, HashMap},
  os::fd::{AsFd, OwnedFd},
  time::Duration,
};

use rustix::fs::{MemfdFlags, memfd_create};
use shingle_core::{Direction, Rect, WindowId};
use shingle_idle::LockState;
use wayland_protocols_misc::zwp_input_method_v2::server::
    zwp_input_method_keyboard_grab_v2::ZwpInputMethodKeyboardGrabV2;
use wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
    backend::{ClientId, ObjectId},
    protocol::{
        wl_keyboard::{self, WlKeyboard},
        wl_pointer::{self, WlPointer},
        wl_seat::{self, WlSeat},
        wl_touch::{self, WlTouch},
    },
};

use crate::{
  shell::{self, Role, XdgKind},
  state::CompositorState,
};

const DEFAULT_KEYMAP: &str = r#"xkb_keymap {
  xkb_keycodes "us" { include "evdev+aliases(qwerty)" };
  xkb_types "complete" { include "complete" };
  xkb_compatibility "complete" { include "complete" };
  xkb_symbols "us" { include "pc+us+inet(evdev)" };
};"#;

/// Serialized xkb modifier state, as `wl_keyboard.modifiers` sends it.
/// The frontend computes it with its own xkb state and passes it to
/// `Compositor::keyboard_modifiers`; the masks are only meaningful with the
/// keymap set by `Compositor::set_keymap`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Modifiers {
  /// Modifiers whose keys are held down.
  pub depressed: u32,
  /// Modifiers latched until the next key press, such as sticky keys.
  pub latched: u32,
  /// Locked modifiers, such as Caps Lock and Num Lock.
  pub locked: u32,
  /// The effective layout (xkb group) index.
  pub group: u32,
}

struct TouchPoint {
  object: ObjectId,
  x: f64,
  y: f64,
}

pub(crate) struct InputState {
  pub(crate) keyboards: HashMap<ObjectId, WlKeyboard>,
  pub(crate) pointers: HashMap<ObjectId, WlPointer>,
  pub(crate) touches: HashMap<ObjectId, WlTouch>,
  active_touches: HashMap<i32, TouchPoint>,
  pub(crate) keyboard_focus: Option<ObjectId>,
  pub(crate) pointer_focus: Option<ObjectId>,
  pub(crate) pointer_x: f64,
  pub(crate) pointer_y: f64,
  pub(crate) cursor_surface: Option<ObjectId>,
  pub(crate) cursor_hotspot: (i32, i32),
  pub(crate) cursor_shape: Option<crate::cursor_shape::Shape>,
  /// Whether the client under the pointer has set a cursor, possibly an
  /// empty one, since the pointer entered its surface.
  pub(crate) cursor_set: bool,
  pub(crate) pointer_serial: u32,
  pointer_batch_depth: usize,
  pointer_event_time: Option<u32>,
  keyboard_event_time: Option<u32>,
  event_time: Option<Duration>,
  pub(crate) pressed_keys: BTreeSet<u32>,
  pub(crate) modifiers: Modifiers,
  keymap: OwnedFd,
  keymap_size: u32,
  layouts: Vec<String>,
  repeat_rate: i32,
  repeat_delay: i32,
  scroll_speed: f64,
  natural_scroll: bool,
  pub(crate) devices: crate::input_device::InputDevices,
}

impl InputState {
  pub(crate) fn new(locked_modifiers: u32) -> Result<Self, String> {
    let (compiled, layouts) = compile_keymap(DEFAULT_KEYMAP)?;
    let (keymap, keymap_size) = create_keymap(&compiled)?;
    Ok(Self {
      keyboards: HashMap::new(),
      pointers: HashMap::new(),
      touches: HashMap::new(),
      active_touches: HashMap::new(),
      keyboard_focus: None,
      pointer_focus: None,
      pointer_x: 0.0,
      pointer_y: 0.0,
      cursor_surface: None,
      cursor_hotspot: (0, 0),
      cursor_shape: None,
      cursor_set: false,
      pointer_serial: 0,
      pointer_batch_depth: 0,
      pointer_event_time: None,
      keyboard_event_time: None,
      event_time: None,
      pressed_keys: BTreeSet::new(),
      modifiers: Modifiers {
        locked: locked_modifiers,
        ..Modifiers::default()
      },
      keymap,
      keymap_size,
      layouts,
      repeat_rate: 40,
      repeat_delay: 400,
      scroll_speed: 1.0,
      natural_scroll: false,
      devices: Default::default(),
    })
  }
}

pub(crate) fn create_globals(handle: &DisplayHandle) {
  handle.create_global::<CompositorState, WlSeat, ()>(8, ());
}

pub(crate) fn set_scroll(
  state: &mut CompositorState,
  speed: f64,
  natural: bool,
) {
  if speed.is_finite() && speed >= 0.0 {
    state.input.scroll_speed = speed;
  }
  state.input.natural_scroll = natural;
}

pub(crate) fn scroll_config(state: &CompositorState) -> (f64, bool) {
  (state.input.scroll_speed, state.input.natural_scroll)
}

pub(crate) fn set_repeat(state: &mut CompositorState, rate: u32, delay: u32) {
  state.input.repeat_rate = rate.min(i32::MAX as u32) as i32;
  state.input.repeat_delay = delay.min(i32::MAX as u32) as i32;
  for keyboard in state.input.keyboards.values() {
    send_repeat(&state.input, keyboard);
  }
  crate::input_method::repeats_changed(state);
}

pub(crate) fn send_repeat(state: &InputState, keyboard: &WlKeyboard) {
  if keyboard.version() >= 4 {
    keyboard.repeat_info(state.repeat_rate, state.repeat_delay);
  }
}

pub(crate) fn send_repeat_v2(
  state: &InputState,
  keyboard: &ZwpInputMethodKeyboardGrabV2,
) {
  keyboard.repeat_info(state.repeat_rate, state.repeat_delay);
}

pub(crate) fn set_keymap(
  state: &mut CompositorState,
  keymap: &str,
) -> Result<(), String> {
  if keymap.as_bytes().contains(&0) {
    return Err("keymap cannot contain an interior null byte".into());
  }
  let (compiled, layouts) = compile_keymap(keymap)?;
  let (fd, size) = create_keymap(&compiled)?;
  state.input.keymap = fd;
  state.input.keymap_size = size;
  state.input.layouts = layouts;
  state.input.modifiers.group = 0;
  for keyboard in state.input.keyboards.values() {
    send_keymap(state, keyboard);
  }
  crate::input_method::keymaps_changed(state);
  keyboard_modifiers(state, state.input.modifiers);
  Ok(())
}

pub(crate) fn keyboard_layouts(state: &CompositorState) -> &[String] {
  &state.input.layouts
}

/// Compiles an XKB keymap, which may use `include` statements, and returns
/// the compiled text clients receive with its layout names.
///
/// Clients get the compiled form, which needs no XKB data on their side, as
/// from wlroots compositors: Wayfire compiles the configured keymap in
/// `wf::keyboard_t::reload_input_options` (`src/core/seat/keyboard.cpp`)
/// and hands it to `wlr_keyboard_set_keymap`, which sends the text of
/// `xkb_keymap_get_as_string`.
fn compile_keymap(keymap: &str) -> Result<(String, Vec<String>), String> {
  use xkbcommon::xkb;
  let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
  let keymap = xkb::Keymap::new_from_string(
    &context,
    keymap.to_owned(),
    xkb::KEYMAP_FORMAT_TEXT_V1,
    xkb::KEYMAP_COMPILE_NO_FLAGS,
  )
  .ok_or_else(|| "failed to compile XKB keymap".to_string())?;
  if keymap.num_layouts() == 0 {
    return Err("XKB keymap contains no layouts".into());
  }
  let layouts = (0..keymap.num_layouts())
    .map(|index| {
      let name = keymap.layout_get_name(index);
      if name.is_empty() {
        "unknown".into()
      } else {
        name.into()
      }
    })
    .collect();
  let compiled = keymap.get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1);
  if compiled.is_empty() {
    return Err("failed to serialize XKB keymap".into());
  }
  Ok((compiled, layouts))
}

fn create_keymap(keymap: &str) -> Result<(OwnedFd, u32), String> {
  let mut bytes = keymap.as_bytes().to_vec();
  bytes.push(0);
  let fd = memfd_create("shingle-keymap", MemfdFlags::CLOEXEC)
    .map_err(|error| error.to_string())?;
  let mut written = 0;
  while written < bytes.len() {
    written += rustix::io::write(&fd, &bytes[written..])
      .map_err(|error| error.to_string())?;
  }
  let size = u32::try_from(bytes.len().saturating_sub(1))
    .map_err(|_| "keymap exceeds protocol size".to_string())?;
  Ok((fd, size))
}

fn seek_keymap(state: &CompositorState) -> bool {
  rustix::fs::seek(state.input.keymap.as_fd(), rustix::fs::SeekFrom::Start(0))
    .is_ok()
}

pub(crate) fn send_keymap(state: &CompositorState, keyboard: &WlKeyboard) {
  if seek_keymap(state) {
    keyboard.keymap(
      wl_keyboard::KeymapFormat::XkbV1,
      state.input.keymap.as_fd(),
      state.input.keymap_size,
    );
  }
}

pub(crate) fn send_keymap_v2(
  state: &CompositorState,
  keyboard: &ZwpInputMethodKeyboardGrabV2,
) {
  if seek_keymap(state) {
    keyboard.keymap(
      wl_keyboard::KeymapFormat::XkbV1,
      state.input.keymap.as_fd(),
      state.input.keymap_size,
    );
  }
}

struct SeatData;
pub(crate) struct KeyboardData;
struct PointerData;
struct TouchData;

impl GlobalDispatch<WlSeat, ()> for CompositorState {
  crate::security_context::protocol_can_view!("wl_seat");

  fn bind(
    _state: &mut Self,
    _handle: &DisplayHandle,
    _client: &Client,
    resource: New<WlSeat>,
    _global_data: &(),
    data_init: &mut DataInit<'_, Self>,
  ) {
    let seat = data_init.init(resource, SeatData);
    if seat.version() >= 2 {
      seat.name("shingle".into());
    }
    seat.capabilities(
      wl_seat::Capability::Pointer
        | wl_seat::Capability::Keyboard
        | wl_seat::Capability::Touch,
    );
  }
}

impl Dispatch<WlSeat, SeatData> for CompositorState {
  fn request(
    state: &mut Self,
    _client: &Client,
    _resource: &WlSeat,
    request: wl_seat::Request,
    _data: &SeatData,
    _dhandle: &DisplayHandle,
    data_init: &mut DataInit<'_, Self>,
  ) {
    match request {
      wl_seat::Request::GetPointer { id } => {
        let pointer = data_init.init(id, PointerData);
        state.input.pointers.insert(pointer.id(), pointer);
        let focus = state.input.pointer_focus.take();
        set_pointer_focus(state, focus);
      }
      wl_seat::Request::GetKeyboard { id } => {
        let keyboard = data_init.init(id, KeyboardData);
        send_keymap(state, &keyboard);
        send_repeat(&state.input, &keyboard);
        state.input.keyboards.insert(keyboard.id(), keyboard);
        sync_focus(state);
        let focus = state.input.keyboard_focus.take();
        set_keyboard_focus(state, focus);
      }
      wl_seat::Request::GetTouch { id } => {
        let touch = data_init.init(id, TouchData);
        state.input.touches.insert(touch.id(), touch);
      }
      wl_seat::Request::Release => {}
      _ => {}
    }
  }
}

impl Dispatch<WlKeyboard, KeyboardData> for CompositorState {
  fn request(
    _state: &mut Self,
    _client: &Client,
    _resource: &WlKeyboard,
    request: wl_keyboard::Request,
    _data: &KeyboardData,
    _dhandle: &DisplayHandle,
    _data_init: &mut DataInit<'_, Self>,
  ) {
    if let wl_keyboard::Request::Release = request {}
  }

  fn destroyed(
    state: &mut Self,
    _client: ClientId,
    resource: &WlKeyboard,
    _data: &KeyboardData,
  ) {
    state.input.keyboards.remove(&resource.id());
    crate::input_method::keyboard_destroyed(state, &resource.id());
  }
}

impl Dispatch<WlPointer, PointerData> for CompositorState {
  fn request(
    state: &mut Self,
    _client: &Client,
    _resource: &WlPointer,
    request: wl_pointer::Request,
    _data: &PointerData,
    _dhandle: &DisplayHandle,
    _data_init: &mut DataInit<'_, Self>,
  ) {
    match request {
      wl_pointer::Request::SetCursor {
        surface,
        hotspot_x,
        hotspot_y,
        ..
      } => {
        state.input.cursor_hotspot = (hotspot_x, hotspot_y);
        state.input.cursor_surface = surface.as_ref().map(Resource::id);
        state.input.cursor_shape = None;
        state.input.cursor_set = true;
        if let Some(surface) = surface
          && let Some(surface) = state.surfaces.get_mut(&surface.id())
          && matches!(surface.role, Role::None | Role::Cursor)
        {
          surface.role = Role::Cursor;
        }
      }
      wl_pointer::Request::Release => {}
      _ => {}
    }
  }

  fn destroyed(
    state: &mut Self,
    _client: ClientId,
    resource: &WlPointer,
    _data: &PointerData,
  ) {
    let object = resource.id();
    state.input.pointers.remove(&object);
    crate::pointer_protocols::pointer_destroyed(state, &object);
    crate::pointer_gestures::pointer_destroyed(state, &object);
  }
}

impl Dispatch<WlTouch, TouchData> for CompositorState {
  fn request(
    _state: &mut Self,
    _client: &Client,
    _resource: &WlTouch,
    request: wl_touch::Request,
    _data: &TouchData,
    _dhandle: &DisplayHandle,
    _data_init: &mut DataInit<'_, Self>,
  ) {
    if let wl_touch::Request::Release = request {}
  }

  fn destroyed(
    state: &mut Self,
    _client: ClientId,
    resource: &WlTouch,
    _data: &TouchData,
  ) {
    state.input.touches.remove(&resource.id());
  }
}

pub(crate) fn sync_focus(state: &mut CompositorState) {
  match state.idle.lock_state() {
    LockState::Pending | LockState::Failed => {
      set_keyboard_focus(state, None);
      set_pointer_focus(state, None);
    }
    LockState::Locked => {
      let valid = state.input.keyboard_focus.as_ref().is_some_and(|object| {
        state.surfaces.get(object).is_some_and(|surface| {
          matches!(surface.role, Role::SessionLock { .. })
            && surface.buffer.is_some()
        })
      });
      if !valid {
        let target = state
          .visible_surfaces()
          .into_iter()
          .find_map(|snapshot| state.surface_ids.get(&snapshot.id).cloned());
        set_keyboard_focus(state, target.clone());
        set_pointer_focus(state, target);
      }
    }
    LockState::Unlocked => {
      if !state.show_desktop && crate::layer_shell::sync_exclusive_focus(state)
      {
        return;
      }
      let valid = !state.show_desktop
        && state.input.keyboard_focus.as_ref().is_some_and(|object| {
          state.surfaces.get(object).is_some_and(|surface| {
            role_accepts_keyboard(&surface.role)
              && surface.buffer.is_some()
              && window_for_object(state, object)
                .is_none_or(|window| !shell::is_minimized(state, window))
          })
        });
      if !valid {
        let target = if state.show_desktop {
          None
        } else {
          state
            .core
            .current_workspace(&state.output_id)
            .ok()
            .and_then(|workspace| {
              state.core.tree(&state.output_id, workspace).ok()
            })
            .and_then(|tree| tree.focused())
            .and_then(|window| object_for_window(state, window))
        };
        set_keyboard_focus(state, target.clone());
        set_pointer_focus(state, target);
      }
    }
  }
}

pub(crate) fn focus_at(
  state: &mut CompositorState,
  x: f64,
  y: f64,
) -> Option<WindowId> {
  let target = surface_at(state, x, y)?;
  focus_object(state, target)
}

pub(crate) fn focus_window(
  state: &mut CompositorState,
  window: WindowId,
) -> Option<WindowId> {
  if state.idle.lock_state() != LockState::Unlocked
    || state.show_desktop
    || shell::is_minimized(state, window)
  {
    return None;
  }
  let object = object_for_window(state, window)?;
  focus_object(state, object)
}

fn focus_object(
  state: &mut CompositorState,
  mut object: ObjectId,
) -> Option<WindowId> {
  if !input_allowed(state, &object) {
    return None;
  }
  let mut window = window_for_object(state, &object);
  if let Some(parent) = window {
    let target = topmost_modal_descendant(state, parent);
    if target != parent {
      object = object_for_window(state, target)?;
      window = Some(target);
    }
  }
  let pointer_target = object.clone();
  let keyboard_target = if state.focus_main_surface_instead_of_popup {
    main_surface(state, object)
  } else {
    object
  };
  set_keyboard_focus(state, Some(keyboard_target));
  set_pointer_focus(state, Some(pointer_target));
  if let Some(window) = window {
    if state.core.focus_window(window).is_ok() {
      state.focus_sequence = state.focus_sequence.wrapping_add(1).max(1);
      state.focus_recency.insert(window, state.focus_sequence);
      let _ = state.core.raise_window(window);
      if !state.all_dialogs_modal {
        raise_descendants(state, window);
      }
    }
    shell::reconfigure_all(state);
  }
  state.events.push(crate::Event::FocusChanged { window });
  window
}

/// Walks nested popups to their main surface for the optional Qt workaround.
fn main_surface(state: &CompositorState, mut object: ObjectId) -> ObjectId {
  loop {
    let Some(surface) = state.surfaces.get(&object) else {
      return object;
    };
    let Role::Xdg {
      kind: Some(XdgKind::Popup(popup)),
      ..
    } = &surface.role
    else {
      return object;
    };
    if popup.parent == object {
      return object;
    }
    object = popup.parent.clone();
  }
}

fn topmost_modal_descendant(
  state: &CompositorState,
  parent: WindowId,
) -> WindowId {
  let Ok(location) = state.core.window_location(parent) else {
    return parent;
  };
  state
    .core
    .tree(&location.output, location.workspace)
    .ok()
    .and_then(|tree| {
      tree.stack_order().iter().rev().copied().find(|candidate| {
        *candidate != parent
          && is_descendant(state, *candidate, parent)
          && (state.all_dialogs_modal
            || crate::dialog::is_modal(state, *candidate))
          && !shell::is_minimized(state, *candidate)
      })
    })
    .unwrap_or(parent)
}

fn is_descendant(
  state: &CompositorState,
  window: WindowId,
  ancestor: WindowId,
) -> bool {
  let mut parent = state
    .core
    .window_info(window)
    .ok()
    .and_then(|info| info.parent);
  while let Some(candidate) = parent {
    if candidate == ancestor {
      return true;
    }
    parent = state
      .core
      .window_info(candidate)
      .ok()
      .and_then(|info| info.parent);
  }
  false
}

fn raise_descendants(state: &mut CompositorState, parent: WindowId) {
  let descendants = state
    .core
    .window_location(parent)
    .ok()
    .and_then(|location| {
      state.core.tree(&location.output, location.workspace).ok()
    })
    .map(|tree| {
      tree
        .stack_order()
        .iter()
        .copied()
        .filter(|candidate| is_descendant(state, *candidate, parent))
        .collect::<Vec<_>>()
    })
    .unwrap_or_default();
  for window in descendants {
    let _ = state.core.raise_window(window);
  }
}

pub(crate) fn focus_directional(
  state: &mut CompositorState,
  direction: Direction,
) -> Result<Option<WindowId>, crate::Error> {
  let Some(current) = focused_window(state) else {
    return Ok(None);
  };
  let workspace = state.core.current_workspace(&state.output_id)?;
  let tree = state.core.tree(&state.output_id, workspace)?;
  let source = tree.window_layout(current)?;
  let next = tree
    .layout()
    .into_iter()
    .filter(|layout| layout.window != current)
    .filter(|layout| {
      tree
        .leaf_state(layout.window)
        .is_ok_and(|state| state.floating.is_none() && !state.minimized)
    })
    .filter_map(|layout| {
      direction
        .score(source.node, layout.node)
        .map(|(primary, cross)| (primary, cross, layout.window))
    })
    .min_by_key(|(primary, cross, window)| (*primary, *cross, *window))
    .map(|(_, _, window)| window);
  if let Some(window) = next {
    let current_state = state.core.window_info(current)?.state;
    if current_state.maximized {
      shell::set_maximized(state, current, false)?;
      shell::set_maximized(state, window, true)?;
    }
    if current_state.fullscreen {
      shell::set_fullscreen(state, current, false)?;
    }
    focus_window(state, window);
    if current_state.fullscreen && state.keep_fullscreen_on_adjacent {
      shell::set_fullscreen(state, window, true)?;
    }
  }
  Ok(next)
}

pub(crate) fn drop_window(
  state: &mut CompositorState,
  source: WindowId,
  target: WindowId,
  direction: Direction,
) -> Result<(), crate::Error> {
  state.core.drop_window(source, target, direction)?;
  shell::reconfigure_all(state);
  focus_window(state, source);
  Ok(())
}

pub(crate) fn resize_window(
  state: &mut CompositorState,
  window: WindowId,
  direction: Direction,
  delta: i32,
  minimum_extent: u32,
) -> Result<bool, crate::Error> {
  if minimum_extent == 0 {
    return Err(shingle_core::ShingleError::MinimumExtent.into());
  }
  if let Some(mut geometry) = state.core.window_info(window)?.state.floating {
    let original = geometry;
    let (client_minimum, client_maximum) =
      shell::size_constraints(state, window).unwrap_or(((0, 0), (0, 0)));
    match direction {
      Direction::Left | Direction::Right => {
        let minimum = i64::from(minimum_extent)
          .max(i64::from(client_minimum.0))
          .max(i64::from(state.resize.min_width));
        let maximum = if client_maximum.0 > 0 {
          i64::from(client_maximum.0).max(minimum)
        } else {
          i64::from(i32::MAX)
        };
        let width = if direction == Direction::Left {
          i64::from(geometry.width) - i64::from(delta)
        } else {
          i64::from(geometry.width) + i64::from(delta)
        }
        .clamp(minimum, maximum);
        if direction == Direction::Left {
          let right = i64::from(geometry.x) + i64::from(geometry.width);
          geometry.x = (right - width) as i32;
        }
        geometry.width = width as u32;
      }
      Direction::Up | Direction::Down => {
        let minimum = i64::from(minimum_extent)
          .max(i64::from(client_minimum.1))
          .max(i64::from(state.resize.min_height));
        let maximum = if client_maximum.1 > 0 {
          i64::from(client_maximum.1).max(minimum)
        } else {
          i64::from(i32::MAX)
        };
        let height = if direction == Direction::Up {
          i64::from(geometry.height) - i64::from(delta)
        } else {
          i64::from(geometry.height) + i64::from(delta)
        }
        .clamp(minimum, maximum);
        if direction == Direction::Up {
          let bottom = i64::from(geometry.y) + i64::from(geometry.height);
          geometry.y = (bottom - height) as i32;
        }
        geometry.height = height as u32;
      }
    }
    if geometry == original {
      return Ok(false);
    }
    state.core.set_floating(window, Some(geometry))?;
    shell::sync_floating_geometry(state, window, geometry);
    shell::reconfigure_all(state);
    return Ok(true);
  }
  let changed =
    state
      .core
      .resize_window(window, direction, delta, minimum_extent)?;
  shell::reconfigure_all(state);
  Ok(changed)
}

pub(crate) fn resize_floating_from(
  state: &mut CompositorState,
  window: WindowId,
  horizontal: Option<Direction>,
  vertical: Option<Direction>,
  original: Rect,
  delta_x: i32,
  delta_y: i32,
) -> Result<bool, crate::Error> {
  let minimum_extent = state.minimum_extent;
  if minimum_extent == 0 {
    return Err(shingle_core::ShingleError::MinimumExtent.into());
  }
  if state.core.window_info(window)?.state.floating.is_none() {
    return Ok(false);
  }
  let (client_minimum, client_maximum) =
    shell::size_constraints(state, window).unwrap_or(((0, 0), (0, 0)));
  let minimum_width = i64::from(minimum_extent)
    .max(i64::from(client_minimum.0))
    .max(i64::from(state.resize.min_width))
    .max(1);
  let minimum_height = i64::from(minimum_extent)
    .max(i64::from(client_minimum.1))
    .max(i64::from(state.resize.min_height))
    .max(1);
  let maximum_width = if client_maximum.0 > 0 {
    i64::from(client_maximum.0).max(minimum_width)
  } else {
    i64::from(i32::MAX)
  };
  let maximum_height = if client_maximum.1 > 0 {
    i64::from(client_maximum.1).max(minimum_height)
  } else {
    i64::from(i32::MAX)
  };

  let mut desired = original;
  match horizontal {
    Some(Direction::Left) => {
      desired.x = desired.x.saturating_add(delta_x);
      desired.width = (i64::from(desired.width) - i64::from(delta_x))
        .clamp(minimum_width, maximum_width) as u32;
    }
    Some(Direction::Right) => {
      desired.width = (i64::from(desired.width) + i64::from(delta_x))
        .clamp(minimum_width, maximum_width) as u32;
    }
    _ => {}
  }
  match vertical {
    Some(Direction::Up) => {
      desired.y = desired.y.saturating_add(delta_y);
      desired.height = (i64::from(desired.height) - i64::from(delta_y))
        .clamp(minimum_height, maximum_height) as u32;
    }
    Some(Direction::Down) => {
      desired.height = (i64::from(desired.height) + i64::from(delta_y))
        .clamp(minimum_height, maximum_height) as u32;
    }
    _ => {}
  }

  let unconstrained = desired;
  let ratio = f64::from(original.width) / f64::from(original.height);
  let width_delta = i64::from(desired.width) - i64::from(original.width);
  let height_delta = i64::from(desired.height) - i64::from(original.height);
  let shrinking = if width_delta.abs() > height_delta.abs() {
    width_delta < 0
  } else {
    height_delta < 0
  };
  let new_ratio = f64::from(desired.width) / f64::from(desired.height);
  let clamp_width =
    |width: f64| (width as i64).clamp(minimum_width, maximum_width) as u32;
  let clamp_height =
    |height: f64| (height as i64).clamp(minimum_height, maximum_height) as u32;
  if (new_ratio < ratio) != shrinking {
    desired.width = clamp_width(f64::from(desired.height) * ratio);
    desired.height = clamp_height(f64::from(desired.width) / ratio);
    desired.width = clamp_width(f64::from(desired.height) * ratio);
  } else {
    desired.height = clamp_height(f64::from(desired.width) / ratio);
    desired.width = clamp_width(f64::from(desired.height) * ratio);
    desired.height = clamp_height(f64::from(desired.width) / ratio);
  }
  desired.width =
    i64::from(desired.width).clamp(minimum_width, maximum_width) as u32;
  desired.height =
    i64::from(desired.height).clamp(minimum_height, maximum_height) as u32;
  if horizontal == Some(Direction::Left) {
    desired.x = desired.x.saturating_add(
      (i64::from(unconstrained.width) - i64::from(desired.width)) as i32,
    );
  }
  if vertical == Some(Direction::Up) {
    desired.y = desired.y.saturating_add(
      (i64::from(unconstrained.height) - i64::from(desired.height)) as i32,
    );
  }
  if desired == original {
    return Ok(false);
  }
  state.core.set_floating(window, Some(desired))?;
  shell::sync_floating_geometry(state, window, desired);
  shell::reconfigure_all(state);
  Ok(true)
}

pub(crate) fn toggle_maximized(
  state: &mut CompositorState,
  window: WindowId,
) -> Result<bool, crate::Error> {
  let maximized = !state.core.window_info(window)?.state.maximized;
  shell::set_maximized(state, window, maximized)?;
  Ok(maximized)
}

pub(crate) fn begin_pointer_batch(state: &mut CompositorState) {
  state.input.pointer_batch_depth += 1;
}

pub(crate) fn pointer_event_time(state: &mut CompositorState, time: u32) {
  state.input.pointer_event_time = Some(time);
}

pub(crate) fn end_pointer_batch(state: &mut CompositorState) {
  state.input.pointer_batch_depth =
    state.input.pointer_batch_depth.saturating_sub(1);
  state.input.pointer_event_time = None;
  if state.input.pointer_batch_depth > 0 {
    return;
  }
  if let Some(target) = state.input.pointer_focus.clone() {
    for pointer in pointers_for(state, &target) {
      pointer_frame(state, &pointer);
    }
  }
}

pub(crate) fn pointer_motion(state: &mut CompositorState, x: f64, y: f64) {
  let dx = x - state.input.pointer_x;
  let dy = y - state.input.pointer_y;
  pointer_motion_inner(state, x, y, Some((dx, dy, dx, dy)));
}

pub(crate) fn pointer_relative_motion(
  state: &mut CompositorState,
  dx: f64,
  dy: f64,
  dx_unaccel: f64,
  dy_unaccel: f64,
) {
  let x = state.input.pointer_x + dx;
  let y = state.input.pointer_y + dy;
  pointer_motion_inner(state, x, y, Some((dx, dy, dx_unaccel, dy_unaccel)));
}

pub(crate) fn pointer_warp(state: &mut CompositorState, x: f64, y: f64) {
  pointer_motion_inner(state, x, y, None);
}

fn pointer_motion_inner(
  state: &mut CompositorState,
  x: f64,
  y: f64,
  relative: Option<(f64, f64, f64, f64)>,
) {
  activity(state);
  if let Some((dx, dy, dx_unaccel, dy_unaccel)) = relative {
    crate::pointer_protocols::relative_motion(
      state, dx, dy, dx_unaccel, dy_unaccel,
    );
  }
  if relative.is_some() && crate::pointer_protocols::locked(state) {
    return;
  }
  let (x, y) = crate::pointer_protocols::constrain(state, x, y);
  state.input.pointer_x = x;
  state.input.pointer_y = y;
  let target = surface_at(state, x, y);
  set_pointer_focus(state, target.clone());
  let Some(target) = target else {
    return;
  };
  let Some((local_x, local_y)) = local_position(state, &target) else {
    return;
  };
  let time = timestamp(state);
  for pointer in pointers_for(state, &target) {
    pointer.motion(time, local_x, local_y);
    pointer_frame(state, &pointer);
  }
}

pub(crate) fn pointer_button(
  state: &mut CompositorState,
  button: u32,
  pressed: bool,
) {
  activity(state);
  let (x, y) = (state.input.pointer_x, state.input.pointer_y);
  let modifiers = state.input.modifiers;
  let modifiers_active =
    modifiers.depressed != 0 || modifiers.latched != 0 || modifiers.locked != 0;
  let previous_focus = focused_window(state);
  let button_name = match button {
    0x110 => Some("btn_left"),
    0x111 => Some("btn_right"),
    0x112 => Some("btn_middle"),
    0x113 => Some("btn_back"),
    0x114 => Some("btn_forward"),
    _ => None,
  };
  let focus_changed = if pressed
    && (state.focus.button_with_modifiers || !modifiers_active)
    && button_name.is_some_and(|name| {
      state
        .focus
        .buttons
        .iter()
        .any(|binding| binding.modifiers.is_empty() && binding.key == name)
    }) {
    focus_at(state, x, y).is_some_and(|window| Some(window) != previous_focus)
  } else {
    false
  };
  if focus_changed && !state.focus.buttons_passthrough {
    return;
  }
  let Some(target) = state.input.pointer_focus.clone() else {
    return;
  };
  let serial = state.serial();
  let time = timestamp(state);
  let button_state = if pressed {
    wl_pointer::ButtonState::Pressed
  } else {
    wl_pointer::ButtonState::Released
  };
  for pointer in pointers_for(state, &target) {
    pointer.button(serial, time, button, button_state);
    pointer_frame(state, &pointer);
  }
  if !pressed {
    crate::data_device::drop_drag(state);
  }
}

pub(crate) fn pointer_axis_detailed(
  state: &mut CompositorState,
  horizontal: f64,
  vertical: f64,
  horizontal_steps: i32,
  vertical_steps: i32,
) {
  activity(state);
  let Some(target) = state.input.pointer_focus.clone() else {
    return;
  };
  if !input_allowed(state, &target) {
    return;
  }
  let speed = state.input.scroll_speed;
  let direction = if state.input.natural_scroll { -1 } else { 1 };
  let horizontal = horizontal * speed * f64::from(direction);
  let vertical = vertical * speed * f64::from(direction);
  let (horizontal_steps, vertical_steps) = if speed == 0.0 {
    (0, 0)
  } else {
    (
      horizontal_steps.saturating_mul(direction),
      vertical_steps.saturating_mul(direction),
    )
  };
  let time = timestamp(state);
  for pointer in pointers_for(state, &target) {
    if pointer.version() >= 5 {
      pointer.axis_source(wl_pointer::AxisSource::Wheel);
    }
    for (axis, value, steps) in [
      (
        wl_pointer::Axis::HorizontalScroll,
        horizontal,
        horizontal_steps,
      ),
      (wl_pointer::Axis::VerticalScroll, vertical, vertical_steps),
    ] {
      if steps != 0 {
        if pointer.version() >= 5 {
          pointer.axis_discrete(axis, steps);
        }
        if pointer.version() >= 8 {
          pointer.axis_value120(axis, steps.saturating_mul(120));
        }
      }
      if value != 0.0 {
        pointer.axis(time, axis, value);
      }
    }
    pointer_frame(state, &pointer);
  }
}

pub(crate) fn pointer_axis_virtual(
  state: &mut CompositorState,
  axis: wl_pointer::Axis,
  value: f64,
  discrete: i32,
  source: Option<wl_pointer::AxisSource>,
  stop: bool,
) {
  activity(state);
  let Some(target) = state.input.pointer_focus.clone() else {
    return;
  };
  if !input_allowed(state, &target) {
    return;
  }
  let time = timestamp(state);
  for pointer in pointers_for(state, &target) {
    if let Some(source) = source
      && pointer.version() >= 5
    {
      pointer.axis_source(source);
    }
    if discrete != 0 {
      if pointer.version() >= 5 {
        pointer.axis_discrete(axis, discrete);
      }
      if pointer.version() >= 8 {
        pointer.axis_value120(axis, discrete.saturating_mul(120));
      }
    }
    if value != 0.0 {
      pointer.axis(time, axis, value);
    }
    if stop && pointer.version() >= 6 {
      pointer.axis_stop(time, axis);
    }
    pointer_frame(state, &pointer);
  }
}

pub(crate) fn touch_down(state: &mut CompositorState, id: i32, x: f64, y: f64) {
  activity(state);
  if state.input.active_touches.contains_key(&id) {
    return;
  }
  let Some(object) = surface_at(state, x, y) else {
    return;
  };
  let Some(surface) = state
    .surfaces
    .get(&object)
    .map(|surface| surface.resource.clone())
  else {
    return;
  };
  let Some(geometry) = shell::surface_geometry(state, &object) else {
    return;
  };
  let local_x = x - f64::from(geometry.x);
  let local_y = y - f64::from(geometry.y);
  state
    .input
    .active_touches
    .insert(id, TouchPoint { object, x, y });
  let serial = state.serial();
  let time = timestamp(state);
  for touch in touches_for(state, id) {
    touch.down(serial, time, &surface, id, local_x, local_y);
    touch_frame(&touch);
  }
}

pub(crate) fn touch_motion(
  state: &mut CompositorState,
  id: i32,
  x: f64,
  y: f64,
) {
  activity(state);
  let Some(point) = state.input.active_touches.get_mut(&id) else {
    return;
  };
  point.x = x;
  point.y = y;
  let object = point.object.clone();
  let Some(geometry) = shell::surface_geometry(state, &object) else {
    return;
  };
  let local_x = x - f64::from(geometry.x);
  let local_y = y - f64::from(geometry.y);
  let time = timestamp(state);
  for touch in touches_for(state, id) {
    touch.motion(time, id, local_x, local_y);
    touch_frame(&touch);
  }
}

pub(crate) fn touch(state: &mut CompositorState, id: i32, x: f64, y: f64) {
  if state.input.active_touches.contains_key(&id) {
    touch_motion(state, id, x, y);
  } else {
    touch_down(state, id, x, y);
  }
}

pub(crate) fn touch_up(state: &mut CompositorState, id: i32) {
  activity(state);
  let touches = touches_for(state, id);
  if state.input.active_touches.remove(&id).is_none() {
    return;
  }
  let serial = state.serial();
  let time = timestamp(state);
  for touch in touches {
    touch.up(serial, time, id);
    touch_frame(&touch);
  }
}

pub(crate) fn touch_cancel(state: &mut CompositorState, id: i32) {
  activity(state);
  let touches = touches_for(state, id);
  let Some(point) = state.input.active_touches.remove(&id) else {
    return;
  };
  // wl_touch.cancel ends every point the client has, so forget them all,
  // as wlroots' wlr_seat_touch_notify_cancel does for the client.
  let client = state
    .surfaces
    .get(&point.object)
    .map(|surface| surface.client.clone());
  state.input.active_touches.retain(|_, other| {
    state
      .surfaces
      .get(&other.object)
      .map(|surface| &surface.client)
      != client.as_ref()
  });
  for touch in touches {
    touch.cancel();
    touch_frame(&touch);
  }
}

fn touches_for(state: &CompositorState, id: i32) -> Vec<WlTouch> {
  let Some(client) = state
    .input
    .active_touches
    .get(&id)
    .and_then(|point| state.surfaces.get(&point.object))
    .map(|surface| surface.client.clone())
  else {
    return Vec::new();
  };
  state
    .input
    .touches
    .values()
    .filter(|touch| {
      touch.client().map(|client| client.id()).as_ref() == Some(&client)
    })
    .cloned()
    .collect()
}

fn touch_frame(touch: &WlTouch) {
  if touch.version() >= 6 {
    touch.frame();
  }
}

pub(crate) fn keyboard_key(
  state: &mut CompositorState,
  key: u32,
  pressed: bool,
) {
  keyboard_key_inner(state, key, pressed, None);
}

pub(crate) fn forward_keyboard_key(
  state: &mut CompositorState,
  key: u32,
  pressed: bool,
  time: u32,
) {
  keyboard_key_inner(state, key, pressed, Some(time));
}

pub(crate) fn virtual_keyboard_key(
  state: &mut CompositorState,
  key: u32,
  pressed: bool,
  time: u32,
) {
  keyboard_key_inner(state, key, pressed, Some(time));
}

fn keyboard_key_inner(
  state: &mut CompositorState,
  key: u32,
  pressed: bool,
  event_time: Option<u32>,
) {
  activity(state);
  state.input.keyboard_event_time = event_time;
  if crate::input_method::hardware_key(state, key, pressed, event_time) {
    state.input.keyboard_event_time = None;
    return;
  }
  let Some(target) = state.input.keyboard_focus.clone() else {
    state.input.keyboard_event_time = None;
    return;
  };
  if !input_allowed(state, &target) {
    state.input.keyboard_event_time = None;
    return;
  }
  let changed = if pressed {
    state.input.pressed_keys.insert(key)
  } else {
    state.input.pressed_keys.remove(&key)
  };
  if !changed {
    state.input.keyboard_event_time = None;
    return;
  }
  let serial = state.serial();
  let time = state
    .input
    .keyboard_event_time
    .unwrap_or_else(|| timestamp(state));
  state.input.keyboard_event_time = None;
  let key_state = if pressed {
    wl_keyboard::KeyState::Pressed
  } else {
    wl_keyboard::KeyState::Released
  };
  for keyboard in keyboards_for(state, &target) {
    keyboard.key(serial, time, key, key_state);
  }
}

pub(crate) fn keyboard_modifiers(
  state: &mut CompositorState,
  modifiers: Modifiers,
) {
  activity(state);
  state.input.modifiers = modifiers;
  let keyboard = crate::ipc::keyboard_state(state);
  state.ipc_events.push(serde_json::json!({
      "event": "keyboard-modifier-state-changed",
      "state": keyboard,
  }));
  if crate::input_method::hardware_modifiers(state, modifiers) {
    return;
  }
  let Some(target) = state.input.keyboard_focus.clone() else {
    return;
  };
  if input_allowed(state, &target) {
    send_modifiers(state, &target);
  }
}

fn set_keyboard_focus(state: &mut CompositorState, target: Option<ObjectId>) {
  if target
    .as_ref()
    .is_some_and(|object| !input_allowed(state, object))
  {
    return;
  }
  if state.input.keyboard_focus == target {
    return;
  }
  let old = state.input.keyboard_focus.take();
  if let Some(old) = old
    && let Some(resource) = state
      .surfaces
      .get(&old)
      .map(|surface| surface.resource.clone())
  {
    let serial = state.serial();
    for keyboard in keyboards_for(state, &old) {
      keyboard.leave(serial, &resource);
    }
  }
  state.input.pressed_keys.clear();
  let Some(target) = target else {
    return;
  };
  let Some(surface) = state.surfaces.get(&target) else {
    return;
  };
  let resource = surface.resource.clone();
  state.input.keyboard_focus = Some(target.clone());
  let serial = state.serial();
  let keys = state
    .input
    .pressed_keys
    .iter()
    .flat_map(|key| key.to_ne_bytes())
    .collect::<Vec<_>>();
  for keyboard in keyboards_for(state, &target) {
    keyboard.enter(serial, &resource, keys.clone());
  }
  send_modifiers(state, &target);
}

fn set_pointer_focus(state: &mut CompositorState, target: Option<ObjectId>) {
  if target
    .as_ref()
    .is_some_and(|object| !input_allowed(state, object))
  {
    return;
  }
  if state.input.pointer_focus == target {
    return;
  }
  state.input.cursor_set = false;
  let old = state.input.pointer_focus.take();
  if let Some(old) = old
    && let Some(resource) = state
      .surfaces
      .get(&old)
      .map(|surface| surface.resource.clone())
  {
    let serial = state.serial();
    for pointer in pointers_for(state, &old) {
      pointer.leave(serial, &resource);
      pointer_frame(state, &pointer);
    }
  }
  let Some(target) = target else {
    return;
  };
  let Some(surface) = state.surfaces.get(&target) else {
    return;
  };
  let resource = surface.resource.clone();
  let Some((x, y)) = local_position(state, &target) else {
    return;
  };
  state.input.pointer_focus = Some(target.clone());
  let serial = state.serial();
  state.input.pointer_serial = serial;
  for pointer in pointers_for(state, &target) {
    pointer.enter(serial, &resource, x, y);
    pointer_frame(state, &pointer);
  }
  crate::pointer_protocols::sync(state);
}

fn send_modifiers(state: &mut CompositorState, target: &ObjectId) {
  let serial = state.serial();
  let modifiers = state.input.modifiers;
  for keyboard in keyboards_for(state, target) {
    keyboard.modifiers(
      serial,
      modifiers.depressed,
      modifiers.latched,
      modifiers.locked,
      modifiers.group,
    );
  }
}

fn keyboards_for(
  state: &CompositorState,
  object: &ObjectId,
) -> Vec<WlKeyboard> {
  let Some(client) = state.surfaces.get(object).map(|surface| &surface.client)
  else {
    return Vec::new();
  };
  state
    .input
    .keyboards
    .values()
    .filter(|keyboard| {
      keyboard.client().map(|client| client.id()).as_ref() == Some(client)
    })
    .cloned()
    .collect()
}

fn pointers_for(state: &CompositorState, object: &ObjectId) -> Vec<WlPointer> {
  let Some(client) = state.surfaces.get(object).map(|surface| &surface.client)
  else {
    return Vec::new();
  };
  state
    .input
    .pointers
    .values()
    .filter(|pointer| {
      pointer.client().map(|client| client.id()).as_ref() == Some(client)
    })
    .cloned()
    .collect()
}

/// Returns whether a surface with this role may take keyboard focus while
/// the session is unlocked.
fn role_accepts_keyboard(role: &Role) -> bool {
  match role {
    Role::Xdg { kind: Some(_), .. } | Role::Subsurface { .. } => true,
    Role::Layer(layer) => layer.accepts_keyboard(),
    _ => false,
  }
}

fn input_allowed(state: &CompositorState, object: &ObjectId) -> bool {
  let Some(surface) = state.surfaces.get(object) else {
    return false;
  };
  match state.idle.lock_state() {
    LockState::Unlocked => role_accepts_keyboard(&surface.role),
    LockState::Locked => matches!(surface.role, Role::SessionLock { .. }),
    LockState::Pending | LockState::Failed => false,
  }
}

fn surface_at(state: &CompositorState, x: f64, y: f64) -> Option<ObjectId> {
  state
    .visible_surfaces()
    .into_iter()
    .rev()
    .filter(|snapshot| snapshot.role != crate::SurfaceRole::Cursor)
    .find_map(|snapshot| {
      if !contains(snapshot.geometry, x, y) {
        return None;
      }
      let object = state.surface_ids.get(&snapshot.id)?;
      let surface = state.surfaces.get(object)?;
      crate::region::contains(
        state,
        surface,
        x - f64::from(snapshot.geometry.x),
        y - f64::from(snapshot.geometry.y),
      )
      .then_some(object.clone())
    })
}

fn contains(rect: Rect, x: f64, y: f64) -> bool {
  x >= f64::from(rect.x)
    && y >= f64::from(rect.y)
    && x < f64::from(rect.x) + f64::from(rect.width)
    && y < f64::from(rect.y) + f64::from(rect.height)
}

fn local_position(
  state: &CompositorState,
  object: &ObjectId,
) -> Option<(f64, f64)> {
  let geometry = shell::surface_geometry(state, object)?;
  Some((
    state.input.pointer_x - f64::from(geometry.x),
    state.input.pointer_y - f64::from(geometry.y),
  ))
}

fn object_for_window(
  state: &CompositorState,
  window: WindowId,
) -> Option<ObjectId> {
  if state
    .core
    .window_info(window)
    .is_ok_and(|info| info.presence != shingle_core::WindowPresence::Mapped)
  {
    return None;
  }
  state.surfaces.iter().find_map(|(object, surface)| {
    let matches = surface.role.toplevel().is_some_and(|toplevel| {
      toplevel.window() == Some(window) && surface.buffer.is_some()
    });
    matches.then_some(object.clone())
  })
}

pub(crate) fn window_for_object(
  state: &CompositorState,
  object: &ObjectId,
) -> Option<WindowId> {
  let surface = state.surfaces.get(object)?;
  if let Some(toplevel) = surface.role.toplevel() {
    return toplevel.window();
  }
  match &surface.role {
    Role::Xdg {
      kind: Some(XdgKind::Popup(popup)),
      ..
    } => window_for_object(state, &popup.parent),
    Role::Subsurface { parent, .. } => window_for_object(state, parent),
    _ => None,
  }
}

pub(crate) fn window_at(
  state: &CompositorState,
  x: f64,
  y: f64,
) -> Option<WindowId> {
  let object = surface_at(state, x, y)?;
  window_for_object(state, &object)
}

pub(crate) fn focused_window(state: &CompositorState) -> Option<WindowId> {
  if state.show_desktop {
    return None;
  }
  if let Some(object) = &state.input.keyboard_focus {
    return window_for_object(state, object);
  }
  let workspace = state.core.current_workspace(&state.output_id).ok()?;
  state.core.tree(&state.output_id, workspace).ok()?.focused()
}

fn pointer_frame(state: &CompositorState, pointer: &WlPointer) {
  if state.input.pointer_batch_depth == 0 && pointer.version() >= 5 {
    pointer.frame();
  }
}

/// Sets the backend's time for the seat input that follows, or clears it.
pub(crate) fn set_event_time(
  state: &mut CompositorState,
  time: Option<Duration>,
) {
  state.input.event_time = time;
}

/// Returns the `CLOCK_MONOTONIC` time of the input being handled: the
/// backend's event time if one is set, otherwise now.
fn event_time(state: &CompositorState) -> Duration {
  state
    .input
    .event_time
    .unwrap_or_else(crate::clock::monotonic_now)
}

/// Returns the millisecond timestamp for an input event. A virtual
/// pointer's own request time wins over the seat's event time.
pub(crate) fn timestamp(state: &CompositorState) -> u32 {
  state
    .input
    .pointer_event_time
    .unwrap_or_else(|| crate::clock::millis(event_time(state)))
}

/// Returns the microsecond timestamp for relative pointer motion.
pub(crate) fn timestamp_usec(state: &CompositorState) -> u64 {
  match state.input.pointer_event_time {
    Some(millis) => u64::from(millis) * 1000,
    None => event_time(state).as_micros() as u64,
  }
}

pub(crate) fn activity(state: &mut CompositorState) {
  let commands = state.idle.activity(state.now());
  state.process_idle_commands(commands);
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::input_device::tests::test_state;

  #[test]
  fn timestamps_default_to_the_monotonic_clock() {
    let state = test_state();
    let before = crate::clock::monotonic_now();
    let usec = timestamp_usec(&state);
    let after = crate::clock::monotonic_now();
    assert!(before.as_micros() as u64 <= usec);
    assert!(usec <= after.as_micros() as u64);
    let millis = timestamp(&state);
    assert!(millis >= crate::clock::millis(before));
  }

  #[test]
  fn backend_event_time_is_used_until_cleared() {
    let mut state = test_state();
    set_event_time(&mut state, Some(Duration::from_micros(123_456_789)));
    assert_eq!(timestamp(&state), 123_456);
    assert_eq!(timestamp_usec(&state), 123_456_789);
    set_event_time(&mut state, None);
    assert_ne!(timestamp_usec(&state), 123_456_789);
  }

  /// A `us,de` keymap as `include` statements, as shingle-config writes it.
  const US_DE: &str = r#"xkb_keymap {
  xkb_keycodes { include "evdev+aliases(qwerty)" };
  xkb_types { include "complete" };
  xkb_compatibility { include "complete" };
  xkb_symbols { include "pc+us+de:2+inet(evdev)" };
};"#;

  /// Compiles a keymap a client received with no XKB data at all, as a
  /// client without the compositor's XKB files would, and returns its
  /// layout names.
  fn compiled_layouts(keymap: &str) -> Vec<String> {
    use xkbcommon::xkb;
    assert!(
      !keymap.contains("include"),
      "the keymap is not compiled:\n{keymap}"
    );
    let context = xkb::Context::new(
      xkb::CONTEXT_NO_DEFAULT_INCLUDES | xkb::CONTEXT_NO_ENVIRONMENT_NAMES,
    );
    let keymap = xkb::Keymap::new_from_string(
      &context,
      keymap.to_owned(),
      xkb::KEYMAP_FORMAT_TEXT_V1,
      xkb::KEYMAP_COMPILE_NO_FLAGS,
    )
    .expect("the keymap compiles without XKB data");
    (0..keymap.num_layouts())
      .map(|index| keymap.layout_get_name(index).to_owned())
      .collect()
  }

  fn us_de() -> Vec<String> {
    vec!["English (US)".to_owned(), "German".to_owned()]
  }

  #[test]
  fn touch_cancel_ends_every_point_of_the_client() {
    use wayland_client::protocol::{wl_seat::WlSeat, wl_touch};
    let (mut compositor, path) = crate::tests::test_compositor();
    let device = compositor.add_input_device(crate::InputDeviceInfo {
      name: "test touch".into(),
      vendor: None,
      product: None,
      kind: crate::InputDeviceKind::Touch,
    });
    let mut client =
      crate::test_client::TestClient::connect(&mut compositor, &path);
    let seat = client.bind::<WlSeat>(7);
    let _touch = seat.get_touch(&client.handle, ());
    let toplevel = client.toplevel(&mut compositor, "touched");
    client.attach_buffer(&mut compositor, &toplevel.surface, 64, 64);
    compositor.touch_down(device, 1, 10.0, 10.0);
    compositor.touch_down(device, 2, 20.0, 20.0);
    // wl_touch.cancel ends the client's whole sequence, both points.
    compositor.touch_cancel(device, 1);
    client.roundtrip(&mut compositor);
    client.events.touch.clear();

    // So point 2 is up for the client: lifting it sends nothing, and a new
    // touch with its ID starts a new point.
    compositor.touch_up(device, 2);
    client.roundtrip(&mut compositor);
    assert!(
      client
        .events
        .touch
        .iter()
        .all(|event| matches!(event, wl_touch::Event::Frame)),
      "{:?}",
      client.events.touch
    );
    compositor.touch_down(device, 2, 30.0, 30.0);
    client.roundtrip(&mut compositor);
    assert!(
      client
        .events
        .touch
        .iter()
        .any(|event| matches!(event, wl_touch::Event::Down { id: 2, .. })),
      "{:?}",
      client.events.touch
    );
  }

  #[test]
  fn wl_keyboard_receives_compiled_keymaps() {
    use wayland_client::protocol::wl_seat::WlSeat;
    let (mut compositor, path) = crate::tests::test_compositor();
    let mut client =
      crate::test_client::TestClient::connect(&mut compositor, &path);
    let seat = client.bind::<WlSeat>(7);
    let _keyboard = seat.get_keyboard(&client.handle, ());
    client.roundtrip(&mut compositor);
    assert_eq!(client.events.keymaps.len(), 1);
    assert_eq!(
      compiled_layouts(&client.events.keymaps[0]),
      vec!["English (US)".to_owned()]
    );

    // A live config reload sends the new keymap to bound keyboards.
    compositor.set_keymap(US_DE).unwrap();
    client.roundtrip(&mut compositor);
    assert_eq!(client.events.keymaps.len(), 2);
    assert_eq!(compiled_layouts(&client.events.keymaps[1]), us_de());
  }

  #[test]
  fn virtual_keyboard_keymaps_reach_clients_compiled() {
    use wayland_client::protocol::wl_seat::WlSeat;
    use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::
      zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1;
    let (mut compositor, path) = crate::tests::test_compositor();
    let mut client =
      crate::test_client::TestClient::connect(&mut compositor, &path);
    let seat = client.bind::<WlSeat>(7);
    let _keyboard = seat.get_keyboard(&client.handle, ());
    let manager = client.bind::<ZwpVirtualKeyboardManagerV1>(1);
    let virtual_keyboard =
      manager.create_virtual_keyboard(&seat, &client.handle, ());
    let fd = memfd_create("shingle-test-keymap", MemfdFlags::CLOEXEC).unwrap();
    rustix::io::pwrite(&fd, US_DE.as_bytes(), 0).unwrap();
    virtual_keyboard.keymap(1, fd.as_fd(), US_DE.len() as u32);
    client.roundtrip(&mut compositor);
    let keymap = client.events.keymaps.last().expect("a keymap was sent");
    assert_eq!(compiled_layouts(keymap), us_de());
  }

  #[test]
  fn input_method_keyboard_grabs_receive_compiled_keymaps() {
    use wayland_client::protocol::wl_seat::WlSeat;
    use wayland_protocols_misc::zwp_input_method_v2::client::
      zwp_input_method_manager_v2::ZwpInputMethodManagerV2;
    let (mut compositor, path) = crate::tests::test_compositor();
    let mut client =
      crate::test_client::TestClient::connect(&mut compositor, &path);
    let seat = client.bind::<WlSeat>(7);
    let manager = client.bind::<ZwpInputMethodManagerV2>(1);
    let input_method = manager.get_input_method(&seat, &client.handle, ());
    let _grab = input_method.grab_keyboard(&client.handle, ());
    client.roundtrip(&mut compositor);
    assert_eq!(client.events.grab_keymaps.len(), 1);
    assert_eq!(
      compiled_layouts(&client.events.grab_keymaps[0]),
      vec!["English (US)".to_owned()]
    );

    compositor.set_keymap(US_DE).unwrap();
    client.roundtrip(&mut compositor);
    assert_eq!(client.events.grab_keymaps.len(), 2);
    assert_eq!(compiled_layouts(&client.events.grab_keymaps[1]), us_de());
  }

  #[test]
  fn virtual_pointer_request_time_wins_over_backend_time() {
    let mut state = test_state();
    set_event_time(&mut state, Some(Duration::from_millis(5_000)));
    pointer_event_time(&mut state, 42);
    assert_eq!(timestamp(&state), 42);
    assert_eq!(timestamp_usec(&state), 42_000);
    end_pointer_batch(&mut state);
    assert_eq!(timestamp(&state), 5_000);
  }
}
