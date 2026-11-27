use std::os::fd::{AsFd, OwnedFd};

use wayland_client::{
  Connection, Dispatch, Proxy, QueueHandle, WEnum,
  protocol::{wl_buffer, wl_output::WlOutput, wl_shm},
};
use wayland_protocols::ext::{
  foreign_toplevel_list::v1::client::ext_foreign_toplevel_list_v1 as list,
  image_capture_source::v1::client::{
    ext_foreign_toplevel_image_capture_source_manager_v1 as toplevel_sources,
    ext_image_capture_source_v1::ExtImageCaptureSourceV1,
    ext_output_image_capture_source_manager_v1 as output_sources,
  },
  image_copy_capture::v1::client::{
    ext_image_copy_capture_cursor_session_v1::{
      self, ExtImageCopyCaptureCursorSessionV1,
    },
    ext_image_copy_capture_frame_v1::{
      self, ExtImageCopyCaptureFrameV1, FailureReason,
    },
    ext_image_copy_capture_manager_v1::{
      ExtImageCopyCaptureManagerV1, Options,
    },
    ext_image_copy_capture_session_v1::{self, ExtImageCopyCaptureSessionV1},
  },
};

type ToplevelSources =
  toplevel_sources::ExtForeignToplevelImageCaptureSourceManagerV1;
type OutputSources = output_sources::ExtOutputImageCaptureSourceManagerV1;

use crate::{
  CaptureRequest, CaptureSource, Compositor, Event as CompositorEvent,
  test_client::{Events, TestClient},
  tests::test_compositor,
};

impl Dispatch<ExtImageCopyCaptureManagerV1, ()> for Events {
  fn event(
    _: &mut Self,
    _: &ExtImageCopyCaptureManagerV1,
    _: <ExtImageCopyCaptureManagerV1 as Proxy>::Event,
    _: &(),
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
  }
}

impl Dispatch<OutputSources, ()> for Events {
  fn event(
    _: &mut Self,
    _: &OutputSources,
    _: <OutputSources as Proxy>::Event,
    _: &(),
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
  }
}

impl Dispatch<ToplevelSources, ()> for Events {
  fn event(
    _: &mut Self,
    _: &ToplevelSources,
    _: <ToplevelSources as Proxy>::Event,
    _: &(),
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
  }
}

impl Dispatch<ExtImageCaptureSourceV1, ()> for Events {
  fn event(
    _: &mut Self,
    _: &ExtImageCaptureSourceV1,
    _: <ExtImageCaptureSourceV1 as Proxy>::Event,
    _: &(),
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
  }
}

impl Dispatch<ExtImageCopyCaptureSessionV1, ()> for Events {
  fn event(
    events: &mut Self,
    session: &ExtImageCopyCaptureSessionV1,
    event: ext_image_copy_capture_session_v1::Event,
    _: &(),
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
    events.capture_sessions.push((session.id(), event));
  }
}

impl Dispatch<ExtImageCopyCaptureFrameV1, ()> for Events {
  fn event(
    events: &mut Self,
    frame: &ExtImageCopyCaptureFrameV1,
    event: ext_image_copy_capture_frame_v1::Event,
    _: &(),
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
    events.capture_frames.push((frame.id(), event));
  }
}

impl Dispatch<ExtImageCopyCaptureCursorSessionV1, ()> for Events {
  fn event(
    events: &mut Self,
    _: &ExtImageCopyCaptureCursorSessionV1,
    event: ext_image_copy_capture_cursor_session_v1::Event,
    _: &(),
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
    events.cursor_sessions.push(event);
  }
}

/// A `width` by `height` XRGB8888 buffer filled with `bgra`, and its fd.
fn filled_buffer(
  client: &TestClient,
  (width, height): (i32, i32),
  bgra: [u8; 4],
) -> (wl_buffer::WlBuffer, OwnedFd) {
  let shm = client.bind::<wl_shm::WlShm>(1);
  let size = width * height * 4;
  let fd = rustix::fs::memfd_create(
    "shingle-capture-test",
    rustix::fs::MemfdFlags::CLOEXEC,
  )
  .expect("memfd_create succeeds");
  let pixels = bgra.repeat((width * height) as usize);
  rustix::io::pwrite(&fd, &pixels, 0).expect("pwrite succeeds");
  let pool = shm.create_pool(fd.as_fd(), size, &client.handle, ());
  let buffer = pool.create_buffer(
    0,
    width,
    height,
    width * 4,
    wl_shm::Format::Xrgb8888,
    &client.handle,
    (),
  );
  pool.destroy();
  (buffer, fd)
}

