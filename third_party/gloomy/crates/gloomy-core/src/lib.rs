//! Gloomy Core - GPU rendering primitives for the gloomy UI library.
//!
//! This crate provides the foundational rendering components:
//! - SDF-based primitive rendering (rectangles, circles, lines)
//! - TTF text rendering
//! - Container-based layout system
//! - RON-based declarative UI definitions
//! - Flexbox-style layout engine
//! - Interactive UI elements (buttons)
//! - GPU context management

pub mod container;
pub mod data_source;
pub mod datagrid;
pub mod headless;
pub mod image_renderer;
pub mod interaction;
pub mod key_chord;
pub mod kpi;
pub mod layout;
pub mod layout_engine;
pub mod primitives;
pub mod rect;
pub mod renderer;
pub mod rich_text;
pub mod style;
pub mod style_context;
pub mod svg_loader;
pub mod text;
pub mod texture;
pub mod theme;
pub mod tree;
pub mod ui;
pub mod validation;
pub mod widget;
pub mod widget_state;

#[cfg(test)]
mod tests;

pub use container::Container;
pub use data_source::{CellValue, DataSource, VecDataSource};
pub use datagrid::{
    ColumnDef, ColumnWidth, DataGrid, DataGridStyle, SelectionMode,
    SortDirection,
};
pub use glam::{Vec2, Vec4};
pub use headless::{HeadlessConfig, HeadlessRenderer};
pub use interaction::InteractionState;
pub use kpi::{KpiCard, KpiCardStyle, KpiTrend, TrendDirection};
pub use layout::{Align, Direction, Justify, Layout};
pub use layout_engine::compute_layout;
pub use primitives::{Instance, PrimitiveRenderer};
pub use rect::Rect;
pub use renderer::GloomyRenderer;
pub use style::{
    Border, BorderStyle, BoxStyle, ButtonStyle, GlobalStyle, Gradient, Shadow,
    TextInputStyle,
};
pub use style_context::StyleContext;
pub use text::TextRenderer;
pub use theme::{ColorPalette, Theme};
pub use ui::{hit_test, load_ui, parse_ui, render_ui, RenderContext};
pub use widget::{Widget, WidgetBounds};
