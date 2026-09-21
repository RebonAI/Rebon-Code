//! UI-independent offline math rendering.
//!
//! Formula source is parsed and laid out by RaTeX, serialized to SVG, and
//! rasterized through `resvg` with the bundled KaTeX fonts. The public result
//! carries only portable data and terminal-cell measurements: UI-specific
//! fallback glyphs and widget metadata are the responsibility of consumers.

#![deny(missing_docs)]

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex, OnceLock},
};

use ratex_layout::{layout as layout_formula, to_display_list, LayoutOptions};
use ratex_svg::{render_to_svg, SvgOptions};
use ratex_types::{color::Color as RatexColor, MathStyle};

/// Maximum accepted formula source length, including delimiters.
pub const MAX_FORMULA_SOURCE_BYTES: usize = 8 * 1024;
/// Maximum number of pixels allocated for one rasterized formula.
pub const MAX_FORMULA_BITMAP_PIXELS: u64 = 4_000_000;
/// Maximum terminal rows reserved for one displayed formula.
pub const MAX_FORMULA_TERMINAL_ROWS: u16 = 16;
/// Pixel width represented by one terminal fallback cell.
pub const FORMULA_CELL_WIDTH_PX: u32 = 8;
/// Pixel height represented by one terminal fallback row.
pub const FORMULA_CELL_HEIGHT_PX: u32 = 16;

const FORMULA_FONT_SIZE: f64 = 32.0;
const FORMULA_SVG_PADDING: f64 = 2.0;
const FORMULA_CACHE_MAX_ENTRIES: usize = 128;
const FORMULA_CACHE_MAX_PIXELS: u64 = 16_000_000;

/// Whether a formula should use inline or display layout rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FormulaDisplayMode {
    /// Text-style math, scaled to a single terminal row.
    Inline,
    /// Display-style math, allowed to occupy multiple terminal rows.
    Display,
}

/// An RGB foreground color independent of any UI toolkit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FormulaColor {
    /// Red channel.
    pub red: u8,
    /// Green channel.
    pub green: u8,
    /// Blue channel.
    pub blue: u8,
}

impl FormulaColor {
    /// White, the foreground assumed when a renderer supplies no color.
    pub const WHITE: Self = Self::new(255, 255, 255);

    /// Construct an RGB color.
    pub const fn new(red: u8, green: u8, blue: u8) -> Self {
        Self { red, green, blue }
    }
}

impl Default for FormulaColor {
    fn default() -> Self {
        Self::WHITE
    }
}

/// Options controlling formula layout and raster dimensions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FormulaRenderOptions {
    /// Inline versus display layout.
    pub display: FormulaDisplayMode,
    /// Maximum width in terminal cells. Zero is treated as one cell.
    pub max_width_cells: usize,
    /// Maximum height in terminal rows before the raster is scaled down.
    ///
    /// `None` selects the per-display-mode default: inline math occupies
    /// exactly one terminal row, while display math is allowed
    /// [`MAX_FORMULA_TERMINAL_ROWS`]. Pixel consumers pass an explicit taller
    /// budget so text-style layout is not crushed down to cell height; a
    /// two-story `\frac{a}{b}` then keeps readable glyphs and the consumer
    /// scales the bitmap to its own typography instead.
    pub max_height_cells: Option<usize>,
    /// Formula foreground color.
    pub foreground: FormulaColor,
}

impl Default for FormulaRenderOptions {
    fn default() -> Self {
        Self {
            display: FormulaDisplayMode::Inline,
            max_width_cells: 80,
            max_height_cells: None,
            foreground: FormulaColor::WHITE,
        }
    }
}

/// Raw straight-alpha RGBA8 pixels in row-major order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FormulaBitmap {
    /// Bitmap width in pixels.
    pub width: u32,
    /// Bitmap height in pixels.
    pub height: u32,
    /// Straight-alpha RGBA8 pixels.
    pub rgba: Vec<u8>,
}

/// Vector and raster representations of one formula.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FormulaAsset {
    /// SVG emitted by the RaTeX layout pipeline.
    pub svg: String,
    /// Rasterized formula image.
    pub bitmap: FormulaBitmap,
}

/// Portable output of a successful formula render.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormulaRender {
    /// Number of terminal columns required by the bitmap.
    pub terminal_columns: u16,
    /// Number of terminal rows required by the bitmap.
    pub terminal_rows: u16,
    /// Baseline position in bitmap pixels measured from the top edge.
    ///
    /// Consumers that scale the bitmap can scale this value by the same factor
    /// to align inline formulas with surrounding text.
    pub baseline_px: u32,
    /// SVG and RGBA formula assets.
    pub asset: FormulaAsset,
}

