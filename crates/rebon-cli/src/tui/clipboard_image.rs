//! OS clipboard / pasted image path → base64 image reader for image paste.
//!
//! `arboard` hands us a raw RGBA pixel buffer (`ImageData`); the model
//! side expects a base64-encoded image blob with a concrete media
//! type, so we re-encode clipboard pixels as PNG and base64 the result.
//! Some Windows screenshot tools / terminals expose copied images as a
//! temporary file path during Ctrl+V instead of clipboard bitmap data;
//! the path helper accepts that exact pasted-path form and base64s the
//! image file directly.
//! Returning `None` for "no image on clipboard" or any backend error
//! keeps the caller (runner) free of arboard types.

use std::path::{Path, PathBuf};

use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};

/// Paste-path BMP conversion is a convenience fallback for clipboard text paths,
/// not a general-purpose image loader. Cap decoded pixels before allocating RGBA
/// so malformed headers cannot force huge memory allocations. 64 MP covers large
/// desktop screenshots (for example 8K is ~33 MP) while bounding RGBA to 256 MiB.
const MAX_BMP_PIXELS: u64 = 64_000_000;

/// Decoded clipboard image ready to feed into `plan_image_paste`.
pub struct ClipboardImage {
    /// Base64-encoded image payload.
    pub data: String,
    /// MIME type for the payload.
    pub media_type: String,
    /// Optional display filename.
    pub filename: Option<String>,
    /// Optional source catalog path.
    pub source_path: Option<String>,
}

/// Read either bitmap clipboard data or a clipboard text value that is
/// exactly one local image path.
pub fn read_clipboard_image_or_path() -> Option<ClipboardImage> {
    let mut clipboard = arboard::Clipboard::new().ok()?;
    read_bitmap_from_clipboard(&mut clipboard).or_else(|| {
        let text = clipboard.get_text().ok()?;
        read_image_file_from_pasted_text(&text)
    })
}

/// Treat a pasted text payload as an image only when it is exactly one
/// local image file path. This covers Windows terminals that turn a
/// copied screenshot/temp image into `F:\\Temp\\image.jpg` on Ctrl+V.
pub fn read_image_file_from_pasted_text(raw_text: &str) -> Option<ClipboardImage> {
    let path = pasted_image_path_from_text(raw_text)?;
    let bytes = std::fs::read(&path).ok()?;
    let (data_bytes, media_type) = image_bytes_for_model(&bytes)?;
    let filename = path
        .file_name()
        .and_then(|name| name.to_str())
        .map(str::to_string);
    let source_path = Some(path.to_string_lossy().to_string());

    Some(ClipboardImage {
        data: BASE64.encode(data_bytes),
        media_type: media_type.to_string(),
        filename,
        source_path,
    })
}

fn encode_png_rgba(width: u32, height: u32, rgba: &[u8]) -> Option<Vec<u8>> {
    let mut png_bytes: Vec<u8> = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut png_bytes, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().ok()?;
        writer.write_image_data(rgba).ok()?;
    }
    Some(png_bytes)
}

fn read_bitmap_from_clipboard(clipboard: &mut arboard::Clipboard) -> Option<ClipboardImage> {
    let image = clipboard.get_image().ok()?;

    let width: u32 = image.width.try_into().ok()?;
    let height: u32 = image.height.try_into().ok()?;
    if width == 0 || height == 0 {
        return None;
    }

    let png_bytes = encode_png_rgba(width, height, &image.bytes)?;

    Some(ClipboardImage {
        data: BASE64.encode(&png_bytes),
        media_type: String::from("image/png"),
        filename: None,
        source_path: None,
    })
}

pub fn pasted_image_path_from_text(raw_text: &str) -> Option<PathBuf> {
    let mut non_empty = raw_text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty());
    let mut candidate = non_empty.next()?;
    if non_empty.next().is_some() {
        return None;
    }

    candidate = trim_matching_quotes(candidate.trim());
    let path_text = normalize_file_url(candidate)?;
    let path = PathBuf::from(path_text);
    if !is_supported_image_extension(&path) || !path.is_file() {
        return None;
    }
    Some(path)
}

fn trim_matching_quotes(value: &str) -> &str {
    if value.len() >= 2 {
        let bytes = value.as_bytes();
        if (bytes[0] == b'"' && bytes[value.len() - 1] == b'"')
            || (bytes[0] == b'\'' && bytes[value.len() - 1] == b'\'')
        {
            return &value[1..value.len() - 1];
        }
    }
    value
}

