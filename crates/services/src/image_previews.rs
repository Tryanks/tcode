use image::{
    DynamicImage, GenericImageView, ImageDecoder, ImageFormat, RgbaImage, imageops::FilterType,
    metadata::Orientation,
};
use std::{
    io::{self, Cursor, Read},
    path::Path,
};

/// Sources past these bounds get no preview; a client opens the original only when asked to.
const MAX_SOURCE_BYTES: u64 = 256 * 1024 * 1024;
const MAX_DECODED_BYTES: u64 = 512 * 1024 * 1024;
const JPEG_QUALITY: u8 = 80;

/// A preview that fits `pixels` square, or the original when it already fits and is a still PNG
/// or JPEG.
pub fn read(path: &Path, pixels: u32, crop_to_square: bool) -> io::Result<Vec<u8>> {
    let file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    if len > MAX_SOURCE_BYTES {
        return Err(io::Error::other("Image is too large to preview"));
    }
    let mut bytes = Vec::with_capacity(len as usize);
    file.take(MAX_SOURCE_BYTES).read_to_end(&mut bytes)?;
    preview(&bytes, pixels, pixels, crop_to_square)
}

pub fn read_bytes(bytes: &[u8], width: u32, height: u32) -> io::Result<Vec<u8>> {
    if bytes.len() as u64 > MAX_SOURCE_BYTES {
        return Err(io::Error::other("Image is too large to preview"));
    }
    preview(bytes, width, height, true)
}

fn preview(bytes: &[u8], target_width: u32, target_height: u32, crop: bool) -> io::Result<Vec<u8>> {
    if !(1..=1024).contains(&target_width) || !(1..=1024).contains(&target_height) {
        return Err(io::Error::other(
            "Preview size is outside the supported range",
        ));
    }
    let reader = image::ImageReader::new(Cursor::new(bytes)).with_guessed_format()?;
    // Animated sources decode their first frame; the original carries the animation.
    let (mut decoder, still): (Box<dyn ImageDecoder>, bool) = match reader.format() {
        Some(ImageFormat::Png) => {
            let decoder = image::codecs::png::PngDecoder::new(reader.into_inner())
                .map_err(io::Error::other)?;
            let still = !decoder.is_apng().map_err(io::Error::other)?;
            (Box::new(decoder), still)
        }
        Some(ImageFormat::Jpeg) => (
            Box::new(
                image::codecs::jpeg::JpegDecoder::new(reader.into_inner())
                    .map_err(io::Error::other)?,
            ),
            true,
        ),
        _ => (
            Box::new(reader.into_decoder().map_err(io::Error::other)?),
            false,
        ),
    };
    if decoder.total_bytes() > MAX_DECODED_BYTES {
        return Err(io::Error::other("Image is too large to preview"));
    }
    let (width, height) = decoder.dimensions();
    let orientation = decoder.orientation().map_err(io::Error::other)?;
    if still
        && orientation == Orientation::NoTransforms
        && width <= target_width
        && height <= target_height
    {
        return Ok(bytes.to_vec());
    }
    let mut image = DynamicImage::from_decoder(decoder).map_err(io::Error::other)?;
    image.apply_orientation(orientation);
    let (mut x, mut y, mut width, mut height) = (0, 0, image.width(), image.height());
    if crop {
        if u64::from(width) * u64::from(target_height) > u64::from(height) * u64::from(target_width)
        {
            let cropped = ((u64::from(height) * u64::from(target_width)) / u64::from(target_height))
                .max(1) as u32;
            x = (width - cropped) / 2;
            width = cropped;
        } else {
            let cropped = ((u64::from(width) * u64::from(target_height)) / u64::from(target_width))
                .max(1) as u32;
            y = (height - cropped) / 2;
            height = cropped;
        }
    }
    if image.color().has_alpha() {
        let mut rgba = image.into_rgba8();
        premultiply(&mut rgba);
        let mut rgba = scale(
            &*image::imageops::crop_imm(&rgba, x, y, width, height),
            target_width,
            target_height,
        );
        unpremultiply(&mut rgba);
        if rgba.pixels().any(|pixel| pixel[3] < u8::MAX) {
            return encode_png(rgba);
        }
        encode_jpeg(DynamicImage::ImageRgba8(rgba).into_rgb8())
    } else {
        let rgb = image.into_rgb8();
        encode_jpeg(scale(
            &*image::imageops::crop_imm(&rgb, x, y, width, height),
            target_width,
            target_height,
        ))
    }
}

/// Scales `image` down to fit `width`×`height`, never up. A box filter first brings a large
/// source near twice the target, so the Lanczos pass and its float rows stay small.
fn scale<P: image::Pixel<Subpixel = u8> + 'static>(
    image: &impl GenericImageView<Pixel = P>,
    width: u32,
    height: u32,
) -> image::ImageBuffer<P, Vec<u8>> {
    let fit = |bound_width: u32, bound_height: u32| {
        let (source_width, source_height) = image.dimensions();
        let ratio = f64::min(
            f64::from(bound_width) / f64::from(source_width),
            f64::from(bound_height) / f64::from(source_height),
        )
        .min(1.);
        (
            ((f64::from(source_width) * ratio).round() as u32).max(1),
            ((f64::from(source_height) * ratio).round() as u32).max(1),
        )
    };
    let (box_width, box_height) = fit(width.saturating_mul(2), height.saturating_mul(2));
    let (final_width, final_height) = fit(width, height);
    if (box_width, box_height) == image.dimensions() {
        return image::imageops::resize(image, final_width, final_height, FilterType::Lanczos3);
    }
    let reduced = image::imageops::thumbnail(image, box_width, box_height);
    image::imageops::resize(&reduced, final_width, final_height, FilterType::Lanczos3)
}

