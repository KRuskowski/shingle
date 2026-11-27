//! The panel as Gloomy widgets, laid out by the launcher itself so that
//! pointer targets are known exactly (docs/design/launcher.md, section 3).
//!
//! Every widget is placed absolutely in panel coordinates. Gloomy draws all
//! text after all boxes, so the context menu and the dragged entry go into
//! a second tree that is drawn on Gloomy's overlay layers.

use gloomy_core::{
  BoxStyle, Direction, Layout, TextInputStyle, Widget,
  widget::{TextAlign, WidgetBounds},
};

use crate::{
  catalog::CategoryKey,
  index::EntryKind,
  model::{Model, Row, Target},
  theme::{Color, Theme},
};

/// The id of the search field, which Gloomy draws with a caret when it has
/// the focus.
pub const FIELD_ID: &str = "query";
/// The smallest margin left around the panel on a small output.
const MARGIN: f32 = 16.0;

/// A rectangle in panel coordinates.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
  /// Left edge.
  pub x: f32,
  /// Top edge.
  pub y: f32,
  /// Width.
  pub width: f32,
  /// Height.
  pub height: f32,
}

impl Rect {
  /// A rectangle.
  pub fn new(x: f32, y: f32, width: f32, height: f32) -> Self {
    Self {
      x,
      y,
      width,
      height,
    }
  }

  /// Whether `point` is inside.
  pub fn contains(&self, point: (f32, f32)) -> bool {
    point.0 >= self.x
      && point.0 < self.x + self.width
      && point.1 >= self.y
      && point.1 < self.y + self.height
  }
}

/// The sizes of the panel's parts, from the theme's font size.
#[derive(Clone, Copy, Debug)]
struct Metrics {
  padding: f32,
  field: f32,
  gap: f32,
  categories: f32,
  category_row: f32,
  row: f32,
  status: f32,
  menu_width: f32,
  menu_item: f32,
  small: f32,
  icon: f32,
}

impl Metrics {
  fn new(theme: &Theme, width: f32) -> Self {
    let size = theme.font_size;
    Self {
      padding: size.round(),
      field: (size * 2.75).round(),
      gap: (size * 0.75).round(),
      categories: (size * 11.25).round().min((width * 0.32).round()),
      category_row: (size * 1.85).round(),
      row: (size * 2.6).round(),
      status: (size * 1.5).round(),
      menu_width: (size * 14.0).round(),
      menu_item: (size * 1.9).round(),
      small: (size * 0.8).round(),
      icon: icon_size(theme),
    }
  }

  fn body(&self, size: (f32, f32)) -> Rect {
    let top = self.padding + self.field + self.gap;
    let bottom = size.1 - self.padding - self.status;
    Rect::new(
      self.padding,
      top,
      size.0 - 2.0 * self.padding,
      (bottom - top - self.gap * 0.5).max(0.0),
    )
  }
}

/// The panel's size for an output of `output` logical pixels: the theme's,
/// shrunk to leave a margin.
pub fn panel_size(theme: &Theme, output: (f32, f32)) -> (f32, f32) {
  let fit = |wanted: f32, available: f32| {
    wanted
      .min((available - 2.0 * MARGIN).max(available * 0.5))
      .floor()
  };
  (fit(theme.width, output.0), fit(theme.height, output.1))
}

/// Where the panel goes on an output of `output` logical pixels.
pub fn panel_origin(theme: &Theme, output: (f32, f32)) -> (f32, f32) {
  let size = panel_size(theme, output);
  (
    ((output.0 - size.0) / 2.0).floor(),
    ((output.1 - size.1) / 2.0).floor(),
  )
}

/// The side of an entry's icon in logical pixels, for the theme's font
/// size.
pub fn icon_size(theme: &Theme) -> f32 {
  (theme.font_size * 1.75).round()
}

/// Where a row's icon goes, from the row's left edge.
const ICON_LEFT: f32 = 16.0;

