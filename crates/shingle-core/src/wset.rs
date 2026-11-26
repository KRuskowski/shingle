//! Workspace sets, as in Wayfire: a grid of workspaces with one tiling tree
//! each, shown on at most one output, and the records that bring windows
//! back to a set when its output returns.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::core::{Place, WsetState};
use crate::session::{prune_workspace, tree_from_snapshot, workspace_snapshot};
use crate::{
  OutputId, Rect, Result, Shingle, ShingleError, TileTree, WindowId,
  WorkspaceId,
};

/// A workspace set's index: Wayfire's wset index, from 1. New sets take
/// the lowest free index.
#[derive(
  Clone,
  Copy,
  Debug,
  Eq,
  Hash,
  Ord,
  PartialEq,
  PartialOrd,
  Serialize,
  Deserialize,
)]
#[serde(transparent)]
pub struct WsetId(pub u32);

impl WsetId {
  /// Creates a set index.
  pub const fn new(index: u32) -> Self {
    Self(index)
  }

  /// Returns the index.
  pub const fn get(self) -> u32 {
    self.0
  }
}

/// An output's EDID identity, which finds a parked set again when its
/// monitor returns on another connector; see [`Shingle::find_parked_wset`].
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputIdentity {
  /// The manufacturer.
  pub make: String,
  /// The model.
  pub model: String,
  /// The serial number, if the EDID has one.
  pub serial: Option<String>,
}

impl OutputIdentity {
  /// Creates an identity.
  pub fn new(
    make: impl Into<String>,
    model: impl Into<String>,
    serial: Option<String>,
  ) -> Self {
    Self {
      make: make.into(),
      model: model.into(),
      serial,
    }
  }
}

/// A copy of one workspace set's state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WsetInfo {
  /// The set.
  pub id: WsetId,
  /// The output showing it, or `None` while it is parked.
  pub output: Option<OutputId>,
  /// The output it was last shown on, kept while it is parked.
  pub home: Option<OutputId>,
  /// That output's EDID identity.
  pub identity: Option<OutputIdentity>,
  /// The workarea its trees are laid out in: its output's, or the last
  /// one it had while parked.
  pub workarea: Rect,
  /// The current workspace.
  pub current_workspace: WorkspaceId,
  /// Its workspaces, in [`WorkspaceId`] order.
  pub workspaces: Vec<WorkspaceId>,
  /// Whether it keeps a copy of its trees for windows that are away from
  /// it; see [`Shingle::transfer_wset`].
  pub keeps_return_copy: bool,
}

impl Shingle {
  /// Creates a parked workspace set with the lowest free index and one
  /// empty workspace, [`WorkspaceId::ORIGIN`], laid out in `workarea`.
  ///
  /// # Errors
  ///
  /// [`ShingleError::InvalidWorkarea`] or [`ShingleError::IdExhausted`].
  pub fn create_wset(&mut self, workarea: Rect) -> Result<WsetId> {
    let state = WsetState::new(workarea)?;
    let id = self.free_wset_id()?;
    self.wsets.insert(id, state);
    Ok(id)
  }

  /// Shows `wset` on `output`, or parks it with `None`.
  ///
  /// A set shown on `output` before is parked, and `wset` stops being shown
  /// where it was; the caller gives that output another set. Attaching
  /// records `output` as the set's home and `identity` as its EDID
  /// identity; parking keeps both, so that
  /// [`Shingle::find_parked_wset`] finds the set when the output returns.
  /// The set keeps its workarea until [`Shingle::set_wset_workarea`].
  ///
  /// # Errors
  ///
  /// [`ShingleError::WsetNotFound`] or [`ShingleError::InvalidOutputId`].
  pub fn attach_wset(
    &mut self,
    wset: WsetId,
    output: Option<OutputId>,
    identity: Option<OutputIdentity>,
  ) -> Result<()> {
    self.wset_state(wset)?;
    let Some(output) = output else {
      self.wset_state_mut(wset)?.output = None;
      return Ok(());
    };
    if output.0.is_empty() {
      return Err(ShingleError::InvalidOutputId);
    }
    if let Some(shown) = self.wset_of(&output).filter(|shown| *shown != wset) {
      self.wset_state_mut(shown)?.output = None;
    }
    let state = self.wset_state_mut(wset)?;
    state.output = Some(output.clone());
    state.home = Some(output);
    state.identity = identity;
    Ok(())
  }

  /// The set shown on `output`.
  pub fn wset_of(&self, output: &OutputId) -> Option<WsetId> {
    self
      .wsets
      .iter()
      .find(|(_, state)| state.output.as_ref() == Some(output))
      .map(|(id, _)| *id)
  }

