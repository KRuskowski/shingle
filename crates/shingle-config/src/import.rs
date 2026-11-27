//! What the Wayfire and i3 importers share: options applied one at a time,
//! so that a value Shingle cannot use is reported and leaves the option as
//! it was, as wf-config does (wf-config:src/file.cpp,
//! `OPTION_PARSED_INVALID_CONTENTS`), instead of rejecting the file.

use crate::{
  Binding, ConfigError, ConfigWarning, RuleAction, ShingleConfig, activator,
};

/// A configuration being imported, with the warnings gathered so far.
pub(crate) struct Import {
  config: ShingleConfig,
  warnings: Vec<ConfigWarning>,
  /// The line each entry of `config.expo.workspace_bindings` came from.
  expo_lines: Vec<usize>,
  /// The line each entry of `config.window_rules` came from.
  rule_lines: Vec<usize>,
  /// The line each entry of the three `config.vswitch` workspace binding
  /// lists came from.
  vswitch_lines: [Vec<usize>; 3],
}

/// Keeps `lines` one entry per item of a list now `length` long, the
/// added ones from `line`.
fn track(lines: &mut Vec<usize>, length: usize, line: usize) {
  let added = length.saturating_sub(lines.len());
  lines.truncate(length);
  lines.extend(std::iter::repeat_n(line, added));
}

impl Import {
  /// Starts from `config`, which must pass [`ShingleConfig::validate`].
  pub(crate) fn new(config: ShingleConfig) -> Self {
    Self {
      config,
      warnings: Vec::new(),
      expo_lines: Vec::new(),
      rule_lines: Vec::new(),
      vswitch_lines: Default::default(),
    }
  }

  /// The configuration as imported so far.
  pub(crate) fn config_mut(&mut self) -> &mut ShingleConfig {
    &mut self.config
  }

  /// Applies one option from `line`. `apply` changes the configuration and
  /// may add notes about parts of the value it left out. When it fails, or
  /// the result breaks a range [`ShingleConfig::validate`] checks, the
  /// configuration goes back to what it was and the failure becomes a
  /// warning; otherwise each note does. `context` names the option in the
  /// warnings, such as `[core] background_color`. Returns whether the
  /// option was applied.
  pub(crate) fn option(
    &mut self,
    line: usize,
    context: &str,
    apply: impl FnOnce(
      &mut ShingleConfig,
      &mut Vec<String>,
    ) -> Result<(), ConfigError>,
  ) -> bool {
    let before = self.config.clone();
    let mut notes = Vec::new();
    let result = apply(&mut self.config, &mut notes)
      .and_then(|()| self.config.validate_fields());
    match result {
      Ok(()) => {
        let config = &self.config;
        track(
          &mut self.expo_lines,
          config.expo.workspace_bindings.len(),
          line,
        );
        track(&mut self.rule_lines, config.window_rules.len(), line);
        let vswitch = &config.vswitch;
        for (lines, length) in self.vswitch_lines.iter_mut().zip([
          vswitch.workspace_bindings.len(),
          vswitch.workspace_bindings_win.len(),
          vswitch.bindings_win.len(),
        ]) {
          track(lines, length, line);
        }
        for note in notes {
          self.warn(Some(line), format!("{context}: {note}"));
        }
        true
      }
      Err(error) => {
        self.config = before;
        self.warn(
          Some(line),
          format!("{context}: {}; keeping the default", describe(&error)),
        );
        false
      }
    }
  }

  /// Adds a warning that is not about one option's value.
  pub(crate) fn warn(&mut self, line: Option<usize>, message: String) {
    self.warnings.push(ConfigWarning { line, message });
  }

