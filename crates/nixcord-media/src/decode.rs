//! Decode + downscale. Runs on a blocking thread; full-size pixels never leave this function.

use crate::{size, Failure, Image};
use std::io::Cursor;

/// Decoder limits: refuse absurd dimensions and cap the decoder's working memory.
const MAX_DIM: u32 = 8192;
const MAX_ALLOC: u64 = 64 * 1024 * 1024;

/// Decode png/jpeg/webp/gif (first frame) and shrink it to fit `max_w` x `max_h` pixels.
pub fn decode(bytes: &[u8], max_w: u32, max_h: u32) -> Result<Image, Failure> {
    let mut reader = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|_| Failure::Decode)?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_DIM);
    limits.max_image_height = Some(MAX_DIM);
    limits.max_alloc = Some(MAX_ALLOC);
    reader.limits(limits);
    let img = reader.decode().map_err(|e| {
        tracing::debug!("image decode failed: {e}");
        Failure::Decode
    })?;
    let (w, h) = size::fit_within((img.width(), img.height()), (max_w, max_h));
    // Resize in the native color type first, then expand the (small) result to RGBA.
    let img = if (w, h) != (img.width(), img.height()) {
        img.thumbnail_exact(w, h)
    } else {
        img
    };
    let rgba = img.into_rgba8();
    Ok(Image {
        width: rgba.width(),
        height: rgba.height(),
        rgba: rgba.into_raw(),
    })
}

/// Encode RGBA pixels as PNG (saving generated demo images).
pub fn encode_png(img: &Image) -> Option<Vec<u8>> {
    let buf = image::RgbaImage::from_raw(img.width, img.height, img.rgba.clone())?;
    let mut out = Vec::new();
    image::DynamicImage::ImageRgba8(buf)
        .write_to(&mut Cursor::new(&mut out), image::ImageFormat::Png)
        .ok()?;
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(w: u32, h: u32) -> Vec<u8> {
        let img = image::RgbaImage::from_fn(w, h, |x, y| image::Rgba([x as u8, y as u8, 0, 255]));
        let mut out = Vec::new();
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut Cursor::new(&mut out), image::ImageFormat::Png)
            .unwrap();
        out
    }

    #[test]
    fn downscales_to_box() {
        let img = decode(&png(200, 100), 50, 50).unwrap();
        assert_eq!((img.width, img.height), (50, 25));
        assert_eq!(img.rgba.len(), 50 * 25 * 4);
    }

    #[test]
    fn never_upscales() {
        let img = decode(&png(20, 10), 64, 64).unwrap();
        assert_eq!((img.width, img.height), (20, 10));
    }

    #[test]
    fn png_round_trip() {
        let src = decode(&png(12, 7), 64, 64).unwrap();
        let bytes = encode_png(&src).unwrap();
        assert_eq!(decode(&bytes, 64, 64).unwrap(), src);
    }

    #[test]
    fn garbage_fails() {
        assert_eq!(decode(b"not an image", 10, 10), Err(Failure::Decode));
    }
}