  /// Every set, shown or parked, in index order.
  pub fn wsets(&self) -> impl Iterator<Item = WsetInfo> + '_ {
    self.wsets.iter().map(|(id, state)| wset_info(*id, state))
  }

  /// One set's state.
  ///
  /// # Errors
  ///
  /// [`ShingleError::WsetNotFound`].
  pub fn wset(&self, wset: WsetId) -> Result<WsetInfo> {
    Ok(wset_info(wset, self.wset_state(wset)?))
  }

  /// Sets the workarea of a set and lays out all of its workspaces in it,
  /// keeping tile proportions.
  pub fn set_wset_workarea(
    &mut self,
    wset: WsetId,
    workarea: Rect,
  ) -> Result<()> {
    let workarea = workarea.validate()?;
    let state = self.wset_state_mut(wset)?;
    state.workarea = workarea;
    for tree in state.workspaces.values_mut() {
      tree.set_workarea(workarea)?;
    }
    Ok(())
  }

  /// Switches a set to an existing workspace.
  ///
  /// # Errors
  ///
  /// [`ShingleError::WorkspaceNotFound`] if it does not exist.
  pub fn set_wset_current_workspace(
    &mut self,
    wset: WsetId,
    workspace: WorkspaceId,
  ) -> Result<()> {
    let state = self.wset_state_mut(wset)?;
    if !state.workspaces.contains_key(&workspace) {
      return Err(ShingleError::WorkspaceNotFound);
    }
    state.current_workspace = workspace;
    Ok(())
  }

  /// Creates a workspace of a set if it does not exist and returns whether
  /// it was created.
  pub fn ensure_wset_workspace(
    &mut self,
    wset: WsetId,
    workspace: WorkspaceId,
  ) -> Result<bool> {
    let state = self.wset_state_mut(wset)?;
    let created = !state.workspaces.contains_key(&workspace);
    state.ensure_tree(workspace)?;
    Ok(created)
  }

  /// Returns the tiling tree of a set's workspace.
  pub fn wset_tree(
    &self,
    wset: WsetId,
    workspace: WorkspaceId,
  ) -> Result<&TileTree> {
    self.tree_at(Place { wset, workspace })
  }

  /// Moves a window, with the windows whose parent chain leads to it in
  /// the same set, to the end of `workspace`'s root split in `wset`,
  /// created if missing, each keeping its leaf state. This is the tree part
  /// of Wayfire's `move_view_to_output`: the moved windows join the target
  /// tree as new tiles rather than at their old place. A user's move ends
  /// the windows' away records. Returns the windows that changed place.
  ///
  /// The model is unchanged if any step fails.
  pub fn move_window_to_wset(
    &mut self,
    window: WindowId,
    wset: WsetId,
    workspace: WorkspaceId,
  ) -> Result<Vec<WindowId>> {
    let source = self.place(window)?;
    self.wset_state(wset)?;
    let mut moving = vec![window];
    moving.extend(self.windows.iter().filter_map(|(id, record)| {
      (record.place.wset == source.wset && self.descends_from(*id, window))
        .then_some(*id)
    }));
    let backup = self.clone();
    let result = (|| {
      self.wset_state_mut(wset)?.ensure_tree(workspace)?;
      let target = Place { wset, workspace };
      let mut moved = Vec::new();
      for id in moving {
        let place = self.place(id)?;
        if place == target {
          continue;
        }
        let state = self.tree_mut(place)?.detach(id)?;
        self.tree_mut(target)?.attach_with_state(id, state)?;
        self.set_place(id, target, true)?;
        moved.push(id);
      }
      Ok(moved)
    })();
    if result.is_err() {
      *self = backup;
    }
    result
  }

  /// Moves every window of `from` to the same workspace of `to`, as
  /// Wayfire's `transfer_views` does when an output goes: tiled windows
  /// are appended to the target trees in their tree order, each keeping
  /// its leaf state, and workspaces missing in `to` are created.
  ///
  /// With `remember`, the set first keeps a copy of its trees, and each
  /// moved window that is not away from another set already becomes away
  /// from `from`, so that [`Shingle::return_windows`] puts it back; a
  /// window moved twice returns to its first set. A window arriving in the
  /// set it is away from is home again. Returns the moved windows.
  ///
  /// The model is unchanged if any step fails.
  pub fn transfer_wset(
    &mut self,
    from: WsetId,
    to: WsetId,
    remember: bool,
  ) -> Result<Vec<WindowId>> {
    self.wset_state(from)?;
    self.wset_state(to)?;
    if from == to {
      return Ok(Vec::new());
    }
    let backup = self.clone();
    let result = (|| {
      let source = self.wset_state(from)?;
      let mut moving = Vec::new();
      for (workspace, tree) in &source.workspaces {
        moving.extend(tree.windows().into_iter().map(|id| (*workspace, id)));
      }
      if remember && !moving.is_empty() {
        let copy = source
          .workspaces
          .iter()
          .map(|(workspace, tree)| workspace_snapshot(*workspace, tree))
          .collect();
        self.wset_state_mut(from)?.away = Some(copy);
      }
      let mut moved = Vec::new();
      for (workspace, id) in moving {
        self.wset_state_mut(to)?.ensure_tree(workspace)?;
        let source = Place {
          wset: from,
          workspace,
        };
        let target = Place {
          wset: to,
          workspace,
        };
        let state = self.tree_mut(source)?.detach(id)?;
        self.tree_mut(target)?.attach_with_state(id, state)?;
        let record = self.record_mut(id)?;
        if remember && record.away_from.is_none() {
          record.away_from = Some(from);
        }
        self.set_place(id, target, false)?;
        moved.push(id);
      }
      Ok(moved)
    })();
    if result.is_err() {
      *self = backup;
    }
    result
  }

  /// Brings back every window that is away from `wset`, as when its output
  /// returns: each is taken from wherever it is now and put back at its
  /// old place in the set's kept copy of its trees, with the leaf state it
  /// had there. Windows closed or moved by the user since, and so no longer
  /// away from the set, are left out of the copy, whose splits close up
  /// around them. Windows the set holds now stay, appended after the
  /// returned ones; an away window missing from the copy is appended to
  /// the current workspace. The copy is dropped. Returns the windows put
  /// back, in id order.
  ///
  /// The model is unchanged if any step fails.
  pub fn return_windows(&mut self, wset: WsetId) -> Result<Vec<WindowId>> {
    self.wset_state(wset)?;
    let returning = self
      .windows
      .iter()
      .filter_map(|(id, record)| {
        (record.away_from == Some(wset)).then_some(*id)
      })
      .collect::<BTreeSet<_>>();
    let backup = self.clone();
    let result = (|| {
      let copy = self.wset_state_mut(wset)?.away.take().unwrap_or_default();
      let mut placed = BTreeSet::new();
      for snapshot in copy {
        let workspace = snapshot.workspace;
        let pruned = prune_workspace(&snapshot, |id| returning.contains(&id));
        let workarea = self.wset_state(wset)?.workarea;
        let mut tree = tree_from_snapshot(pruned, workarea)?;
        let place = Place { wset, workspace };
        for id in tree.windows() {
          let live = self.tree_mut(self.place(id)?)?.detach(id)?;
          // The hold belongs to the client's current map, not to the copy.
          tree.set_held(id, live.held)?;
          placed.insert(id);
        }
        self.wset_state_mut(wset)?.ensure_tree(workspace)?;
        let current = self.tree_mut(place)?;
        for id in current.windows() {
          let state = current.detach(id)?;
          tree.attach_with_state(id, state)?;
        }
        *self.tree_mut(place)? = tree;
        for id in &placed {
          if self.place(*id)?.wset != wset {
            self.set_place(*id, place, false)?;
          }
        }
      }
      let current = Place {
        wset,
        workspace: self.wset_state(wset)?.current_workspace,
      };
      for id in &returning {
        if !placed.contains(id) {
          let state = self.tree_mut(self.place(*id)?)?.detach(*id)?;
          self.tree_mut(current)?.attach_with_state(*id, state)?;
          self.set_place(*id, current, false)?;
        }
        self.record_mut(*id)?.away_from = None;
      }
      Ok(returning.iter().copied().collect())
    })();
    if result.is_err() {
      *self = backup;
    }
    result
  }

  /// Removes a parked, empty workspace set. Windows away from it no longer
  /// return anywhere.
  ///
  /// # Errors
  ///
  /// [`ShingleError::WsetNotFound`], [`ShingleError::WsetAttached`] for a
  /// set shown on an output, or [`ShingleError::WsetNotEmpty`].
  pub fn destroy_wset(&mut self, wset: WsetId) -> Result<()> {
    if self.wset_state(wset)?.output.is_some() {
      return Err(ShingleError::WsetAttached(wset.0));
    }
    if self
      .windows
      .values()
      .any(|record| record.place.wset == wset)
    {
      return Err(ShingleError::WsetNotEmpty(wset.0));
    }
    self.wsets.remove(&wset);
    for record in self.windows.values_mut() {
      if record.away_from == Some(wset) {
        record.away_from = None;
      }
    }
    Ok(())
  }

  /// The parked set an output that appears as `name` with `identity`
  /// should show, as Wayfire's preserve-output finds a set by
  /// `make|model|serial`.
  ///
  /// A parked set with the same EDID identity wins, the lowest index first,
  /// when the identity has a serial or only one parked set has it.
  /// Otherwise a parked set whose home is `name` is taken, except one whose
  /// identity has a serial that differs from `identity`'s, since that set
  /// belongs to another monitor. `None` if neither matches.
  pub fn find_parked_wset(
    &self,
    name: &OutputId,
    identity: Option<&OutputIdentity>,
  ) -> Option<WsetId> {
    let parked = || {
      self
        .wsets
        .iter()
        .filter(|(_, state)| state.output.is_none())
    };
    if let Some(identity) = identity {
      let matching = parked()
        .filter(|(_, state)| state.identity.as_ref() == Some(identity))
        .map(|(id, _)| *id)
        .collect::<Vec<_>>();
      if (identity.serial.is_some() && !matching.is_empty())
        || matching.len() == 1
      {
        return matching.first().copied();
      }
    }
    let other_monitor = |state: &WsetState| {
      let serial = |identity: Option<&OutputIdentity>| {
        identity.and_then(|identity| identity.serial.clone())
      };
      match (serial(state.identity.as_ref()), serial(identity)) {
        (Some(saved), Some(new)) => saved != new,
        _ => false,
      }
    };
    parked()
      .find(|(_, state)| {
        state.home.as_ref() == Some(name) && !other_monitor(state)
      })
      .map(|(id, _)| *id)
  }

  /// The lowest index from 1 that no set has.
  pub(crate) fn free_wset_id(&self) -> Result<WsetId> {
    let mut candidate = 1u32;
    for id in self.wsets.keys() {
      if id.0 != candidate {
        break;
      }
      candidate = candidate.checked_add(1).ok_or(ShingleError::IdExhausted)?;
    }
    Ok(WsetId(candidate))
  }

  /// Whether `ancestor` is on `window`'s parent chain.
  fn descends_from(&self, window: WindowId, ancestor: WindowId) -> bool {
    let mut current =
      self.windows.get(&window).and_then(|record| record.parent);
    while let Some(id) = current {
      if id == ancestor {
        return true;
      }
      current = self.windows.get(&id).and_then(|record| record.parent);
    }
    false
  }
}