/// Failure produced while parsing, laying out, or rasterizing a formula.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormulaRenderError {
    /// Delimited source exceeded [`MAX_FORMULA_SOURCE_BYTES`].
    SourceTooLarge,
    /// RaTeX rejected the expression.
    Parse,
    /// Layout or SVG dimensions were non-finite, empty, or unscalable.
    InvalidDimensions,
    /// Raster dimensions exceeded [`MAX_FORMULA_BITMAP_PIXELS`].
    BitmapTooLarge,
    /// A display formula exceeded [`MAX_FORMULA_TERMINAL_ROWS`].
    TooManyRows,
    /// The generated SVG could not be parsed.
    Svg,
    /// The RGBA raster could not be allocated.
    Raster,
}

impl std::fmt::Display for FormulaRenderError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            Self::SourceTooLarge => "formula source is too large",
            Self::Parse => "formula could not be parsed",
            Self::InvalidDimensions => "formula has invalid dimensions",
            Self::BitmapTooLarge => "formula bitmap is too large",
            Self::TooManyRows => "formula occupies too many terminal rows",
            Self::Svg => "formula SVG could not be parsed",
            Self::Raster => "formula raster could not be allocated",
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for FormulaRenderError {}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct FormulaCacheKey {
    expression: String,
    options: FormulaRenderOptions,
}

#[derive(Debug, Clone)]
struct FormulaCacheEntry {
    key: FormulaCacheKey,
    rendered: FormulaRender,
    pixels: u64,
}

#[derive(Debug, Default)]
struct FormulaRenderCache {
    entries: VecDeque<FormulaCacheEntry>,
    pixels: u64,
}

impl FormulaRenderCache {
    fn get(&mut self, key: &FormulaCacheKey) -> Option<FormulaRender> {
        let index = self.entries.iter().position(|entry| &entry.key == key)?;
        let entry = self.entries.remove(index)?;
        let rendered = entry.rendered.clone();
        self.entries.push_back(entry);
        Some(rendered)
    }

    fn insert(&mut self, key: FormulaCacheKey, rendered: FormulaRender) {
        let pixels = bitmap_pixels(&rendered.asset.bitmap);
        if pixels > FORMULA_CACHE_MAX_PIXELS {
            return;
        }
        self.pixels = self.pixels.saturating_add(pixels);
        self.entries.push_back(FormulaCacheEntry {
            key,
            rendered,
            pixels,
        });
        while self.entries.len() > FORMULA_CACHE_MAX_ENTRIES
            || self.pixels > FORMULA_CACHE_MAX_PIXELS
        {
            let Some(entry) = self.entries.pop_front() else {
                break;
            };
            self.pixels = self.pixels.saturating_sub(entry.pixels);
        }
    }
}

/// Render one delimited expression into portable SVG and RGBA assets.
///
/// `expression` excludes delimiters while `source` includes them. Keeping both
/// inputs lets the renderer enforce a source-size limit without imposing one
/// delimiter syntax on callers.
pub fn render_formula(
    expression: &str,
    source: &str,
    options: FormulaRenderOptions,
) -> Result<FormulaRender, FormulaRenderError> {
    if source.len() > MAX_FORMULA_SOURCE_BYTES {
        return Err(FormulaRenderError::SourceTooLarge);
    }
    let key = FormulaCacheKey {
        expression: expression.to_string(),
        options,
    };
    {
        let mut cache = formula_render_cache()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(rendered) = cache.get(&key) {
            return Ok(rendered);
        }
    }

    let rendered = render_formula_uncached(expression, source, options)?;
    {
        let mut cache = formula_render_cache()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        cache.insert(key, rendered.clone());
    }
    Ok(rendered)
}

fn formula_render_cache() -> &'static Mutex<FormulaRenderCache> {
    static CACHE: OnceLock<Mutex<FormulaRenderCache>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(FormulaRenderCache::default()))
}