/// Convert a local `file://` paste payload into a filesystem path string.
///
/// This intentionally accepts only local file URL shapes used by terminals and
/// file managers (`file:///tmp/a.png`, `file:///C:/a.png`, `file://C:/a.png`,
/// and `file://localhost/tmp/a.png`). Non-local hosts and malformed percent
/// escapes are rejected so ambiguous text falls through to normal paste.
fn normalize_file_url(value: &str) -> Option<String> {
    let Some(scheme) = value.get(..7) else {
        return Some(value.to_string());
    };
    if !scheme.eq_ignore_ascii_case("file://") {
        return Some(value.to_string());
    }
    let rest = &value[7..];

    let rest = strip_localhost_file_url_prefix(rest).unwrap_or_else(|| rest.to_string());
    if rest.contains('/') && !rest.starts_with('/') && !looks_like_windows_drive_path(&rest) {
        return None;
    }
    let decoded = percent_decode(&rest)?;

    #[cfg(windows)]
    {
        let rest = if decoded.len() >= 3
            && decoded.as_bytes()[0] == b'/'
            && decoded.as_bytes()[2] == b':'
            && decoded.as_bytes()[1].is_ascii_alphabetic()
        {
            &decoded[1..]
        } else {
            decoded.as_str()
        };
        Some(rest.replace('/', "\\"))
    }

    #[cfg(not(windows))]
    {
        Some(decoded)
    }
}

fn strip_localhost_file_url_prefix(rest: &str) -> Option<String> {
    let host = "localhost";
    let prefix = rest.get(..host.len())?;
    if !prefix.eq_ignore_ascii_case(host) {
        return None;
    }

    match rest.as_bytes().get(host.len()) {
        Some(b'/') => Some(format!("/{}", &rest[host.len() + 1..])),
        None => Some(String::new()),
        _ => None,
    }
}

fn looks_like_windows_drive_path(value: &str) -> bool {
    value.len() >= 3
        && value.as_bytes()[0].is_ascii_alphabetic()
        && value.as_bytes()[1] == b':'
        && value.as_bytes()[2] == b'/'
}

fn percent_decode(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut idx = 0;
    while idx < bytes.len() {
        if bytes[idx] == b'%' {
            let hi = *bytes.get(idx + 1)?;
            let lo = *bytes.get(idx + 2)?;
            decoded.push(hex_value(hi)? << 4 | hex_value(lo)?);
            idx += 3;
        } else {
            decoded.push(bytes[idx]);
            idx += 1;
        }
    }
    String::from_utf8(decoded).ok()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn is_supported_image_extension(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|ext| ext.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some("png" | "jpg" | "jpeg" | "webp" | "gif" | "bmp")
    )
}

fn image_bytes_for_model(bytes: &[u8]) -> Option<(Vec<u8>, &'static str)> {
    if is_bmp(bytes) {
        return Some((decode_bmp_to_png(bytes)?, "image/png"));
    }
    Some((bytes.to_vec(), detect_image_media_type(bytes)?))
}

fn detect_image_media_type(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Some("image/png");
    }
    if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        return Some("image/jpeg");
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some("image/gif");
    }
    if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    None
}

fn is_bmp(bytes: &[u8]) -> bool {
    bytes.starts_with(b"BM")
}

fn decode_bmp_to_png(bytes: &[u8]) -> Option<Vec<u8>> {
    let header = BmpHeader::parse(bytes)?;
    let pixel_count = u64::from(header.width).checked_mul(u64::from(header.height))?;
    if pixel_count > MAX_BMP_PIXELS {
        return None;
    }
    let pixel_count: usize = pixel_count.try_into().ok()?;
    let mut rgba = vec![0_u8; pixel_count.checked_mul(4)?];

    match header.bits_per_pixel {
        24 => decode_bmp_rgb24(bytes, &header, &mut rgba)?,
        32 => decode_bmp_bgra32(bytes, &header, &mut rgba)?,
        _ => return None,
    }

    encode_png_rgba(header.width, header.height, &rgba)
}

struct BmpHeader {
    width: u32,
    height: u32,
    top_down: bool,
    bits_per_pixel: u16,
    pixel_offset: usize,
}

