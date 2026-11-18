//! Terminal rendering module.
//! Dauerbot modifications adapt Herdr App's Apache-2.0 retained row drawing
//! and cell rendering mechanisms; see DAUERBOT-PATCHES.md for attribution.
//!
//! This module provides [`TerminalRenderer`], which handles efficient rendering of
//! terminal content using GPUI's text and drawing systems.
//!
//! # Rendering Pipeline
//!
//! The renderer processes the terminal grid in several stages:
//!
//! ```text
//! Terminal Grid → Layout Phase → Paint Phase
//!                      │              │
//!                      ├─ Collect backgrounds
//!                      ├─ Batch text runs
//!                      │              │
//!                      │              ├─ Paint default background
//!                      │              ├─ Paint non-default backgrounds
//!                      │              ├─ Paint text characters
//!                      │              └─ Paint cursor
//! ```
//!
//! # Optimizations
//!
//! The renderer includes several optimizations to minimize draw calls:
//!
//! 1. **Background Merging**: Adjacent cells with the same background color are
//!    merged into single rectangles, reducing the number of quads to paint.
//!
//! 2. **Text Batching**: Adjacent cells with identical styling (color, bold, italic)
//!    are grouped into [`BatchedTextRun`]s for efficient text shaping.
//!
//! 3. **Default Background Skip**: Cells with the default background color don't
//!    generate separate background rectangles.
//!
//! 4. **Cell Measurement**: Font metrics are measured once using the 'M' character
//!    and cached for consistent cell dimensions.
//!
//! # Cell Dimensions
//!
//! Cell size is calculated from actual font metrics:
//!
//! - **Width**: Measured from shaped 'M' character (typically widest in monospace)
//! - **Height**: `(ascent + descent) × line_height_multiplier`
//!
//! The `line_height_multiplier` (default 1.2) adds extra vertical space to
//! accommodate tall glyphs from nerd fonts and other icon fonts.
//!
//! # Example
//!
//! ```ignore
//! use gpui::px;
//! use gpui_terminal::{ColorPalette, TerminalRenderer};
//!
//! let renderer = TerminalRenderer::new(
//!     "JetBrains Mono".to_string(),
//!     px(14.0),
//!     1.2,  // line height multiplier
//!     ColorPalette::default(),
//! );
//! ```

use crate::colors::ColorPalette;
use crate::event::GpuiEventProxy;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line, Point as AlacPoint};
use alacritty_terminal::term::Term;
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::term::color::Colors;
use alacritty_terminal::vte::ansi::{Color, CursorShape};
use gpui::{
    App, Bounds, Edges, Font, FontFeatures, FontStyle, FontWeight, Hsla, Pixels, Point, ShapedLine,
    Size, StrikethroughStyle, TextRun, UnderlineStyle, Window, px, quad, transparent_black,
};
use std::{cell::RefCell, rc::Rc};

/// A batched run of text with consistent styling.
///
/// This struct groups adjacent terminal cells with identical visual attributes
/// to reduce the number of text rendering calls.
#[derive(Debug, Clone)]
pub struct BatchedTextRun {
    /// The text content to render
    pub text: String,

    /// Starting column position
    pub start_col: usize,

    /// Row position
    pub row: usize,

    /// Foreground color
    pub fg_color: Hsla,

    /// Background color
    pub bg_color: Hsla,

    /// Bold flag
    pub bold: bool,

    /// Italic flag
    pub italic: bool,

    /// Underline flag
    pub underline: bool,
}

/// Background rectangle to paint.
///
/// Represents a rectangular region with a solid color background.
#[derive(Debug, Clone)]
pub struct BackgroundRect {
    /// Starting column position
    pub start_col: usize,

    /// Ending column position (exclusive)
    pub end_col: usize,

    /// Row position
    pub row: usize,

    /// Background color
    pub color: Hsla,
}

impl BackgroundRect {
    /// Check if this rectangle can be merged with another.
    ///
    /// Two rectangles can be merged if they:
    /// - Are on the same row
    /// - Have the same color
    /// - Are horizontally adjacent
    fn can_merge_with(&self, other: &Self) -> bool {
        self.row == other.row && self.color == other.color && self.end_col == other.start_col
    }
}