  /// Drops Expo and vswitch workspace bindings and window rules that name
  /// a workspace outside the grid, which can only be judged once the whole
  /// file is read, and returns the validated result with the warnings in
  /// line order.
  ///
  /// # Errors
  ///
  /// Whatever [`ShingleConfig::validate`] still rejects, which would be a
  /// bug in the importer.
  pub(crate) fn finish(
    mut self,
  ) -> Result<(ShingleConfig, Vec<ConfigWarning>), ConfigError> {
    let width = self.config.workspaces.width;
    let height = self.config.workspaces.height;
    let count = u64::from(width) * u64::from(height);
    let expo = std::mem::take(&mut self.config.expo.workspace_bindings);
    let lines = std::mem::take(&mut self.expo_lines);
    for (binding, line) in expo.into_iter().zip(lines) {
      if binding.workspace == 0 || binding.workspace as u64 > count {
        self.warn(
          Some(line),
          format!(
            "Expo workspace {} is outside the {width}x{height} workspace \
             grid; the binding is ignored",
            binding.workspace
          ),
        );
      } else {
        self.config.expo.workspace_bindings.push(binding);
      }
    }
    let vswitch = &mut self.config.vswitch;
    let lists = [
      std::mem::take(&mut vswitch.workspace_bindings),
      std::mem::take(&mut vswitch.workspace_bindings_win),
      std::mem::take(&mut vswitch.bindings_win),
    ];
    let mut kept: [Vec<crate::WorkspaceBinding>; 3] = Default::default();
    let all_lines = std::mem::take(&mut self.vswitch_lines);
    for ((list, lines), kept) in lists.into_iter().zip(all_lines).zip(&mut kept)
    {
      for (binding, line) in list.into_iter().zip(lines) {
        if binding.workspace == 0 || binding.workspace as u64 > count {
          self.warnings.push(ConfigWarning {
            line: Some(line),
            message: format!(
              "vswitch workspace {} is outside the {width}x{height} \
               workspace grid; the binding is ignored",
              binding.workspace
            ),
          });
        } else {
          kept.push(binding);
        }
      }
    }
    let [switch, with_window, send] = kept;
    let vswitch = &mut self.config.vswitch;
    vswitch.workspace_bindings = switch;
    vswitch.workspace_bindings_win = with_window;
    vswitch.bindings_win = send;
    let rules = std::mem::take(&mut self.config.window_rules);
    let lines = std::mem::take(&mut self.rule_lines);
    for (rule, line) in rules.into_iter().zip(lines) {
      if let RuleAction::AssignWorkspace { x, y } = rule.action
        && (x >= width || y >= height)
      {
        self.warn(
          Some(line),
          format!(
            "window rule workspace {x},{y} is outside the {width}x{height} \
             workspace grid; the rule is ignored"
          ),
        );
      } else {
        self.config.window_rules.push(rule);
      }
    }
    self.config.validate()?;
    self.warnings.sort_by_key(|warning| warning.line);
    Ok((self.config, self.warnings))
  }
}

/// The text of `error` without the "invalid config" prefix of its
/// `Display`.
fn describe(error: &ConfigError) -> String {
  match error {
    ConfigError::Invalid(message) => message.clone(),
    ConfigError::Binding(binding) => format!("invalid binding {binding:?}"),
    other => other.to_string(),
  }
}

/// An animation name as Wayfire writes it, lower-cased; validation
/// rejects the names that are not one of [`crate::ANIMATIONS`].
pub(crate) fn animation(value: &str) -> String {
  value.trim().to_lowercase()
}

/// The keys and buttons of a Wayfire activator, for options that take a
/// list. Alternatives Shingle cannot bind become notes.
pub(crate) fn bindings(
  value: &str,
  notes: &mut Vec<String>,
) -> Result<Vec<Binding>, ConfigError> {
  let mut bindings = Vec::new();
  for alternative in activator::parse(value)? {
    match alternative {
      activator::Alternative::Binding(binding) => bindings.push(binding),
      other => notes.extend(other.unsupported()),
    }
  }
  Ok(bindings)
}

/// The key or button, or the modifiers alone, of a Wayfire activator, for
/// the options that act on a modifier-alone binding
/// (`wayfire-shell/toggle_menu` and commands): the first alternative of
/// either kind, or the disabled binding if there is none. Other
/// alternatives become notes.
pub(crate) fn binding_or_modifiers(
  value: &str,
  notes: &mut Vec<String>,
) -> Result<Binding, ConfigError> {
  let mut bindings = Vec::new();
  for alternative in activator::parse(value)? {
    match alternative {
      activator::Alternative::Binding(binding) => bindings.push(binding),
      activator::Alternative::Modifiers(modifiers) => {
        bindings.push(Binding::modifiers_alone(modifiers));
      }
      other => notes.extend(other.unsupported()),
    }
  }
  Ok(first_binding(bindings, notes))
}

/// The key or button of a Wayfire activator, for options that take one:
/// the first that Shingle can bind, or the disabled binding if there is
/// none. Other alternatives become notes.
pub(crate) fn binding(
  value: &str,
  notes: &mut Vec<String>,
) -> Result<Binding, ConfigError> {
  let bindings = bindings(value, notes)?;
  Ok(first_binding(bindings, notes))
}

