//! Attachment presentation with core-owned validation semantics and localized errors.

use std::path::PathBuf;

use gpui::{App, ParentElement as _, Styled as _, Window, div, img, px};
use gpui_component::{ActiveTheme as _, WindowExt as _};
use tcode_core::attachments::AttachError;

/// Open an image as a window-level lightbox. The dialog lives on the Root
/// layer, so its backdrop covers the whole window and it inherits
/// backdrop-click / Escape / `x` dismissal. Shared by the composer's pending
/// strip and the chat timeline's sent-message thumbnails.
pub(crate) fn open_image_lightbox(path: PathBuf, title: String, window: &mut Window, cx: &mut App) {
    window.open_dialog(cx, move |builder, window, cx| {
        let viewport = window.viewport_size();
        let width = (viewport.width - px(160.)).clamp(px(320.), px(760.));
        let max_h = viewport.height * 0.65;
        let path = path.clone();
        builder
            .w(width)
            .rounded(crate::material::radius_overlay())
            .bg(cx.theme().popover)
            .border_1()
            .border_color(cx.theme().border)
            .shadow_xl()
            .title(title.clone())
            .content(move |content_el, _, _| {
                content_el.child(
                    div().w_full().flex().items_center().justify_center().child(
                        img(path.clone())
                            .max_w_full()
                            .max_h(max_h)
                            .rounded(crate::material::radius_card()),
                    ),
                )
            })
    });
}

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