impl BmpHeader {
    fn parse(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < 54 || !is_bmp(bytes) {
            return None;
        }
        let pixel_offset = read_u32_le(bytes, 10)? as usize;
        let dib_header_size = read_u32_le(bytes, 14)?;
        if dib_header_size < 40 {
            return None;
        }
        let width = read_i32_le(bytes, 18)?;
        let signed_height = read_i32_le(bytes, 22)?;
        let planes = read_u16_le(bytes, 26)?;
        let bits_per_pixel = read_u16_le(bytes, 28)?;
        let compression = read_u32_le(bytes, 30)?;
        if width <= 0 || signed_height == 0 || planes != 1 || compression != 0 {
            return None;
        }
        Some(Self {
            width: width as u32,
            height: signed_height.unsigned_abs(),
            top_down: signed_height < 0,
            bits_per_pixel,
            pixel_offset,
        })
    }

    fn row_stride(&self) -> Option<usize> {
        let bits_per_row = (self.width as usize).checked_mul(self.bits_per_pixel as usize)?;
        bits_per_row
            .checked_add(31)?
            .checked_div(32)?
            .checked_mul(4)
    }

    fn source_y(&self, y: u32) -> u32 {
        if self.top_down {
            y
        } else {
            self.height - 1 - y
        }
    }
}

fn decode_bmp_rgb24(bytes: &[u8], header: &BmpHeader, rgba: &mut [u8]) -> Option<()> {
    let row_stride = header.row_stride()?;
    for y in 0..header.height {
        let row_start = header
            .pixel_offset
            .checked_add((header.source_y(y) as usize).checked_mul(row_stride)?)?;
        for x in 0..header.width {
            let src = row_start.checked_add((x as usize).checked_mul(3)?)?;
            let b = *bytes.get(src)?;
            let g = *bytes.get(src + 1)?;
            let r = *bytes.get(src + 2)?;
            let dst = ((y as usize)
                .checked_mul(header.width as usize)?
                .checked_add(x as usize)?)
            .checked_mul(4)?;
            rgba[dst..dst + 4].copy_from_slice(&[r, g, b, 0xff]);
        }
    }
    Some(())
}

fn decode_bmp_bgra32(bytes: &[u8], header: &BmpHeader, rgba: &mut [u8]) -> Option<()> {
    let row_stride = header.row_stride()?;
    for y in 0..header.height {
        let row_start = header
            .pixel_offset
            .checked_add((header.source_y(y) as usize).checked_mul(row_stride)?)?;
        for x in 0..header.width {
            let src = row_start.checked_add((x as usize).checked_mul(4)?)?;
            let b = *bytes.get(src)?;
            let g = *bytes.get(src + 1)?;
            let r = *bytes.get(src + 2)?;
            let a = *bytes.get(src + 3)?;
            let dst = ((y as usize)
                .checked_mul(header.width as usize)?
                .checked_add(x as usize)?)
            .checked_mul(4)?;
            rgba[dst..dst + 4].copy_from_slice(&[r, g, b, a]);
        }
    }
    Some(())
}

fn read_u16_le(bytes: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_le_bytes(
        bytes.get(offset..offset + 2)?.try_into().ok()?,
    ))
}

fn read_u32_le(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        bytes.get(offset..offset + 4)?.try_into().ok()?,
    ))
}

