//! GPUI application lifetime and the `tcode_ios_start` entry point.

use std::borrow::Cow;
use std::cell::OnceCell;
use std::rc::Rc;

use gpui::{Application, ApplicationHandle, WindowBackgroundAppearance, WindowOptions};
use tcode_client::host::ClientHost;
use tcode_ui::{ShellOptions, ShellSetup};

thread_local! {
    static APPLICATION: OnceCell<ApplicationHandle> = const { OnceCell::new() };
}

pub(crate) fn refresh_windows() {
    APPLICATION.with(|slot| {
        if let Some(app) = slot.get() {
            app.to_async().update(|cx| cx.refresh_windows());
        }
    });
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
                let (native_host, system_locale) = crate::host::native_host(cx);
                let host: Rc<dyn ClientHost> = native_host;
                tcode_ui::run_shell(
                    cx,
                    host.clone(),
                    ShellOptions {
                        window: WindowOptions {
                            // UIKit owns the geometry; the shell reads it back.
                            window_bounds: None,
                            titlebar: None,
                            window_background: WindowBackgroundAppearance::Opaque,
                            ..Default::default()
                        },
                        theme_json: Cow::Owned(tcode_ui::flattened_theme_json()),
                        activate: true,
                        system_locale,
                        // UIKit suspends the app; the shell reconnects on return.
                        lifecycle: Some(gpui_ios::platform()),
                        setup: ShellSetup {
                            initial: tcode_ui::last_host_target(host.as_ref()),
                            initial_pairing_error: None,
                            client_host: Some(host),
                            // A phone runs no host of its own.
                            local: None,
                            local_failure: None,
                            seed_blocking: false,
                            restore_navigation: true,
                        },
                        ..Default::default()
                    }
                    .with_bundled_monospace(),
                );
            });
        if slot.set(handle).is_err() {
            log::warn!("tcode's embedded GPUI application was already started");
        }
    });
}

/// Whether UIKit's left-edge pan may begin: only where it pops a page, so an
/// edge contact elsewhere stays an ordinary touch.
#[unsafe(no_mangle)]
pub extern "C" fn tcode_ios_can_swipe_back() -> u8 {
    APPLICATION.with(|slot| {
        slot.get()
            .is_some_and(|app| app.update(|cx| tcode_ui::can_swipe_back(cx)))
            .into()
    })
}

/// UIKit's left-edge pan ended past its commit point.
#[unsafe(no_mangle)]
pub extern "C" fn tcode_ios_swipe_back() {
    APPLICATION.with(|slot| {
        if let Some(app) = slot.get() {
            app.to_async().update(|cx| {
                tcode_ui::handle_back(cx);
            });
        }
    });
}