fn render_formula_uncached(
    expression: &str,
    source: &str,
    options: FormulaRenderOptions,
) -> Result<FormulaRender, FormulaRenderError> {
    if source.len() > MAX_FORMULA_SOURCE_BYTES {
        return Err(FormulaRenderError::SourceTooLarge);
    }
    let ast = ratex_parser::parse(expression).map_err(|_| FormulaRenderError::Parse)?;
    let layout_options = LayoutOptions::default()
        .with_style(match options.display {
            FormulaDisplayMode::Inline => MathStyle::Text,
            FormulaDisplayMode::Display => MathStyle::Display,
        })
        .with_color(ratex_foreground(options.foreground));
    let layout = layout_formula(&ast, &layout_options);
    let display_list = to_display_list(&layout);
    if !display_list.width.is_finite()
        || !display_list.height.is_finite()
        || !display_list.depth.is_finite()
        || display_list.width <= 0.0
        || display_list.height + display_list.depth <= 0.0
    {
        return Err(FormulaRenderError::InvalidDimensions);
    }

    let svg = render_to_svg(
        &display_list,
        &SvgOptions {
            font_size: FORMULA_FONT_SIZE,
            padding: FORMULA_SVG_PADDING,
            ..SvgOptions::default()
        },
    );
    let usvg_options = resvg::usvg::Options {
        fontdb: Arc::clone(embedded_katex_fontdb()),
        ..resvg::usvg::Options::default()
    };
    let tree =
        resvg::usvg::Tree::from_str(&svg, &usvg_options).map_err(|_| FormulaRenderError::Svg)?;
    let natural = tree.size();
    let natural_width = natural.width();
    let natural_height = natural.height();
    if !natural_width.is_finite()
        || !natural_height.is_finite()
        || natural_width <= 0.0
        || natural_height <= 0.0
    {
        return Err(FormulaRenderError::InvalidDimensions);
    }

    let max_columns = options.max_width_cells.max(1).min(u16::MAX as usize) as u32;
    let max_width_px = max_columns.saturating_mul(FORMULA_CELL_WIDTH_PX).max(1);
    let max_height_px = match options.max_height_cells {
        Some(cells) => (cells.clamp(1, usize::from(MAX_FORMULA_TERMINAL_ROWS)) as u32)
            .saturating_mul(FORMULA_CELL_HEIGHT_PX),
        None => match options.display {
            FormulaDisplayMode::Inline => FORMULA_CELL_HEIGHT_PX,
            FormulaDisplayMode::Display => {
                u32::from(MAX_FORMULA_TERMINAL_ROWS).saturating_mul(FORMULA_CELL_HEIGHT_PX)
            }
        },
    };
    let width_scale = max_width_px as f32 / natural_width;
    let height_scale = max_height_px as f32 / natural_height;
    // An explicit height budget scales display math down to fit. Without one,
    // display math keeps its natural size and row validation rejects formulas
    // that overflow the terminal budget; inline math always clamps so it fits
    // in its single reserved row.
    let clamp_height =
        options.max_height_cells.is_some() || options.display == FormulaDisplayMode::Inline;
    let scale = 1.0_f32
        .min(width_scale)
        .min(if clamp_height { height_scale } else { 1.0 });
    if !scale.is_finite() || scale <= 0.0 {
        return Err(FormulaRenderError::InvalidDimensions);
    }
    let width = (natural_width * scale).ceil().max(1.0) as u32;
    let height = (natural_height * scale).ceil().max(1.0) as u32;
    let terminal_rows = validate_formula_bitmap_dimensions(width, height)?;

    let mut pixmap =
        resvg::tiny_skia::Pixmap::new(width, height).ok_or(FormulaRenderError::Raster)?;
    let transform = resvg::tiny_skia::Transform::from_scale(scale, scale);
    resvg::render(&tree, transform, &mut pixmap.as_mut());
    let bitmap = FormulaBitmap {
        width,
        height,
        rgba: straight_rgba(pixmap.data()),
    };
    let terminal_columns = width
        .div_ceil(FORMULA_CELL_WIDTH_PX)
        .min(u32::from(u16::MAX)) as u16;
    let baseline_px = ((FORMULA_SVG_PADDING + display_list.height * FORMULA_FONT_SIZE) as f32
        * scale)
        .round()
        .clamp(0.0, height as f32) as u32;
    Ok(FormulaRender {
        terminal_columns,
        terminal_rows: terminal_rows.min(u32::from(u16::MAX)) as u16,
        baseline_px,
        asset: FormulaAsset { svg, bitmap },
    })
}

fn validate_formula_bitmap_dimensions(width: u32, height: u32) -> Result<u32, FormulaRenderError> {
    let pixels = u64::from(width)
        .checked_mul(u64::from(height))
        .ok_or(FormulaRenderError::BitmapTooLarge)?;
    if pixels > MAX_FORMULA_BITMAP_PIXELS {
        return Err(FormulaRenderError::BitmapTooLarge);
    }
    let terminal_rows = height.div_ceil(FORMULA_CELL_HEIGHT_PX);
    if terminal_rows > u32::from(MAX_FORMULA_TERMINAL_ROWS) {
        return Err(FormulaRenderError::TooManyRows);
    }
    Ok(terminal_rows)
}

