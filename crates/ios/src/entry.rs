//! GPUI application lifetime and the `tcode_ios_start` entry point.

use gpui::{Application, ApplicationHandle};
use std::cell::OnceCell;
use std::rc::Rc;

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
                tcode_mobile::run_with_host(cx, Rc::new(crate::host::native_host()));
            });
        if slot.set(handle).is_err() {
            log::warn!("tcode's embedded GPUI application was already started");
        }
    });
}
