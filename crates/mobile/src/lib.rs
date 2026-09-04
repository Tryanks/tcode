//! Shared placeholder shell for the first native-mobile bring-up.

use gpui::{
    App, AppContext as _, Bounds, Context, IntoElement, ParentElement as _, Render, Styled as _,
    Window, WindowBounds, WindowOptions, div, point, px, rgb, size,
};

struct MobileRoot;

impl Render for MobileRoot {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        #[cfg(target_os = "ios")]
        let safe = gpui_ios::safe_area();
        #[cfg(target_os = "android")]
        let safe = gpui_android::safe_area();
        #[cfg(not(any(target_os = "ios", target_os = "android")))]
        let safe: gpui::Edges<gpui::Pixels> = gpui::Edges::default();

        div()
            .size_full()
            .bg(rgb(0x111318))
            .text_color(rgb(0xf2f3f5))
            .pt(safe.top)
            .pr(safe.right)
            .pb(safe.bottom)
            .pl(safe.left)
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_2()
            .child(div().text_3xl().child("tcode"))
            .child(
                div()
                    .text_sm()
                    .text_color(rgb(0x9298a3))
                    .child("remote client"),
            )
    }
}

/// Opens the single native mobile window.
pub fn run(cx: &mut App) {
    cx.activate(true);
    let bounds = Bounds::new(point(px(0.0), px(0.0)), size(px(402.0), px(874.0)));
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        ..Default::default()
    };
    cx.open_window(options, |_window, cx| cx.new(|_| MobileRoot))
        .expect("failed to open tcode mobile window");
}