/// How many entry rows fit in a panel of `size`.
pub fn page_size(theme: &Theme, size: (f32, f32)) -> usize {
  let metrics = Metrics::new(theme, size.0);
  ((metrics.body(size).height / metrics.row).floor() as usize).max(1)
}

/// An icon to draw over the main tree, below its text.
#[derive(Clone, Debug, PartialEq)]
pub struct IconSlot {
  /// Where, in panel coordinates.
  pub rect: Rect,
  /// The icon's name or path, as the entry gives it.
  pub icon: String,
  /// The row it belongs to, an index of [`Model::rows`].
  pub row: usize,
}

/// One frame of the panel.
pub struct Frame {
  /// Everything but the overlays.
  pub main: Widget,
  /// The context menu and the dragged entry.
  pub overlay: Widget,
  /// The rows' icons, drawn after the main tree's boxes and before its
  /// text; rows whose icon is not loaded get a letter instead.
  pub icons: Vec<IconSlot>,
  /// Pointer targets in drawing order; the last that contains a point is
  /// what the point is over.
  pub regions: Vec<(Rect, Target)>,
}

impl Frame {
  /// What `point` (panel coordinates) of a panel of `size` is over.
  pub fn hit(&self, point: (f32, f32), size: (f32, f32)) -> Target {
    if !Rect::new(0.0, 0.0, size.0, size.1).contains(point) {
      return Target::Backdrop;
    }
    self
      .regions
      .iter()
      .rev()
      .find(|(rect, _)| rect.contains(point))
      .map(|(_, target)| *target)
      .unwrap_or(Target::Panel)
  }
}

/// Builds the frame of `model` for a panel of `size`. `measure` gives the
/// width of a text at a font size, for truncating names, and `loaded`
/// whether an icon is ready to draw.
pub fn build(
  model: &Model,
  theme: &Theme,
  size: (f32, f32),
  measure: &dyn Fn(&str, f32) -> f32,
  loaded: &dyn Fn(&str) -> bool,
) -> Frame {
  let metrics = Metrics::new(theme, size.0);
  let mut builder = Builder {
    model,
    theme,
    metrics,
    measure,
    loaded,
    main: Vec::new(),
    overlay: Vec::new(),
    icons: Vec::new(),
    regions: vec![(Rect::new(0.0, 0.0, size.0, size.1), Target::Panel)],
  };
  builder.field(size);
  let body = metrics.body(size);
  builder.categories(body);
  builder.rows(body);
  builder.status(size);
  builder.menu(size);
  builder.drag();
  let mut main = boxed(
    Rect::new(0.0, 0.0, size.0, size.1),
    BoxStyle::fill(theme.panel).with_border(theme.selection, 1.0),
  );
  set_children(&mut main, builder.main);
  let mut overlay =
    boxed(Rect::new(0.0, 0.0, size.0, size.1), BoxStyle::default());
  set_children(&mut overlay, builder.overlay);
  Frame {
    main,
    overlay,
    icons: builder.icons,
    regions: builder.regions,
  }
}

struct Builder<'a> {
  model: &'a Model,
  theme: &'a Theme,
  metrics: Metrics,
  measure: &'a dyn Fn(&str, f32) -> f32,
  loaded: &'a dyn Fn(&str) -> bool,
  main: Vec<Widget>,
  overlay: Vec<Widget>,
  icons: Vec<IconSlot>,
  regions: Vec<(Rect, Target)>,
}

