//! Pure image-attachment validation and image-only message semantics.

use std::path::Path;

/// Maximum images per message.
pub const MAX_IMAGES: usize = 8;
/// Maximum bytes per image as stored on the host and sent to the model.
pub const MAX_BYTES: u64 = 10 * 1024 * 1024;
/// Largest source file a client decodes before fitting it to the limits.
pub const MAX_SOURCE_BYTES: u64 = 64 * 1024 * 1024;
/// Longest edge a client keeps when fitting an image before upload; models
/// gain nothing from more pixels and phone photos start far larger.
pub const MAX_EDGE_PX: u32 = 2048;
/// Default per-image ceiling a device applies when it reaches the machine
/// across the internet (tunnel or relay). Adjustable on the device.
pub const DEFAULT_REMOTE_BYTES: u64 = 2 * 1024 * 1024;

/// Largest non-image file a client uploads to the host. Files are carried in
/// chunks, so this bounds the host's disk and a phone's memory, not the wire.
pub const MAX_UPLOAD_BYTES: u64 = 256 * 1024 * 1024;
/// Bytes of an upload carried by one protocol message.
pub const UPLOAD_CHUNK_BYTES: usize = 512 * 1024;

/// Whether `ext` is safe to append to a generated attachment file name.
pub fn is_safe_extension(ext: &str) -> bool {
    !ext.is_empty() && ext.bytes().all(|byte| byte.is_ascii_alphanumeric())
}

/// Best-effort MIME type from a file extension.
pub fn mime_from_path(path: &Path) -> String {
    let ext = path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| extension.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "gif" => "image/gif",
        "svg" => "image/svg+xml",
        "bmp" => "image/bmp",
        "tif" | "tiff" => "image/tiff",
        _ => "application/octet-stream",
    }
    .to_string()
}

/// Whether a file of this type goes to the model as an image; anything else
/// reaches it as a path in the message text.
pub fn is_image_attachment(mime: &str) -> bool {
    matches!(
        mime,
        "image/png" | "image/jpeg" | "image/gif" | "image/webp" | "image/tiff" | "image/bmp"
    )
}

/// The message text with attached files' paths appended, one per line. This
/// is exactly what the model reads and the timeline shows.
pub fn text_with_file_paths(text: &str, paths: &[String]) -> String {
    let paths = paths.join("\n");
    match (text.is_empty(), paths.is_empty()) {
        (_, true) => text.to_string(),
        (true, false) => paths,
        (false, false) => format!("{text}\n\n{paths}"),
    }
}

/// A rejected attachment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttachError {
    UnsupportedType { name: String },
    TooLarge { name: String },
    TooMany,
}

/// Validate one candidate attachment against the type, size, and count limits.
pub fn validate_attachment(
    name: &str,
    mime: &str,
    size: u64,
    current_count: usize,
) -> Result<(), AttachError> {
    if !is_image_attachment(mime) {
        return Err(AttachError::UnsupportedType {
            name: name.to_string(),
        });
    }
    if size > MAX_BYTES {
        return Err(AttachError::TooLarge {
            name: name.to_string(),
        });
    }
    if current_count >= MAX_IMAGES {
        return Err(AttachError::TooMany);
    }
    Ok(())
}

/// Synthetic text sent when a message carries only images.
pub const IMAGE_ONLY_MESSAGE: &str = "[User attached one or more images without additional text. Respond using the conversation context and the attached image(s).]";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attachment_admission_checks_supported_formats_and_both_limits() {
        for (name, mime) in [
            ("photo.PNG", "image/png"),
            ("photo.jpg", "image/jpeg"),
            ("photo.jpeg", "image/jpeg"),
            ("animation.gif", "image/gif"),
            ("photo.webp", "image/webp"),
            ("scan.tif", "image/tiff"),
            ("scan.tiff", "image/tiff"),
            ("photo.bmp", "image/bmp"),
        ] {
            assert_eq!(mime_from_path(Path::new(name)), mime, "{name}");
            for size in [0, MAX_BYTES] {
                assert_eq!(
                    validate_attachment(name, mime, size, MAX_IMAGES - 1),
                    Ok(()),
                    "{name}"
                );
            }
            assert_eq!(
                validate_attachment(name, mime, MAX_BYTES + 1, 0),
                Err(AttachError::TooLarge { name: name.into() })
            );
            assert_eq!(
                validate_attachment(name, mime, 1, MAX_IMAGES),
                Err(AttachError::TooMany)
            );
        }
        for name in ["drawing.svg", "notes.txt", "no-extension"] {
            assert_eq!(
                validate_attachment(name, &mime_from_path(Path::new(name)), 10, 0),
                Err(AttachError::UnsupportedType { name: name.into() })
            );
        }
    }
}
