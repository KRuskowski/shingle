use std::collections::HashMap;

use wayland_protocols::wp::pointer_gestures::zv1::server::{
  zwp_pointer_gesture_hold_v1::{self, ZwpPointerGestureHoldV1},
  zwp_pointer_gesture_pinch_v1::{self, ZwpPointerGesturePinchV1},
  zwp_pointer_gesture_swipe_v1::{self, ZwpPointerGestureSwipeV1},
  zwp_pointer_gestures_v1::{self, ZwpPointerGesturesV1},
};
use wayland_server::{
  Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
  backend::{ClientId, ObjectId},
  protocol::wl_surface::WlSurface,
};

use crate::{
  input,
  input_device::{InputDeviceId, InputDeviceKind},
  state::CompositorState,
};

/// One step of a touchpad gesture.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Gesture {
  /// The gesture began with `fingers` fingers.
  Begin {
    /// The number of fingers.
    fingers: u32,
  },
  /// The gesture moved. Swipes use only `dx` and `dy`.
  Update {
    /// Horizontal motion of the centre.
    dx: f64,
    /// Vertical motion of the centre.
    dy: f64,
    /// Pinch scale relative to the start.
    scale: f64,
    /// Pinch rotation in degrees since the last update.
    rotation: f64,
  },
  /// The gesture ended, or was cancelled.
  End {
    /// Whether it was cancelled.
    cancelled: bool,
  },
}

/// The kind of a touchpad gesture, as `zwp_pointer_gestures_v1` has them.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PointerGesture {
  /// A swipe.
  Swipe,
  /// A pinch.
  Pinch,
  /// A hold. Its updates are ignored.
  Hold,
}

/// Feeds one step of a `kind` gesture of pointer `device` to the clients,
/// as the backend's touchpad would.
pub(crate) fn feed(
  state: &mut CompositorState,
  device: InputDeviceId,
  kind: PointerGesture,
  gesture: Gesture,
) {
  match (kind, gesture) {
    (PointerGesture::Swipe, Gesture::Begin { fingers }) => {
      swipe_begin(state, device, fingers)
    }
    (PointerGesture::Swipe, Gesture::Update { dx, dy, .. }) => {
      swipe_update(state, device, dx, dy)
    }
    (PointerGesture::Swipe, Gesture::End { cancelled }) => {
      swipe_end(state, device, cancelled)
    }
    (PointerGesture::Pinch, Gesture::Begin { fingers }) => {
      pinch_begin(state, device, fingers)
    }
    (
      PointerGesture::Pinch,
      Gesture::Update {
        dx,
        dy,
        scale,
        rotation,
      },
    ) => pinch_update(state, device, dx, dy, scale, rotation),
    (PointerGesture::Pinch, Gesture::End { cancelled }) => {
      pinch_end(state, device, cancelled)
    }
    (PointerGesture::Hold, Gesture::Begin { fingers }) => {
      hold_begin(state, device, fingers)
    }
    (PointerGesture::Hold, Gesture::Update { .. }) => {}
    (PointerGesture::Hold, Gesture::End { cancelled }) => {
      hold_end(state, device, cancelled)
    }
  }
}

#[derive(Default)]
pub(crate) struct PointerGestures {
  swipes: HashMap<ObjectId, Vec<ZwpPointerGestureSwipeV1>>,
  pinches: HashMap<ObjectId, Vec<ZwpPointerGesturePinchV1>>,
  holds: HashMap<ObjectId, Vec<ZwpPointerGestureHoldV1>>,
  /// The active gesture and the pointer device that began it.
  active: Option<(GestureKind, InputDeviceId)>,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum GestureKind {
  Swipe,
  Pinch,
  Hold,
}

pub(crate) fn create_global(handle: &DisplayHandle) {
  handle.create_global::<CompositorState, ZwpPointerGesturesV1, ()>(3, ());
}

impl GlobalDispatch<ZwpPointerGesturesV1, ()> for CompositorState {
  crate::security_context::protocol_can_view!("zwp_pointer_gestures_v1");

