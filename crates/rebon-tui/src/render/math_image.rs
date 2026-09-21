use std::{
    collections::VecDeque,
    hash::{Hash, Hasher},
    sync::{Mutex, OnceLock},
};

use image::{DynamicImage, Rgb, RgbImage};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span, Text},
    widgets::{Paragraph, Widget, Wrap},
};
use ratatui_image::{
    picker::{Picker, ProtocolType},
    protocol::Protocol,
    Image as RatatuiImage, Resize,
};
use rebon_width::WidthStr;

const MATH_IMAGE_CACHE_MAX_ENTRIES: usize = 128;
const MATH_IMAGE_CACHE_MAX_PIXELS: u64 = 16_000_000;
const FORMULA_MASK_MARKER: Color = Color::Rgb(1, 2, 3);

/// Native terminal graphics protocol used for formula image overlays.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MathGraphicsProtocol {
    /// Kitty graphics protocol.
    Kitty,
    /// Sixel graphics protocol.
    Sixel,
    /// iTerm2 inline image protocol.
    Iterm2,
}

/// Formula display policy carried by the TUI render theme.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum MathDisplayMode {
    /// Preserve the existing Markdown behavior without parsing formulas.
    #[default]
    Off,
    /// Render formulas with the portable Unicode half-block fallback.
    Unicode,
    /// Render native terminal images when possible, with Unicode fallback.
    Graphics {
        /// Selected native terminal image protocol.
        protocol: MathGraphicsProtocol,
        /// Terminal cell width and height in pixels.
        cell_size: (u16, u16),
    },
}

impl MathDisplayMode {
    /// Whether the Markdown parser should enable math events.
    pub const fn math_enabled(self) -> bool {
        !matches!(self, Self::Off)
    }

    /// Return the same layout policy without native image enhancement.
    pub const fn without_graphics(self) -> Self {
        if self.math_enabled() {
            Self::Unicode
        } else {
            Self::Off
        }
    }

    pub(super) fn cache_discriminant(self) -> u64 {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        self.hash(&mut hasher);
        hasher.finish()
    }
}

pub(super) fn markdown_render_options(
    mode: MathDisplayMode,
) -> rebon_message_tui::MarkdownRenderOptions {
    rebon_message_tui::MarkdownRenderOptions::default().with_math(mode.math_enabled())
}

