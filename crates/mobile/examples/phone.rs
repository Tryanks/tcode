use gpui::{px, size};
use std::rc::Rc;
fn main() {
    let android = std::env::args().any(|arg| arg == "--android");
    gpui_platform::application()
        .with_assets(tcode_ui::assets::Assets)
        .run(move |cx| {
            let dimensions = if android {
                size(px(412.), px(915.))
            } else {
                size(px(393.), px(852.))
            };
            tcode_mobile::run_with_size(
                cx,
                Rc::new(tcode_mobile::host::NativeHost::from_env()),
                dimensions,
            );
        });
}