  fn bind(
    _state: &mut Self,
    _handle: &DisplayHandle,
    _client: &Client,
    resource: New<ZwpPointerGesturesV1>,
    _global_data: &(),
    data_init: &mut DataInit<'_, Self>,
  ) {
    data_init.init(resource, ());
  }
}

impl Dispatch<ZwpPointerGesturesV1, ()> for CompositorState {
  fn request(
    state: &mut Self,
    _client: &Client,
    _resource: &ZwpPointerGesturesV1,
    request: zwp_pointer_gestures_v1::Request,
    _data: &(),
    _dhandle: &DisplayHandle,
    data_init: &mut DataInit<'_, Self>,
  ) {
    match request {
      zwp_pointer_gestures_v1::Request::Release => {}
      zwp_pointer_gestures_v1::Request::GetSwipeGesture { id, pointer } => {
        let gesture = data_init.init(id, ());
        state
          .pointer_gestures
          .swipes
          .entry(pointer.id())
          .or_default()
          .push(gesture);
      }
      zwp_pointer_gestures_v1::Request::GetPinchGesture { id, pointer } => {
        let gesture = data_init.init(id, ());
        state
          .pointer_gestures
          .pinches
          .entry(pointer.id())
          .or_default()
          .push(gesture);
      }
      zwp_pointer_gestures_v1::Request::GetHoldGesture { id, pointer } => {
        let gesture = data_init.init(id, ());
        state
          .pointer_gestures
          .holds
          .entry(pointer.id())
          .or_default()
          .push(gesture);
      }
      _ => {}
    }
  }
}

macro_rules! gesture_dispatch {
  ($type:ty, $request:ty, $field:ident) => {
    impl Dispatch<$type, ()> for CompositorState {
      fn request(
        state: &mut Self,
        _client: &Client,
        resource: &$type,
        _request: $request,
        _data: &(),
        _dhandle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
      ) {
        remove_gesture(state, resource.id(), |gestures| &mut gestures.$field);
      }

      fn destroyed(
        state: &mut Self,
        _client: ClientId,
        resource: &$type,
        _data: &(),
      ) {
        remove_gesture(state, resource.id(), |gestures| &mut gestures.$field);
      }
    }
  };
}

gesture_dispatch!(
  ZwpPointerGestureSwipeV1,
  zwp_pointer_gesture_swipe_v1::Request,
  swipes
);
gesture_dispatch!(
  ZwpPointerGesturePinchV1,
  zwp_pointer_gesture_pinch_v1::Request,
  pinches
);
gesture_dispatch!(
  ZwpPointerGestureHoldV1,
  zwp_pointer_gesture_hold_v1::Request,
  holds
);

fn remove_gesture<T: Resource>(
  state: &mut CompositorState,
  id: ObjectId,
  field: impl FnOnce(&mut PointerGestures) -> &mut HashMap<ObjectId, Vec<T>>,
) {
  for gestures in field(&mut state.pointer_gestures).values_mut() {
    gestures.retain(|gesture| gesture.id() != id);
  }
}

pub(crate) fn pointer_destroyed(
  state: &mut CompositorState,
  pointer: &ObjectId,
) {
  state.pointer_gestures.swipes.remove(pointer);
  state.pointer_gestures.pinches.remove(pointer);
  state.pointer_gestures.holds.remove(pointer);
}

pub(crate) fn swipe_begin(
  state: &mut CompositorState,
  device: InputDeviceId,
  fingers: u32,
) {
  if !begin(state, device, GestureKind::Swipe) {
    return;
  }
  let time = timestamp(state);
  let serial = state.serial();
  for (gesture, surface) in swipes(state) {
    gesture.begin(serial, time, &surface, fingers);
  }
}

pub(crate) fn swipe_update(
  state: &mut CompositorState,
  device: InputDeviceId,
  dx: f64,
  dy: f64,
) {
  if !active(state, device, GestureKind::Swipe) {
    return;
  }
  let time = timestamp(state);
  for (gesture, _) in swipes(state) {
    gesture.update(time, dx, dy);
  }
}

pub(crate) fn swipe_end(
  state: &mut CompositorState,
  device: InputDeviceId,
  cancelled: bool,
) {
  if !end(state, device, GestureKind::Swipe) {
    return;
  }
  let time = timestamp(state);
  let serial = state.serial();
  for (gesture, _) in swipes(state) {
    gesture.end(serial, time, i32::from(cancelled));
  }
}

pub(crate) fn pinch_begin(
  state: &mut CompositorState,
  device: InputDeviceId,
  fingers: u32,
) {
  if !begin(state, device, GestureKind::Pinch) {
    return;
  }
  let time = timestamp(state);
  let serial = state.serial();
  for (gesture, surface) in pinches(state) {
    gesture.begin(serial, time, &surface, fingers);
  }
}

pub(crate) fn pinch_update(
  state: &mut CompositorState,
  device: InputDeviceId,
  dx: f64,
  dy: f64,
  scale: f64,
  rotation: f64,
) {
  if !active(state, device, GestureKind::Pinch) {
    return;
  }
  let time = timestamp(state);
  for (gesture, _) in pinches(state) {
    gesture.update(time, dx, dy, scale, rotation);
  }
}

pub(crate) fn pinch_end(
  state: &mut CompositorState,
  device: InputDeviceId,
  cancelled: bool,
) {
  if !end(state, device, GestureKind::Pinch) {
    return;
  }
  let time = timestamp(state);
  let serial = state.serial();
  for (gesture, _) in pinches(state) {
    gesture.end(serial, time, i32::from(cancelled));
  }
}

pub(crate) fn hold_begin(
  state: &mut CompositorState,
  device: InputDeviceId,
  fingers: u32,
) {
  if !begin(state, device, GestureKind::Hold) {
    return;
  }
  let time = timestamp(state);
  let serial = state.serial();
  for (gesture, surface) in holds(state) {
    gesture.begin(serial, time, &surface, fingers);
  }
}

pub(crate) fn hold_end(
  state: &mut CompositorState,
  device: InputDeviceId,
  cancelled: bool,
) {
  if !end(state, device, GestureKind::Hold) {
    return;
  }
  let time = timestamp(state);
  let serial = state.serial();
  for (gesture, _) in holds(state) {
    gesture.end(serial, time, i32::from(cancelled));
  }
}

/// Ends the active gesture as cancelled if `device` began it. Used when that
/// pointer is disabled or removed mid-gesture.
pub(crate) fn cancel_active(
  state: &mut CompositorState,
  device: InputDeviceId,
) {
  match state.pointer_gestures.active {
    Some((GestureKind::Swipe, owner)) if owner == device => {
      swipe_end(state, device, true);
    }
    Some((GestureKind::Pinch, owner)) if owner == device => {
      pinch_end(state, device, true);
    }
    Some((GestureKind::Hold, owner)) if owner == device => {
      hold_end(state, device, true);
    }
    _ => {}
  }
}

#[cfg(test)]
pub(crate) fn is_active(state: &CompositorState) -> bool {
  state.pointer_gestures.active.is_some()
}

/// Gestures come from a seat pointer, so a disabled pointer or an inactive
/// session starts, updates and ends none.
fn pointer_enabled(state: &CompositorState, device: InputDeviceId) -> bool {
  crate::input_device::delivers(state, device, InputDeviceKind::Pointer)
}

fn begin(
  state: &mut CompositorState,
  device: InputDeviceId,
  kind: GestureKind,
) -> bool {
  if !pointer_enabled(state, device) {
    return false;
  }
  input::activity(state);
  if state.pointer_gestures.active.is_some() {
    return false;
  }
  state.pointer_gestures.active = Some((kind, device));
  true
}

/// Ends the active gesture if it is of `kind` and `device` began it.
fn end(
  state: &mut CompositorState,
  device: InputDeviceId,
  kind: GestureKind,
) -> bool {
  if !active(state, device, kind) {
    return false;
  }
  state.pointer_gestures.active = None;
  true
}

/// Returns whether the active gesture is of `kind` and `device` began it.
fn active(
  state: &mut CompositorState,
  device: InputDeviceId,
  kind: GestureKind,
) -> bool {
  if !pointer_enabled(state, device) {
    return false;
  }
  input::activity(state);
  state.pointer_gestures.active == Some((kind, device))
}

fn focused_pointers(state: &CompositorState) -> Vec<(ObjectId, WlSurface)> {
  let Some((_, surface)) =
    state.input.pointer_focus.as_ref().and_then(|object| {
      state.surfaces.get(object).map(|surface| (object, surface))
    })
  else {
    return Vec::new();
  };
  state
    .input
    .pointers
    .values()
    .filter(|pointer| {
      pointer.client().map(|client| client.id()) == Some(surface.client.clone())
    })
    .map(|pointer| (pointer.id(), surface.resource.clone()))
    .collect()
}

fn swipes(
  state: &CompositorState,
) -> Vec<(ZwpPointerGestureSwipeV1, WlSurface)> {
  focused_pointers(state)
    .into_iter()
    .flat_map(|(pointer, surface)| {
      state
        .pointer_gestures
        .swipes
        .get(&pointer)
        .into_iter()
        .flatten()
        .map(move |gesture| (gesture.clone(), surface.clone()))
    })
    .collect()
}

fn pinches(
  state: &CompositorState,
) -> Vec<(ZwpPointerGesturePinchV1, WlSurface)> {
  focused_pointers(state)
    .into_iter()
    .flat_map(|(pointer, surface)| {
      state
        .pointer_gestures
        .pinches
        .get(&pointer)
        .into_iter()
        .flatten()
        .map(move |gesture| (gesture.clone(), surface.clone()))
    })
    .collect()
}

fn holds(state: &CompositorState) -> Vec<(ZwpPointerGestureHoldV1, WlSurface)> {
  focused_pointers(state)
    .into_iter()
    .flat_map(|(pointer, surface)| {
      state
        .pointer_gestures
        .holds
        .get(&pointer)
        .into_iter()
        .flatten()
        .map(move |gesture| (gesture.clone(), surface.clone()))
    })
    .collect()
}

fn timestamp(state: &CompositorState) -> u32 {
  crate::input::timestamp(state)
}