fn read(fd: &OwnedFd, length: usize) -> Vec<u8> {
  let mut bytes = vec![0; length];
  let read = rustix::io::pread(fd, &mut bytes, 0).expect("pread");
  bytes.truncate(read);
  bytes
}

/// A capture client: the copy manager and a capture session of `source`.
struct Capturer {
  client: TestClient,
  manager: ExtImageCopyCaptureManagerV1,
}

impl Capturer {
  fn new(compositor: &mut Compositor, socket: &std::path::Path) -> Self {
    let client = TestClient::connect(compositor, socket);
    let manager = client.bind::<ExtImageCopyCaptureManagerV1>(1);
    Self { client, manager }
  }

  /// A source for the first output.
  fn output_source(&self) -> ExtImageCaptureSourceV1 {
    let output = self.client.bind::<WlOutput>(4);
    let sources = self.client.bind::<OutputSources>(1);
    sources.create_source(&output, &self.client.handle, ())
  }

  fn session(
    &mut self,
    compositor: &mut Compositor,
    source: &ExtImageCaptureSourceV1,
    options: Options,
  ) -> ExtImageCopyCaptureSessionV1 {
    let session =
      self
        .manager
        .create_session(source, options, &self.client.handle, ());
    self.client.roundtrip(compositor);
    session
  }

  /// The session's events, by name: `shm_format`, `buffer_size WxH`,
  /// `done`, `stopped`.
  fn session_events(
    &self,
    session: &ExtImageCopyCaptureSessionV1,
  ) -> Vec<String> {
    self
      .client
      .events
      .capture_sessions
      .iter()
      .filter(|(id, _)| *id == session.id())
      .map(|(_, event)| match event {
        ext_image_copy_capture_session_v1::Event::BufferSize {
          width,
          height,
        } => format!("buffer_size {width}x{height}"),
        ext_image_copy_capture_session_v1::Event::ShmFormat { .. } => {
          "shm_format".into()
        }
        ext_image_copy_capture_session_v1::Event::Done => "done".into(),
        ext_image_copy_capture_session_v1::Event::Stopped => "stopped".into(),
        _ => "other".into(),
      })
      .collect()
  }

  /// Captures one frame of `session` into a new `size` buffer; returns
  /// the frame and the buffer's fd.
  fn capture(
    &mut self,
    compositor: &mut Compositor,
    session: &ExtImageCopyCaptureSessionV1,
    size: (i32, i32),
  ) -> (ExtImageCopyCaptureFrameV1, OwnedFd) {
    let (buffer, fd) = filled_buffer(&self.client, size, [9, 9, 9, 9]);
    let frame = session.create_frame(&self.client.handle, ());
    frame.attach_buffer(&buffer);
    frame.damage_buffer(0, 0, size.0, size.1);
    frame.capture();
    self.client.roundtrip(compositor);
    (frame, fd)
  }

  /// The frame's events, by name, with `failed REASON`.
  fn frame_events(&self, frame: &ExtImageCopyCaptureFrameV1) -> Vec<String> {
    self
      .client
      .events
      .capture_frames
      .iter()
      .filter(|(id, _)| *id == frame.id())
      .map(|(_, event)| match event {
        ext_image_copy_capture_frame_v1::Event::Transform { .. } => {
          "transform".into()
        }
        ext_image_copy_capture_frame_v1::Event::Damage { .. } => {
          "damage".into()
        }
        ext_image_copy_capture_frame_v1::Event::PresentationTime { .. } => {
          "presentation_time".into()
        }
        ext_image_copy_capture_frame_v1::Event::Ready => "ready".into(),
        ext_image_copy_capture_frame_v1::Event::Failed { reason } => {
          format!("failed {reason:?}")
        }
        _ => "other".into(),
      })
      .collect()
  }
}

const READY: [&str; 4] = ["transform", "damage", "presentation_time", "ready"];

