//! Project defaults and bounded image decoding for the host-owned icon picker.
use image::{ImageFormat, ImageReader, Limits};
use serde::Deserialize;
use std::{
    fs,
    io::{self, Cursor, Read},
    path::Path,
};
use tcode_core::project::Project;
use tcode_protocol::{PathEntry, QueryResponse};

const MAX_BYTES: u64 = 8 * 1024 * 1024;
const ICON_SIZE: u32 = 128;

fn supported(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| {
            matches!(
                ext.to_ascii_lowercase().as_str(),
                "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "tif" | "tiff" | "ico"
            )
        })
}

/// List one directory, including folders for navigation and supported images only.
pub fn browse(directory: &Path) -> io::Result<QueryResponse> {
    let directory = directory.canonicalize()?;
    let mut entries = Vec::new();
    for entry in fs::read_dir(&directory)? {
        let entry = entry?;
        let Ok(metadata) = fs::metadata(entry.path()) else {
            continue;
        };
        let is_dir = metadata.is_dir();
        if !is_dir && (!metadata.is_file() || !supported(&entry.path())) {
            continue;
        }
        entries.push(PathEntry::from_rel(
            entry.file_name().to_string_lossy().into_owned(),
            is_dir,
        ));
    }
    entries.sort_by_cached_key(|entry| (!entry.is_dir, entry.basename.to_lowercase()));
    Ok(QueryResponse::IconImages {
        parent: directory.parent().map(Path::to_path_buf),
        directory,
        entries,
    })
}

fn read_bounded(path: &Path, limit: u64) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take(limit + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(io::Error::other("image exceeds the 8 MiB size limit"));
    }
    Ok(bytes)
}

fn decode(bytes: &[u8], max_dimension: u32) -> io::Result<image::DynamicImage> {
    let mut reader = ImageReader::new(Cursor::new(bytes)).with_guessed_format()?;
    let mut limits = Limits::default();
    limits.max_image_width = Some(max_dimension);
    limits.max_image_height = Some(max_dimension);
    limits.max_alloc = Some(128 * 1024 * 1024);
    reader.limits(limits);
    reader.decode().map_err(io::Error::other)
}

/// Decode away from the host event loop and send only a small, static PNG to clients.
pub fn thumbnail(path: &Path) -> io::Result<Vec<u8>> {
    let mut rgba = decode(&read_bounded(path, MAX_BYTES)?, 8192)?.into_rgba32f();
    // Average premultiplied colors so transparent pixels cannot darken the preview.
    for pixel in rgba.pixels_mut() {
        let alpha = pixel[3];
        for channel in &mut pixel.0[..3] {
            *channel *= alpha;
        }
    }
    let mut resized = image::DynamicImage::ImageRgba32F(rgba)
        .resize(ICON_SIZE, ICON_SIZE, image::imageops::FilterType::Lanczos3)
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

pub fn read_project_icon(project: &Project) -> io::Result<Vec<u8>> {
    if let Some(path) = &project.icon_path {
        return thumbnail(path);
    }
    #[derive(Deserialize)]
    struct Config {
        #[serde(rename = "iconPath")]
        icon_path: Option<String>,
    }
    let bytes = match read_bounded(&project.root.join("tcode.json"), 1024 * 1024) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            read_bounded(&project.root.join("t3.json"), 1024 * 1024)?
        }
        result => result?,
    };
    let config: Config = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
    let path = config
        .icon_path
        .filter(|path| !path.trim().is_empty())
        .ok_or_else(|| io::Error::other("project has no iconPath"))?;
    thumbnail(&project.root.join(path))
}

