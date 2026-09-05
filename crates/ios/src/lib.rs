//! iOS static-library entry point used by the Swift host.

mod host;

use gpui::{App, Application, ApplicationHandle, AsyncApp};
use std::cell::OnceCell;
use std::rc::Rc;

thread_local! {
    static APPLICATION: OnceCell<ApplicationHandle> = const { OnceCell::new() };
    static ASYNC_APPLICATION: OnceCell<AsyncApp> = const { OnceCell::new() };
}

/// Re-enter the embedded app without borrowing it synchronously from a UIKit callback.
pub(crate) fn dispatch_to_app(callback: impl FnOnce(&mut App) + 'static) {
    ASYNC_APPLICATION.with(|slot| {
        let Some(app) = slot.get().cloned() else {
            log::warn!("dropping iOS host callback before GPUI finished starting");
            return;
        };
        let executor = app.foreground_executor().clone();
        executor.spawn(async move { app.update(callback) }).detach();
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
                tcode_mobile::run_with_host(cx, Rc::new(host::native_host()));
            });
        let async_app = handle.to_async();
        ASYNC_APPLICATION.with(|async_slot| {
            if async_slot.set(async_app).is_err() {
                log::warn!("tcode's asynchronous GPUI application was already retained");
            }
        });
        if slot.set(handle).is_err() {
            log::warn!("tcode's embedded GPUI application was already started");
        }
    });
}