/// Terminal renderer with font settings and cell dimensions.
///
/// This struct manages the rendering of terminal content, including text,
/// backgrounds, and cursor. It maintains font metrics and provides the
/// [`paint`](Self::paint) method for drawing the terminal grid.
///
/// # Font Metrics
///
/// Cell dimensions are calculated from actual font measurements via
/// [`measure_cell`](Self::measure_cell). This ensures accurate character
/// positioning regardless of the font used.
///
/// # Usage
///
/// The renderer is typically used internally by [`TerminalView`](crate::TerminalView),
/// but can also be used directly for custom rendering:
///
/// ```ignore
/// // Measure cell dimensions (call once per font change)
/// renderer.measure_cell(window);
///
/// // Paint the terminal grid
/// renderer.paint(bounds, padding, &term, window, cx);
/// ```
///
/// # Performance
///
/// For optimal performance:
/// - Call `measure_cell` only when font settings change
/// - The `paint` method is designed to be called every frame
/// - Background and text batching minimize GPU draw calls
#[derive(Clone)]
pub struct TerminalRenderer {
    /// Font family name (e.g., "Fira Code", "Menlo")
    pub font_family: String,

    /// Font size in pixels
    pub font_size: Pixels,

    /// Width of a single character cell
    pub cell_width: Pixels,

    /// Height of a single character cell (line height)
    pub cell_height: Pixels,

    /// Multiplier for line height to accommodate tall glyphs
    pub line_height_multiplier: f32,

    /// Color palette for resolving terminal colors
    pub palette: ColorPalette,
    baseline: Pixels,
    cache: Rc<RefCell<RowCache>>,
}

#[derive(Debug, PartialEq)]
struct DrawingFacts {
    font: String,
    font_size: Pixels,
    cell: (Pixels, Pixels),
    baseline: Pixels,
    palette: ColorPalette,
    overrides: Vec<Option<alacritty_terminal::vte::ansi::Rgb>>,
}

#[derive(Debug, Default)]
struct RowCache {
    facts: Option<DrawingFacts>,
    rows: Vec<Option<CachedRow>>,
}

impl RowCache {
    fn prepare(&mut self, facts: DrawingFacts, count: usize) {
        if self.facts.as_ref() != Some(&facts) {
            self.rows.clear();
            self.facts = Some(facts);
        }
        self.rows.resize_with(count, || None);
    }
}

#[derive(Debug)]
struct CachedRow {
    cells: Vec<Cell>,
    backgrounds: Vec<BackgroundRect>,
    text: Vec<(usize, ShapedLine)>,
}

struct TextSegment {
    column: usize,
    text: String,
    style: Cell,
    ascii: bool,
}

/// GPUI 0.2 cannot place glyphs at explicit per-cell positions as gpui-pre
/// does. Batch fixed-width ASCII runs; keep wide/combining clusters separate.
fn text_segments(cells: &[Cell]) -> Vec<TextSegment> {
    let mut segments = Vec::<TextSegment>::new();
    let style_flags = Flags::BOLD
        | Flags::ITALIC
        | Flags::ALL_UNDERLINES
        | Flags::STRIKEOUT
        | Flags::INVERSE
        | Flags::DIM;
    for (column, cell) in cells.iter().enumerate() {
        let text = cell_text(cell);
        if text.is_empty() {
            continue;
        }
        let ascii = text.len() == 1 && text.is_ascii() && !cell.flags.contains(Flags::WIDE_CHAR);
        if let Some(previous) = segments.last_mut()
            && ascii
            && previous.ascii
            && previous.column + previous.text.len() == column
            && previous.style.fg == cell.fg
            && previous.style.bg == cell.bg
            && previous.style.flags & style_flags == cell.flags & style_flags
            && previous.style.underline_color() == cell.underline_color()
        {
            previous.text.push_str(&text);
        } else {
            segments.push(TextSegment {
                column,
                text,
                style: cell.clone(),
                ascii,
            });
        }
    }
    segments.retain(|s| {
        !s.text.trim().is_empty()
            || s.style
                .flags
                .intersects(Flags::ALL_UNDERLINES | Flags::STRIKEOUT)
    });
    segments
}

