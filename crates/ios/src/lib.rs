//! iOS static-library entry point used by the Swift host.

use gpui::{Application, ApplicationHandle};
use std::cell::OnceCell;

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
        let handle =
            Application::with_platform(gpui_ios::platform()).run_embedded(tcode_mobile::run);
        if slot.set(handle).is_err() {
            log::warn!("tcode's embedded GPUI application was already started");
        }
    });
}