pub(super) fn apply_math_image_layers(
    layers: &[rebon_message_tui::HyperlinkPaintLayer],
    theme: &super::RenderTheme,
    buf: &mut Buffer,
) {
    let MathDisplayMode::Graphics {
        protocol,
        cell_size,
    } = theme.math_display
    else {
        return;
    };
    let background = theme_background(theme.name);
    let Ok(mut cache) = math_image_cache().lock() else {
        return;
    };

    for layer in layers {
        for formula in &layer.formulas {
            let Some(area) = formula_paint_area(layer, formula, buf) else {
                continue;
            };
            let _ = cache.render(
                &formula.asset.bitmap,
                protocol,
                cell_size,
                background,
                area,
                buf,
            );
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct MathImageCacheKey {
    bitmap_hash: u64,
    protocol: MathGraphicsProtocol,
    cell_size: (u16, u16),
    background: [u8; 3],
    width: u16,
    height: u16,
}

struct MathImageCacheEntry {
    key: MathImageCacheKey,
    protocol: Protocol,
    pixels: u64,
}

struct MathImagePicker {
    protocol: MathGraphicsProtocol,
    cell_size: (u16, u16),
    picker: Picker,
}

#[derive(Default)]
struct MathImageCache {
    entries: VecDeque<MathImageCacheEntry>,
    pickers: Vec<MathImagePicker>,
    pixels: u64,
}

impl MathImageCache {
    fn render(
        &mut self,
        bitmap: &rebon_message_tui::FormulaBitmap,
        protocol: MathGraphicsProtocol,
        cell_size: (u16, u16),
        background: [u8; 3],
        area: Rect,
        buf: &mut Buffer,
    ) -> bool {
        let key = MathImageCacheKey {
            bitmap_hash: bitmap_hash(bitmap),
            protocol,
            cell_size,
            background,
            width: area.width,
            height: area.height,
        };
        if let Some(index) = self.entries.iter().position(|entry| entry.key == key) {
            let Some(entry) = self.entries.remove(index) else {
                return false;
            };
            RatatuiImage::new(&entry.protocol).render(area, buf);
            self.entries.push_back(entry);
            return true;
        }

        let Some(image) = composite_formula_bitmap(bitmap, background) else {
            return false;
        };
        let pixels = u64::from(bitmap.width).saturating_mul(u64::from(bitmap.height));
        let picker_index = if let Some(index) = self
            .pickers
            .iter()
            .position(|picker| picker.protocol == protocol && picker.cell_size == cell_size)
        {
            index
        } else {
            let mut picker = Picker::from_fontsize(cell_size);
            picker.set_protocol_type(protocol_type(protocol));
            self.pickers.push(MathImagePicker {
                protocol,
                cell_size,
                picker,
            });
            self.pickers.len() - 1
        };
        let picker = &mut self.pickers[picker_index].picker;
        picker.set_background_color(Some(Rgb(background)));
        let Ok(encoded) = picker.new_protocol(
            image,
            Rect::new(0, 0, area.width, area.height),
            Resize::Fit(Some(image::imageops::FilterType::Triangle)),
        ) else {
            return false;
        };

        RatatuiImage::new(&encoded).render(area, buf);
        self.pixels = self.pixels.saturating_add(pixels);
        self.entries.push_back(MathImageCacheEntry {
            key,
            protocol: encoded,
            pixels,
        });
        while self.entries.len() > MATH_IMAGE_CACHE_MAX_ENTRIES
            || self.pixels > MATH_IMAGE_CACHE_MAX_PIXELS
        {
            let Some(entry) = self.entries.pop_front() else {
                break;
            };
            self.pixels = self.pixels.saturating_sub(entry.pixels);
        }
        true
    }
}

fn math_image_cache() -> &'static Mutex<MathImageCache> {
    static CACHE: OnceLock<Mutex<MathImageCache>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(MathImageCache::default()))
}

fn protocol_type(protocol: MathGraphicsProtocol) -> ProtocolType {
    match protocol {
        MathGraphicsProtocol::Kitty => ProtocolType::Kitty,
        MathGraphicsProtocol::Sixel => ProtocolType::Sixel,
        MathGraphicsProtocol::Iterm2 => ProtocolType::Iterm2,
    }
}

fn bitmap_hash(bitmap: &rebon_message_tui::FormulaBitmap) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    bitmap.width.hash(&mut hasher);
    bitmap.height.hash(&mut hasher);
    bitmap.rgba.hash(&mut hasher);
    hasher.finish()
}

fn composite_formula_bitmap(
    bitmap: &rebon_message_tui::FormulaBitmap,
    background: [u8; 3],
) -> Option<DynamicImage> {
    let pixel_count = usize::try_from(bitmap.width.checked_mul(bitmap.height)?).ok()?;
    if bitmap.rgba.len() != pixel_count.checked_mul(4)? {
        return None;
    }
    let mut rgb = Vec::with_capacity(pixel_count * 3);
    for pixel in bitmap.rgba.chunks_exact(4) {
        let alpha = u16::from(pixel[3]);
        for channel in 0..3 {
            let foreground = u16::from(pixel[channel]);
            let background = u16::from(background[channel]);
            rgb.push(((foreground * alpha + background * (255 - alpha) + 127) / 255) as u8);
        }
    }
    RgbImage::from_raw(bitmap.width, bitmap.height, rgb).map(DynamicImage::ImageRgb8)
}