impl TerminalRenderer {
    /// Creates a new terminal renderer with the given font settings and color palette.
    ///
    /// # Arguments
    ///
    /// * `font_family` - The name of the font family to use
    /// * `font_size` - The font size in pixels
    /// * `line_height_multiplier` - Multiplier for line height (e.g., 1.2 for 20% extra)
    /// * `palette` - The color palette to use for terminal colors
    ///
    /// # Returns
    ///
    /// A new `TerminalRenderer` instance with default cell dimensions.
    ///
    /// # Examples
    ///
    /// ```
    /// use gpui::px;
    /// use gpui_terminal::render::TerminalRenderer;
    /// use gpui_terminal::ColorPalette;
    ///
    /// let renderer = TerminalRenderer::new("Fira Code".to_string(), px(14.0), 1.2, ColorPalette::default());
    /// ```
    pub fn new(
        font_family: String,
        font_size: Pixels,
        line_height_multiplier: f32,
        palette: ColorPalette,
    ) -> Self {
        // Default cell dimensions - will be measured on first paint
        // Using 0.6 as approximate em-width ratio for monospace fonts
        let cell_width = font_size * 0.6;
        let cell_height = font_size * 1.4; // Line height with some spacing

        Self {
            font_family,
            font_size,
            cell_width,
            cell_height,
            line_height_multiplier,
            palette,
            baseline: font_size,
            cache: Rc::new(RefCell::new(RowCache::default())),
        }
    }

    /// Measure cell dimensions based on actual font metrics.
    ///
    /// This method measures the actual width and height of characters
    /// using the GPUI text system.
    ///
    /// # Arguments
    ///
    /// * `window` - The GPUI window for text system access
    pub fn measure_cell(&mut self, window: &mut Window) {
        // Measure using a reference character (M is typically the widest)
        let font = self.cell_font(false, false);

        let text_run = TextRun {
            len: 1,
            font,
            color: gpui::black(),
            background_color: None,
            underline: None,
            strikethrough: None,
        };

        // Shape a single 'M' character to get its metrics
        let shaped = window
            .text_system()
            .shape_line("M".into(), self.font_size, &[text_run], None);

        // Get the width from the shaped line (accessed via Deref to LineLayout)
        if shaped.width > px(0.0) {
            self.cell_width =
                px((f32::from(shaped.width) * window.scale_factor()).round()
                    / window.scale_factor());
        }

        // Calculate height from ascent + descent with multiplier for tall glyphs (nerd fonts, etc.)
        let line_height = shaped.ascent + shaped.descent;
        if line_height > px(0.0) {
            self.cell_height = px((f32::from(line_height)
                * self.line_height_multiplier
                * window.scale_factor())
            .round()
                / window.scale_factor());
        }
        self.baseline = px(
            (f32::from(shaped.ascent + (self.cell_height - line_height) / 2.)
                * window.scale_factor())
            .round()
                / window.scale_factor(),
        );
    }

