use std::mem::size_of;

use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BitBlt, CAPTUREBLT, CreateCompatibleBitmap,
    CreateCompatibleDC, DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDIBits, GetWindowDC, HBITMAP,
    HDC, HGDIOBJ, ReleaseDC, SRCCOPY, SelectObject,
};
use windows::Win32::UI::WindowsAndMessaging::{GetWindowRect, PW_RENDERFULLCONTENT};
use windows::core::BOOL;

use super::super::{BackendError, BackendErrorCode, RootInfo};
use super::uia;

#[link(name = "user32")]
unsafe extern "system" {
    fn PrintWindow(hwnd: HWND, hdc: HDC, flags: u32) -> BOOL;
}

pub(super) fn capture_window(root: &RootInfo) -> Result<Vec<u8>, BackendError> {
    let hwnd = uia::locate_hwnd(root)?;
    let mut rect = windows::Win32::Foundation::RECT::default();
    unsafe { GetWindowRect(hwnd, &mut rect) }.map_err(|error| {
        capture_error(format!(
            "GetWindowRect failed for window {}: {error}",
            root.window_id
        ))
    })?;
    let width = rect.right.saturating_sub(rect.left);
    let height = rect.bottom.saturating_sub(rect.top);
    let width_u32 = u32::try_from(width)
        .ok()
        .filter(|width| *width > 0)
        .ok_or_else(|| capture_error("window capture width is empty or too large"))?;
    let height_u32 = u32::try_from(height)
        .ok()
        .filter(|height| *height > 0)
        .ok_or_else(|| capture_error("window capture height is empty or too large"))?;

    let window_dc = WindowDc::new(hwnd)?;
    let memory_dc = MemoryDc::new(window_dc.handle())?;
    let bitmap = OwnedBitmap::new(window_dc.handle(), width, height)?;
    let selection = SelectedBitmap::new(memory_dc.handle(), bitmap.handle())?;

    let printed = unsafe { PrintWindow(hwnd, memory_dc.handle(), PW_RENDERFULLCONTENT) }.as_bool();
    if !printed {
        unsafe {
            BitBlt(
                memory_dc.handle(),
                0,
                0,
                width,
                height,
                Some(window_dc.handle()),
                0,
                0,
                SRCCOPY | CAPTUREBLT,
            )
        }
        .map_err(|error| capture_error(format!("BitBlt fallback failed: {error}")))?;
    }
    // GetDIBits requires the bitmap not to be selected into a device context.
    drop(selection);

    let byte_len = usize::try_from(width_u32)
        .ok()
        .and_then(|width| {
            usize::try_from(height_u32)
                .ok()
                .and_then(|height| width.checked_mul(height))
        })
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| capture_error("window capture pixel buffer is too large"))?;
    let mut pixels = vec![0_u8; byte_len];
    let header_size = u32::try_from(size_of::<BITMAPINFOHEADER>())
        .map_err(|_| capture_error("BITMAPINFOHEADER size does not fit u32"))?;
    let image_size = u32::try_from(byte_len)
        .map_err(|_| capture_error("window capture pixel buffer exceeds the GDI limit"))?;
    let mut bitmap_info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: header_size,
            biWidth: width,
            // A negative height requests top-down scan lines.
            biHeight: -height,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            biSizeImage: image_size,
            ..BITMAPINFOHEADER::default()
        },
        ..BITMAPINFO::default()
    };
    let copied_lines = unsafe {
        GetDIBits(
            memory_dc.handle(),
            bitmap.handle(),
            0,
            height_u32,
            Some(pixels.as_mut_ptr().cast()),
            &mut bitmap_info,
            DIB_RGB_COLORS,
        )
    };
    if copied_lines != height {
        return Err(capture_error(format!(
            "GetDIBits copied {copied_lines} of {height} scan lines"
        )));
    }
    for pixel in pixels.chunks_exact_mut(4) {
        pixel.swap(0, 2);
        pixel[3] = 255;
    }
    encode_png(width_u32, height_u32, &pixels)
}

fn encode_png(width: u32, height: u32, rgba: &[u8]) -> Result<Vec<u8>, BackendError> {
    let mut output = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut output, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder
            .write_header()
            .map_err(|error| capture_error(format!("PNG header encoding failed: {error}")))?;
        writer
            .write_image_data(rgba)
            .map_err(|error| capture_error(format!("PNG pixel encoding failed: {error}")))?;
    }
    Ok(output)
}

struct WindowDc {
    hwnd: HWND,
    handle: HDC,
}

impl WindowDc {
    fn new(hwnd: HWND) -> Result<Self, BackendError> {
        let handle = unsafe { GetWindowDC(Some(hwnd)) };
        if handle.is_invalid() {
            Err(capture_error("GetWindowDC returned a null device context"))
        } else {
            Ok(Self { hwnd, handle })
        }
    }

    fn handle(&self) -> HDC {
        self.handle
    }
}

impl Drop for WindowDc {
    fn drop(&mut self) {
        unsafe { ReleaseDC(Some(self.hwnd), self.handle) };
    }
}

struct MemoryDc(HDC);

impl MemoryDc {
    fn new(source: HDC) -> Result<Self, BackendError> {
        let handle = unsafe { CreateCompatibleDC(Some(source)) };
        if handle.is_invalid() {
            Err(capture_error(
                "CreateCompatibleDC returned a null device context",
            ))
        } else {
            Ok(Self(handle))
        }
    }

    fn handle(&self) -> HDC {
        self.0
    }
}

impl Drop for MemoryDc {
    fn drop(&mut self) {
        let _ = unsafe { DeleteDC(self.0) };
    }
}

struct OwnedBitmap(HBITMAP);

impl OwnedBitmap {
    fn new(source: HDC, width: i32, height: i32) -> Result<Self, BackendError> {
        let bitmap = unsafe { CreateCompatibleBitmap(source, width, height) };
        if bitmap.is_invalid() {
            Err(capture_error(
                "CreateCompatibleBitmap returned a null bitmap",
            ))
        } else {
            Ok(Self(bitmap))
        }
    }

    fn handle(&self) -> HBITMAP {
        self.0
    }
}

impl Drop for OwnedBitmap {
    fn drop(&mut self) {
        let _ = unsafe { DeleteObject(HGDIOBJ::from(self.0)) };
    }
}

struct SelectedBitmap {
    dc: HDC,
    previous: HGDIOBJ,
}

impl SelectedBitmap {
    fn new(dc: HDC, bitmap: HBITMAP) -> Result<Self, BackendError> {
        let previous = unsafe { SelectObject(dc, HGDIOBJ::from(bitmap)) };
        if previous.is_invalid() {
            Err(capture_error("SelectObject rejected the capture bitmap"))
        } else {
            Ok(Self { dc, previous })
        }
    }
}

impl Drop for SelectedBitmap {
    fn drop(&mut self) {
        unsafe { SelectObject(self.dc, self.previous) };
    }
}

fn capture_error(message: impl Into<String>) -> BackendError {
    BackendError::new(BackendErrorCode::CaptureFailed, message)
}
