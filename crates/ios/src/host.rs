//! UIKit services used by `tcode-mobile`.

use std::{
    cell::RefCell,
    collections::HashMap,
    ptr, slice,
    sync::atomic::{AtomicU64, Ordering},
};

use tcode_mobile::host::{BrowseDone, NativeHost, ScanDone};

use crate::entry::dispatch_to_app;

thread_local! {
    static BROWSE_CALLBACKS: RefCell<HashMap<u64, BrowseDone>> = RefCell::new(HashMap::new());
    static CAMERA_CALLBACKS: RefCell<HashMap<u64, ScanDone>> = RefCell::new(HashMap::new());
}

static NEXT_REQUEST_ID: AtomicU64 = AtomicU64::new(1);

unsafe extern "C" {
    fn tcode_ios_host_device_name(destination: *mut u8, capacity: usize) -> usize;
    fn tcode_ios_host_start_camera_scan(request_id: u64);
    fn tcode_ios_host_browse(request_id: u64);
}

pub(crate) fn native_host() -> NativeHost {
    let device_name = read_native_string(|destination, capacity| {
        // SAFETY: Swift writes no more than `capacity` bytes during the call.
        unsafe { tcode_ios_host_device_name(destination, capacity) }
    })
    .filter(|name| !name.trim().is_empty())
    .unwrap_or_else(|| "iPhone".into());

    NativeHost::from_env_with_device_name(device_name)
        .with_browser(|done, _cx| {
            let request_id = NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed);
            BROWSE_CALLBACKS.with(|callbacks| {
                callbacks.borrow_mut().insert(request_id, done);
            });
            // SAFETY: Swift completes once on the main thread with bounded JSON.
            unsafe {
                tcode_ios_host_browse(request_id);
            }
        })
        .with_qr_scanner(|done, _cx| {
            let request_id = NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed);
            CAMERA_CALLBACKS.with(|callbacks| {
                callbacks.borrow_mut().insert(request_id, done);
            });
            // SAFETY: UIKit retains the id and completes the request exactly once.
            unsafe { tcode_ios_host_start_camera_scan(request_id) };
        })
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
    dispatch_to_app(move |cx| callback(result, cx));
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

/// Completes the Bonjour browse on the main thread.
#[unsafe(no_mangle)]
pub extern "C" fn tcode_ios_browse_completed(request_id: u64, bytes: *const u8, length: usize) {
    let callback = BROWSE_CALLBACKS.with(|callbacks| callbacks.borrow_mut().remove(&request_id));
    let Some(callback) = callback else {
        return;
    };
    // SAFETY: Swift holds the JSON buffer for this call; reject unbounded input.
    let json = if length <= 65536 {
        unsafe { ffi_string(bytes, length) }
    } else {
        None
    };
    let hosts = json
        .map(|s| tcode_mobile::host::parse_discovered_hosts(&s))
        .unwrap_or_default();
    dispatch_to_app(move |cx| callback(hosts, cx));
}