    /// Layout cells into batched text runs and background rects for a single row.
    ///
    /// This method processes a row of terminal cells and groups adjacent cells
    /// with identical styling into batched runs. It also collects background
    /// rectangles that need to be painted.
    ///
    /// # Arguments
    ///
    /// * `row` - The row number
    /// * `cells` - Iterator over (column, Cell) pairs
    /// * `colors` - Terminal color configuration
    ///
    /// # Returns
    ///
    /// A tuple of `(backgrounds, text_runs)` where:
    /// - `backgrounds` is a vector of merged background rectangles
    /// - `text_runs` is a vector of batched text runs
    pub fn layout_row(
        &self,
        row: usize,
        cells: impl Iterator<Item = (usize, Cell)>,
        colors: &Colors,
    ) -> (Vec<BackgroundRect>, Vec<BatchedTextRun>) {
        let mut backgrounds = Vec::new();
        let mut text_runs = Vec::new();

        let mut current_run: Option<BatchedTextRun> = None;
        let mut current_bg: Option<BackgroundRect> = None;

        for (col, cell) in cells {
            // Skip wide character spacers
            if cell
                .flags
                .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
            {
                continue;
            }

            // Extract cell styling
            let (fg_color, bg_color) = self.cell_colors(&cell, colors);
            let end_col = col
                + if cell.flags.contains(Flags::WIDE_CHAR) {
                    2
                } else {
                    1
                };
            let bold = cell.flags.contains(Flags::BOLD);
            let italic = cell.flags.contains(Flags::ITALIC);
            let underline = cell.flags.contains(Flags::UNDERLINE);

            // Get the character (or space if empty)
            let text = cell_text(&cell);

            // Handle background rectangles
            if let Some(ref mut bg_rect) = current_bg {
                if bg_rect.color == bg_color && bg_rect.end_col == col {
                    // Extend current background
                    bg_rect.end_col = end_col;
                } else {
                    // Save current background and start new one
                    backgrounds.push(bg_rect.clone());
                    current_bg = Some(BackgroundRect {
                        start_col: col,
                        end_col,
                        row,
                        color: bg_color,
                    });
                }
            } else {
                // Start new background
                current_bg = Some(BackgroundRect {
                    start_col: col,
                    end_col,
                    row,
                    color: bg_color,
                });
            }

            // Handle text runs
            if let Some(ref mut run) = current_run {
                if run.fg_color == fg_color
                    && run.bg_color == bg_color
                    && run.bold == bold
                    && run.italic == italic
                    && run.underline == underline
                {
                    // Extend current run
                    run.text.push_str(&text);
                } else {
                    // Save current run and start new one
                    text_runs.push(run.clone());
                    current_run = Some(BatchedTextRun {
                        text,
                        start_col: col,
                        row,
                        fg_color,
                        bg_color,
                        bold,
                        italic,
                        underline,
                    });
                }
            } else {
                // Start new run
                current_run = Some(BatchedTextRun {
                    text,
                    start_col: col,
                    row,
                    fg_color,
                    bg_color,
                    bold,
                    italic,
                    underline,
                });
            }
        }

        // Push final run and background
        if let Some(run) = current_run {
            text_runs.push(run);
        }
        if let Some(bg) = current_bg {
            backgrounds.push(bg);
        }

        // Merge adjacent backgrounds with same color
        let merged_backgrounds = self.merge_backgrounds(backgrounds);

        (merged_backgrounds, text_runs)
    }

    /// Resolve real VT cell attributes before shaping or merging backgrounds.
    /// This follows Herdr App's cell-based renderer rather than treating ANSI
    /// output as plain text (Apache-2.0, origin/master 321c9a84; adapted here).
    fn cell_colors(&self, cell: &Cell, colors: &Colors) -> (Hsla, Hsla) {
        let mut foreground = self.palette.resolve(cell.fg, colors);
        let mut background = self.palette.resolve(cell.bg, colors);
        if cell.flags.contains(Flags::INVERSE) {
            std::mem::swap(&mut foreground, &mut background);
        }
        if cell.flags.contains(Flags::DIM) {
            foreground.l *= 0.66;
        }
        if cell.flags.contains(Flags::HIDDEN) {
            foreground = background;
        }
        (foreground, background)
    }

    // Herdr App 321c9a84 terminal.rs cell_font/ligature_features disable all
    // three features before measuring or shaping fixed terminal columns.
    fn cell_font(&self, bold: bool, italic: bool) -> Font {
        static FEATURES: std::sync::LazyLock<FontFeatures> = std::sync::LazyLock::new(|| {
            FontFeatures(std::sync::Arc::new(vec![
                ("calt".into(), 0),
                ("liga".into(), 0),
                ("dlig".into(), 0),
            ]))
        });
        Font {
            family: self.font_family.clone().into(),
            features: FEATURES.clone(),
            fallbacks: None,
            weight: if bold {
                FontWeight::BOLD
            } else {
                FontWeight::NORMAL
            },
            style: if italic {
                FontStyle::Italic
            } else {
                FontStyle::Normal
            },
        }
    }

