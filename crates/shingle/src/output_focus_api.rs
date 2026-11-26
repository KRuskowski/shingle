//! [`Compositor`] calls for the active output, the pointer between outputs
//! and moving windows between outputs and workspace sets
//! (docs/design/multi-output.md, sections 3 and 4).

use shingle_core::{WindowId, WsetId};

use crate::{
  Compositor, Error, MoveGeometry, OutputAction, OutputHandle, OutputSwitch,
  output,
};

impl Compositor {
  /// Makes `output` the active output and gives keyboard focus to the
  /// focused window of its current workspace, emitting
  /// [`crate::Event::OutputFocused`]. Returns whether the active output
  /// changed; an output that is not there or dark is not focused.
  pub fn focus_output(&mut self, output: &OutputHandle) -> bool {
    output::focus_output(&mut self.state, &output.id())
  }

  /// Runs an `oswitch` action: focuses the output `switch` names, with the
  /// focused window if `with_window`, and warps the pointer into it unless
  /// it is there already ([`crate::Event::PointerWarp`]). Returns the
  /// active output afterwards.
  pub fn switch_output(
    &mut self,
    switch: OutputSwitch,
    with_window: bool,
  ) -> OutputHandle {
    let id = output::oswitch(&mut self.state, switch, with_window);
    OutputHandle::from(&id)
  }

  /// Moves `window`, or the window whose dialog it is, with its dialogs to
  /// the workspace set `output` shows, as Wayfire's `move_view_to_output`:
  /// tiled windows join the target tree, floating ones get `geometry`,
  /// snapped and fullscreen ones are applied again there, and the window
  /// lands on the workspace with the same offset from the current one.
  ///
  /// # Errors
  ///
  /// [`Error::Core`] for an unknown window or output.
  pub fn move_window_to_output(
    &mut self,
    window: WindowId,
    output: &OutputHandle,
    geometry: MoveGeometry,
  ) -> Result<(), Error> {
    let window = output::topmost_parent(&self.state, window);
    output::move_window_to_output(
      &mut self.state,
      window,
      &output.id(),
      geometry,
    )
    .map(|_| ())
  }

  /// Shows workspace set `index` on `output`, as Wayfire's
  /// `wsets/set-output-wset`: the set is created if needed, an output that
  /// showed it gets a new empty one, and the set `output` showed is parked,
  /// or destroyed if empty.
  ///
  /// # Errors
  ///
  /// [`Error::Core`] for an unknown output or index 0.
  pub fn show_wset(
    &mut self,
    output: &OutputHandle,
    index: u32,
  ) -> Result<(), Error> {
    output::select_wset(&mut self.state, &output.id(), WsetId(index))
  }

  /// Sends `window` with its dialogs to workspace set `index`, created if
  /// needed, keeping its place relative to the output.
  ///
  /// # Errors
  ///
  /// [`Error::Core`] for an unknown window or index 0.
  pub fn send_window_to_wset(
    &mut self,
    window: WindowId,
    index: u32,
  ) -> Result<(), Error> {
    let window = output::topmost_parent(&self.state, window);
    output::send_to_wset(&mut self.state, window, WsetId(index))
  }

  /// `(x, y)` in layout coordinates pulled onto the closest point of an
  /// output, as the pointer is kept on the layout.
  pub fn clamp_to_layout(&self, x: f64, y: f64) -> (f64, f64) {
    output::clamp(&self.state, x, y)
  }

  /// The output configured for absolute input device `name`
  /// (`input_devices.NAME.output`), if it is there.
  pub fn input_device_output(&self, name: &str) -> Option<OutputHandle> {
    let output = self.state.focus_settings.device_outputs.get(name)?;
    let id = shingle_core::OutputId::new(output.clone());
    self.state.outputs.get(&id).map(|_| OutputHandle::from(&id))
  }

  /// The `oswitch` or `wsets` action whose binding matches `key` with
  /// `modifiers`, for a frontend's key dispatch.
  pub fn output_binding(
    &self,
    modifiers: shingle_config::Modifiers,
    key: &str,
  ) -> Option<OutputAction> {
    output::binding_matching(&self.state.focus_settings, modifiers, key)
  }

  /// Runs an `oswitch` or `wsets` action. Returns whether it changed
  /// anything.
  pub fn run_output_action(&mut self, action: OutputAction) -> bool {
    output::run_binding(&mut self.state, action)
  }
}