fn read_i32_le(bytes: &[u8], offset: usize) -> Option<i32> {
    Some(i32::from_le_bytes(
        bytes.get(offset..offset + 4)?.try_into().ok()?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write_bytes(path: &Path, bytes: &[u8]) {
        fs::write(path, bytes).unwrap();
    }

    fn file_url_for(path: &Path) -> String {
        let path = path
            .to_string_lossy()
            .replace('\\', "/")
            .replace(' ', "%20");
        #[cfg(windows)]
        {
            format!("file:///{path}")
        }
        #[cfg(not(windows))]
        {
            format!("file://{path}")
        }
    }

    fn bmp_1x1_rgb24() -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"BM");
        bytes.extend_from_slice(&58_u32.to_le_bytes());
        bytes.extend_from_slice(&[0, 0, 0, 0]);
        bytes.extend_from_slice(&54_u32.to_le_bytes());
        bytes.extend_from_slice(&40_u32.to_le_bytes());
        bytes.extend_from_slice(&1_i32.to_le_bytes());
        bytes.extend_from_slice(&1_i32.to_le_bytes());
        bytes.extend_from_slice(&1_u16.to_le_bytes());
        bytes.extend_from_slice(&24_u16.to_le_bytes());
        bytes.extend_from_slice(&0_u32.to_le_bytes());
        bytes.extend_from_slice(&4_u32.to_le_bytes());
        bytes.extend_from_slice(&[0; 16]);
        bytes.extend_from_slice(&[0x33, 0x22, 0x11, 0x00]);
        bytes
    }

    fn bmp_rgb24_with_declared_size(width: i32, height: i32) -> Vec<u8> {
        let mut bytes = bmp_1x1_rgb24();
        bytes[18..22].copy_from_slice(&width.to_le_bytes());
        bytes[22..26].copy_from_slice(&height.to_le_bytes());
        bytes
    }

    #[test]
    fn pasted_image_path_accepts_supported_extensions_and_wrappers() {
        let dir = tempfile::tempdir().unwrap();
        let cases = [
            ("image.png", "{}"),
            ("image.jpg", "{}"),
            ("image.jpeg", "{}"),
            ("image.webp", "{}"),
            ("image.gif", "{}"),
            ("UPPER.PNG", "{}"),
            ("quoted double.png", "\"{}\""),
            ("quoted single.jpg", "'{}'"),
        ];

        for (filename, wrapper) in cases {
            let path = dir.path().join(filename);
            write_bytes(&path, b"not read by path parser");
            let raw = format!("  \n{}\n  ", wrapper.replace("{}", &path.to_string_lossy()));
            assert_eq!(pasted_image_path_from_text(&raw), Some(path));
        }
    }

    #[test]
    fn pasted_image_path_accepts_file_url_and_percent_encoded_space() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("space name.bmp");
        write_bytes(&path, b"BMnot read by path parser");

        let raw = format!("\"{}\"", file_url_for(&path));
        assert_eq!(pasted_image_path_from_text(&raw), Some(path));
    }

    #[test]
    fn pasted_image_path_accepts_case_insensitive_file_url_scheme() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mixed case.bmp");
        write_bytes(&path, b"BMnot read by path parser");

        let raw = file_url_for(&path).replacen("file://", "FiLe://", 1);
        assert_eq!(pasted_image_path_from_text(&raw), Some(path));
    }

    #[test]
    fn pasted_image_path_rejects_ambiguous_text() {
        let dir = tempfile::tempdir().unwrap();
        let one = dir.path().join("one.png");
        let two = dir.path().join("two.jpg");
        write_bytes(&one, b"x");
        write_bytes(&two, b"x");

        assert_eq!(pasted_image_path_from_text("hello\nworld"), None);
        assert_eq!(
            pasted_image_path_from_text(&format!("{}\n{}", one.display(), two.display())),
            None
        );
        assert_eq!(
            pasted_image_path_from_text("file://example.com/not-local.png"),
            None
        );
    }

    #[test]
    fn read_image_file_from_pasted_text_converts_bmp_to_png() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("screen shot.BMP");
        write_bytes(&path, &bmp_1x1_rgb24());

        let image =
            read_image_file_from_pasted_text(&format!("'{}'", file_url_for(&path))).unwrap();

        assert_eq!(image.media_type, "image/png");
        assert_ne!(image.media_type, "image/bmp");
        assert_eq!(image.filename, Some("screen shot.BMP".to_string()));
        assert_eq!(image.source_path, Some(path.to_string_lossy().to_string()));
        let png = BASE64.decode(image.data).unwrap();
        assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"));
    }

    #[test]
    fn read_image_file_from_pasted_text_rejects_bmp_above_pixel_limit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("huge.bmp");
        let over_limit_height = (MAX_BMP_PIXELS / 8_000 + 1) as i32;
        write_bytes(
            &path,
            &bmp_rgb24_with_declared_size(8_000, over_limit_height),
        );

        assert!(read_image_file_from_pasted_text(&path.to_string_lossy()).is_none());
    }

    #[test]
    fn read_image_file_from_pasted_text_keeps_existing_image_media_types() {
        let dir = tempfile::tempdir().unwrap();
        let cases: &[(&str, &[u8], &str)] = &[
            ("pixel.png", b"\x89PNG\r\n\x1a\nrest", "image/png"),
            ("pixel.jpg", &[0xff, 0xd8, 0xff, 0xd9], "image/jpeg"),
            ("pixel.jpeg", &[0xff, 0xd8, 0xff, 0xd9], "image/jpeg"),
            ("pixel.gif", b"GIF89arest", "image/gif"),
            ("pixel.webp", b"RIFFxxxxWEBPrest", "image/webp"),
        ];

        for (filename, bytes, media_type) in cases {
            let path = dir.path().join(filename);
            write_bytes(&path, bytes);
            let image = read_image_file_from_pasted_text(&path.to_string_lossy()).unwrap();
            assert_eq!(image.media_type, *media_type);
            assert_eq!(image.data, BASE64.encode(bytes));
        }
    }
}
