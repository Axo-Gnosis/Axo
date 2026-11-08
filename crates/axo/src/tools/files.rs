//! Helpers shared by the file tools: BOM and line-ending handling (pi `edit-diff.ts`,
//! `utils/text.ts`), Node-style filesystem error text, and image detection and resizing
//! (pi `utils/mime.ts`, `image-process.ts`, `image-resize-core.ts`).

use std::borrow::Cow;
use std::io::{self, Cursor, ErrorKind};
use std::path::Path;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use image::codecs::jpeg::JpegEncoder;
use image::imageops::FilterType;
use image::metadata::Orientation;
use image::{DynamicImage, ImageDecoder, ImageFormat, ImageReader};

/// The UTF-8 byte order mark.
pub const BOM: &str = "\u{FEFF}";

/// Splits a leading BOM off `text`: `(bom, rest)` with `bom` empty when there is none.
pub fn split_bom(text: &str) -> (&str, &str) {
    match text.strip_prefix(BOM) {
        Some(rest) => (BOM, rest),
        None => ("", text),
    }
}

/// `"\r\n"` when the first line break is CRLF, else `"\n"`.
pub fn detect_line_ending(content: &str) -> &'static str {
    match (content.find("\r\n"), content.find('\n')) {
        (Some(crlf), Some(lf)) if crlf < lf => "\r\n",
        _ => "\n",
    }
}

/// CRLF and lone CR become LF.
pub fn normalize_to_lf(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

/// LF becomes `ending`.
pub fn restore_line_endings<'a>(text: &'a str, ending: &str) -> Cow<'a, str> {
    if ending == "\r\n" {
        Cow::Owned(text.replace('\n', "\r\n"))
    } else {
        Cow::Borrowed(text)
    }
}

/// The Node error code and message for an I/O error, when it has one of the common ones.
pub fn error_code(error: &io::Error) -> Option<(&'static str, &'static str)> {
    match error.kind() {
        ErrorKind::NotFound => Some(("ENOENT", "no such file or directory")),
        ErrorKind::PermissionDenied => Some(("EACCES", "permission denied")),
        ErrorKind::IsADirectory => Some(("EISDIR", "illegal operation on a directory")),
        ErrorKind::NotADirectory => Some(("ENOTDIR", "not a directory")),
        ErrorKind::ReadOnlyFilesystem => Some(("EROFS", "read-only file system")),
        _ => None,
    }
}

/// Node's filesystem error text, e.g. `ENOENT: no such file or directory, access '/a/b'`,
/// which is what pi's tools report.
pub fn fs_error(error: &io::Error, syscall: &str, path: &Path) -> String {
    match error_code(error) {
        Some((code, message)) => format!("{code}: {message}, {syscall} '{}'", path.display()),
        None => error.to_string(),
    }
}

/// How many leading bytes image detection looks at.
pub const IMAGE_SNIFF_BYTES: usize = 4100;

/// The image MIME type of `bytes` by magic bytes (pi `detectSupportedImageMimeType`):
/// JPEG (not JPEG-LS), non-animated PNG, GIF, WebP and BMP.
pub fn detect_image_mime(bytes: &[u8]) -> Option<&'static str> {
    const PNG_SIGNATURE: &[u8] = &[0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a];
    let at = |offset: usize, text: &[u8]| bytes.get(offset..offset + text.len()) == Some(text);
    let u16_le = |offset: usize| {
        u16::from_le_bytes([
            bytes.get(offset).copied().unwrap_or(0),
            bytes.get(offset + 1).copied().unwrap_or(0),
        ])
    };
    let u32_at = |offset: usize, big_endian: bool| {
        let mut word = [0; 4];
        for (i, byte) in word.iter_mut().enumerate() {
            *byte = bytes.get(offset + i).copied().unwrap_or(0);
        }
        if big_endian {
            u32::from_be_bytes(word)
        } else {
            u32::from_le_bytes(word)
        }
    };

    if at(0, &[0xff, 0xd8, 0xff]) {
        return (bytes.get(3) != Some(&0xf7)).then_some("image/jpeg");
    }
    if at(0, PNG_SIGNATURE) {
        let is_png = bytes.len() >= 16 && u32_at(8, true) == 13 && at(12, b"IHDR");
        let mut animated = false;
        let mut offset = PNG_SIGNATURE.len();
        while offset + 8 <= bytes.len() {
            if at(offset + 4, b"acTL") {
                animated = true;
                break;
            }
            if at(offset + 4, b"IDAT") {
                break;
            }
            let next = offset + 12 + u32_at(offset, true) as usize;
            if next > bytes.len() {
                break;
            }
            offset = next;
        }
        return (is_png && !animated).then_some("image/png");
    }
    if at(0, b"GIF87a") || at(0, b"GIF89a") {
        return Some("image/gif");
    }
    if at(0, b"RIFF") && at(8, b"WEBP") {
        return Some("image/webp");
    }
    if at(0, b"BM") && bytes.len() >= 26 {
        let file_size = u32_at(2, false);
        let pixel_offset = u32_at(10, false);
        let dib_size = u32_at(14, false);
        if (file_size != 0 && file_size < 26)
            || u64::from(pixel_offset) < 14 + u64::from(dib_size)
            || (file_size != 0 && pixel_offset >= file_size)
        {
            return None;
        }
        let (planes, bits) = match dib_size {
            12 => (u16_le(22), u16_le(24)),
            40..=124 if bytes.len() >= 30 => (u16_le(26), u16_le(28)),
            _ => return None,
        };
        return (planes == 1 && [1, 4, 8, 16, 24, 32].contains(&bits)).then_some("image/bmp");
    }
    None
}