    fn drawing_facts(&self, colors: &Colors) -> DrawingFacts {
        DrawingFacts {
            font: self.font_family.clone(),
            font_size: self.font_size,
            cell: (self.cell_width, self.cell_height),
            baseline: self.baseline,
            palette: self.palette.clone(),
            overrides: (0..alacritty_terminal::term::color::COUNT)
                .map(|index| colors[index])
                .collect(),
        }
    }

    fn build_row(&self, cells: Vec<Cell>, colors: &Colors, window: &mut Window) -> CachedRow {
        let (backgrounds, _) = self.layout_row(0, cells.iter().cloned().enumerate(), colors);
        let text = text_segments(&cells)
            .into_iter()
            .map(|segment| {
                let flags = segment.style.flags;
                let (color, _) = self.cell_colors(&segment.style, colors);
                let underline_color = segment
                    .style
                    .underline_color()
                    .map(|color| self.palette.resolve(color, colors))
                    .unwrap_or(color);
                let run = TextRun {
                    len: segment.text.len(),
                    font: self
                        .cell_font(flags.contains(Flags::BOLD), flags.contains(Flags::ITALIC)),
                    color,
                    background_color: None,
                    underline: flags
                        .intersects(Flags::ALL_UNDERLINES)
                        .then(|| UnderlineStyle {
                            thickness: px(1.),
                            color: Some(underline_color),
                            wavy: flags.contains(Flags::UNDERCURL),
                        }),
                    strikethrough: flags
                        .contains(Flags::STRIKEOUT)
                        .then(|| StrikethroughStyle {
                            thickness: px(1.),
                            color: Some(color),
                        }),
                };
                let shaped = window.text_system().shape_line(
                    segment.text.into(),
                    self.font_size,
                    &[run],
                    segment.ascii.then_some(self.cell_width),
                );
                (segment.column, shaped)
            })
            .collect();
        CachedRow {
            cells,
            backgrounds,
            text,
        }
    }

    /// Merge adjacent background rects with same color.
    ///
    /// This optimization reduces the number of rectangles to paint by
    /// combining horizontally adjacent rectangles that share the same color.
    ///
    /// # Arguments
    ///
    /// * `rects` - Vector of background rectangles to merge
    ///
    /// # Returns
    ///
    /// A new vector with merged rectangles
    fn merge_backgrounds(&self, mut rects: Vec<BackgroundRect>) -> Vec<BackgroundRect> {
        if rects.is_empty() {
            return rects;
        }

        let mut merged = Vec::new();
        let mut current = rects.remove(0);

        for rect in rects {
            if current.can_merge_with(&rect) {
                current.end_col = rect.end_col;
            } else {
                merged.push(current);
                current = rect;
            }
        }

        merged.push(current);
        merged
    }

