//! Which host window shows which output (docs/design/multi-output.md,
//! section 7, nested): nested opens one window per output, as wlroots'
//! Wayland backend opens one toplevel per output, and routes each
//! window's events to its own output and each output's frames to its own
//! window.

use std::fmt::Write as _;

use shingle::OutputHandle;
use shingle_config::ShingleConfig;

/// The name of the `number`th output nested starts with: `HEADLESS-1` to
/// `HEADLESS-N`, as headless and offscreen name theirs, so that configs
/// and sessions that name `HEADLESS-1` keep working (Decisions 2).
pub(crate) fn start_output_name(number: u32) -> String {
  format!("HEADLESS-{number}")
}

/// The title of the window that shows `output`: "Shingle" for the first,
/// as nested has always called its window, and "Shingle - NAME" for the
/// others, as wlroots titles its windows after their outputs.
pub(crate) fn window_title(output: &str) -> String {
  let mut title = String::from("Shingle");
  if output != shingle::OUTPUT_NAME {
    let _ = write!(title, " - {output}");
  }
  title
}

/// The size the window of output `name` opens at: an explicit configured
/// mode of that output, as a nested Wayfire sets a custom mode on its
/// window, else 1280x720.
pub(crate) fn initial_window_size(
  config: &ShingleConfig,
  name: &str,
) -> (u32, u32) {
  config
    .output_config(name)
    .mode
    .size()
    .unwrap_or((1280, 720))
}

/// One window and the output it shows.
struct Entry<K, W> {
  id: K,
  output: OutputHandle,
  window: W,
}

/// The host windows by window id `K`, each with the output it shows and
/// the window's own state `W`. At most one window shows an output.
pub(crate) struct Windows<K, W> {
  entries: Vec<Entry<K, W>>,
}

impl<K, W> Default for Windows<K, W> {
  fn default() -> Self {
    Self {
      entries: Vec::new(),
    }
  }
}

impl<K: PartialEq + Copy, W> Windows<K, W> {
  /// Adds window `id` showing `output`. A window that showed `output`
  /// before, or had `id`, is replaced and returned.
  pub(crate) fn insert(
    &mut self,
    id: K,
    output: OutputHandle,
    window: W,
  ) -> Option<W> {
    let old = self
      .entries
      .iter()
      .position(|entry| entry.id == id || entry.output == output)
      .map(|index| self.entries.remove(index).window);
    self.entries.push(Entry { id, output, window });
    old
  }

  /// How many windows are open.
  pub(crate) fn len(&self) -> usize {
    self.entries.len()
  }

  /// The output window `id` shows.
  pub(crate) fn output_of(&self, id: K) -> Option<&OutputHandle> {
    self.entry(id).map(|entry| &entry.output)
  }

  /// Window `id`.
  pub(crate) fn get(&self, id: K) -> Option<&W> {
    self.entry(id).map(|entry| &entry.window)
  }

  /// Window `id` with the output it shows, to change.
  pub(crate) fn get_mut(&mut self, id: K) -> Option<(&OutputHandle, &mut W)> {
    self
      .entries
      .iter_mut()
      .find(|entry| entry.id == id)
      .map(|entry| (&entry.output, &mut entry.window))
  }

  /// The window that shows `output`.
  pub(crate) fn showing(&self, output: &OutputHandle) -> Option<&W> {
    self
      .entries
      .iter()
      .find(|entry| entry.output == *output)
      .map(|entry| &entry.window)
  }

  /// The window that shows `output`, to change.
  pub(crate) fn showing_mut(
    &mut self,
    output: &OutputHandle,
  ) -> Option<&mut W> {
    self
      .entries
      .iter_mut()
      .find(|entry| entry.output == *output)
      .map(|entry| &mut entry.window)
  }

  /// Takes window `id` out, with the output it showed.
  pub(crate) fn remove(&mut self, id: K) -> Option<(OutputHandle, W)> {
    let index = self.entries.iter().position(|entry| entry.id == id)?;
    let entry = self.entries.remove(index);
    Some((entry.output, entry.window))
  }

  /// Takes the window that shows `output` out.
  pub(crate) fn remove_showing(&mut self, output: &OutputHandle) -> Option<W> {
    let index = self
      .entries
      .iter()
      .position(|entry| entry.output == *output)?;
    Some(self.entries.remove(index).window)
  }

  /// Every window with the output it shows, in the order they opened.
  pub(crate) fn iter(&self) -> impl Iterator<Item = (K, &OutputHandle, &W)> {
    self
      .entries
      .iter()
      .map(|entry| (entry.id, &entry.output, &entry.window))
  }

  fn entry(&self, id: K) -> Option<&Entry<K, W>> {
    self.entries.iter().find(|entry| entry.id == id)
  }
}

/// Which host windows have keyboard focus. The host moves focus between
/// nested windows as between any two windows, so nested's keyboard counts
/// as focused while any of them is.
#[derive(Debug)]
pub(crate) struct Focus<K> {
  focused: Vec<K>,
}

