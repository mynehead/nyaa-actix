//! Uploaded avatars: any common image, scaled and cropped to 256x256, stored as PNG.

use std::io::Cursor;
use std::path::PathBuf;

use image::{imageops::FilterType, ImageFormat, ImageReader, Limits};

use crate::config::Config;

pub const AVATAR_SIZE: u32 = 256;
/// Largest file the avatar form accepts.
pub const MAX_AVATAR_UPLOAD: usize = 4 * 1024 * 1024;
/// Bigger images are refused before decoding, so a small file can't expand into gigabytes.
const MAX_DIMENSION: u32 = 8192;

pub fn path(cfg: &Config, user_id: i32) -> PathBuf {
    PathBuf::from(&cfg.avatar_storage_path).join(format!("{}.png", user_id))
}

/// Decodes a PNG, JPEG, GIF (first frame) or WebP and returns the 256x256 PNG,
/// or a message for the user.
pub fn process(data: &[u8]) -> Result<Vec<u8>, &'static str> {
    const UNSUPPORTED: &str = "Unsupported image. Upload a PNG, JPEG, GIF or WebP file.";
    let mut reader = ImageReader::new(Cursor::new(data))
        .with_guessed_format()
        .map_err(|_| UNSUPPORTED)?;
    if !matches!(reader.format(),
        Some(ImageFormat::Png | ImageFormat::Jpeg | ImageFormat::Gif | ImageFormat::WebP)) {
        return Err(UNSUPPORTED);
    }
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_DIMENSION);
    limits.max_image_height = Some(MAX_DIMENSION);
    reader.limits(limits);
    let img = reader.decode().map_err(|e| match e {
        image::ImageError::Limits(_) => "Image is too large; at most 8192x8192 pixels.",
        _ => UNSUPPORTED,
    })?;
    let img = img.resize_to_fill(AVATAR_SIZE, AVATAR_SIZE, FilterType::Lanczos3);
    let mut out = Vec::new();
    img.write_to(&mut Cursor::new(&mut out), ImageFormat::Png).map_err(|_| UNSUPPORTED)?;
    Ok(out)
}

/// Writes the avatar next to its final name first, so readers never see half a file.
pub fn save(cfg: &Config, user_id: i32, png: &[u8]) -> std::io::Result<()> {
    let dest = path(cfg, user_id);
    std::fs::create_dir_all(&cfg.avatar_storage_path)?;
    let tmp = dest.with_extension("png.tmp");
    std::fs::write(&tmp, png)?;
    std::fs::rename(&tmp, &dest)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A `w`x`h` PNG, left half red and right half blue.
    pub fn sample_png(w: u32, h: u32) -> Vec<u8> {
        let img = image::RgbImage::from_fn(w, h, |x, _| if x < w / 2 { image::Rgb([255, 0, 0]) } else { image::Rgb([0, 0, 255]) });
        let mut out = Vec::new();
        img.write_to(&mut Cursor::new(&mut out), ImageFormat::Png).unwrap();
        out
    }

    #[test]
    fn scales_and_crops_to_256_square() {
        let png = process(&sample_png(800, 400)).unwrap();
        let img = image::load_from_memory(&png).unwrap().to_rgb8();
        assert_eq!(img.dimensions(), (256, 256));
        // Cropped around the center: both halves still show
        assert_eq!(img.get_pixel(10, 128), &image::Rgb([255, 0, 0]));
        assert_eq!(img.get_pixel(245, 128), &image::Rgb([0, 0, 255]));
    }

    #[test]
    fn rejects_non_images_and_huge_dimensions() {
        assert!(process(b"not an image").is_err());
        assert!(process(b"<svg xmlns='http://www.w3.org/2000/svg'/>").is_err());
        assert_eq!(process(&sample_png(9000, 1)), Err("Image is too large; at most 8192x8192 pixels."));
    }
}
