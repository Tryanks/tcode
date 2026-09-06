//! GPUI application lifetime and the `tcode_ios_start` entry point.

use std::borrow::Cow;
use std::cell::OnceCell;
use std::rc::Rc;

use gpui::{Application, ApplicationHandle, WindowBackgroundAppearance, WindowOptions};
use tcode_client::host::ClientHost;
use tcode_ui::{ShellOptions, ShellSetup, WindowSeam};

thread_local! {
    static APPLICATION: OnceCell<ApplicationHandle> = const { OnceCell::new() };
}

#[unsafe(no_mangle)]
pub extern "C" fn tcode_ios_start() {
    APPLICATION.with(|slot| {
        if slot.get().is_some() {
            return;
        }
        std::panic::set_hook(Box::new(|panic| log::error!("GPUI iOS panic: {panic}")));
        let handle = Application::with_platform(gpui_ios::platform())
            .with_assets(tcode_ui::assets::Assets)
            .run_embedded(|cx| {
                let host: Rc<dyn ClientHost> = Rc::new(crate::host::native_host());
                tcode_ui::run_shell(
                    cx,
                    host.clone(),
                    // UIKit's safe area and keyboard frame. The platform
                    // schedules a frame whenever either changes.
                    WindowSeam::new(gpui_ios::insets),
                    ShellOptions {
                        window: WindowOptions {
                            // UIKit owns the geometry; the shell reads it back.
                            window_bounds: None,
                            titlebar: None,
                            window_background: WindowBackgroundAppearance::Opaque,
                            ..Default::default()
                        },
                        fonts: vec![Cow::Borrowed(tcode_ui::assets::DM_SANS)],
                        theme_json: Cow::Owned(tcode_ui::flattened_theme_json()),
                        activate: true,
                        setup: ShellSetup {
                            initial: tcode_ui::last_host_target(host.as_ref()),
                            client_host: Some(host),
                            // A phone runs no host of its own.
                            local: None,
                            seed_blocking: false,
                        },
                        ..Default::default()
                    },
                );
            });
        if slot.set(handle).is_err() {
            log::warn!("tcode's embedded GPUI application was already started");
        }
    });
}