fn formula_paint_area(
    layer: &rebon_message_tui::HyperlinkPaintLayer,
    formula: &rebon_message_tui::RenderedFormula,
    buf: &Buffer,
) -> Option<Rect> {
    if layer.area.width == 0 || layer.area.height == 0 || formula.ranges.is_empty() {
        return None;
    }
    let mask_text = formula_mask_text(&layer.text, formula)?;
    let mask_area = Rect::new(0, 0, layer.area.width, layer.area.height);
    let mut mask = Buffer::empty(mask_area);
    Paragraph::new(mask_text)
        .wrap(Wrap { trim: false })
        .render(mask_area, &mut mask);

    let mut found: Option<Rect> = None;
    for y in 0..mask_area.height {
        let Some((start, end)) = marked_run(&mask, y) else {
            continue;
        };
        let width = end.saturating_sub(start).saturating_add(1);
        match found.as_mut() {
            None => found = Some(Rect::new(start, y, width, 1)),
            Some(rect)
                if rect.x == start
                    && rect.width == width
                    && rect.y.saturating_add(rect.height) == y =>
            {
                rect.height = rect.height.saturating_add(1);
            }
            Some(_) => return None,
        }
    }

    let relative = found?;
    if relative.width != formula.terminal_columns || relative.height != formula.terminal_rows {
        return None;
    }
    let area = Rect::new(
        layer.area.x.saturating_add(relative.x),
        layer.area.y.saturating_add(relative.y),
        relative.width,
        relative.height,
    );
    let right = area.x.checked_add(area.width)?.checked_sub(1)?;
    let bottom = area.y.checked_add(area.height)?.checked_sub(1)?;
    if !buf.area().contains((area.x, area.y).into()) || !buf.area().contains((right, bottom).into())
    {
        return None;
    }
    Some(area)
}

fn formula_mask_text(
    text: &Text<'static>,
    formula: &rebon_message_tui::RenderedFormula,
) -> Option<Text<'static>> {
    let mut ranges = formula.ranges.iter().collect::<Vec<_>>();
    ranges.sort_by_key(|range| (range.line, range.start_byte));
    let mut lines = Vec::with_capacity(text.lines.len());

    for (line_index, line) in text.lines.iter().enumerate() {
        let content = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        let line_ranges = ranges
            .iter()
            .copied()
            .filter(|range| range.line == line_index)
            .collect::<Vec<_>>();
        let mut cursor = 0;
        let mut spans = Vec::new();
        for range in line_ranges {
            if range.start_byte < cursor
                || range.start_byte >= range.end_byte
                || range.end_byte > content.len()
                || !content.is_char_boundary(range.start_byte)
                || !content.is_char_boundary(range.end_byte)
            {
                return None;
            }
            if cursor < range.start_byte {
                spans.push(Span::raw(content[cursor..range.start_byte].to_string()));
            }
            spans.push(Span::styled(
                content[range.start_byte..range.end_byte].to_string(),
                Style::new().fg(FORMULA_MASK_MARKER),
            ));
            cursor = range.end_byte;
        }
        if cursor < content.len() {
            spans.push(Span::raw(content[cursor..].to_string()));
        }
        let mut masked = Line::from(spans);
        masked.alignment = line.alignment;
        lines.push(masked);
    }
    Some(Text::from(lines))
}

fn marked_run(mask: &Buffer, y: u16) -> Option<(u16, u16)> {
    let mut run_start = None;
    let mut run_end = None;
    let mut marked_until = 0;
    let mut finished_run = false;
    for x in 0..mask.area.width {
        let cell = &mask[(x, y)];
        if cell.fg == FORMULA_MASK_MARKER {
            let symbol_width = WidthStr::width(cell.symbol()).max(1) as u16;
            marked_until = marked_until.max(x.saturating_add(symbol_width));
        }
        let marked = x < marked_until;
        if marked {
            if finished_run {
                return None;
            }
            run_start.get_or_insert(x);
            run_end = Some(x);
        } else if run_start.is_some() {
            finished_run = true;
        }
    }
    run_start.map(|start| (start, run_end.unwrap_or(start)))
}

