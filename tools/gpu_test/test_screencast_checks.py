"""Tests for screencast_checks.py's, capture_wire.py's and
portal_client.py's helpers; nothing here starts a process or connects to
a compositor or bus."""

import json
import os
import struct
import tempfile
import unittest

import capture_wire
import portal_client
import run
import screencast_checks

class ScreencastHelperTest(unittest.TestCase):
  """The pure helpers of the screencast checks."""

  def test_the_portal_config_names_the_output_without_a_chooser(self):
    config = screencast_checks.portal_wlr_config('HEADLESS-1', 30)
    self.assertIn('[screencast]', config)
    self.assertIn('output_name=HEADLESS-1', config)
    self.assertIn('chooser_type=none', config)
    self.assertIn('max_fps=30', config)

  def test_json_events_skip_other_lines_and_filter_by_name(self):
    text = '\n'.join([
        json.dumps({'event': 'frame', 't': 1.0}),
        'gst warning',
        json.dumps({'event': 'started', 'node': 4}),
        json.dumps([1, 2]),
        json.dumps({'event': 'frame', 't': 2.5}),
    ])
    self.assertEqual(len(screencast_checks.json_events(text)), 3)
    self.assertEqual(
        [event['t'] for event in screencast_checks.json_events(text, 'frame')],
        [1.0, 2.5])

  def test_frames_are_counted_in_a_half_open_interval(self):
    events = [{'event': 'frame', 't': t} for t in (0.5, 1.0, 1.5, 2.0)]
    events.append({'event': 'caps', 't': 1.2})
    self.assertEqual(screencast_checks.frames_between(events, 1.0, 2.0), 2)

  def test_rgba_and_bgrx_rows_become_rgb_pixels(self):
    # Two pixels a row, a stride of 12 bytes with 4 of padding.
    rgba = bytes([1, 2, 3, 255, 4, 5, 6, 255, 0, 0, 0, 0,
                  7, 8, 9, 255, 10, 11, 12, 255, 0, 0, 0, 0])
    shot = screencast_checks.rgba_capture(rgba, 2, 2, 12)
    self.assertEqual(shot.pixel(1, 0), (4, 5, 6))
    self.assertEqual(shot.pixel(0, 1), (7, 8, 9))
    bgrx = bytes([3, 2, 1, 0, 6, 5, 4, 0])
    shot = screencast_checks.rgba_capture(bgrx, 2, 1, 8, 'BGRx')
    self.assertEqual(shot.pixel(0, 0), (1, 2, 3))
    self.assertEqual(shot.pixel(1, 0), (4, 5, 6))

  def test_colours_are_near_within_the_tolerance(self):
    self.assertTrue(screencast_checks.near((250, 250, 10), (255, 255, 0)))
    self.assertFalse(screencast_checks.near((255, 0, 0), (255, 255, 0)))

  def test_a_frame_is_ready_with_its_metadata_only(self):
    self.assertTrue(screencast_checks.frame_ready(
        ['transform', 'damage', 'presentation_time', 'ready']))
    self.assertFalse(screencast_checks.frame_ready(['failed']))
    self.assertFalse(screencast_checks.frame_ready(['ready']))

  def test_the_bus_config_allows_only_the_private_service_directory(self):
    config = screencast_checks.BUS_CONFIG.format(socket='/tmp/x/bus',
                                                 services='/tmp/x/services')
    self.assertIn('<listen>unix:path=/tmp/x/bus</listen>', config)
    self.assertIn('<servicedir>/tmp/x/services</servicedir>', config)
    self.assertNotIn('standard_session_servicedirs', config)

  def test_the_groups_are_in_run_py(self):
    for group in screencast_checks.GROUPS:
      self.assertIn(group, run.CHECK_GROUPS)

class CaptureWireTest(unittest.TestCase):
  """Parsing what ext-image-copy-capture sends."""

  def test_dmabuf_formats_unpack_the_modifier_arrays(self):

    class Client:
      def events_of(self, session):
        modifiers = struct.pack('=2Q', 0, 0x0300000000606015)
        return [('shm_format', [0]),
                ('dmabuf_format', [0x34325258, modifiers]),
                ('done', [])]
    self.assertEqual(capture_wire.dmabuf_formats(Client(), 5),
                     [(0x34325258, [0, 0x0300000000606015])])
    self.assertEqual(capture_wire.fourcc_name(0x34325258), 'XR24')

  def test_a_pixel_is_read_from_bgra_bytes(self):
    data = bytes([0, 0, 0, 0, 30, 20, 10, 255])
    self.assertEqual(capture_wire.pixel(data, 2, 1, 0), (10, 20, 30))

  def test_the_protocol_tables_extend_wayland_wire(self):
    self.assertEqual(
        capture_wire.wire.REQUESTS['ext_image_copy_capture_frame_v1'][
            'capture'], 3)
    self.assertEqual(
        capture_wire.wire.EVENTS['ext_data_control_source_v1'][0],
        ('send', 's'))
    self.assertEqual(
        capture_wire.wire.NEW_OBJECTS[('ext_data_control_device_v1',
                                       'data_offer')],
        'ext_data_control_offer_v1')

class PortalClientTest(unittest.TestCase):
  """The portal client's frame records and files."""

  def test_the_latest_frame_is_written_with_its_shape(self):
    with tempfile.TemporaryDirectory() as directory:
      portal_client.write_latest(directory, 3, 2, 1, 8, b'\1' * 8, 'BGRx')
      with open(os.path.join(directory, 'latest.json')) as file:
        shape = json.load(file)
      with open(os.path.join(directory, 'latest.rgba'), 'rb') as file:
        data = file.read()
      self.assertEqual(shape, {'number': 3, 'width': 2, 'height': 1,
                               'stride': 8, 'format': 'BGRx'})
      self.assertEqual(data, b'\1' * 8)
      self.assertFalse(os.path.exists(os.path.join(directory,
                                                   'latest.rgba.tmp')))

  def test_a_frame_record_has_its_number_size_and_memory(self):
    record = portal_client.frame_record(4, 1280, 720, True)
    self.assertEqual((record['number'], record['width'], record['height'],
                      record['dmabuf']), (4, 1280, 720, True))
    self.assertIsInstance(record['t'], float)

if __name__ == '__main__':
  unittest.main()