    /// Paint terminal content to the window.
    ///
    /// This is the main rendering method that draws the terminal grid,
    /// including backgrounds, text, and cursor.
    ///
    /// # Arguments
    ///
    /// * `bounds` - The bounding box to render within
    /// * `padding` - Padding around the terminal content
    /// * `term` - The terminal state
    /// * `window` - The GPUI window
    /// * `cx` - The application context
    pub fn paint(
        &self,
        bounds: Bounds<Pixels>,
        padding: Edges<Pixels>,
        term: &Term<GpuiEventProxy>,
        selection: Option<((i32, usize), (i32, usize))>,
        window: &mut Window,
        _cx: &mut App,
    ) {
        // Get terminal dimensions
        let grid = term.grid();
        let num_lines = grid.screen_lines();
        let num_cols = grid.columns();
        let colors = term.colors();

        // Calculate default background color
        let default_bg = self.palette.resolve(
            Color::Named(alacritty_terminal::vte::ansi::NamedColor::Background),
            colors,
        );

        // Paint default background (covers full bounds including padding)
        window.paint_quad(quad(
            bounds,
            px(0.0),
            default_bg,
            Edges::<Pixels>::default(),
            transparent_black(),
            Default::default(),
        ));

        // Calculate origin offset (content starts after padding)
        let origin = Point {
            x: bounds.origin.x + padding.left,
            y: bounds.origin.y + padding.top,
        };

        // Keep GPUI shaping alive across window frames. A spinner changes one
        // row; selection, palette, font, source size and viewport changes are
        // part of the row/cache facts, so retained drawing cannot become stale.
        let mut cache = self.cache.borrow_mut();
        cache.prepare(self.drawing_facts(colors), num_lines);
        for row in 0..num_lines {
            let line = Line(row as i32 - grid.display_offset() as i32);
            let cells: Vec<Cell> = (0..num_cols)
                .map(|column| grid[AlacPoint::new(line, Column(column))].clone())
                .collect();
            if cache.rows[row]
                .as_ref()
                .is_none_or(|old| old.cells != cells)
            {
                cache.rows[row] = Some(self.build_row(cells, colors, window));
            }
        }
        // Draw backgrounds first, selection as an overlay, then shaped text.
        // Selection never mutates program cells or invalidates shaped rows.
        for (row, drawing) in cache.rows.iter().enumerate() {
            let Some(drawing) = drawing else { continue };
            for background in &drawing.backgrounds {
                if background.color == default_bg {
                    continue;
                }
                window.paint_quad(quad(
                    Bounds::new(
                        Point {
                            x: origin.x + self.cell_width * background.start_col as f32,
                            y: origin.y + self.cell_height * row as f32,
                        },
                        Size {
                            width: self.cell_width
                                * (background.end_col - background.start_col) as f32,
                            height: self.cell_height,
                        },
                    ),
                    px(0.),
                    background.color,
                    Edges::default(),
                    transparent_black(),
                    Default::default(),
                ));
            }
        }
        if let Some((a, b)) = selection {
            let (start, end) = if a <= b { (a, b) } else { (b, a) };
            for row in 0..num_lines {
                let line = row as i32 - grid.display_offset() as i32;
                if line < start.0 || line > end.0 {
                    continue;
                }
                let from = if line == start.0 { start.1 } else { 0 }.min(num_cols);
                let to = if line == end.0 {
                    end.1.saturating_add(1)
                } else {
                    num_cols
                }
                .min(num_cols);
                if from >= to {
                    continue;
                }
                let mut color: Hsla = gpui::rgb(0x38536f).into();
                color.a = 0.65;
                window.paint_quad(quad(
                    Bounds::new(
                        Point {
                            x: origin.x + self.cell_width * from as f32,
                            y: origin.y + self.cell_height * row as f32,
                        },
                        Size {
                            width: self.cell_width * (to - from) as f32,
                            height: self.cell_height,
                        },
                    ),
                    px(0.),
                    color,
                    Edges::default(),
                    transparent_black(),
                    Default::default(),
                ));
            }
        }
        for (row, drawing) in cache.rows.iter().enumerate() {
            let Some(drawing) = drawing else { continue };
            for (column, line) in &drawing.text {
                // All fallback glyphs share the primary font's snapped baseline.
                let native_baseline =
                    (self.cell_height - line.ascent - line.descent) / 2. + line.ascent;
                let position = Point {
                    x: origin.x + self.cell_width * *column as f32,
                    y: origin.y + self.cell_height * row as f32 + self.baseline - native_baseline,
                };
                let _ = line.paint(position, self.cell_height, window, _cx);
            }
        }
        drop(cache);

        if grid.display_offset() != 0
            || !term
                .mode()
                .contains(alacritty_terminal::term::TermMode::SHOW_CURSOR)
        {
            return;
        }
        // Paint cursor
        let cursor_point = grid.cursor.point;
        let cursor_x = origin.x + self.cell_width * (cursor_point.column.0 as f32);
        let cursor_y = origin.y + self.cell_height * (cursor_point.line.0 as f32);

        let mut cursor_color = self.palette.resolve(
            Color::Named(alacritty_terminal::vte::ansi::NamedColor::Cursor),
            colors,
        );
        // Keep the glyph under a block cursor readable instead of painting an
        // opaque rectangle over the already-shaped text.
        cursor_color.a = 0.45;

        let mut cursor_bounds = Bounds {
            origin: Point {
                x: cursor_x,
                y: cursor_y,
            },
            size: Size {
                width: self.cell_width,
                height: self.cell_height,
            },
        };
        match term.cursor_style().shape {
            CursorShape::Hidden => return,
            CursorShape::Beam => cursor_bounds.size.width = px(2.0),
            CursorShape::Underline => {
                cursor_bounds.origin.y += cursor_bounds.size.height - px(2.0);
                cursor_bounds.size.height = px(2.0);
            }
            CursorShape::Block | CursorShape::HollowBlock => {}
        }
        let hollow = term.cursor_style().shape == CursorShape::HollowBlock;

        window.paint_quad(quad(
            cursor_bounds,
            px(0.0),
            if hollow {
                transparent_black()
            } else {
                cursor_color
            },
            if hollow {
                Edges::all(px(1.0))
            } else {
                Edges::default()
            },
            cursor_color,
            Default::default(),
        ));
    }
}