fn theme_background(name: rebon_design_system::theme::ThemeName) -> [u8; 3] {
    use rebon_design_system::theme::ThemeName;

    match name {
        ThemeName::Light | ThemeName::LightDaltonized | ThemeName::LightAnsi => [255, 255, 255],
        ThemeName::Dark | ThemeName::DarkDaltonized | ThemeName::DarkAnsi => [0, 0, 0],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::text::Line;

    #[test]
    fn disabling_graphics_preserves_math_layout_policy() {
        assert_eq!(
            MathDisplayMode::Graphics {
                protocol: MathGraphicsProtocol::Kitty,
                cell_size: (8, 16),
            }
            .without_graphics(),
            MathDisplayMode::Unicode
        );
        assert_eq!(
            MathDisplayMode::Unicode.without_graphics(),
            MathDisplayMode::Unicode
        );
        assert_eq!(
            MathDisplayMode::Off.without_graphics(),
            MathDisplayMode::Off
        );
    }

    #[test]
    fn native_protocol_and_cell_metrics_participate_in_cache_keys() {
        let kitty = MathDisplayMode::Graphics {
            protocol: MathGraphicsProtocol::Kitty,
            cell_size: (8, 16),
        };
        let resized = MathDisplayMode::Graphics {
            protocol: MathGraphicsProtocol::Kitty,
            cell_size: (9, 18),
        };
        let sixel = MathDisplayMode::Graphics {
            protocol: MathGraphicsProtocol::Sixel,
            cell_size: (8, 16),
        };

        assert_ne!(kitty.cache_discriminant(), resized.cache_discriminant());
        assert_ne!(kitty.cache_discriminant(), sixel.cache_discriminant());
    }

    #[test]
    fn formula_mask_tracks_wrapping_and_rejects_split_images() {
        let formula = rebon_message_tui::RenderedFormula {
            source: "$x$".into(),
            expression: "x".into(),
            display: rebon_message_tui::FormulaDisplayMode::Inline,
            ranges: vec![rebon_message_tui::FormulaRange {
                line: 0,
                start_byte: 5,
                end_byte: 11,
            }],
            terminal_columns: 2,
            terminal_rows: 1,
            fallback: Text::from(Line::raw("▀▀")),
            asset: rebon_message_tui::FormulaAsset::default(),
        };
        let layer = rebon_message_tui::HyperlinkPaintLayer {
            area: Rect::new(3, 4, 10, 2),
            text: Text::from(Line::raw("12345▀▀")),
            hyperlinks: Vec::new(),
            formulas: vec![formula.clone()],
        };
        let buf = Buffer::empty(Rect::new(0, 0, 20, 10));
        assert_eq!(
            formula_paint_area(&layer, &formula, &buf),
            Some(Rect::new(8, 4, 2, 1))
        );

        let wrapped_layer = rebon_message_tui::HyperlinkPaintLayer {
            area: Rect::new(3, 4, 6, 2),
            ..layer
        };
        assert_eq!(formula_paint_area(&wrapped_layer, &formula, &buf), None);
    }

    #[test]
    fn graphics_mode_overlays_the_unicode_fallback_with_protocol_cells() {
        let bitmap = rebon_message_tui::FormulaBitmap {
            width: 8,
            height: 16,
            rgba: [255, 255, 255, 255].repeat(8 * 16),
        };
        let formula = rebon_message_tui::RenderedFormula {
            source: "$x$".into(),
            expression: "x".into(),
            display: rebon_message_tui::FormulaDisplayMode::Inline,
            ranges: vec![rebon_message_tui::FormulaRange {
                line: 0,
                start_byte: 0,
                end_byte: "▀".len(),
            }],
            terminal_columns: 1,
            terminal_rows: 1,
            fallback: Text::from(Line::raw("▀")),
            asset: rebon_message_tui::FormulaAsset {
                svg: String::new(),
                bitmap,
            },
        };
        let layer = rebon_message_tui::HyperlinkPaintLayer {
            area: Rect::new(0, 0, 1, 1),
            text: Text::from(Line::raw("▀")),
            hyperlinks: Vec::new(),
            formulas: vec![formula],
        };
        let mut theme = super::super::RenderTheme::plain();
        theme.math_display = MathDisplayMode::Graphics {
            protocol: MathGraphicsProtocol::Kitty,
            cell_size: (8, 16),
        };
        let mut buf = Buffer::empty(Rect::new(0, 0, 1, 1));
        buf[(0, 0)].set_symbol("▀");

        apply_math_image_layers(&[layer], &theme, &mut buf);

        assert_ne!(buf[(0, 0)].symbol(), "▀");
        assert!(buf[(0, 0)].symbol().contains('\u{1b}'));
    }

    #[test]
    fn bitmap_compositing_keeps_fallback_available_for_invalid_assets() {
        let invalid = rebon_message_tui::FormulaBitmap {
            width: 1,
            height: 1,
            rgba: vec![255, 255, 255],
        };
        assert!(composite_formula_bitmap(&invalid, [0, 0, 0]).is_none());

        let valid = rebon_message_tui::FormulaBitmap {
            width: 1,
            height: 1,
            rgba: vec![255, 0, 0, 128],
        };
        let image = composite_formula_bitmap(&valid, [0, 0, 255]).unwrap();
        assert_eq!(image.to_rgb8().get_pixel(0, 0).0, [128, 0, 127]);
    }
}
