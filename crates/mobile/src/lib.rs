//! The tcode phone client (see docs/mobile-design.md).

pub mod host;

use std::rc::Rc;

use crate::host::SharedHost;

use gpui::{
    App, AppContext as _, Bounds, Context, IntoElement, ParentElement as _, Render, Styled as _,
    Window, WindowBounds, WindowOptions, div, point, px, rgb, size,
};

struct MobileRoot {
    host: SharedHost,
}

impl Render for MobileRoot {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let safe = self.host.safe_area();

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

/// Opens the single mobile window with the native platform seam.
#[cfg(feature = "native")]
pub fn run(cx: &mut App) {
    run_with_host(cx, Rc::new(host::NativeHost::from_env()));
}

/// Opens the single mobile window with a caller-supplied platform seam.
pub fn run_with_host(cx: &mut App, host: SharedHost) {
    cx.activate(true);
    let bounds = Bounds::new(point(px(0.0), px(0.0)), size(px(402.0), px(874.0)));
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        ..Default::default()
    };
    cx.open_window(options, |_window, cx| cx.new(|_| MobileRoot { host }))
        .expect("failed to open tcode mobile window");
}