#[test]
fn a_session_advertises_the_outputs_buffer_and_copies_it() {
  let (mut compositor, socket) = test_compositor();
  let mut capturer = Capturer::new(&mut compositor, &socket);
  let source = capturer.output_source();
  let session = capturer.session(&mut compositor, &source, Options::empty());
  assert_eq!(
    capturer.session_events(&session),
    ["shm_format", "shm_format", "buffer_size 1280x720", "done"]
  );
  let (frame, fd) = capturer.capture(&mut compositor, &session, (1280, 720));
  assert_eq!(capturer.frame_events(&frame), READY);
  // The default background, 0.1, is the byte 26, in BGRA.
  assert_eq!(read(&fd, 8), [26, 26, 26, 255, 26, 26, 26, 255]);
}

#[test]
fn later_frames_wait_until_the_output_changes() {
  let (mut compositor, socket) = test_compositor();
  let mut capturer = Capturer::new(&mut compositor, &socket);
  let source = capturer.output_source();
  let session = capturer.session(&mut compositor, &source, Options::empty());
  let (first, _) = capturer.capture(&mut compositor, &session, (1280, 720));
  assert_eq!(capturer.frame_events(&first), READY);
  first.destroy();
  let (second, _fd) = capturer.capture(&mut compositor, &session, (1280, 720));
  assert!(capturer.frame_events(&second).is_empty(), "nothing changed");
  // Another client's commit is damage.
  let mut other = TestClient::connect(&mut compositor, &socket);
  let toplevel = other.toplevel(&mut compositor, "damage");
  other.attach_buffer(&mut compositor, &toplevel.surface, 8, 8);
  capturer.client.roundtrip(&mut compositor);
  assert_eq!(capturer.frame_events(&second), READY);
}

#[test]
fn a_buffer_of_the_wrong_size_fails_with_buffer_constraints() {
  let (mut compositor, socket) = test_compositor();
  let mut capturer = Capturer::new(&mut compositor, &socket);
  let source = capturer.output_source();
  let session = capturer.session(&mut compositor, &source, Options::empty());
  let (frame, _) = capturer.capture(&mut compositor, &session, (10, 10));
  assert_eq!(
    capturer.frame_events(&frame),
    [format!(
      "failed {:?}",
      WEnum::Value(FailureReason::BufferConstraints)
    )]
  );
}

#[test]
fn a_second_frame_and_a_capture_without_buffer_are_protocol_errors() {
  let (mut compositor, socket) = test_compositor();
  let mut capturer = Capturer::new(&mut compositor, &socket);
  let source = capturer.output_source();
  let session = capturer.session(&mut compositor, &source, Options::empty());
  let _first = session.create_frame(&capturer.client.handle, ());
  let _second = session.create_frame(&capturer.client.handle, ());
  let error = capturer.client.protocol_error(&mut compositor);
  assert_eq!(
    error.map(|error| (error.code, error.object_interface)),
    Some((
      ext_image_copy_capture_session_v1::Error::DuplicateFrame as u32,
      "ext_image_copy_capture_session_v1".into()
    ))
  );
  let mut capturer = Capturer::new(&mut compositor, &socket);
  let source = capturer.output_source();
  let session = capturer.session(&mut compositor, &source, Options::empty());
  let frame = session.create_frame(&capturer.client.handle, ());
  frame.capture();
  let error = capturer.client.protocol_error(&mut compositor);
  assert_eq!(
    error.map(|error| error.code),
    Some(ext_image_copy_capture_frame_v1::Error::NoBuffer as u32)
  );
}

/// Maps a toplevel with `app_id` showing a `bgra` buffer.
fn colored_toplevel(
  compositor: &mut Compositor,
  client: &mut TestClient,
  app_id: &str,
  bgra: [u8; 4],
) -> crate::test_client::Toplevel {
  let toplevel = client.toplevel(compositor, app_id);
  let (buffer, _fd) = filled_buffer(client, (16, 16), bgra);
  toplevel.surface.attach(Some(&buffer), 0, 0);
  toplevel.surface.commit();
  client.roundtrip(compositor);
  toplevel
}

