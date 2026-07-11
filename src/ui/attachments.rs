//! Image-attachment validation, limits, and the image-only synthetic message.
//!
//! Mirrors T3's composer attachment rules exactly (S1 §7): at most 8 images,
//! each at most 10 MiB, `image/*` MIME only, with T3's verbatim error strings.

/// Maximum images per message (T3: `MAX_ATTACHMENTS`).
pub const MAX_IMAGES: usize = 8;
/// Maximum bytes per image (T3: 10 MiB).
pub const MAX_BYTES: u64 = 10 * 1024 * 1024;

/// A rejected attachment, carrying the exact T3 user-facing reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttachError {
    /// Non-image MIME type.
    UnsupportedType { name: String },
    /// Over the 10 MiB per-file limit.
    TooLarge { name: String },
    /// Would exceed the 8-image count limit.
    TooMany,
}

impl AttachError {
    /// The localized (English-default) message, matching T3's copy verbatim.
    pub fn message(&self) -> String {
        match self {
            AttachError::UnsupportedType { name } => {
                rust_i18n::t!("attach.unsupported_type", name = name).into_owned()
            }
            AttachError::TooLarge { name } => {
                rust_i18n::t!("attach.too_large", name = name).into_owned()
            }
            AttachError::TooMany => rust_i18n::t!("attach.too_many").into_owned(),
        }
    }
}

/// Validate one candidate attachment against the type/size/count limits.
/// `current_count` is how many images are already attached.
pub fn validate_attachment(
    name: &str,
    mime: &str,
    size: u64,
    current_count: usize,
) -> Result<(), AttachError> {
    if !mime.starts_with("image/") {
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

/// The exact synthetic text sent when a message carries only images (T3
/// verbatim).
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
    fn rejects_oversized() {
        assert_eq!(
            validate_attachment("big.png", "image/png", MAX_BYTES + 1, 0),
            Err(AttachError::TooLarge {
                name: "big.png".into()
            })
        );
        // Exactly at the limit is allowed.
        assert!(validate_attachment("edge.png", "image/png", MAX_BYTES, 0).is_ok());
    }

    #[test]
    fn rejects_over_count() {
        assert_eq!(
            validate_attachment("x.png", "image/png", 1, MAX_IMAGES),
            Err(AttachError::TooMany)
        );
    }

    #[test]
    fn error_copy_is_t3_verbatim() {
        assert_eq!(
            AttachError::UnsupportedType {
                name: "notes.txt".into()
            }
            .message(),
            "Unsupported file type for 'notes.txt'. Please attach image files only."
        );
        assert_eq!(
            AttachError::TooLarge {
                name: "big.png".into()
            }
            .message(),
            "'big.png' exceeds the 10MB attachment limit."
        );
        assert_eq!(
            AttachError::TooMany.message(),
            "You can attach up to 8 images per message."
        );
    }
}