impl Builder<'_> {
  /// `text` cut with an ellipsis to fit `width` at `size`.
  fn fit(&self, text: &str, width: f32, size: f32) -> String {
    let text = plain(text);
    if (self.measure)(&text, size) <= width {
      return text;
    }
    let characters = text.chars().collect::<Vec<_>>();
    let mut count = characters.len();
    while count > 0 {
      count -= 1;
      let cut = format!("{}…", characters[..count].iter().collect::<String>());
      if (self.measure)(&cut, size) <= width {
        return cut;
      }
    }
    String::new()
  }

  /// A rounded square at `rect` with the first letter of `name`, for an
  /// entry whose icon is missing or not loaded yet.
  fn letter_tile(&self, name: &str, rect: Rect) -> Widget {
    let theme = self.theme;
    let letter = name
      .chars()
      .find(|character| character.is_alphanumeric())
      .map(|character| character.to_uppercase().collect::<String>())
      .unwrap_or_default();
    let mut tile = boxed(
      rect,
      BoxStyle::fill(theme.field).with_radius(rect.width / 4.0),
    );
    set_children(
      &mut tile,
      vec![label(
        &letter,
        Rect::new(0.0, 0.0, rect.width, rect.height),
        theme.font_size,
        theme.dim_text,
        TextAlign::Center,
      )],
    );
    tile
  }

  fn field(&mut self, size: (f32, f32)) {
    let theme = self.theme;
    let metrics = self.metrics;
    let rect = Rect::new(
      metrics.padding,
      metrics.padding,
      size.0 - 2.0 * metrics.padding,
      metrics.field,
    );
    let prompt = self.model.prompt.as_ref();
    let border = if prompt.is_some() {
      theme.accent
    } else {
      theme.selection
    };
    let mut field = boxed(
      rect,
      BoxStyle::fill(theme.field)
        .with_radius(6.0)
        .with_border(border, 1.0),
    );
    let mut children = Vec::new();
    let mut input_x = 0.0;
    if let Some(prompt) = prompt {
      let label_text = prompt.kind.label();
      let width = (self.measure)(label_text, theme.font_size);
      children.push(label(
        label_text,
        Rect::new(12.0, 0.0, width, rect.height),
        theme.font_size,
        theme.accent,
        TextAlign::Left,
      ));
      input_x = width + 12.0;
    }
    let (value, placeholder) = match prompt {
      Some(prompt) => (prompt.text.clone(), String::new()),
      None => (
        self.model.query.clone(),
        "Type to search, Enter to launch".to_owned(),
      ),
    };
    children.push(text_input(
      Rect::new(input_x, 0.0, rect.width - input_x, rect.height),
      value,
      placeholder,
      theme,
    ));
    set_children(&mut field, children);
    self.main.push(field);
    self.regions.push((rect, Target::Field));
  }

  fn categories(&mut self, body: Rect) {
    let theme = self.theme;
    let metrics = self.metrics;
    let categories = &self.model.catalog.categories;
    let fit = ((body.height / metrics.category_row).floor() as usize).max(1);
    let first = (self.model.category + 1).saturating_sub(fit);
    let drop_target =
      self.model.drag.as_ref().and_then(|drag| match drag.over {
        Target::Category(index) => Some(index),
        _ => None,
      });
    for (slot, index) in (first..categories.len()).take(fit).enumerate() {
      let category = &categories[index];
      let rect = Rect::new(
        body.x,
        body.y + slot as f32 * metrics.category_row,
        metrics.categories,
        metrics.category_row - 2.0,
      );
      let selected = index == self.model.category && !self.model.searching();
      let mut style = if selected {
        BoxStyle::fill(theme.selection)
      } else if self.model.hover == Target::Category(index) {
        BoxStyle::fill(theme.field)
      } else {
        BoxStyle::default()
      };
      if drop_target == Some(index) {
        style = style.with_border(theme.accent, 2.0);
      }
      let mut row = boxed(rect, style.with_radius(5.0));
      let count = category.entries.len().to_string();
      let count_width = (self.measure)(&count, metrics.small) + 10.0;
      let special = !matches!(category.key, CategoryKey::Named(_));
      let colour = if special && !selected {
        theme.accent_text()
      } else {
        theme.text
      };
      set_children(
        &mut row,
        vec![
          label(
            &self.fit(
              &category.title,
              rect.width - count_width - 12.0,
              theme.font_size,
            ),
            Rect::new(10.0, 0.0, rect.width, rect.height),
            theme.font_size,
            colour,
            TextAlign::Left,
          ),
          label(
            &count,
            Rect::new(0.0, 0.0, rect.width - 10.0, rect.height),
            metrics.small,
            theme.dim_text,
            TextAlign::Right,
          ),
        ],
      );
      self.main.push(row);
      self.regions.push((rect, Target::Category(index)));
    }
  }

  fn rows(&mut self, body: Rect) {
    let theme = self.theme;
    let metrics = self.metrics;
    let model = self.model;
    let rows = model.rows();
    let left = body.x + metrics.categories + metrics.gap;
    let width = body.x + body.width - left;
    let page = ((body.height / metrics.row).floor() as usize).max(1);
    let drop_row = model.drag.as_ref().and_then(|drag| match drag.over {
      Target::Row(row) if !model.searching() => Some(row),
      _ => None,
    });
    if rows.is_empty() {
      let empty = if model.searching() {
        "No matches"
      } else {
        "Nothing here yet: drag entries onto this category, or press Ctrl+D \
         on one to make it a favourite"
      };
      self.main.push(label(
        &self.fit(empty, width - 20.0, metrics.small),
        Rect::new(left + 10.0, body.y, width, metrics.row),
        metrics.small,
        theme.dim_text,
        TextAlign::Left,
      ));
    }
    for (slot, index) in (model.scroll..rows.len()).take(page).enumerate() {
      let rect = Rect::new(
        left,
        body.y + slot as f32 * metrics.row,
        width,
        metrics.row - 2.0,
      );
      let selected = index == model.selected;
      let style = if selected {
        BoxStyle::fill(theme.selection)
      } else if model.hover == Target::Row(index) {
        BoxStyle::fill(theme.field)
      } else {
        BoxStyle::default()
      };
      let mut row = boxed(rect, style.with_radius(5.0));
      let mut children = Vec::new();
      let text_left = ICON_LEFT + metrics.icon + 10.0;
      let catalog = &model.catalog;
      let shown = match rows[index] {
        Row::Entry(position) => {
          let entry = &catalog.entries[position];
          let detail = if model.searching() {
            match entry.kind {
              EntryKind::Command => "command".to_owned(),
              _ => catalog.category_of(position).unwrap_or_default().to_owned(),
            }
          } else {
            [&entry.generic_name, &entry.comment]
              .into_iter()
              .find(|text| {
                !text.is_empty() && **text != catalog.names[position]
              })
              .cloned()
              .unwrap_or_default()
          };
          Some((
            catalog.names[position].as_str(),
            detail,
            entry.icon.as_str(),
            catalog.is_favourite(position),
          ))
        }
        Row::Action(position, action) => {
          let entry = &catalog.entries[position];
          let action = &entry.actions[action];
          let icon = if action.icon.is_empty() {
            &entry.icon
          } else {
            &action.icon
          };
          Some((
            action.name.as_str(),
            catalog.names[position].clone(),
            icon.as_str(),
            false,
          ))
        }
        Row::Run => None,
      };
      match shown {
        Some((name, detail, icon, favourite)) => {
          if favourite {
            children.push(boxed(
              Rect::new(5.0, rect.height / 2.0 - 3.0, 6.0, 6.0),
              BoxStyle::fill(theme.accent_text()).with_radius(3.0),
            ));
          }
          let icon_rect = Rect::new(
            ICON_LEFT,
            ((rect.height - metrics.icon) / 2.0).round(),
            metrics.icon,
            metrics.icon,
          );
          if !icon.is_empty() && (self.loaded)(icon) {
            self.icons.push(IconSlot {
              rect: Rect::new(
                rect.x + icon_rect.x,
                rect.y + icon_rect.y,
                icon_rect.width,
                icon_rect.height,
              ),
              icon: icon.to_owned(),
              row: index,
            });
          } else {
            children.push(self.letter_tile(name, icon_rect));
          }
          let detail = self.fit(&detail, rect.width * 0.4, metrics.small);
          let detail_width = (self.measure)(&detail, metrics.small);
          let name = self.fit(
            name,
            rect.width - detail_width - text_left - 24.0,
            theme.font_size,
          );
          children.push(label(
            &name,
            Rect::new(text_left, 0.0, rect.width, rect.height),
            theme.font_size,
            theme.text,
            TextAlign::Left,
          ));
          children.push(label(
            &detail,
            Rect::new(0.0, 0.0, rect.width - 12.0, rect.height),
            metrics.small,
            theme.dim_text,
            TextAlign::Right,
          ));
        }
        None => {
          let text = format!("Run: {}", model.query.trim());
          children.push(label(
            &self.fit(&text, rect.width - text_left - 12.0, theme.font_size),
            Rect::new(text_left, 0.0, rect.width, rect.height),
            theme.font_size,
            theme.accent_text(),
            TextAlign::Left,
          ));
        }
      }
      set_children(&mut row, children);
      self.main.push(row);
      if drop_row == Some(index) {
        self.main.push(boxed(
          Rect::new(rect.x, rect.y - 1.0, rect.width, 2.0),
          BoxStyle::fill(theme.accent_text()),
        ));
      }
      self.regions.push((rect, Target::Row(index)));
    }
    if rows.len() > page {
      let track = page as f32 * metrics.row;
      let thumb = (track * page as f32 / rows.len() as f32).max(12.0);
      let top =
        (track - thumb) * model.scroll as f32 / (rows.len() - page) as f32;
      self.main.push(boxed(
        Rect::new(body.x + body.width + 4.0, body.y + top, 3.0, thumb),
        BoxStyle::fill(theme.dim_text).with_radius(1.5),
      ));
    }
  }

  fn status(&mut self, size: (f32, f32)) {
    let theme = self.theme;
    let metrics = self.metrics;
    let model = self.model;
    let (text, colour) = if !model.status.is_empty() {
      (model.status.clone(), theme.accent_text())
    } else if model.prompt.is_some() {
      (
        "Enter accepts · Esc cancels · Ctrl+U clears".to_owned(),
        theme.dim_text,
      )
    } else if model.menu.is_some() {
      ("↑↓ choose · Enter · Esc closes".to_owned(), theme.dim_text)
    } else if model.drag.is_some() {
      (
        "Drop on a row to reorder, on a category to move · Esc cancels"
          .to_owned(),
        theme.dim_text,
      )
    } else {
      (
        "Enter launch · Ctrl+D favourite · F2 rename · Ctrl+N shortcut · \
         Ctrl+M move · Ctrl+H hide · Ctrl+↑↓ reorder · Tab category"
          .to_owned(),
        theme.dim_text,
      )
    };
    let width = size.0 - 2.0 * metrics.padding;
    self.main.push(label(
      &self.fit(&text, width, metrics.small),
      Rect::new(
        metrics.padding,
        size.1 - metrics.padding - metrics.status,
        width,
        metrics.status,
      ),
      metrics.small,
      colour,
      TextAlign::Left,
    ));
  }

  fn menu(&mut self, size: (f32, f32)) {
    let Some(menu) = &self.model.menu else {
      return;
    };
    let theme = self.theme;
    let metrics = self.metrics;
    let height = menu.items.len() as f32 * metrics.menu_item + 8.0;
    // A menu opened by a key goes below the selected row's name.
    let at = menu
      .at
      .or_else(|| {
        self.regions.iter().find_map(|(rect, target)| {
          (*target == Target::Row(self.model.selected)).then_some((
            rect.x + ICON_LEFT + metrics.icon,
            rect.y + rect.height,
          ))
        })
      })
      .unwrap_or((size.0 / 3.0, size.1 / 3.0));
    let x = at.0.min(size.0 - metrics.menu_width - 4.0).max(4.0);
    let y = at.1.min(size.1 - height - 4.0).max(4.0);
    let rect = Rect::new(x, y, metrics.menu_width, height);
    let mut panel = boxed(
      rect,
      BoxStyle::fill(theme.field)
        .with_radius(6.0)
        .with_border(theme.selection, 1.0)
        .with_shadow((0.0, 4.0), 12.0, (0.0, 0.0, 0.0, 0.5)),
    );
    let mut children = Vec::new();
    for (item, (_, text)) in menu.items.iter().enumerate() {
      let item_rect = Rect::new(
        4.0,
        4.0 + item as f32 * metrics.menu_item,
        rect.width - 8.0,
        metrics.menu_item,
      );
      let style = if item == menu.selected {
        BoxStyle::fill(theme.selection).with_radius(4.0)
      } else {
        BoxStyle::default()
      };
      let mut row = boxed(item_rect, style);
      set_children(
        &mut row,
        vec![label(
          text,
          Rect::new(10.0, 0.0, item_rect.width, item_rect.height),
          metrics.small + 1.0,
          theme.text,
          TextAlign::Left,
        )],
      );
      children.push(row);
      self.regions.push((
        Rect::new(
          rect.x + item_rect.x,
          rect.y + item_rect.y,
          item_rect.width,
          item_rect.height,
        ),
        Target::MenuItem(item),
      ));
    }
    set_children(&mut panel, children);
    self.overlay.push(panel);
  }

  fn drag(&mut self) {
    let Some(drag) = &self.model.drag else {
      return;
    };
    let theme = self.theme;
    let metrics = self.metrics;
    let width = (self.measure)(&plain(&drag.name), theme.font_size) + 28.0;
    let rect = Rect::new(
      drag.at.0 + 8.0,
      drag.at.1 - metrics.row / 2.0,
      width,
      metrics.row - 6.0,
    );
    let (r, g, b, _) = theme.selection;
    let mut ghost = boxed(
      rect,
      BoxStyle::fill((r, g, b, 0.9))
        .with_radius(5.0)
        .with_border(theme.accent_text(), 1.0),
    );
    set_children(
      &mut ghost,
      vec![label(
        &plain(&drag.name),
        Rect::new(14.0, 0.0, rect.width, rect.height),
        theme.font_size,
        theme.text,
        TextAlign::Left,
      )],
    );
    self.overlay.push(ghost);
  }
}

