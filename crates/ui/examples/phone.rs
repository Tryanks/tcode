//! The compact layout in a desktop window, at phone geometry.
//!
//! Nothing here is phone-specific: it opens the same `run_shell` every client
//! opens, at a width below the compact breakpoint, so the desktop can review
//! the compact layout without a device.
//!
//! ```sh
//! cargo run -p tcode-ui --example phone            # 393×852
//! cargo run -p tcode-ui --example phone -- --android  # 412×915
//! ```

use std::borrow::Cow;
use std::rc::Rc;

use gpui::{Bounds, WindowBackgroundAppearance, WindowBounds, WindowOptions, point, px, size};
use tcode_client::host::ClientHost;
use tcode_ui::{ShellOptions, ShellSetup, WindowSeam};

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
            let host: Rc<dyn ClientHost> = Rc::new(tcode_remote::NativeClientHost::from_env());
            tcode_ui::run_shell(
                cx,
                host.clone(),
                // A desktop window stands in for the device: no notch, no
                // software keyboard.
                WindowSeam::flush(),
                ShellOptions {
                    window: WindowOptions {
                        window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                            point(px(0.), px(0.)),
                            dimensions,
                        ))),
                        titlebar: None,
                        window_background: WindowBackgroundAppearance::Opaque,
                        ..Default::default()
                    },
                    title: "Tcode phone".into(),
                    theme_json: Cow::Owned(tcode_ui::flattened_theme_json()),
                    activate: true,
                    setup: ShellSetup {
                        initial: tcode_ui::last_host_target(host.as_ref()),
                        initial_pairing_error: None,
                        client_host: Some(host),
                        local: None,
                        seed_blocking: false,
                        restore_navigation: true,
                    },
                    ..Default::default()
                },
            );
        });
}