fn bitmap_pixels(bitmap: &FormulaBitmap) -> u64 {
    u64::from(bitmap.width).saturating_mul(u64::from(bitmap.height))
}

fn straight_rgba(premultiplied: &[u8]) -> Vec<u8> {
    let mut rgba = premultiplied.to_vec();
    for pixel in rgba.chunks_exact_mut(4) {
        let alpha = u32::from(pixel[3]);
        if alpha == 0 {
            pixel[0] = 0;
            pixel[1] = 0;
            pixel[2] = 0;
            continue;
        }
        for channel in &mut pixel[..3] {
            *channel = ((u32::from(*channel) * 255 + alpha / 2) / alpha).min(255) as u8;
        }
    }
    rgba
}

fn embedded_katex_fontdb() -> &'static Arc<resvg::usvg::fontdb::Database> {
    static FONTDB: OnceLock<Arc<resvg::usvg::fontdb::Database>> = OnceLock::new();
    FONTDB.get_or_init(|| {
        const FONT_FILES: &[&str] = &[
            "KaTeX_AMS-Regular.ttf",
            "KaTeX_Caligraphic-Bold.ttf",
            "KaTeX_Caligraphic-Regular.ttf",
            "KaTeX_Fraktur-Bold.ttf",
            "KaTeX_Fraktur-Regular.ttf",
            "KaTeX_Main-Bold.ttf",
            "KaTeX_Main-BoldItalic.ttf",
            "KaTeX_Main-Italic.ttf",
            "KaTeX_Main-Regular.ttf",
            "KaTeX_Math-BoldItalic.ttf",
            "KaTeX_Math-Italic.ttf",
            "KaTeX_SansSerif-Bold.ttf",
            "KaTeX_SansSerif-Italic.ttf",
            "KaTeX_SansSerif-Regular.ttf",
            "KaTeX_Script-Regular.ttf",
            "KaTeX_Size1-Regular.ttf",
            "KaTeX_Size2-Regular.ttf",
            "KaTeX_Size3-Regular.ttf",
            "KaTeX_Size4-Regular.ttf",
            "KaTeX_Typewriter-Regular.ttf",
        ];
        let mut database = resvg::usvg::fontdb::Database::new();
        for filename in FONT_FILES {
            if let Some(bytes) = ratex_katex_fonts::ttf_bytes(filename) {
                database.load_font_data(bytes.into_owned());
            }
        }
        Arc::new(database)
    })
}