/// A toplevel source for the window with `app_id`, through
/// ext-foreign-toplevel-list.
fn toplevel_source(
  compositor: &mut Compositor,
  capturer: &mut Capturer,
  app_id: &str,
) -> ExtImageCaptureSourceV1 {
  let _list = capturer.client.bind::<list::ExtForeignToplevelListV1>(1);
  capturer.client.roundtrip(compositor);
  let handle = capturer
    .client
    .events
    .ext_toplevels
    .iter()
    .find(|(_, known)| known.as_deref() == Some(app_id))
    .map(|(handle, _)| handle.clone())
    .expect("the toplevel is listed");
  let sources = capturer.client.bind::<ToplevelSources>(1);
  sources.create_source(&handle, &capturer.client.handle, ())
}

#[test]
fn a_window_capture_draws_only_that_toplevel() {
  let (mut compositor, socket) = test_compositor();
  let mut apps = TestClient::connect(&mut compositor, &socket);
  // Red and green, side by side in the layout.
  let _red =
    colored_toplevel(&mut compositor, &mut apps, "red", [0, 0, 255, 255]);
  let _green =
    colored_toplevel(&mut compositor, &mut apps, "green", [0, 255, 0, 255]);
  let mut capturer = Capturer::new(&mut compositor, &socket);
  let source = toplevel_source(&mut compositor, &mut capturer, "red");
  let session = capturer.session(&mut compositor, &source, Options::empty());
  let size = capturer
    .client
    .events
    .capture_sessions
    .iter()
    .find_map(|(_, event)| match event {
      ext_image_copy_capture_session_v1::Event::BufferSize {
        width,
        height,
      } => Some((*width as i32, *height as i32)),
      _ => None,
    })
    .expect("a buffer size");
  assert!(size.0 < 1280, "one tile of two, not the output: {size:?}");
  let (frame, fd) = capturer.capture(&mut compositor, &session, size);
  assert_eq!(capturer.frame_events(&frame), READY);
  let pixels = read(&fd, (size.0 * size.1 * 4) as usize);
  assert!(
    pixels.chunks(4).all(|pixel| pixel == [0, 0, 255, 255]),
    "only red, no green and no background"
  );
}

#[test]
fn with_frame_capture_a_window_capture_goes_to_the_frontend() {
  let (mut compositor, socket) = test_compositor();
  compositor.set_frame_capture(true);
  let mut apps = TestClient::connect(&mut compositor, &socket);
  let _red =
    colored_toplevel(&mut compositor, &mut apps, "red", [0, 0, 255, 255]);
  let mut capturer = Capturer::new(&mut compositor, &socket);
  let source = toplevel_source(&mut compositor, &mut capturer, "red");
  let session = capturer.session(&mut compositor, &source, Options::empty());
  compositor.drain_events();
  let (frame, fd) = capturer.capture(&mut compositor, &session, (1280, 720));
  let requests = compositor
    .drain_events()
    .into_iter()
    .filter_map(|event| match event {
      CompositorEvent::CaptureRequested(request) => Some(request),
      _ => None,
    })
    .collect::<Vec<_>>();
  let [
    CaptureRequest {
      id,
      source: CaptureSource::Window { rect, .. },
      cursor: false,
      dmabuf: None,
    },
  ] = requests.as_slice()
  else {
    panic!("one window capture: {requests:?}");
  };
  assert_eq!((rect.width, rect.height), (1280, 720), "the only tile");
  let rgba = [255, 0, 0, 255].repeat(1280 * 720);
  compositor.finish_capture(*id, Ok(rgba));
  capturer.client.roundtrip(&mut compositor);
  assert_eq!(capturer.frame_events(&frame), READY);
  assert_eq!(read(&fd, 4), [0, 0, 255, 255]);
}

#[test]
fn a_session_stops_when_its_window_closes() {
  let (mut compositor, socket) = test_compositor();
  let mut apps = TestClient::connect(&mut compositor, &socket);
  let red =
    colored_toplevel(&mut compositor, &mut apps, "red", [0, 0, 255, 255]);
  let mut capturer = Capturer::new(&mut compositor, &socket);
  let source = toplevel_source(&mut compositor, &mut capturer, "red");
  let session = capturer.session(&mut compositor, &source, Options::empty());
  red.toplevel.destroy();
  red._xdg_surface.destroy();
  red.surface.destroy();
  apps.roundtrip(&mut compositor);
  capturer.client.roundtrip(&mut compositor);
  assert_eq!(
    capturer.session_events(&session).last().map(String::as_str),
    Some("stopped")
  );
  let (frame, _) = capturer.capture(&mut compositor, &session, (1280, 720));
  assert_eq!(
    capturer.frame_events(&frame),
    [format!("failed {:?}", WEnum::Value(FailureReason::Stopped))]
  );
}