/// An image ready to inline: base64 data, its MIME type, and notes for the model.
pub struct ProcessedImage {
    pub data: String,
    pub mime_type: &'static str,
    pub hints: Vec<String>,
}

/// Max width and height of an inlined image.
const MAX_IMAGE_DIMENSION: u32 = 2000;
/// 4.5 MB of base64 payload, headroom below Anthropic's 5 MB limit.
const MAX_IMAGE_BASE64_BYTES: usize = 4_718_592;
/// JPEG qualities tried in order (pi: default 80, then 85, 70, 55, 40).
const JPEG_QUALITIES: [u8; 5] = [80, 85, 70, 55, 40];

/// Prepares an image for inlining (pi `processImage` with auto-resize on): BMP is converted
/// to PNG; the image is shrunk to fit 2000×2000 and 4.5 MB of base64. CPU-bound; call it
/// from a blocking thread. `Err` is the model-facing omission message.
pub fn process_image(bytes: &[u8], mime: &'static str) -> Result<ProcessedImage, &'static str> {
    const NOT_CONVERTED: &str =
        "[Image omitted: could not be converted to a supported inline image format.]";
    const NOT_RESIZED: &str =
        "[Image omitted: could not be resized below the inline image size limit.]";

    let (bytes, mime, converted_from) = if mime == "image/bmp" {
        let png = image::load_from_memory_with_format(bytes, ImageFormat::Bmp)
            .ok()
            .and_then(|image| encode_png(&image))
            .ok_or(NOT_CONVERTED)?;
        (Cow::Owned(png), "image/png", Some("image/bmp"))
    } else {
        (Cow::Borrowed(bytes), mime, None)
    };

    let base64_len = |len: usize| len.div_ceil(3) * 4;
    let format = ImageFormat::from_mime_type(mime).ok_or(NOT_RESIZED)?;
    let mut decoder = ImageReader::with_format(Cursor::new(&*bytes), format)
        .into_decoder()
        .map_err(|_| NOT_RESIZED)?;
    let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);
    let mut image = DynamicImage::from_decoder(decoder).map_err(|_| NOT_RESIZED)?;
    image.apply_orientation(orientation);
    let (original_width, original_height) = (image.width(), image.height());

    let mut hints = Vec::new();
    let push_conversion_hint = |hints: &mut Vec<String>, to: &str| {
        if let Some(from) = converted_from
            && from != to
        {
            hints.push(format!("[Image converted from {from} to {to}.]"));
        }
    };

    if original_width <= MAX_IMAGE_DIMENSION
        && original_height <= MAX_IMAGE_DIMENSION
        && base64_len(bytes.len()) < MAX_IMAGE_BASE64_BYTES
    {
        push_conversion_hint(&mut hints, mime);
        return Ok(ProcessedImage {
            data: STANDARD.encode(&bytes),
            mime_type: mime,
            hints,
        });
    }

    let (mut width, mut height) = (original_width, original_height);
    if width > MAX_IMAGE_DIMENSION {
        height = (f64::from(height) * f64::from(MAX_IMAGE_DIMENSION) / f64::from(width))
            .round()
            .max(1.0) as u32;
        width = MAX_IMAGE_DIMENSION;
    }
    if height > MAX_IMAGE_DIMENSION {
        width = (f64::from(width) * f64::from(MAX_IMAGE_DIMENSION) / f64::from(height))
            .round()
            .max(1.0) as u32;
        height = MAX_IMAGE_DIMENSION;
    }

    loop {
        let resized = image.resize_exact(width, height, FilterType::Lanczos3);
        let png = encode_png(&resized).map(|png| (png, "image/png"));
        let rgb = resized.to_rgb8();
        let candidate = png
            .into_iter()
            .chain(JPEG_QUALITIES.iter().filter_map(|&quality| {
                let mut jpeg = Vec::new();
                let encoded = JpegEncoder::new_with_quality(&mut jpeg, quality).encode_image(&rgb);
                encoded.ok().map(|()| (jpeg, "image/jpeg"))
            }))
            .find(|(encoded, _)| base64_len(encoded.len()) < MAX_IMAGE_BASE64_BYTES);
        if let Some((encoded, mime_type)) = candidate {
            push_conversion_hint(&mut hints, mime_type);
            hints.push(format!(
                "[Image: original {original_width}x{original_height}, displayed at {width}x{height}. Multiply coordinates by {:.2} to map to original image.]",
                f64::from(original_width) / f64::from(width)
            ));
            return Ok(ProcessedImage {
                data: STANDARD.encode(encoded),
                mime_type,
                hints,
            });
        }
        if width == 1 && height == 1 {
            return Err(NOT_RESIZED);
        }
        let next_width = (width * 3 / 4).max(1);
        let next_height = (height * 3 / 4).max(1);
        if next_width == width && next_height == height {
            return Err(NOT_RESIZED);
        }
        (width, height) = (next_width, next_height);
    }
}

/// `image` encoded as PNG, or `None` when encoding fails.
fn encode_png(image: &DynamicImage) -> Option<Vec<u8>> {
    let mut png = Vec::new();
    image
        .write_to(Cursor::new(&mut png), ImageFormat::Png)
        .ok()
        .map(|()| png)
}
