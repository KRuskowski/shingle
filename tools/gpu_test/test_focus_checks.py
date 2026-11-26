"""Tests for focus_checks.py's helpers; nothing here starts a process."""

import unittest

import focus_checks

class FakeCompositor:
  """Answers `window-rules/get-focused-view` with a fixed reply."""

  def __init__(self, reply):
    self.reply = reply

  def call(self, method, data=None):
    assert method == 'window-rules/get-focused-view'
    return self.reply

class HelperTest(unittest.TestCase):
  """The focus order and the focused view."""

  def test_views_are_ordered_by_their_last_focus(self):
    views = [{'app-id': 'alpha', 'last-focus-timestamp': 5},
             {'app-id': 'charlie', 'last-focus-timestamp': 9},
             {'app-id': 'bravo', 'last-focus-timestamp': 7}]
    self.assertEqual(focus_checks.by_focus_time(views),
                     ['charlie', 'bravo', 'alpha'])

  def test_the_focused_app_or_none(self):
    self.assertEqual(focus_checks.focused_app(
        FakeCompositor({'info': {'app-id': 'bravo'}})), 'bravo')
    self.assertIsNone(focus_checks.focused_app(
        FakeCompositor({'info': None})))
    self.assertIsNone(focus_checks.focused_app(FakeCompositor({})))

  def test_minimizing_never_restamps_in_the_expectation(self):
    _, before = focus_checks.EXPECTED['mapped']
    _, after = focus_checks.EXPECTED['minimized']
    self.assertEqual(before, after)

if __name__ == '__main__':
  unittest.main()
