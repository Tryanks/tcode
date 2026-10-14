use image::{ImageDecoder, ImageFormat, Limits};
use std::{
    io::{self, BufReader, Cursor},
    path::Path,
};

/// A preview is optional; callers retain the original asset on any failure.
pub fn read(path: &Path, pixels: u32, crop_to_square: bool) -> io::Result<Vec<u8>> {
    if !(1..=1024).contains(&pixels) {
        return Err(io::Error::other(
            "Preview size is outside the supported range",
        ));
    }
    let file = std::fs::File::open(path)?;
    if file.metadata()?.len() > 64 * 1024 * 1024 {
        return Err(io::Error::other("Use the original image"));
    }
    let reader = image::ImageReader::new(BufReader::new(file)).with_guessed_format()?;
    let mut decoder: Box<dyn ImageDecoder> = match reader.format() {
        Some(ImageFormat::Png) => {
            let decoder = image::codecs::png::PngDecoder::new(reader.into_inner())
                .map_err(io::Error::other)?;
            if decoder.is_apng().map_err(io::Error::other)? {
                return Err(io::Error::other("Use the animated original"));
            }
            Box::new(decoder)
        }
        Some(ImageFormat::Jpeg) => Box::new(
            image::codecs::jpeg::JpegDecoder::new(reader.into_inner()).map_err(io::Error::other)?,
        ),
        _ => return Err(io::Error::other("Use the original image format")),
    };
    let (width, height) = decoder.dimensions();
    if width <= pixels && height <= pixels {
        return Err(io::Error::other("Original image already fits the preview"));
    }
    // RGBA32F resampling needs 16 bytes per source pixel as well as decoder storage.
    if u64::from(width) * u64::from(height) > 16 * 1024 * 1024 {
        return Err(io::Error::other(
            "Use the original image for this resolution",
        ));
    }
    let mut limits = Limits::default();
    limits.max_alloc = Some(128 * 1024 * 1024);
    decoder.set_limits(limits).map_err(io::Error::other)?;
    let orientation = decoder.orientation().map_err(io::Error::other)?;
    let mut image = image::DynamicImage::from_decoder(decoder).map_err(io::Error::other)?;
    image.apply_orientation(orientation);
    if crop_to_square {
        let side = image.width().min(image.height());
        image = image.crop_imm(
            (image.width() - side) / 2,
            (image.height() - side) / 2,
            side,
            side,
        );
    }
    resize_to_png(image, pixels)
}

pub(crate) fn resize_to_png(image: image::DynamicImage, pixels: u32) -> io::Result<Vec<u8>> {
    let mut rgba = image.into_rgba32f();
    // Average premultiplied colors so transparent pixels cannot darken the preview.
    for pixel in rgba.pixels_mut() {
        let alpha = pixel[3];
        for channel in &mut pixel.0[..3] {
            *channel *= alpha;
        }
    }
    let mut resized = image::DynamicImage::ImageRgba32F(rgba)
        .resize(pixels, pixels, image::imageops::FilterType::Lanczos3)
        .into_rgba32f();
    for pixel in resized.pixels_mut() {
        let alpha = pixel[3];
        for channel in &mut pixel.0[..3] {
            *channel = if alpha > 0. { *channel / alpha } else { 0. };
        }
    }
    let mut png = Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba32F(resized)
        .to_rgba8()
        .write_to(&mut png, ImageFormat::Png)
        .map_err(io::Error::other)?;
    Ok(png.into_inner())
}
