//! The seam between the shared frontend and the binary that hosts it.
//!
//! A host owns the event loop, the input devices and the display. It turns
//! its input into [`InputEvent`]s, in output pixels and evdev codes, and
//! passes them to [`crate::Frontend::handle_input`] together with the seat
//! device they came from. The frontend calls back through [`Host`] for the
//! few things only the host can do. The nested host implements it on top of
//! winit; the native backend (NB-6) will implement it on top of libseat and
//! libinput.

pub use shingle::{Axis, AxisSource};
use shingle::{InputDeviceId, OutputHandle};

/// What a frontend binary provides to the shared frontend.
///
/// The frontend calls these from [`crate::Frontend::handle_input`],
/// [`crate::Frontend::process`], [`crate::Frontend::render`] and
/// [`crate::Frontend::update_keyboard`], never from anywhere else.
pub trait Host {
  /// Asks for [`crate::Frontend::render`] to be called soon. Several
  /// requests before the next frame are one request.
  fn request_redraw(&mut self);

  /// Moves the host pointer to `(x, y)` in output pixels, zoom already
  /// applied. Nested warps the host cursor; native only records the new
  /// position.
  fn warp_pointer(&mut self, x: f64, y: f64);

  /// Switches to virtual terminal `vt`, for the `XF86Switch_VT_n` keysyms
  /// that Ctrl+Alt+Fn produces. Returns false if the host cannot switch,
  /// in which case the key is handled like any other.
  fn switch_vt(&mut self, vt: u32) -> bool;

  /// Reports that IPC enabled or disabled seat device `device`, after the
  /// compositor released its held input and the frontend dropped the
  /// state it held. Native applies the libinput send-events mode here;
  /// nested resets its button emulation.
  fn input_device_changed(&mut self, device: InputDeviceId, enabled: bool);

  /// Ends the host's event loop. The host calls
  /// [`crate::Frontend::shutdown`] on the way out, which saves the session.
  fn exit(&mut self);
}

/// Frontend-neutral input, in output pixels and evdev codes.
///
/// Positions are in the compositor's output layout, before the frontend's
/// zoom is undone; the host has already applied its own scale and output
/// transform. An absolute position that names its output, as a nested
/// window's pointer does, is in that output's logical coordinates instead,
/// with its top-left corner at 0,0.
#[derive(Clone, Debug, PartialEq)]
pub enum InputEvent {
  /// A key press or release, by evdev code. Hosts pass no repeats: the
  /// frontend repeats held keys itself, at the configured rate.
  Key {
    /// The evdev code, such as 30 for `KEY_A`.
    code: u32,
    /// Whether the key went down.
    pressed: bool,
  },
  /// Relative pointer motion for `zwp_relative_pointer_v1` clients. It is
  /// forwarded only while a client constrains the pointer, and it does not
  /// move the pointer; a host that gets only relative motion also sends
  /// [`InputEvent::PointerMotionAbsolute`].
  PointerMotion {
    /// Accelerated horizontal motion.
    dx: f64,
    /// Accelerated vertical motion.
    dy: f64,
    /// Unaccelerated horizontal motion.
    dx_unaccel: f64,
    /// Unaccelerated vertical motion.
    dy_unaccel: f64,
  },
  /// The pointer moved to `(x, y)`.
  PointerMotionAbsolute {
    /// Horizontal position in output pixels.
    x: f64,
    /// Vertical position in output pixels.
    y: f64,
    /// The output the position is on, for a device mapped to one output
    /// such as a nested window's pointer; `None` for layout coordinates.
    output: Option<OutputHandle>,
  },
  /// A pointer button, by evdev code (`BTN_LEFT` is `0x110`).
  PointerButton {
    /// The evdev code.
    code: u32,
    /// Whether the button went down.
    pressed: bool,
  },
  /// Scrolling.
  PointerAxis(Axis),
  /// One touch point.
  Touch(Touch),
  /// A touchpad swipe.
  Swipe(Gesture),
  /// A touchpad pinch.
  Pinch(Gesture),
  /// A touchpad hold. Updates are ignored.
  Hold(Gesture),
  /// The host lost keyboard focus or the seat: the keys the frontend
  /// thinks are held are forgotten without releases, and key repeat stops.
  Reset,
}

/// What happened to a touch point.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TouchPhase {
  /// The point touched down.
  Down,
  /// The point moved.
  Motion,
  /// The point lifted.
  Up,
  /// The touch sequence was cancelled.
  Cancel,
}

/// One touch point's event.
#[derive(Clone, Debug, PartialEq)]
pub struct Touch {
  /// The touch point, unique while it is down.
  pub id: u64,
  /// What happened to it.
  pub phase: TouchPhase,
  /// Horizontal position in output pixels.
  pub x: f64,
  /// Vertical position in output pixels.
  pub y: f64,
  /// The output the position is on, for a touch screen mapped to one
  /// output; `None` for layout coordinates.
  pub output: Option<OutputHandle>,
}

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