impl Theme {
  /// The accent, lightened enough to read as text on the panel.
  fn accent_text(&self) -> Color {
    let (r, g, b, a) = self.accent;
    let lift = |value: f32| value + (1.0 - value) * 0.45;
    (lift(r), lift(g), lift(b), a)
  }
}

/// `text` with `<` and `>` replaced when both occur, since Gloomy reads
/// such a label as markup.
fn plain(text: &str) -> String {
  if text.contains('<') && text.contains('>') {
    text.replace('<', "‹").replace('>', "›")
  } else {
    text.to_owned()
  }
}

/// A box at `rect` with `style` and no layout of its own.
fn boxed(rect: Rect, style: BoxStyle) -> Widget {
  let mut widget = Widget::container();
  if let Widget::Container {
    bounds,
    width,
    height,
    style: widget_style,
    layout,
    ..
  } = &mut widget
  {
    *bounds = WidgetBounds {
      x: rect.x,
      y: rect.y,
      width: rect.width,
      height: rect.height,
    };
    *width = Some(rect.width);
    *height = Some(rect.height);
    *widget_style = style;
    *layout = Layout {
      direction: Direction::None,
      ..Layout::default()
    };
  }
  widget
}

fn set_children(widget: &mut Widget, new: Vec<Widget>) {
  if let Widget::Container { children, .. } = widget {
    *children = new;
  }
}