#[test]
fn the_cursor_is_painted_only_when_asked_for() {
  let (mut compositor, socket) = test_compositor();
  let device = compositor.add_input_device(crate::InputDeviceInfo {
    name: "test pointer".into(),
    vendor: None,
    product: None,
    kind: crate::InputDeviceKind::Pointer,
  });
  let mut apps = TestClient::connect(&mut compositor, &socket);
  let cursor = apps.cursor_surface(&mut compositor);
  let (white, _fd) = filled_buffer(&apps, (4, 4), [255, 255, 255, 255]);
  cursor.attach(Some(&white), 0, 0);
  cursor.commit();
  apps.roundtrip(&mut compositor);
  compositor.pointer_motion(device, 1.0, 1.0);
  let mut capturer = Capturer::new(&mut compositor, &socket);
  let source = capturer.output_source();
  for (options, expected) in [
    (Options::empty(), [26, 26, 26, 255]),
    (Options::PaintCursors, [255, 255, 255, 255]),
  ] {
    let session = capturer.session(&mut compositor, &source, options);
    let (frame, fd) = capturer.capture(&mut compositor, &session, (1280, 720));
    assert_eq!(capturer.frame_events(&frame), READY);
    let row = read(&fd, 1280 * 4 * 3);
    let pixel = &row[(2 * 1280 + 2) * 4..][..4];
    assert_eq!(pixel, expected, "{options:?}");
  }
}

#[test]
fn a_cursor_session_reports_the_hotspot_and_copies_the_image() {
  let (mut compositor, socket) = test_compositor();
  let device = compositor.add_input_device(crate::InputDeviceInfo {
    name: "test pointer".into(),
    vendor: None,
    product: None,
    kind: crate::InputDeviceKind::Pointer,
  });
  let mut apps = TestClient::connect(&mut compositor, &socket);
  let cursor = apps.cursor_surface(&mut compositor);
  let (white, _fd) = filled_buffer(&apps, (4, 4), [255, 255, 255, 255]);
  cursor.attach(Some(&white), 0, 0);
  cursor.commit();
  apps.roundtrip(&mut compositor);
  compositor.pointer_motion(device, 100.0, 50.0);
  let mut capturer = Capturer::new(&mut compositor, &socket);
  let source = capturer.output_source();
  let pointer = capturer
    .client
    .bind::<wayland_client::protocol::wl_seat::WlSeat>(5)
    .get_pointer(&capturer.client.handle, ());
  let cursor_session = capturer.manager.create_pointer_cursor_session(
    &source,
    &pointer,
    &capturer.client.handle,
    (),
  );
  capturer.client.roundtrip(&mut compositor);
  let names = capturer
    .client
    .events
    .cursor_sessions
    .iter()
    .map(|event| match event {
      ext_image_copy_capture_cursor_session_v1::Event::Enter => {
        "enter".to_owned()
      }
      ext_image_copy_capture_cursor_session_v1::Event::Position { x, y } => {
        format!("position {x},{y}")
      }
      ext_image_copy_capture_cursor_session_v1::Event::Hotspot { x, y } => {
        format!("hotspot {x},{y}")
      }
      _ => "other".to_owned(),
    })
    .collect::<Vec<_>>();
  assert_eq!(names, ["enter", "position 100,50", "hotspot 0,0"]);
  let session = cursor_session.get_capture_session(&capturer.client.handle, ());
  capturer.client.roundtrip(&mut compositor);
  assert_eq!(
    capturer.session_events(&session),
    ["shm_format", "shm_format", "buffer_size 4x4", "done"]
  );
  let (frame, fd) = capturer.capture(&mut compositor, &session, (4, 4));
  assert_eq!(capturer.frame_events(&frame), READY);
  assert_eq!(read(&fd, 4), [255, 255, 255, 255]);
  // The same image again waits.
  frame.destroy();
  let (again, _fd) = capturer.capture(&mut compositor, &session, (4, 4));
  assert!(capturer.frame_events(&again).is_empty());
}