impl<K> Default for Focus<K> {
  fn default() -> Self {
    Self {
      focused: Vec::new(),
    }
  }
}

impl<K: PartialEq> Focus<K> {
  /// Notes that window `id` gained or lost focus. Returns the new focus
  /// of nested as a whole if it changed: `Some(true)` when the first
  /// window gained it, `Some(false)` when the last one lost it.
  pub(crate) fn changed(&mut self, id: K, focused: bool) -> Option<bool> {
    let before = !self.focused.is_empty();
    if focused {
      if !self.focused.contains(&id) {
        self.focused.push(id);
      }
    } else {
      self.focused.retain(|other| *other != id);
    }
    let after = !self.focused.is_empty();
    (before != after).then_some(after)
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn handle(name: &str) -> OutputHandle {
    OutputHandle::new(name)
  }

  #[test]
  fn each_window_maps_to_its_own_output_and_back() {
    let mut windows = Windows::default();
    assert_eq!(windows.len(), 0);
    assert!(windows.insert(1, handle("HEADLESS-1"), "first").is_none());
    assert!(windows.insert(2, handle("HEADLESS-2"), "second").is_none());
    assert_eq!(windows.len(), 2);
    assert_eq!(windows.output_of(2), Some(&handle("HEADLESS-2")));
    assert_eq!(windows.get(1), Some(&"first"));
    assert_eq!(windows.showing(&handle("HEADLESS-2")), Some(&"second"));
    assert_eq!(windows.output_of(3), None);
    assert_eq!(windows.showing(&handle("HEADLESS-3")), None);
    let (output, window) = windows.get_mut(1).unwrap();
    assert_eq!(output, &handle("HEADLESS-1"));
    *window = "changed";
    *windows.showing_mut(&handle("HEADLESS-2")).unwrap() = "again";
    let order = windows
      .iter()
      .map(|(id, output, window)| (id, output.name().to_owned(), *window))
      .collect::<Vec<_>>();
    assert_eq!(
      order,
      [
        (1, "HEADLESS-1".to_owned(), "changed"),
        (2, "HEADLESS-2".to_owned(), "again")
      ]
    );
  }

  #[test]
  fn closing_a_window_forgets_its_output_and_one_output_has_one_window() {
    let mut windows = Windows::default();
    windows.insert(1, handle("HEADLESS-1"), 'a');
    windows.insert(2, handle("HEADLESS-2"), 'b');
    assert_eq!(windows.remove(1), Some((handle("HEADLESS-1"), 'a')));
    assert_eq!(windows.remove(1), None);
    assert_eq!(windows.showing(&handle("HEADLESS-1")), None);
    assert_eq!(windows.remove_showing(&handle("HEADLESS-2")), Some('b'));
    assert_eq!(windows.len(), 0);
    windows.insert(3, handle("HEADLESS-2"), 'c');
    assert_eq!(
      windows.insert(4, handle("HEADLESS-2"), 'd'),
      Some('c'),
      "a second window for one output replaces the first"
    );
    assert_eq!(windows.output_of(3), None);
    assert_eq!(windows.output_of(4), Some(&handle("HEADLESS-2")));
  }

  #[test]
  fn nested_keeps_focus_while_any_window_has_it() {
    let mut focus = Focus::default();
    assert_eq!(focus.changed(1, true), Some(true));
    assert_eq!(focus.changed(2, true), None, "focus came from a sibling");
    assert_eq!(focus.changed(1, false), None);
    assert_eq!(focus.changed(1, false), None, "a repeated loss");
    assert_eq!(focus.changed(2, false), Some(false));
    assert_eq!(focus.changed(2, false), None);
  }

  #[test]
  fn start_outputs_are_headless_n_and_windows_are_titled_after_them() {
    assert_eq!(start_output_name(1), shingle::OUTPUT_NAME);
    assert_eq!(start_output_name(3), "HEADLESS-3");
    assert_eq!(window_title("HEADLESS-1"), "Shingle");
    assert_eq!(window_title("HEADLESS-2"), "Shingle - HEADLESS-2");
  }

  #[test]
  fn windows_open_at_their_outputs_explicit_configured_mode() {
    let mut config = ShingleConfig::default();
    assert_eq!(initial_window_size(&config, "HEADLESS-1"), (1280, 720));
    config.output.width = 1920;
    assert_eq!(
      initial_window_size(&config, "HEADLESS-1"),
      (1280, 720),
      "only a mode sets the window size"
    );
    config.output.mode = "1600x900@75".parse().unwrap();
    assert_eq!(initial_window_size(&config, "HEADLESS-1"), (1600, 900));
    assert_eq!(
      initial_window_size(&config, "HEADLESS-2"),
      (1600, 900),
      "the default entry applies to every output"
    );
    config.outputs.push(shingle_config::OutputConfig {
      mode: "800x600".parse().unwrap(),
      ..shingle_config::OutputConfig::named("HEADLESS-2")
    });
    assert_eq!(initial_window_size(&config, "HEADLESS-2"), (800, 600));
    assert_eq!(initial_window_size(&config, "HEADLESS-1"), (1600, 900));
  }
}