/// Accept only the small PNG produced by the picker, including on remote writes.
pub fn save_override(path: &Path, png: &[u8]) -> io::Result<()> {
    if png.len() > 128 * 1024 || image::guess_format(png).ok() != Some(ImageFormat::Png) {
        return Err(io::Error::other(
            "project icon must be a PNG of at most 128 KiB",
        ));
    }
    decode(png, ICON_SIZE)?;
    fs::create_dir_all(path.parent().expect("managed icon has a parent"))?;
    fs::write(path, png)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thumbnail_preserves_color_at_transparent_edges() {
        let root = std::env::temp_dir().join(format!("tcode-icons-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let path = root.join("edge.png");
        // The edge crosses a thumbnail pixel; transparent black must not darken red.
        image::RgbaImage::from_fn(512, 256, |x, _| {
            if x < 242 {
                image::Rgba([255, 0, 0, 255])
            } else {
                image::Rgba([0, 0, 0, 0])
            }
        })
        .save(&path)
        .unwrap();
        let png = thumbnail(&path).unwrap();
        fs::remove_dir_all(root).unwrap();
        let resized = image::load_from_memory(&png).unwrap().into_rgba8();
        assert_eq!(resized.dimensions(), (128, 64));
        let edge = resized.get_pixel(60, 32);
        assert!(edge[3] > 0 && edge[3] < 255);
        assert_eq!(&edge.0[..3], &[255, 0, 0]);
    }

    #[test]
    fn tcode_config_takes_precedence_over_t3_config() {
        let root = std::env::temp_dir().join(format!("tcode-icons-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        for (name, color) in [("red", [255, 0, 0, 255]), ("blue", [0, 0, 255, 255])] {
            image::RgbaImage::from_pixel(16, 16, image::Rgba(color))
                .save(root.join(format!("{name}.png")))
                .unwrap();
        }
        let project = Project::from_root(root.clone());
        fs::write(root.join("tcode.json"), r#"{"iconPath":"red.png"}"#).unwrap();
        let expected = thumbnail(&root.join("red.png")).unwrap();
        assert_eq!(read_project_icon(&project).unwrap(), expected);

        fs::write(root.join("t3.json"), r#"{"iconPath":"blue.png"}"#).unwrap();
        assert_eq!(read_project_icon(&project).unwrap(), expected);
        fs::write(root.join("tcode.json"), "broken json").unwrap();
        assert!(read_project_icon(&project).is_err());

        fs::remove_file(root.join("tcode.json")).unwrap();
        assert_eq!(
            read_project_icon(&project).unwrap(),
            thumbnail(&root.join("blue.png")).unwrap()
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn picker_filters_files_and_project_defaults_yield_to_custom_images() {
        let root = std::env::temp_dir().join(format!("tcode-icons-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(root.join("assets")).unwrap();
        let sample = include_bytes!("../../../assets/icons/app/tcode.png");
        fs::write(root.join("assets/logo.PNG"), sample).unwrap();
        fs::write(root.join("notes.txt"), "notes").unwrap();
        fs::write(
            root.join("t3.json"),
            r#"{"iconPath":"assets/logo.PNG","scripts":[{"command":"ignored"}]}"#,
        )
        .unwrap();
        let QueryResponse::IconImages { entries, .. } = browse(&root).unwrap() else {
            panic!()
        };
        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.basename.as_str())
                .collect::<Vec<_>>(),
            ["assets"]
        );
        let QueryResponse::IconImages { entries, .. } = browse(&root.join("assets")).unwrap()
        else {
            panic!()
        };
        assert_eq!(entries[0].basename, "logo.PNG");
        let mut project = Project::from_root(root.clone());
        let png = read_project_icon(&project).unwrap();
        let decoded = image::load_from_memory(&png).unwrap();
        assert!(decoded.width() <= 128 && decoded.height() <= 128);
        let custom = root.join("custom.png");
        save_override(&custom, &png).unwrap();
        project.icon_path = Some(custom);
        fs::write(root.join("t3.json"), "broken json").unwrap();
        assert!(read_project_icon(&project).is_ok());
        project.icon_path = None;
        assert!(read_project_icon(&project).is_err());
        fs::write(root.join("t3.json"), r#"{"iconPath":"missing.png"}"#).unwrap();
        assert!(read_project_icon(&project).is_err());
        assert!(save_override(&root.join("bad.png"), b"not an image").is_err());
        fs::remove_dir_all(root).unwrap();
    }
}