/// A one-line label in `rect`, centred vertically; `rect.width` is where a
/// right-aligned label ends.
fn label(
  text: &str,
  rect: Rect,
  size: f32,
  color: Color,
  align: TextAlign,
) -> Widget {
  let mut widget = Widget::label(plain(text));
  if let Widget::Label {
    x,
    y,
    width,
    height,
    size: label_size,
    color: label_color,
    text_align,
    ..
  } = &mut widget
  {
    *x = rect.x;
    *y = rect.y + ((rect.height - size * 1.2) / 2.0).round();
    *width = rect.width;
    *height = rect.height;
    *label_size = size;
    *label_color = color;
    *text_align = align;
  }
  widget
}

fn text_input(
  rect: Rect,
  value: String,
  placeholder: String,
  theme: &Theme,
) -> Widget {
  let clear = BoxStyle::default();
  Widget::TextInput {
    value,
    placeholder,
    id: FIELD_ID.to_owned(),
    font_size: theme.font_size + 2.0,
    text_align: TextAlign::Left,
    bounds: WidgetBounds {
      x: rect.x,
      y: rect.y,
      width: rect.width,
      height: rect.height,
    },
    validation: None,
    style: TextInputStyle {
      idle: clear.clone(),
      focused: clear,
      placeholder_color: theme.dim_text,
      text_color: theme.text,
      cursor_color: theme.accent_text(),
      font: None,
    },
    width: rect.width,
    height: rect.height,
    flex: 0.0,
    grid_col: None,
    grid_row: None,
    col_span: 1,
    row_span: 1,
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::{
    history::History,
    index::{Entry, Index},
    layout::Layout,
    model::{Button, Key, Modifiers, Storage},
  };

  fn measure(text: &str, size: f32) -> f32 {
    text.chars().count() as f32 * size * 0.5
  }

  fn model() -> Model {
    let entries = (0..30)
      .map(|number| Entry {
        id: format!("app{number}.desktop"),
        kind: EntryKind::Application,
        name: format!("Application number {number:02}"),
        generic_name: String::new(),
        comment: "Does things".into(),
        keywords: Vec::new(),
        program: format!("app{number}"),
        command: format!("app{number}"),
        terminal: false,
        category: if number % 2 == 0 { "Games" } else { "Office" }.into(),
        icon: String::new(),
        actions: Vec::new(),
      })
      .collect();
    Model::new(
      Index {
        version: 1,
        entries,
      },
      Layout::default(),
      History::default(),
      Storage::default(),
      0,
    )
  }

  fn labels(widget: &Widget, found: &mut Vec<String>) {
    match widget {
      Widget::Label { text, .. } => found.push(text.clone()),
      Widget::TextInput { value, .. } => found.push(format!("[{value}]")),
      Widget::Container { children, .. } => {
        for child in children {
          labels(child, found);
        }
      }
      _ => {}
    }
  }

  #[test]
  fn the_panel_fits_and_centres_on_small_outputs() {
    let theme = Theme::default();
    assert_eq!(panel_size(&theme, (1920.0, 1080.0)), (760.0, 480.0));
    assert_eq!(panel_origin(&theme, (1920.0, 1080.0)), (580.0, 300.0));
    assert_eq!(panel_size(&theme, (640.0, 400.0)), (608.0, 368.0));
    assert_eq!(page_size(&theme, (760.0, 480.0)), 8);
  }

  #[test]
  fn rows_and_categories_are_targets_where_they_are_drawn() {
    let theme = Theme::default();
    let size = (760.0, 480.0);
    let mut model = model();
    model.page = page_size(&theme, size);
    let frame = build(&model, &theme, size, &measure, &|_| false);
    assert_eq!(frame.hit((-1.0, 10.0), size), Target::Backdrop);
    assert_eq!(frame.hit((5.0, 5.0), size), Target::Panel);
    assert_eq!(frame.hit((100.0, 30.0), size), Target::Field);
    // The category column starts with Favourites, then Recent.
    assert_eq!(frame.hit((30.0, 80.0), size), Target::Category(0));
    assert_eq!(frame.hit((30.0, 115.0), size), Target::Category(1));
    // The first visible row, then the eighth and last that fits.
    assert_eq!(frame.hit((400.0, 80.0), size), Target::Row(0));
    assert_eq!(frame.hit((400.0, 72.0 + 7.5 * 42.0), size), Target::Row(7));
    let mut text = Vec::new();
    labels(&frame.main, &mut text);
    assert!(text.contains(&"[]".to_owned()));
    assert!(text.contains(&"Application number 00".to_owned()));
    assert!(text.iter().any(|text| text.starts_with("Enter launch")));
  }

  #[test]
  fn long_names_are_cut_and_markup_is_defused() {
    let theme = Theme::default();
    let model = model();
    let builder = Builder {
      model: &model,
      theme: &theme,
      metrics: Metrics::new(&theme, 760.0),
      measure: &measure,
      loaded: &|_| false,
      main: Vec::new(),
      overlay: Vec::new(),
      icons: Vec::new(),
      regions: Vec::new(),
    };
    assert_eq!(builder.fit("abcdef", 30.0, 10.0), "abcdef");
    assert_eq!(builder.fit("abcdefgh", 30.0, 10.0), "abcde…");
    assert_eq!(plain("a <b> c"), "a ‹b› c");
    assert_eq!(plain("a < b"), "a < b");
  }

  #[test]
  fn the_menu_and_drag_go_to_the_overlay_on_top_of_rows() {
    let theme = Theme::default();
    let size = (760.0, 480.0);
    let mut model = model();
    model.pointer_button(Button::Right, true, (400.0, 90.0), Target::Row(0));
    let frame = build(&model, &theme, size, &measure, &|_| false);
    assert_eq!(frame.hit((410.0, 100.0), size), Target::MenuItem(0));
    let mut text = Vec::new();
    labels(&frame.overlay, &mut text);
    assert_eq!(text[0], "Add to Favourites");
    model.key(Key::Escape, Modifiers::NONE);
    model.pointer_button(Button::Left, true, (400.0, 90.0), Target::Row(0));
    model.pointer_motion((300.0, 200.0), Target::Row(2));
    let frame = build(&model, &theme, size, &measure, &|_| false);
    let mut text = Vec::new();
    labels(&frame.overlay, &mut text);
    assert_eq!(text, ["Application number 00"]);
  }

  #[test]
  fn a_prompt_shows_its_label_and_text() {
    let theme = Theme::default();
    let mut model = model();
    model.key(Key::F2, Modifiers::NONE);
    let frame = build(&model, &theme, (760.0, 480.0), &measure, &|_| false);
    let mut text = Vec::new();
    labels(&frame.main, &mut text);
    assert_eq!(text[0], "Rename to:");
    assert_eq!(text[1], "[Application number 00]");
  }

  #[test]
  fn loaded_icons_are_slots_and_missing_ones_letters() {
    let theme = Theme::default();
    let size = (760.0, 480.0);
    let mut index = model().index().clone();
    index.entries[0].icon = "loaded-icon".into();
    index.entries[2].icon = "pending-icon".into();
    let mut model = model();
    model.set_index(index);
    model.page = page_size(&theme, size);
    let frame = build(&model, &theme, size, &measure, &|icon| {
      icon == "loaded-icon"
    });
    assert_eq!(
      frame.icons,
      [IconSlot {
        rect: Rect::new(208.0 + ICON_LEFT, 78.0, 28.0, 28.0),
        icon: "loaded-icon".into(),
        row: 0,
      }]
    );
    // Every other row shows its name's first letter in a tile.
    let mut text = Vec::new();
    labels(&frame.main, &mut text);
    assert_eq!(text.iter().filter(|text| *text == "A").count(), 7);
  }

  #[test]
  fn a_menu_opened_by_key_goes_below_the_selected_row() {
    let theme = Theme::default();
    let size = (760.0, 480.0);
    let mut model = model();
    model.page = page_size(&theme, size);
    model.key(Key::Down, Modifiers::NONE);
    model.key(Key::Menu, Modifiers::NONE);
    let frame = build(&model, &theme, size, &measure, &|_| false);
    let row = frame
      .regions
      .iter()
      .find(|(_, target)| *target == Target::Row(1))
      .unwrap()
      .0;
    let item = frame
      .regions
      .iter()
      .find(|(_, target)| *target == Target::MenuItem(0))
      .unwrap()
      .0;
    assert_eq!(item.x, row.x + ICON_LEFT + 28.0 + 4.0);
    assert_eq!(item.y, row.y + row.height + 4.0);
  }
}