fn ratex_foreground(color: FormulaColor) -> RatexColor {
    RatexColor::rgb(
        color.red as f32 / 255.0,
        color.green as f32 / 255.0,
        color.blue as f32 / 255.0,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(display: FormulaDisplayMode, max_width_cells: usize) -> FormulaRenderOptions {
        FormulaRenderOptions {
            display,
            max_width_cells,
            max_height_cells: None,
            foreground: FormulaColor::new(35, 209, 139),
        }
    }

    // Core rendering coverage matrix:
    // valid inline/display -> SVG + bounded RGBA; invalid -> Parse;
    // source/pixel/row limits -> typed rejection; repeated render -> identical.

    #[test]
    fn common_formula_families_render_to_portable_assets() {
        for (expression, display) in [
            (r"x^2", FormulaDisplayMode::Inline),
            (r"\frac{1}{2}", FormulaDisplayMode::Display),
            (r"\sqrt{x_1 + y^2}", FormulaDisplayMode::Display),
            (
                r"\begin{matrix}a & b \\ c & d\end{matrix}",
                FormulaDisplayMode::Display,
            ),
        ] {
            let rendered = render_formula(expression, expression, options(display, 48)).unwrap();
            assert!(rendered.asset.svg.starts_with("<svg"), "{expression}");
            assert!(!rendered.asset.bitmap.rgba.is_empty(), "{expression}");
            assert!(rendered
                .asset
                .bitmap
                .rgba
                .chunks_exact(4)
                .any(|pixel| pixel[3] != 0));
            assert!(bitmap_pixels(&rendered.asset.bitmap) <= MAX_FORMULA_BITMAP_PIXELS);
            assert!((1..=MAX_FORMULA_TERMINAL_ROWS).contains(&rendered.terminal_rows));
            if display == FormulaDisplayMode::Inline {
                assert_eq!(rendered.terminal_rows, 1);
            }
        }
    }

    #[test]
    fn width_and_color_are_part_of_the_render_contract() {
        let narrow =
            render_formula("x + y", "$x + y$", options(FormulaDisplayMode::Inline, 2)).unwrap();
        let wide =
            render_formula("x + y", "$x + y$", options(FormulaDisplayMode::Inline, 80)).unwrap();
        assert!(narrow.terminal_columns <= 2);
        assert!(wide.terminal_columns >= narrow.terminal_columns);
        assert_ne!(narrow.asset.bitmap.width, 0);
    }

    #[test]
    fn invalid_and_resource_limited_formulas_return_typed_errors() {
        let oversized_source = format!("${}$", "x".repeat(MAX_FORMULA_SOURCE_BYTES));
        assert_eq!(
            render_formula("x", &oversized_source, FormulaRenderOptions::default()),
            Err(FormulaRenderError::SourceTooLarge)
        );
        assert_eq!(
            render_formula(
                r"\definitely_not_a_ratex_command{x}",
                r"$\definitely_not_a_ratex_command{x}$",
                FormulaRenderOptions::default(),
            ),
            Err(FormulaRenderError::Parse)
        );
        assert_eq!(
            render_formula(
                r"\rule{100000em}{100000em}",
                r"$$\rule{100000em}{100000em}$$",
                options(FormulaDisplayMode::Display, u16::MAX as usize),
            ),
            Err(FormulaRenderError::BitmapTooLarge)
        );
        assert_eq!(
            render_formula(
                r"\rule{1em}{1000em}",
                r"$$\rule{1em}{1000em}$$",
                options(FormulaDisplayMode::Display, 80),
            ),
            Err(FormulaRenderError::TooManyRows)
        );
    }

    #[test]
    fn repeated_render_is_deterministic_through_the_shared_cache() {
        let request = || {
            render_formula(
                r"\sum_{i=1}^{n} i",
                r"$$\sum_{i=1}^{n} i$$",
                options(FormulaDisplayMode::Display, 64),
            )
            .unwrap()
        };
        assert_eq!(request(), request());
    }

    #[test]
    fn cache_promotes_hits_and_enforces_entry_limit() {
        let mut cache = FormulaRenderCache::default();
        let rendered = FormulaRender {
            terminal_columns: 1,
            terminal_rows: 1,
            baseline_px: 1,
            asset: FormulaAsset {
                svg: "<svg/>".to_string(),
                bitmap: FormulaBitmap {
                    width: 1,
                    height: 1,
                    rgba: vec![0, 0, 0, 0],
                },
            },
        };
        for index in 0..=FORMULA_CACHE_MAX_ENTRIES {
            cache.insert(
                FormulaCacheKey {
                    expression: format!("x_{index}"),
                    options: FormulaRenderOptions::default(),
                },
                rendered.clone(),
            );
        }
        assert_eq!(cache.entries.len(), FORMULA_CACHE_MAX_ENTRIES);
        assert!(cache
            .get(&FormulaCacheKey {
                expression: "x_0".to_string(),
                options: FormulaRenderOptions::default(),
            })
            .is_none());
        let newest = FormulaCacheKey {
            expression: format!("x_{}", FORMULA_CACHE_MAX_ENTRIES),
            options: FormulaRenderOptions::default(),
        };
        assert!(cache.get(&newest).is_some());
        assert_eq!(cache.entries.back().map(|entry| &entry.key), Some(&newest));
    }

    #[test]
    fn cache_enforces_aggregate_pixel_budget() {
        let mut cache = FormulaRenderCache::default();
        let pixels = FORMULA_CACHE_MAX_PIXELS / 2 + 1;
        let rendered = FormulaRender {
            terminal_columns: 1,
            terminal_rows: 1,
            baseline_px: 1,
            asset: FormulaAsset {
                svg: "<svg/>".to_string(),
                bitmap: FormulaBitmap {
                    width: u32::try_from(pixels).unwrap(),
                    height: 1,
                    rgba: Vec::new(),
                },
            },
        };
        for expression in ["first", "second"] {
            cache.insert(
                FormulaCacheKey {
                    expression: expression.to_string(),
                    options: FormulaRenderOptions::default(),
                },
                rendered.clone(),
            );
        }
        assert_eq!(cache.entries.len(), 1);
        assert_eq!(cache.entries[0].key.expression, "second");
        assert!(cache.pixels <= FORMULA_CACHE_MAX_PIXELS);
    }

    #[test]
    fn premultiplied_pixels_are_converted_to_straight_alpha() {
        assert_eq!(
            straight_rgba(&[64, 32, 16, 128, 9, 8, 7, 0]),
            vec![128, 64, 32, 128, 0, 0, 0, 0]
        );
    }
}
