use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use image::ImageFormat;
use image::codecs::png::{CompressionType, FilterType as PngFilter, PngEncoder};
use serde_json::Value;
use std::io::Cursor;

/// Default cap on an image's long edge, in pixels. Claude bills images by
/// pixel area (~width×height/750 tokens), not by bytes, so shrinking the
/// dimensions is the only thing that saves tokens. 1280px keeps UI text
/// readable on a Retina screenshot (≈0.9× its logical size) while cutting a
/// full-size screenshot roughly in half versus what the API would otherwise
/// accept. `SCHLIFFE_IMAGE_MAX_EDGE` overrides it; `0` turns resizing off.
const DEFAULT_MAX_EDGE: u32 = 1280;

pub fn max_edge() -> u32 {
    std::env::var("SCHLIFFE_IMAGE_MAX_EDGE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_MAX_EDGE)
}

/// Tokens an image is actually billed for: Anthropic's approximation
/// (width×height/750), after the API's own downscale — images whose long
/// edge exceeds 1568px or that exceed ~1.15 megapixels are resized by the
/// API before the model sees them. Counting the raw size instead would
/// credit Schliffe with savings the API was going to make anyway.
pub fn image_tokens(width: u32, height: u32) -> u64 {
    let (w, h) = (width as f64, height as f64);
    let scale = 1f64
        .min(1568.0 / w.max(h))
        .min((1_150_000.0 / (w * h)).sqrt());
    ((w * scale).round() * (h * scale).round() / 750.0) as u64
}

/// One resized image: tokens before and after.
pub struct Shrunk {
    pub tokens_before: u64,
    pub tokens_after: u64,
}

/// Walks a tool result and shrinks every embedded base64 image whose long
/// edge exceeds `max_edge`, in place. Tool results carry images in a few
/// shapes (MCP: `{"type":"image","data":..,"mimeType":..}`; Claude Code's
/// Read: base64 under a nested object), so this looks for any object with a
/// long base64 string in `data`/`base64` next to an image media type — and
/// otherwise leaves the value alone. Keeps the original format (PNG stays
/// PNG, JPEG stays JPEG); formats it can't re-encode are left untouched.
pub fn shrink_images(value: &mut Value, max_edge: u32, out: &mut Vec<Shrunk>) {
    if max_edge == 0 {
        return;
    }
    match value {
        Value::Array(items) => {
            for item in items {
                shrink_images(item, max_edge, out);
            }
        }
        Value::Object(map) => {
            let media_type = ["mimeType", "media_type", "mediaType", "type"]
                .iter()
                .find_map(|k| map.get(*k).and_then(Value::as_str))
                .filter(|t| t.starts_with("image/"))
                .map(str::to_string);
            for key in ["data", "base64"] {
                let Some(Value::String(b64)) = map.get(key) else {
                    continue;
                };
                if b64.len() < 1024 {
                    continue;
                }
                if let Some((new_b64, shrunk)) = shrink_one(b64, media_type.as_deref(), max_edge) {
                    map.insert(key.to_string(), Value::String(new_b64));
                    out.push(shrunk);
                }
            }
            for (_, v) in map.iter_mut() {
                if v.is_object() || v.is_array() {
                    shrink_images(v, max_edge, out);
                }
            }
        }
        _ => {}
    }
}

/// Largest image Schliffe will decode, in bytes of pixel buffer (~256 MB,
/// comfortably above any real screenshot).
const MAX_DECODED_BYTES: u64 = 256 * 1024 * 1024;

/// Decodes with an explicit memory ceiling, so a "decompression bomb" — a
/// 1 MB PNG declaring 20000×20000, which expands to 1.2 GB — is refused
/// instead of exhausting RAM. The `image` crate happens to apply a default
/// limit today, but a security property must not depend on a dependency's
/// default: this states it, and `bomb_is_refused` locks it in.
fn decode_bounded(bytes: &[u8], format: ImageFormat) -> Option<image::DynamicImage> {
    let mut reader = image::ImageReader::with_format(Cursor::new(bytes), format);
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(MAX_DECODED_BYTES);
    reader.limits(limits);
    reader.decode().ok()
}

fn shrink_one(b64: &str, media_type: Option<&str>, max_edge: u32) -> Option<(String, Shrunk)> {
    let bytes = STANDARD.decode(b64).ok()?;
    let format = match media_type {
        Some(t) => ImageFormat::from_mime_type(t)?,
        None => image::guess_format(&bytes).ok()?,
    };
    if !matches!(format, ImageFormat::Png | ImageFormat::Jpeg) {
        return None; // re-encoding would change the declared media type
    }
    let img = decode_bounded(&bytes, format)?;
    let (w, h) = (img.width(), img.height());
    if w.max(h) <= max_edge {
        return None;
    }
    let resized = img.thumbnail(max_edge, max_edge);
    let mut buf = Cursor::new(Vec::new());
    if format == ImageFormat::Png {
        // Fast compression: the hook runs synchronously after the tool call,
        // and the model is billed by pixels, not bytes.
        let encoder =
            PngEncoder::new_with_quality(&mut buf, CompressionType::Fast, PngFilter::Adaptive);
        resized.write_with_encoder(encoder).ok()?;
    } else {
        resized.write_to(&mut buf, format).ok()?;
    }
    let shrunk = Shrunk {
        tokens_before: image_tokens(w, h),
        tokens_after: image_tokens(resized.width(), resized.height()),
    };
    Some((STANDARD.encode(buf.into_inner()), shrunk))
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use image::{ImageBuffer, Rgb};
    use serde_json::json;

    pub fn png_b64(w: u32, h: u32) -> String {
        let img = ImageBuffer::from_fn(w, h, |x, y| Rgb([(x % 256) as u8, (y % 256) as u8, 90]));
        let mut buf = Cursor::new(Vec::new());
        img.write_to(&mut buf, ImageFormat::Png).unwrap();
        STANDARD.encode(buf.into_inner())
    }

    fn dims(b64: &str) -> (u32, u32) {
        let img = image::load_from_memory(&STANDARD.decode(b64).unwrap()).unwrap();
        (img.width(), img.height())
    }

    #[test]
    fn shrinks_large_mcp_image_keeping_aspect() {
        let mut v = json!([{"type": "image", "data": png_b64(960, 600), "mimeType": "image/png"}]);
        let mut out = Vec::new();
        shrink_images(&mut v, 320, &mut out);
        assert_eq!(dims(v[0]["data"].as_str().unwrap()), (320, 200));
        assert_eq!(out.len(), 1);
        assert!(out[0].tokens_after * 3 < out[0].tokens_before);
    }

    #[test]
    fn billed_tokens_account_for_the_api_downscale() {
        // A Retina screenshot is billed at the API's cap, not its raw size.
        let retina = image_tokens(2940, 1912);
        assert!((1400..=1600).contains(&retina), "{retina}");
        assert_eq!(image_tokens(750, 100), 100); // small: exact
    }

    #[test]
    fn small_images_and_disabled_cap_are_untouched() {
        let small = png_b64(300, 200);
        let mut v = json!({"type": "image", "data": small.clone(), "mimeType": "image/png"});
        let mut out = Vec::new();
        shrink_images(&mut v, 320, &mut out);
        assert_eq!(v["data"], small);
        let big = png_b64(800, 400);
        let mut v = json!({"data": big.clone(), "mimeType": "image/png"});
        shrink_images(&mut v, 0, &mut out);
        assert_eq!(v["data"], big);
        assert!(out.is_empty());
    }

    /// A "decompression bomb": a tiny file declaring huge dimensions. It
    /// must be refused quickly, with the image left untouched, rather than
    /// allocating the ~4.8 GB it asks for.
    #[test]
    fn decompression_bomb_is_refused() {
        use std::io::Write;
        let (w, h) = (40_000u32, 40_000u32);
        let chunk = |kind: &[u8], data: &[u8]| {
            let mut out = (data.len() as u32).to_be_bytes().to_vec();
            out.extend_from_slice(kind);
            out.extend_from_slice(data);
            let mut crc = crc32(kind);
            crc = crc32_continue(crc, data);
            out.extend_from_slice(&crc.to_be_bytes());
            out
        };
        let mut ihdr = Vec::new();
        ihdr.write_all(&w.to_be_bytes()).unwrap();
        ihdr.write_all(&h.to_be_bytes()).unwrap();
        ihdr.write_all(&[8, 2, 0, 0, 0]).unwrap();
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        png.extend_from_slice(&chunk(b"IHDR", &ihdr));
        png.extend_from_slice(&chunk(b"IDAT", &[0x78, 0x9c, 0x03, 0x00, 0, 0, 0, 1]));
        png.extend_from_slice(&chunk(b"IEND", b""));
        assert!(png.len() < 4096, "the bomb must be small: {}", png.len());

        let started = std::time::Instant::now();
        assert!(decode_bounded(&png, ImageFormat::Png).is_none());
        assert!(started.elapsed().as_secs() < 5);

        // End to end: the block is left exactly as it came in.
        let mut v =
            json!({"type": "image", "data": STANDARD.encode(&png), "mimeType": "image/png"});
        let before = v.clone();
        let mut out = Vec::new();
        shrink_images(&mut v, 1280, &mut out);
        assert_eq!(v, before);
        assert!(out.is_empty());
    }

    // Minimal CRC-32 so the fixture is a byte-valid PNG (the decoder must
    // refuse it on size, not on a malformed chunk).
    fn crc32(data: &[u8]) -> u32 {
        crc32_continue(0, data)
    }
    fn crc32_continue(prev: u32, data: &[u8]) -> u32 {
        let mut c = prev ^ 0xffff_ffff;
        for &b in data {
            c ^= b as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 {
                    0xedb8_8320 ^ (c >> 1)
                } else {
                    c >> 1
                };
            }
        }
        c ^ 0xffff_ffff
    }

    #[test]
    fn nested_base64_without_mime_is_detected() {
        let mut v =
            json!({"type": "image", "file": {"base64": png_b64(900, 300), "type": "image/png"}});
        let mut out = Vec::new();
        shrink_images(&mut v, 300, &mut out);
        assert_eq!(dims(v["file"]["base64"].as_str().unwrap()), (300, 100));
    }
}