/// Averaging premultiplied colours keeps transparent pixels from darkening the edges they border.
fn premultiply(image: &mut RgbaImage) {
    for pixel in image.pixels_mut() {
        let alpha = u16::from(pixel[3]);
        for channel in &mut pixel.0[..3] {
            *channel = ((u16::from(*channel) * alpha + 127) / 255) as u8;
        }
    }
}

fn unpremultiply(image: &mut RgbaImage) {
    for pixel in image.pixels_mut() {
        let alpha = u16::from(pixel[3]);
        for channel in &mut pixel.0[..3] {
            *channel = match alpha {
                0 => 0,
                alpha => ((u16::from(*channel) * 255 + alpha / 2) / alpha).min(255) as u8,
            };
        }
    }
}

fn encode_png(image: RgbaImage) -> io::Result<Vec<u8>> {
    let mut png = Cursor::new(Vec::new());
    image
        .write_to(&mut png, ImageFormat::Png)
        .map_err(io::Error::other)?;
    Ok(png.into_inner())
}

fn encode_jpeg(image: image::RgbImage) -> io::Result<Vec<u8>> {
    let mut jpeg = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, JPEG_QUALITY)
        .encode_image(&image)
        .map_err(io::Error::other)?;
    Ok(jpeg)
}

pub(crate) fn resize_to_png(image: DynamicImage, pixels: u32) -> io::Result<Vec<u8>> {
    let mut rgba = image.into_rgba8();
    premultiply(&mut rgba);
    let mut rgba = scale(&rgba, pixels, pixels);
    unpremultiply(&mut rgba);
    encode_png(rgba)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encoded(image: impl Into<DynamicImage>, format: ImageFormat) -> Vec<u8> {
        let mut out = Cursor::new(Vec::new());
        image.into().write_to(&mut out, format).unwrap();
        out.into_inner()
    }

    fn attachment_preview(bytes: &[u8], pixels: u32, crop_to_square: bool) -> Vec<u8> {
        let path = std::env::temp_dir().join(format!("tcode-preview-{}", uuid::Uuid::new_v4()));
        std::fs::write(&path, bytes).unwrap();
        let preview = read(&path, pixels, crop_to_square);
        std::fs::remove_file(&path).unwrap();
        preview.unwrap()
    }

    /// Camera-resolution sources are previewed rather than handed to the client in full.
    #[test]
    fn sources_over_sixteen_megapixels_get_a_preview() {
        let source = encoded(image::GrayImage::new(4100, 4100), ImageFormat::Png);
        let preview = attachment_preview(&source, 360, true);
        let decoded = image::load_from_memory(&preview).unwrap();
        assert_eq!(decoded.dimensions(), (360, 360));
    }

    /// An opaque photo crosses the link as a JPEG, smaller than the PNG previews it replaces.
    #[test]
    fn opaque_sources_are_previewed_as_jpeg() {
        let photo = image::RgbImage::from_fn(1600, 1000, |x, y| {
            let noise = (x.wrapping_mul(2_654_435_761) ^ y.wrapping_mul(40_503)) % 32;
            image::Rgb([
                (x * 200 / 1600 + noise) as u8,
                (y * 200 / 1000 + noise) as u8,
                (120 + noise) as u8,
            ])
        });
        let preview = attachment_preview(&encoded(photo, ImageFormat::Jpeg), 512, false);
        assert_eq!(image::guess_format(&preview).unwrap(), ImageFormat::Jpeg);
        let decoded = image::load_from_memory(&preview).unwrap();
        assert_eq!(decoded.dimensions(), (512, 320));
        assert!(preview.len() < encoded(decoded.into_rgba8(), ImageFormat::Png).len());
    }

    /// Transparency survives the preview, and an original that already fits is the preview.
    #[test]
    fn transparent_sources_stay_png() {
        let source = encoded(
            image::RgbaImage::from_fn(1600, 800, |x, _| {
                image::Rgba([255, 0, 0, if x < 800 { 255 } else { 0 }])
            }),
            ImageFormat::Png,
        );
        let preview = attachment_preview(&source, 240, false);
        assert_eq!(image::guess_format(&preview).unwrap(), ImageFormat::Png);
        let decoded = image::load_from_memory(&preview).unwrap().into_rgba8();
        assert_eq!(decoded.dimensions(), (240, 120));
        assert_eq!(decoded.get_pixel(0, 0).0, [255, 0, 0, 255]);
        assert_eq!(decoded.get_pixel(239, 0)[3], 0);
        let small = encoded(image::RgbaImage::new(200, 100), ImageFormat::Png);
        assert_eq!(attachment_preview(&small, 240, false), small);
    }
}
