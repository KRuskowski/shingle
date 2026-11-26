use std::collections::HashMap;

use wayland_protocols_wlr::virtual_pointer::v1::server::{
  zwlr_virtual_pointer_manager_v1::{self, ZwlrVirtualPointerManagerV1},
  zwlr_virtual_pointer_v1::{self, ZwlrVirtualPointerV1},
};
use wayland_server::{
  Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
  WEnum,
  backend::{ClientId, ObjectId},
  protocol::wl_pointer,
};

use crate::{input, state::CompositorState};

#[derive(Default)]
pub(crate) struct VirtualPointers {
  devices: HashMap<ObjectId, VirtualPointer>,
}

struct VirtualPointer {
  pending: Vec<VirtualEvent>,
  axis_source: wl_pointer::AxisSource,
}

enum VirtualEvent {
  Motion {
    time: u32,
    dx: f64,
    dy: f64,
  },
  Absolute {
    time: u32,
    x: u32,
    y: u32,
    x_extent: u32,
    y_extent: u32,
  },
  Button {
    time: u32,
    button: u32,
    pressed: bool,
  },
  Axis {
    time: u32,
    axis: wl_pointer::Axis,
    value: f64,
    discrete: i32,
  },
  AxisStop {
    time: u32,
    axis: wl_pointer::Axis,
  },
  AxisSource(wl_pointer::AxisSource),
}

pub(crate) fn create_global(handle: &DisplayHandle) {
  handle
    .create_global::<CompositorState, ZwlrVirtualPointerManagerV1, ()>(2, ());
}

impl GlobalDispatch<ZwlrVirtualPointerManagerV1, ()> for CompositorState {
  crate::security_context::protocol_can_view!(
    "zwlr_virtual_pointer_manager_v1"
  );

  fn bind(
    _state: &mut Self,
    _handle: &DisplayHandle,
    _client: &Client,
    resource: New<ZwlrVirtualPointerManagerV1>,
    _global_data: &(),
    data_init: &mut DataInit<'_, Self>,
  ) {
    data_init.init(resource, ());
  }
}

impl Dispatch<ZwlrVirtualPointerManagerV1, ()> for CompositorState {
  fn request(
    state: &mut Self,
    _client: &Client,
    _resource: &ZwlrVirtualPointerManagerV1,
    request: zwlr_virtual_pointer_manager_v1::Request,
    _data: &(),
    _dhandle: &DisplayHandle,
    data_init: &mut DataInit<'_, Self>,
  ) {
    use zwlr_virtual_pointer_manager_v1::Request;
    let id = match request {
      Request::Destroy => return,
      Request::CreateVirtualPointer { id, .. }
      | Request::CreateVirtualPointerWithOutput { id, .. } => id,
      _ => return,
    };
    let pointer = data_init.init(id, ());
    state.virtual_pointers.devices.insert(
      pointer.id(),
      VirtualPointer {
        pending: Vec::new(),
        axis_source: wl_pointer::AxisSource::Wheel,
      },
    );
  }
}