/// The first of `bindings`, or the disabled binding without any; a note
/// names the others, which are ignored.
fn first_binding(bindings: Vec<Binding>, notes: &mut Vec<String>) -> Binding {
  let mut bindings = bindings.into_iter();
  let first = bindings.next().unwrap_or_else(Binding::disabled);
  let rest = bindings.map(String::from).collect::<Vec<_>>();
  if !rest.is_empty() {
    notes.push(format!(
      "this option takes one binding; {} is used and {} ignored",
      String::from(first.clone()),
      rest.join(", ")
    ));
  }
  first
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn a_failed_option_keeps_the_previous_value() {
    let mut import = Import::new(ShingleConfig::default());
    import.option(3, "[test] gap", |config, _| {
      config.tiling.gaps.inner = 9;
      Ok(())
    });
    import.option(4, "[test] gap", |config, _| {
      config.tiling.gaps.inner = 1;
      Err(ConfigError::Invalid("invalid number \"x\"".into()))
    });
    // A value that parses but fails validation is undone too.
    import.option(5, "[test] alpha", |config, _| {
      config.alpha.min_value = 2.0;
      Ok(())
    });
    let (config, warnings) = import.finish().unwrap();
    assert_eq!(config.tiling.gaps.inner, 9);
    assert_eq!(
      config.alpha.min_value,
      ShingleConfig::default().alpha.min_value
    );
    assert_eq!(
      warnings.iter().map(ToString::to_string).collect::<Vec<_>>(),
      [
        "line 4: [test] gap: invalid number \"x\"; keeping the default",
        "line 5: [test] alpha: alpha.min_value must be between 0 and 1; \
         keeping the default",
      ]
    );
  }

  #[test]
  fn notes_of_a_successful_option_become_warnings() {
    let mut import = Import::new(ShingleConfig::default());
    import.option(2, "[scale] toggle", |config, notes| {
      config.keybindings.scale_toggle =
        binding("<super> KEY_P | pinch in 4", notes)?;
      Ok(())
    });
    let (config, warnings) = import.finish().unwrap();
    assert_eq!(String::from(config.keybindings.scale_toggle), "super+p");
    assert_eq!(warnings.len(), 1);
    assert!(
      warnings[0]
        .message
        .contains("\"pinch in 4\" is a touch gesture")
    );
  }

  #[test]
  fn workspaces_outside_the_final_grid_are_dropped() {
    let mut import = Import::new(ShingleConfig::default());
    for (line, workspace) in [(1, 2), (2, 5)] {
      import.option(line, "[expo] select_workspace", |config, _| {
        config
          .expo
          .workspace_bindings
          .push(crate::ExpoWorkspaceBinding {
            workspace,
            binding: Binding::new("2", Vec::new()),
          });
        Ok(())
      });
    }
    import.option(3, "[window-rules] rule", |config, _| {
      config.window_rules.push(
        "on created if app_id is \"a\" then assign_workspace 3 0"
          .to_owned()
          .try_into()?,
      );
      Ok(())
    });
    // The grid shrinks after the bindings were read, as when [core]
    // follows [expo].
    import.option(4, "[core] vwidth", |config, _| {
      config.workspaces.width = 2;
      config.workspaces.height = 2;
      Ok(())
    });
    let (config, warnings) = import.finish().unwrap();
    assert_eq!(config.expo.workspace_bindings.len(), 1);
    assert_eq!(config.expo.workspace_bindings[0].workspace, 2);
    assert!(config.window_rules.is_empty());
    let lines = warnings.iter().map(|w| w.line).collect::<Vec<_>>();
    assert_eq!(lines, [Some(2), Some(3)]);
  }

  #[test]
  fn animation_names_are_lower_cased() {
    assert_eq!(animation(" Squeezimize "), "squeezimize");
    assert_eq!(animation("wobble"), "wobble");
  }

  #[test]
  fn single_bindings_take_the_first_key() {
    let mut notes = Vec::new();
    let first = binding("<super> KEY_A | <super> KEY_B", &mut notes).unwrap();
    assert_eq!(String::from(first), "super+a");
    assert_eq!(notes.len(), 1);
    let mut notes = Vec::new();
    let none = binding("pinch in 4", &mut notes).unwrap();
    assert_eq!(none, Binding::disabled());
    assert_eq!(notes.len(), 1);
    assert_eq!(
      binding("disabled", &mut notes).unwrap(),
      Binding::disabled()
    );
    assert!(binding("KEY_A |", &mut notes).is_err());
  }
}