fn wset_info(id: WsetId, state: &WsetState) -> WsetInfo {
  WsetInfo {
    id,
    output: state.output.clone(),
    home: state.home.clone(),
    identity: state.identity.clone(),
    workarea: state.workarea,
    current_workspace: state.current_workspace,
    workspaces: state.workspaces.keys().copied().collect(),
    keeps_return_copy: state.away.is_some(),
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::{Direction, LaunchIdentity, LeafState, WindowPresence};

  const LAPTOP: Rect = Rect::new(0, 0, 1000, 800);
  const MONITOR: Rect = Rect::new(1000, 0, 2000, 1000);
  const SECOND: WorkspaceId = WorkspaceId::new(1, 0);

  fn laptop() -> OutputId {
    OutputId::from("eDP-1")
  }

  fn monitor() -> OutputId {
    OutputId::from("DP-1")
  }

  fn dell(serial: Option<&str>) -> OutputIdentity {
    OutputIdentity::new("Dell", "U2720Q", serial.map(str::to_owned))
  }

  /// A laptop panel and a monitor, each showing its own set.
  fn docked() -> (Shingle, WsetId, WsetId) {
    let mut shingle = Shingle::new();
    shingle.add_output(laptop(), LAPTOP).unwrap();
    shingle.add_output(monitor(), MONITOR).unwrap();
    let panel = shingle.wset_of(&laptop()).unwrap();
    let desk = shingle.wset_of(&monitor()).unwrap();
    (shingle, panel, desk)
  }

  fn attach(shingle: &mut Shingle, output: &OutputId, app: &str) -> WindowId {
    shingle
      .attach_window(output, LaunchIdentity::new(app))
      .unwrap()
  }

  fn order(
    shingle: &Shingle,
    wset: WsetId,
    workspace: WorkspaceId,
  ) -> Vec<u64> {
    shingle
      .wset_tree(wset, workspace)
      .unwrap()
      .windows()
      .into_iter()
      .map(WindowId::get)
      .collect()
  }

  /// Unplugs the monitor under the `return` policy: its set is parked and
  /// its windows join the laptop's set, remembering where they were.
  fn unplug(shingle: &mut Shingle, from: WsetId, to: WsetId) {
    shingle.attach_wset(from, None, None).unwrap();
    shingle.transfer_wset(from, to, true).unwrap();
  }

  #[test]
  fn new_sets_take_the_lowest_free_index() {
    let (mut shingle, panel, desk) = docked();
    assert_eq!((panel.get(), desk.get()), (1, 2));
    let third = shingle.create_wset(LAPTOP).unwrap();
    assert_eq!(third, WsetId(3));
    shingle.destroy_wset(third).unwrap();
    shingle.attach_wset(panel, None, None).unwrap();
    shingle.destroy_wset(panel).unwrap();
    assert_eq!(shingle.create_wset(LAPTOP).unwrap(), WsetId(1));
    assert_eq!(shingle.create_wset(LAPTOP).unwrap(), WsetId(3));
    assert!(matches!(
      shingle.create_wset(Rect::new(0, 0, 0, 1)),
      Err(ShingleError::InvalidWorkarea)
    ));
    let info = shingle.wset(WsetId(3)).unwrap();
    assert_eq!(info.output, None);
    assert_eq!(info.home, None);
    assert_eq!(info.workspaces, vec![WorkspaceId::ORIGIN]);
  }

  #[test]
  fn attaching_moves_a_set_and_parks_the_one_it_replaces() {
    let (mut shingle, panel, desk) = docked();
    let window = attach(&mut shingle, &monitor(), "editor");
    // Show the monitor's set on the laptop: the laptop's set is parked
    // and the monitor shows nothing.
    shingle
      .attach_wset(desk, Some(laptop()), Some(dell(Some("7"))))
      .unwrap();
    assert_eq!(shingle.wset_of(&laptop()), Some(desk));
    assert_eq!(shingle.wset_of(&monitor()), None);
    assert_eq!(shingle.wset(panel).unwrap().output, None);
    assert_eq!(shingle.wset(panel).unwrap().home, Some(laptop()));
    assert_eq!(shingle.outputs().collect::<Vec<_>>(), [&laptop()]);
    let info = shingle.window_info(window).unwrap();
    assert_eq!(info.wset, desk);
    assert_eq!(info.location.output, laptop());
    // Parking keeps the home and identity, and the location names the
    // home while parked.
    shingle.attach_wset(desk, None, None).unwrap();
    let parked = shingle.wset(desk).unwrap();
    assert_eq!(parked.home, Some(laptop()));
    assert_eq!(parked.identity, Some(dell(Some("7"))));
    assert_eq!(shingle.window_location(window).unwrap().output, laptop());
    assert!(shingle.tree(&laptop(), WorkspaceId::ORIGIN).is_err());
    assert!(matches!(
      shingle.attach_wset(WsetId(9), None, None),
      Err(ShingleError::WsetNotFound(9))
    ));
    assert!(matches!(
      shingle.attach_wset(desk, Some(OutputId::new("")), None),
      Err(ShingleError::InvalidOutputId)
    ));
  }

  #[test]
  fn parked_sets_keep_their_trees_and_workarea_until_laid_out_again() {
    let (mut shingle, _, desk) = docked();
    let left = attach(&mut shingle, &monitor(), "left");
    let right = attach(&mut shingle, &monitor(), "right");
    shingle.attach_wset(desk, None, None).unwrap();
    assert_eq!(shingle.window_layout(left).unwrap().node.width, 1000);
    shingle.attach_wset(desk, Some(laptop()), None).unwrap();
    shingle.set_wset_workarea(desk, LAPTOP).unwrap();
    assert_eq!(shingle.output_workarea(&laptop()).unwrap(), LAPTOP);
    assert_eq!(shingle.window_layout(right).unwrap().node.width, 500);
    assert!(
      shingle
        .set_wset_workarea(desk, Rect::new(0, 0, 0, 0))
        .is_err()
    );
  }

  #[test]
  fn wset_calls_reach_parked_sets() {
    let mut shingle = Shingle::new();
    let parked = shingle.create_wset(LAPTOP).unwrap();
    assert!(shingle.ensure_wset_workspace(parked, SECOND).unwrap());
    assert!(!shingle.ensure_wset_workspace(parked, SECOND).unwrap());
    shingle.set_wset_current_workspace(parked, SECOND).unwrap();
    assert!(matches!(
      shingle.set_wset_current_workspace(parked, WorkspaceId::new(4, 4)),
      Err(ShingleError::WorkspaceNotFound)
    ));
    let window = shingle
      .attach_window_to_wset(
        parked,
        SECOND,
        LaunchIdentity::new("a"),
        LeafState::default(),
        None,
      )
      .unwrap();
    assert_eq!(order(&shingle, parked, SECOND), [window.get()]);
    let info = shingle.window_info(window).unwrap();
    assert_eq!(info.location.output, OutputId::new(""));
    assert_eq!(shingle.wset(parked).unwrap().current_workspace, SECOND);
    assert_eq!(shingle.wsets().count(), 1);
  }

  #[test]
  fn moving_to_a_set_appends_with_state_and_takes_dialogs_along() {
    let (mut shingle, panel, desk) = docked();
    let parent = attach(&mut shingle, &laptop(), "parent");
    let dialog = attach(&mut shingle, &laptop(), "dialog");
    let other = attach(&mut shingle, &laptop(), "other");
    shingle.set_window_parent(dialog, Some(parent)).unwrap();
    shingle
      .set_floating(dialog, Some(Rect::new(10, 20, 300, 200)))
      .unwrap();
    let resident = attach(&mut shingle, &monitor(), "resident");
    let moved = shingle.move_window_to_wset(parent, desk, SECOND).unwrap();
    assert_eq!(moved, [parent, dialog]);
    assert_eq!(order(&shingle, panel, WorkspaceId::ORIGIN), [other.get()]);
    assert_eq!(order(&shingle, desk, SECOND), [parent.get(), dialog.get()]);
    assert_eq!(order(&shingle, desk, WorkspaceId::ORIGIN), [resident.get()]);
    let info = shingle.window_info(dialog).unwrap();
    assert_eq!(info.state.floating, Some(Rect::new(10, 20, 300, 200)));
    assert_eq!(info.location.output, monitor());
    // A move to where the window already is changes nothing.
    assert!(
      shingle
        .move_window_to_wset(other, panel, WorkspaceId::ORIGIN)
        .unwrap()
        .is_empty()
    );
    let before = shingle.snapshot();
    assert!(
      shingle
        .move_window_to_wset(other, WsetId(9), WorkspaceId::ORIGIN)
        .is_err()
    );
    assert_eq!(shingle.snapshot(), before);
  }

  #[test]
  fn transfer_without_remembering_moves_everything_once() {
    let (mut shingle, panel, desk) = docked();
    let a = attach(&mut shingle, &monitor(), "a");
    shingle.ensure_workspace(&monitor(), SECOND).unwrap();
    shingle.set_current_workspace(&monitor(), SECOND).unwrap();
    let b = attach(&mut shingle, &monitor(), "b");
    let c = attach(&mut shingle, &monitor(), "c");
    shingle.set_maximized(c, true).unwrap();
    let local = attach(&mut shingle, &laptop(), "local");
    shingle.attach_wset(desk, None, None).unwrap();
    let moved = shingle.transfer_wset(desk, panel, false).unwrap();
    assert_eq!(moved, [a, b, c]);
    assert_eq!(
      order(&shingle, panel, WorkspaceId::ORIGIN),
      [local.get(), a.get()]
    );
    assert_eq!(order(&shingle, panel, SECOND), [b.get(), c.get()]);
    assert!(shingle.window_info(c).unwrap().state.maximized);
    assert_eq!(shingle.window_info(a).unwrap().away_from, None);
    assert!(!shingle.wset(desk).unwrap().keeps_return_copy);
    shingle.destroy_wset(desk).unwrap();
    assert!(
      shingle
        .transfer_wset(panel, panel, true)
        .unwrap()
        .is_empty()
    );
  }

  #[test]
  fn returning_windows_go_back_to_their_old_places() {
    let (mut shingle, panel, desk) = docked();
    let a = attach(&mut shingle, &monitor(), "a");
    let b = attach(&mut shingle, &monitor(), "b");
    let c = attach(&mut shingle, &monitor(), "c");
    shingle.drop_window(c, b, Direction::Down).unwrap();
    shingle.resize_window(a, Direction::Right, 300, 50).unwrap();
    shingle
      .set_floating(b, Some(Rect::new(1100, 100, 400, 300)))
      .unwrap();
    let before = shingle
      .wset_tree(desk, WorkspaceId::ORIGIN)
      .unwrap()
      .clone();
    let local = attach(&mut shingle, &laptop(), "local");

    unplug(&mut shingle, desk, panel);
    assert!(shingle.wset(desk).unwrap().keeps_return_copy);
    assert_eq!(
      order(&shingle, panel, WorkspaceId::ORIGIN),
      [local.get(), a.get(), b.get(), c.get()]
    );
    for window in [a, b, c] {
      assert_eq!(shingle.window_info(window).unwrap().away_from, Some(desk));
    }
    // While away, the window's state may change; the return restores the
    // state it had in its own set.
    shingle
      .set_floating(b, Some(Rect::new(5, 5, 50, 50)))
      .unwrap();

    shingle.attach_wset(desk, Some(monitor()), None).unwrap();
    assert_eq!(shingle.return_windows(desk).unwrap(), [a, b, c]);
    let after = shingle.wset_tree(desk, WorkspaceId::ORIGIN).unwrap();
    assert_eq!(after.windows(), before.windows());
    assert_eq!(after.layout(), before.layout());
    assert_eq!(
      after.leaf_state(b).unwrap().floating,
      Some(Rect::new(1100, 100, 400, 300))
    );
    assert_eq!(order(&shingle, panel, WorkspaceId::ORIGIN), [local.get()]);
    assert_eq!(shingle.window_info(a).unwrap().away_from, None);
    assert!(!shingle.wset(desk).unwrap().keeps_return_copy);
    assert!(shingle.return_windows(desk).unwrap().is_empty());
  }

  #[test]
  fn closed_and_moved_windows_are_pruned_from_the_return() {
    let (mut shingle, panel, desk) = docked();
    let a = attach(&mut shingle, &monitor(), "a");
    let b = attach(&mut shingle, &monitor(), "b");
    let c = attach(&mut shingle, &monitor(), "c");
    let d = attach(&mut shingle, &monitor(), "d");
    // A nested split [b, c] that loses one child closes up around it.
    shingle.drop_window(c, b, Direction::Down).unwrap();
    unplug(&mut shingle, desk, panel);
    shingle.detach_window(c).unwrap();
    // A move by the user ends the away record.
    shingle.move_window_to_wset(d, panel, SECOND).unwrap();
    assert_eq!(shingle.window_info(d).unwrap().away_from, None);
    // A window that mapped on the parked set meanwhile stays there.
    let late = shingle
      .attach_window_to_wset(
        desk,
        WorkspaceId::ORIGIN,
        LaunchIdentity::new("late"),
        LeafState::default(),
        None,
      )
      .unwrap();

    assert_eq!(shingle.return_windows(desk).unwrap(), [a, b]);
    assert_eq!(
      order(&shingle, desk, WorkspaceId::ORIGIN),
      [a.get(), b.get(), late.get()]
    );
    let tree = shingle.wset_tree(desk, WorkspaceId::ORIGIN).unwrap();
    assert_eq!(tree.direction(), crate::SplitDirection::Vertical);
    assert_eq!(order(&shingle, panel, SECOND), [d.get()]);
    shingle.snapshot().validate().unwrap();
  }

  #[test]
  fn a_window_moved_twice_returns_to_its_first_set() {
    let (mut shingle, panel, desk) = docked();
    shingle.add_output("HDMI-A-1".into(), MONITOR).unwrap();
    let third = shingle.wset_of(&"HDMI-A-1".into()).unwrap();
    let a = attach(&mut shingle, &monitor(), "a");
    let native = attach(&mut shingle, &"HDMI-A-1".into(), "native");
    // The monitor goes and its window moves to the third output, which
    // then goes too, taking both windows to the laptop.
    unplug(&mut shingle, desk, third);
    unplug(&mut shingle, third, panel);
    assert_eq!(shingle.window_info(a).unwrap().away_from, Some(desk));
    assert_eq!(shingle.window_info(native).unwrap().away_from, Some(third));
    // The third returns first and takes only its own window back.
    shingle
      .attach_wset(third, Some("HDMI-A-1".into()), None)
      .unwrap();
    assert_eq!(shingle.return_windows(third).unwrap(), [native]);
    assert_eq!(order(&shingle, third, WorkspaceId::ORIGIN), [native.get()]);
    assert_eq!(shingle.window_info(a).unwrap().wset, panel);
    shingle.attach_wset(desk, Some(monitor()), None).unwrap();
    assert_eq!(shingle.return_windows(desk).unwrap(), [a]);
    assert_eq!(order(&shingle, desk, WorkspaceId::ORIGIN), [a.get()]);
    assert!(
      shingle
        .wset_tree(panel, WorkspaceId::ORIGIN)
        .unwrap()
        .is_empty()
    );
  }

  #[test]
  fn a_window_arriving_in_the_set_it_is_away_from_is_home() {
    let (mut shingle, panel, desk) = docked();
    let a = attach(&mut shingle, &monitor(), "a");
    unplug(&mut shingle, desk, panel);
    // The laptop's set moves to the monitor's set (both parked and
    // unparked again in some order): the window is back in its own set.
    shingle.attach_wset(desk, Some(monitor()), None).unwrap();
    shingle.attach_wset(panel, None, None).unwrap();
    shingle.transfer_wset(panel, desk, true).unwrap();
    let info = shingle.window_info(a).unwrap();
    assert_eq!((info.wset, info.away_from), (desk, None));
  }

  #[test]
  fn away_windows_without_a_copy_join_the_current_workspace() {
    let (mut shingle, panel, desk) = docked();
    let a = attach(&mut shingle, &monitor(), "a");
    unplug(&mut shingle, desk, panel);
    shingle.wset_state_mut(desk).unwrap().away = None;
    shingle.ensure_wset_workspace(desk, SECOND).unwrap();
    shingle.set_wset_current_workspace(desk, SECOND).unwrap();
    assert_eq!(shingle.return_windows(desk).unwrap(), [a]);
    assert_eq!(order(&shingle, desk, SECOND), [a.get()]);
  }

  #[test]
  fn destroying_a_set_needs_it_parked_and_empty() {
    let (mut shingle, panel, desk) = docked();
    let a = attach(&mut shingle, &monitor(), "a");
    assert!(matches!(
      shingle.destroy_wset(desk),
      Err(ShingleError::WsetAttached(2))
    ));
    shingle.attach_wset(desk, None, None).unwrap();
    assert!(matches!(
      shingle.destroy_wset(desk),
      Err(ShingleError::WsetNotEmpty(2))
    ));
    shingle.transfer_wset(desk, panel, true).unwrap();
    shingle.destroy_wset(desk).unwrap();
    assert_eq!(shingle.window_info(a).unwrap().away_from, None);
    assert!(matches!(
      shingle.destroy_wset(desk),
      Err(ShingleError::WsetNotFound(2))
    ));
  }

  #[test]
  fn parked_sets_are_found_by_identity_then_by_name() {
    let mut shingle = Shingle::new();
    let with_serial = shingle.create_wset(LAPTOP).unwrap();
    shingle
      .attach_wset(with_serial, Some("DP-1".into()), Some(dell(Some("A"))))
      .unwrap();
    shingle.attach_wset(with_serial, None, None).unwrap();
    // The monitor comes back on another connector.
    assert_eq!(
      shingle.find_parked_wset(&"DP-2".into(), Some(&dell(Some("A")))),
      Some(with_serial)
    );
    // Another monitor on its connector does not take it.
    assert_eq!(
      shingle.find_parked_wset(&"DP-1".into(), Some(&dell(Some("B")))),
      None
    );
    // An output without an identity falls back to the name.
    assert_eq!(
      shingle.find_parked_wset(&"DP-1".into(), None),
      Some(with_serial)
    );

    // Two identical monitors without serials are told apart by connector.
    let mut shingle = Shingle::new();
    let first = shingle.create_wset(LAPTOP).unwrap();
    let second = shingle.create_wset(LAPTOP).unwrap();
    for (wset, name) in [(first, "DP-1"), (second, "DP-2")] {
      shingle
        .attach_wset(wset, Some(name.into()), Some(dell(None)))
        .unwrap();
      shingle.attach_wset(wset, None, None).unwrap();
    }
    assert_eq!(
      shingle.find_parked_wset(&"DP-2".into(), Some(&dell(None))),
      Some(second)
    );
    assert_eq!(
      shingle.find_parked_wset(&"DP-3".into(), Some(&dell(None))),
      None
    );
    // With only one of them parked, its identity is unique.
    shingle
      .attach_wset(second, Some("DP-2".into()), None)
      .unwrap();
    assert_eq!(
      shingle.find_parked_wset(&"DP-3".into(), Some(&dell(None))),
      Some(first)
    );
    // Shown sets are never found.
    assert_eq!(shingle.find_parked_wset(&"DP-2".into(), None), None);
  }

  #[test]
  fn renaming_onto_a_shown_output_keeps_away_records() {
    let (mut shingle, panel, desk) = docked();
    let a = attach(&mut shingle, &monitor(), "a");
    let headless = OutputId::from("HEADLESS-1");
    shingle.add_output(headless.clone(), LAPTOP).unwrap();
    let live = shingle.wset_of(&headless).unwrap();
    unplug(&mut shingle, desk, panel);
    // The laptop's set merges into the headless one: the window moves
    // with it and still returns to the monitor's set.
    shingle.rename_output(&laptop(), headless.clone()).unwrap();
    let info = shingle.window_info(a).unwrap();
    assert_eq!((info.wset, info.away_from), (live, Some(desk)));
    assert!(shingle.wset(panel).is_err());
    assert_eq!(
      shingle.window_info(a).unwrap().presence,
      WindowPresence::Mapped
    );
  }
}