impl Dispatch<ZwlrVirtualPointerV1, ()> for CompositorState {
  fn request(
    state: &mut Self,
    _client: &Client,
    resource: &ZwlrVirtualPointerV1,
    request: zwlr_virtual_pointer_v1::Request,
    _data: &(),
    _dhandle: &DisplayHandle,
    _data_init: &mut DataInit<'_, Self>,
  ) {
    match request {
      zwlr_virtual_pointer_v1::Request::Destroy => {
        state.virtual_pointers.devices.remove(&resource.id());
      }
      zwlr_virtual_pointer_v1::Request::Motion { time, dx, dy } => {
        push(state, resource, VirtualEvent::Motion { time, dx, dy });
      }
      zwlr_virtual_pointer_v1::Request::MotionAbsolute {
        time,
        x,
        y,
        x_extent,
        y_extent,
      } => push(
        state,
        resource,
        VirtualEvent::Absolute {
          time,
          x,
          y,
          x_extent,
          y_extent,
        },
      ),
      zwlr_virtual_pointer_v1::Request::Button {
        time,
        button,
        state: button_state,
      } => {
        let WEnum::Value(button_state) = button_state else {
          return;
        };
        push(
          state,
          resource,
          VirtualEvent::Button {
            time,
            button,
            pressed: button_state == wl_pointer::ButtonState::Pressed,
          },
        );
      }
      zwlr_virtual_pointer_v1::Request::Axis { time, axis, value } => {
        if let Some(axis) = valid_axis(resource, axis) {
          push(
            state,
            resource,
            VirtualEvent::Axis {
              time,
              axis,
              value,
              discrete: 0,
            },
          );
        }
      }
      zwlr_virtual_pointer_v1::Request::AxisDiscrete {
        time,
        axis,
        value,
        discrete,
      } => {
        if let Some(axis) = valid_axis(resource, axis) {
          push(
            state,
            resource,
            VirtualEvent::Axis {
              time,
              axis,
              value,
              discrete,
            },
          );
        }
      }
      zwlr_virtual_pointer_v1::Request::AxisSource { axis_source } => {
        let WEnum::Value(source) = axis_source else {
          resource.post_error(
            zwlr_virtual_pointer_v1::Error::InvalidAxisSource,
            "unknown pointer axis source",
          );
          return;
        };
        push(state, resource, VirtualEvent::AxisSource(source));
      }
      zwlr_virtual_pointer_v1::Request::AxisStop { time, axis } => {
        if let Some(axis) = valid_axis(resource, axis) {
          push(state, resource, VirtualEvent::AxisStop { time, axis });
        }
      }
      zwlr_virtual_pointer_v1::Request::Frame => apply_frame(state, resource),
      _ => {}
    }
  }

  fn destroyed(
    state: &mut Self,
    _client: ClientId,
    resource: &ZwlrVirtualPointerV1,
    _data: &(),
  ) {
    state.virtual_pointers.devices.remove(&resource.id());
  }
}

fn valid_axis(
  resource: &ZwlrVirtualPointerV1,
  axis: WEnum<wl_pointer::Axis>,
) -> Option<wl_pointer::Axis> {
  match axis {
    WEnum::Value(axis) => Some(axis),
    WEnum::Unknown(_) => {
      resource.post_error(
        zwlr_virtual_pointer_v1::Error::InvalidAxis,
        "unknown pointer axis",
      );
      None
    }
  }
}

fn push(
  state: &mut CompositorState,
  resource: &ZwlrVirtualPointerV1,
  event: VirtualEvent,
) {
  if let Some(pointer) = state.virtual_pointers.devices.get_mut(&resource.id())
  {
    pointer.pending.push(event);
  }
}

fn apply_frame(state: &mut CompositorState, resource: &ZwlrVirtualPointerV1) {
  let Some((events, mut source)) = state
    .virtual_pointers
    .devices
    .get_mut(&resource.id())
    .map(|pointer| (std::mem::take(&mut pointer.pending), pointer.axis_source))
  else {
    return;
  };
  input::begin_pointer_batch(state);
  for event in events {
    match event {
      VirtualEvent::Motion { time, dx, dy } => {
        input::pointer_event_time(state, time);
        input::pointer_relative_motion(state, dx, dy, dx, dy);
      }
      VirtualEvent::Absolute {
        time,
        x,
        y,
        x_extent,
        y_extent,
      } => {
        input::pointer_event_time(state, time);
        if x_extent > 0 && y_extent > 0 {
          let output = state.primary().rect;
          let x = f64::from(output.x)
            + f64::from(output.width) * f64::from(x.min(x_extent))
              / f64::from(x_extent);
          let y = f64::from(output.y)
            + f64::from(output.height) * f64::from(y.min(y_extent))
              / f64::from(y_extent);
          input::pointer_motion(state, x, y);
        }
      }
      VirtualEvent::Button {
        time,
        button,
        pressed,
      } => {
        input::pointer_event_time(state, time);
        input::pointer_button(state, button, pressed);
      }
      VirtualEvent::Axis {
        time,
        axis,
        value,
        discrete,
      } => {
        input::pointer_event_time(state, time);
        input::pointer_axis_virtual(
          state,
          axis,
          value,
          discrete,
          Some(source),
          false,
        );
      }
      VirtualEvent::AxisStop { time, axis } => {
        input::pointer_event_time(state, time);
        input::pointer_axis_virtual(state, axis, 0.0, 0, None, true);
      }
      VirtualEvent::AxisSource(new_source) => source = new_source,
    }
  }
  if let Some(pointer) = state.virtual_pointers.devices.get_mut(&resource.id())
  {
    pointer.axis_source = source;
  }
  input::end_pointer_batch(state);
}
