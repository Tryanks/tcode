//! UIKit services behind this client's `ClientHost`.

use std::{
    cell::RefCell,
    collections::HashMap,
    ptr,
    rc::{Rc, Weak},
    slice,
    sync::atomic::{AtomicU64, Ordering},
};

use tcode_client::host::{DiscoveredHost, HostFuture};
use tcode_remote::NativeClientHost;

type ScanDone = Box<dyn FnOnce(Result<String, String>)>;

thread_local! {
    static CAMERA_CALLBACKS: RefCell<HashMap<u64, ScanDone>> = RefCell::new(HashMap::new());
    /// The shell owns the host; scene callbacks reach it while it lives.
    static HOST: RefCell<Weak<NativeClientHost>> = const { RefCell::new(Weak::new()) };
}

static NEXT_REQUEST_ID: AtomicU64 = AtomicU64::new(1);

unsafe extern "C" {
    fn tcode_ios_host_set_app_background_dark(dark: u8);
    fn tcode_ios_host_device_name(destination: *mut u8, capacity: usize) -> usize;
    fn tcode_ios_host_device_platform(destination: *mut u8, capacity: usize) -> usize;
    fn tcode_ios_host_system_locale(destination: *mut u8, capacity: usize) -> usize;
    fn tcode_ios_host_start_camera_scan(request_id: u64);
}

pub(crate) fn native_host(cx: &mut gpui::App) -> (Rc<NativeClientHost>, Option<String>) {
    // Native chrome follows the resolved app theme, which may differ from UIKit.
    cx.observe_global::<tcode_ui::theme::Theme>(|cx| {
        let dark = cx.global::<tcode_ui::theme::Theme>().mode.is_dark();
        // SAFETY: GPUI's foreground observer runs on the UIKit main thread.
        unsafe { tcode_ios_host_set_app_background_dark(u8::from(dark)) };
    })
    .detach();
    let device_name = read_native_string(|destination, capacity| {
        // SAFETY: Swift writes no more than `capacity` bytes during the call.
        unsafe { tcode_ios_host_device_name(destination, capacity) }
    })
    .filter(|name| !name.trim().is_empty())
    .unwrap_or_else(|| "iPhone".into());
    let platform = read_native_string(|destination, capacity| {
        // SAFETY: Swift writes no more than `capacity` bytes during the call.
        unsafe { tcode_ios_host_device_platform(destination, capacity) }
    })
    .filter(|platform| !platform.trim().is_empty())
    .unwrap_or_else(|| "iOS".into());
    let system_locale = read_native_string(|destination, capacity| {
        // SAFETY: Swift writes no more than `capacity` bytes during the call.
        unsafe { tcode_ios_host_system_locale(destination, capacity) }
    })
    .filter(|locale| !locale.trim().is_empty());

    // LAN browsing reports nothing on iOS; pairing goes through invites.
    // Machines advertise `<base32 EndpointId>._tcode._udp.local.` through
    // iroh's mDNS lookup (swarm-discovery), which answers a PTR query with
    // SRV, TXT and A/AAAA records but never a PTR record, so Bonjour
    // (`NWBrowser`/`NetServiceBrowser`, which browse by PTR) cannot list
    // these machines. Running `tcode_traverse::browse` here instead needs the
    // `com.apple.developer.networking.multicast` entitlement, which is not
    // decided; until then an empty list is preferred to a browser that never
    // finds anything.
    let host = NativeClientHost::from_env_with_device_name(device_name)
        .with_platform(platform)
        .with_browser(|| -> HostFuture<'static, Vec<DiscoveredHost>> {
            Box::pin(async { Vec::new() })
        })
        .with_qr_scanner(|| -> HostFuture<'static, Result<String, String>> {
            let (sender, receiver) = async_channel::bounded(1);
            let request_id = NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed);
            CAMERA_CALLBACKS.with(|callbacks| {
                callbacks.borrow_mut().insert(
                    request_id,
                    Box::new(move |result| {
                        let _ = sender.try_send(result);
                    }),
                );
            });
            // SAFETY: UIKit retains the id and completes the request exactly once.
            unsafe { tcode_ios_host_start_camera_scan(request_id) };
            Box::pin(async move {
                receiver
                    .recv()
                    .await
                    .unwrap_or_else(|error| Err(error.to_string()))
            })
        });
    let host = Rc::new(host);
    HOST.with(|slot| *slot.borrow_mut() = Rc::downgrade(&host));
    (host, system_locale)
}

/// The scene entered the foreground: the device endpoint rebinds its paths
/// and every live transport is probed at once. The endpoint outlives
/// backgrounding; the shell's foreground policy decides between a probe and
/// a full reconnect, and this call covers a network that changed while the
/// process was suspended.
#[unsafe(no_mangle)]
pub extern "C" fn tcode_ios_network_changed() {
    HOST.with(|slot| {
        if let Some(host) = slot.borrow().upgrade() {
            host.network_changed();
        }
    });
}

/// Completes a one-shot AVFoundation QR scan from Swift.
#[unsafe(no_mangle)]
pub extern "C" fn tcode_ios_camera_scan_completed(
    request_id: u64,
    value_bytes: *const u8,
    value_length: usize,
    error_bytes: *const u8,
    error_length: usize,
) {
    let callback = CAMERA_CALLBACKS.with(|callbacks| callbacks.borrow_mut().remove(&request_id));
    let Some(callback) = callback else {
        log::warn!("unknown iOS camera request {request_id}");
        return;
    };
    // SAFETY: Swift keeps both temporary buffers alive through this call.
    let value = unsafe { ffi_string(value_bytes, value_length) };
    // SAFETY: same as above.
    let error = unsafe { ffi_string(error_bytes, error_length) };
    let result = value
        .filter(|value| !value.is_empty())
        .ok_or_else(|| error.unwrap_or_else(|| "此设备没有可用的相机".to_string()));
    callback(result);
}

fn read_native_string(read: impl Fn(*mut u8, usize) -> usize) -> Option<String> {
    let length = read(ptr::null_mut(), 0);
    if length == 0 {
        return None;
    }
    let mut bytes = vec![0_u8; length];
    let written = read(bytes.as_mut_ptr(), bytes.len());
    if written > bytes.len() {
        return None;
    }
    bytes.truncate(written);
    String::from_utf8(bytes).ok()
}

unsafe fn ffi_string(bytes: *const u8, length: usize) -> Option<String> {
    if length == 0 {
        return None;
    }
    if bytes.is_null() {
        return None;
    }
    // SAFETY: the caller guarantees `bytes` is readable for `length` bytes.
    let bytes = unsafe { slice::from_raw_parts(bytes, length) };
    String::from_utf8(bytes.to_vec()).ok()
}
