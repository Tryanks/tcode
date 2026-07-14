//! Attachment presentation with core-owned validation semantics and localized errors.

use tcode_core::attachments::AttachError;

pub(crate) fn attach_error_message(error: &AttachError) -> String {
    match error {
        AttachError::UnsupportedType { name } => {
            tcode_i18n::tr!("attach.unsupported_type", name = name).into_owned()
        }
        AttachError::TooLarge { name } => {
            tcode_i18n::tr!("attach.too_large", name = name).into_owned()
        }
        AttachError::TooMany => tcode_i18n::tr!("attach.too_many").into_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_copy_is_t3_verbatim() {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        assert_eq!(
            attach_error_message(&AttachError::UnsupportedType {
                name: "notes.txt".into()
            }),
            "Unsupported file type for 'notes.txt'. Please attach image files only."
        );
        assert_eq!(
            attach_error_message(&AttachError::TooLarge {
                name: "big.png".into()
            }),
            "'big.png' exceeds the 10MB attachment limit."
        );
        assert_eq!(
            attach_error_message(&AttachError::TooMany),
            "You can attach up to 8 images per message."
        );
    }
}