fn cell_text(cell: &Cell) -> String {
    if cell
        .flags
        .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER | Flags::HIDDEN)
    {
        return String::new();
    }
    let mut text = if cell.c == '\0' {
        " ".into()
    } else {
        cell.c.to_string()
    };
    if let Some(combining) = cell.zerowidth() {
        text.extend(combining.iter());
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ligature_prone_ascii_runs_keep_terminal_columns_across_font_styles() {
        let renderer =
            TerminalRenderer::new("Fira Code".into(), px(13.), 1.2, ColorPalette::default());
        for (bold, italic) in [(false, false), (true, false), (false, true), (true, true)] {
            let font = renderer.cell_font(bold, italic);
            for feature in ["calt", "liga", "dlig"] {
                assert!(
                    font.features
                        .tag_value_list()
                        .iter()
                        .any(|(tag, value)| tag == feature && *value == 0),
                    "{feature} was lost when selecting a styled terminal face"
                );
            }
            let flags = if bold { Flags::BOLD } else { Flags::empty() }
                | if italic {
                    Flags::ITALIC
                } else {
                    Flags::empty()
                };
            let mut cells: Vec<_> = "fi->ffi"
                .chars()
                .map(|c| Cell {
                    c,
                    flags,
                    ..Cell::default()
                })
                .collect();
            cells.extend([
                Cell {
                    c: '界',
                    flags: flags | Flags::WIDE_CHAR,
                    ..Cell::default()
                },
                Cell {
                    flags: Flags::WIDE_CHAR_SPACER,
                    ..Cell::default()
                },
                Cell {
                    c: 'Z',
                    flags,
                    ..Cell::default()
                },
            ]);
            let segments = text_segments(&cells);
            assert_eq!(
                segments
                    .iter()
                    .map(|segment| (segment.column, segment.text.as_str(), segment.ascii))
                    .collect::<Vec<_>>(),
                vec![(0, "fi->ffi", true), (7, "界", false), (9, "Z", true)]
            );
        }
    }

    #[test]
    fn retained_rows_invalidate_on_palette_metrics_and_style_changes_but_not_selection() {
        let mut renderer =
            TerminalRenderer::new("Menlo".into(), px(13.), 1.2, ColorPalette::default());
        let mut colors = Colors::default();
        let mut cache = RowCache::default();
        cache.prepare(renderer.drawing_facts(&colors), 2);
        cache.rows[0] = Some(CachedRow {
            cells: vec![Cell {
                c: 'A',
                ..Cell::default()
            }],
            backgrounds: Vec::new(),
            text: Vec::new(),
        });
        cache.prepare(renderer.drawing_facts(&colors), 2);
        assert!(
            cache.rows[0].is_some(),
            "unchanged rows reuse their shaped drawing across selection and cursor paints"
        );
        colors[1] = Some(alacritty_terminal::vte::ansi::Rgb {
            r: 12,
            g: 34,
            b: 56,
        });
        cache.prepare(renderer.drawing_facts(&colors), 2);
        assert!(
            cache.rows[0].is_none(),
            "OSC palette updates invalidate text with unchanged raw cells"
        );
        cache.rows[0] = Some(CachedRow {
            cells: vec![Cell::default()],
            backgrounds: Vec::new(),
            text: Vec::new(),
        });
        renderer.cell_width += px(1.);
        cache.prepare(renderer.drawing_facts(&colors), 2);
        assert!(
            cache.rows[0].is_none(),
            "scale/font metric changes must invalidate retained drawing"
        );

        let cells = [
            Cell {
                c: 'A',
                ..Cell::default()
            },
            Cell {
                c: 'B',
                ..Cell::default()
            },
            Cell {
                c: '界',
                flags: Flags::WIDE_CHAR,
                ..Cell::default()
            },
            Cell {
                flags: Flags::WIDE_CHAR_SPACER,
                ..Cell::default()
            },
            Cell {
                c: 'C',
                flags: Flags::BOLD,
                ..Cell::default()
            },
        ];
        let segments = text_segments(&cells);
        assert_eq!(
            segments
                .iter()
                .map(|s| (s.column, s.text.as_str(), s.ascii))
                .collect::<Vec<_>>(),
            vec![(0, "AB", true), (2, "界", false), (4, "C", true)]
        );
    }

    #[test]
    fn ansi_cells_keep_rgb_reverse_wide_combining_and_hidden_attributes() {
        let (events, _) = std::sync::mpsc::channel();
        let mut terminal = crate::TerminalState::new(16, 2, GpuiEventProxy::new(events));
        terminal.process_frame(
            "\x1b[38;2;18;52;86;48;2;101;67;33mA\x1b[7mB\x1b[0m界e\u{301} \x1b[8mH".as_bytes(),
            Some((16, 2)),
        );
        let renderer = TerminalRenderer::new("Menlo".into(), px(13.), 1.2, ColorPalette::default());
        terminal.with_term(|term| {
            let row = &term.grid()[Line(0)];
            let foreground: Hsla = gpui::rgb(0x123456).into();
            let background: Hsla = gpui::rgb(0x654321).into();
            assert_eq!(
                renderer.cell_colors(&row[Column(0)], term.colors()),
                (foreground, background)
            );
            assert_eq!(
                renderer.cell_colors(&row[Column(1)], term.colors()),
                (background, foreground)
            );
            assert_eq!(cell_text(&row[Column(2)]), "界");
            assert!(cell_text(&row[Column(3)]).is_empty());
            assert_eq!(cell_text(&row[Column(4)]), "e\u{301}");
            assert!(cell_text(&row[Column(6)]).is_empty());
            let (backgrounds, _) = renderer.layout_row(
                0,
                (2..4).map(|col| (col, row[Column(col)].clone())),
                term.colors(),
            );
            assert_eq!(backgrounds[0].start_col, 2);
            assert_eq!(
                backgrounds[0].end_col, 4,
                "wide-cell background spans both columns"
            );
        });
    }

    #[test]
    fn test_renderer_creation() {
        let renderer = TerminalRenderer::new(
            "Fira Code".to_string(),
            px(14.0),
            1.2,
            ColorPalette::default(),
        );
        assert_eq!(renderer.font_family, "Fira Code");
        assert_eq!(renderer.font_size, px(14.0));
        assert_eq!(renderer.line_height_multiplier, 1.2);
    }

    #[test]
    fn test_background_rect_merge() {
        let black = Hsla::black();

        let rect1 = BackgroundRect {
            start_col: 0,
            end_col: 5,
            row: 0,
            color: black,
        };

        let rect2 = BackgroundRect {
            start_col: 5,
            end_col: 10,
            row: 0,
            color: black,
        };

        assert!(rect1.can_merge_with(&rect2));

        let rect3 = BackgroundRect {
            start_col: 5,
            end_col: 10,
            row: 1,
            color: black,
        };

        assert!(!rect1.can_merge_with(&rect3));
    }

    #[test]
    fn test_merge_backgrounds() {
        let renderer = TerminalRenderer::new(
            "monospace".to_string(),
            px(14.0),
            1.2,
            ColorPalette::default(),
        );
        let black = Hsla::black();

        let rects = vec![
            BackgroundRect {
                start_col: 0,
                end_col: 5,
                row: 0,
                color: black,
            },
            BackgroundRect {
                start_col: 5,
                end_col: 10,
                row: 0,
                color: black,
            },
        ];

        let merged = renderer.merge_backgrounds(rects);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].start_col, 0);
        assert_eq!(merged[0].end_col, 10);
    }
}
