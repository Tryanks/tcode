//! Pure image-attachment validation and image-only message semantics.

/// Maximum images per message.
pub const MAX_IMAGES: usize = 8;
/// Maximum bytes per image (10 MiB).
pub const MAX_BYTES: u64 = 10 * 1024 * 1024;

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
    if !matches!(
        mime,
        "image/png" | "image/jpeg" | "image/gif" | "image/webp" | "image/tiff" | "image/bmp"
    ) {
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
pub fn image_only_message() -> &'static str {
    "[User attached one or more images without additional text. Respond using the conversation context and the attached image(s).]"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_a_normal_image() {
        assert!(validate_attachment("a.png", "image/png", 1_000, 0).is_ok());
    }

    #[test]
    fn rejects_non_image_type() {
        assert_eq!(
            validate_attachment("notes.txt", "text/plain", 10, 0),
            Err(AttachError::UnsupportedType {
                name: "notes.txt".into()
            })
        );
    }

    #[test]
    fn rejects_unsupported_image_type() {
        assert_eq!(
            validate_attachment("drawing.svg", "image/svg+xml", 10, 0),
            Err(AttachError::UnsupportedType {
                name: "drawing.svg".into()
            })
        );
    }

    #[test]
    fn accepts_tiff_for_transcoding() {
        assert!(validate_attachment("scan.tiff", "image/tiff", 1_000, 0).is_ok());
    }

    #[test]
    fn rejects_oversized() {
        assert_eq!(
            validate_attachment("big.png", "image/png", MAX_BYTES + 1, 0),
            Err(AttachError::TooLarge {
                name: "big.png".into()
            })
        );
        assert!(validate_attachment("edge.png", "image/png", MAX_BYTES, 0).is_ok());
    }

    #[test]
    fn rejects_over_count() {
        assert_eq!(
            validate_attachment("x.png", "image/png", 1, MAX_IMAGES),
            Err(AttachError::TooMany)
        );
    }
}
