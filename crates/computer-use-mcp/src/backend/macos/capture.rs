use super::super::{BackendError, BackendErrorCode, RootInfo};

pub(super) fn capture_window(root: &RootInfo) -> Result<Vec<u8>, BackendError> {
    match capture_window_screen_capture_kit(root) {
        Ok(png) => Ok(png),
        Err(error) => {
            log::warn!(
                "computer-use-mcp: ScreenCaptureKit capture failed for window {}: {error}; falling back to screencapture",
                root.window_id
            );
            capture_window_cli(root)
        }
    }
}

fn capture_window_screen_capture_kit(root: &RootInfo) -> Result<Vec<u8>, String> {
    use std::ptr::NonNull;
    use std::sync::mpsc;
    use std::time::Duration;

    use block2::RcBlock;
    use objc2::AnyThread;
    use objc2::runtime::AnyClass;
    use objc2_core_foundation::{CFMutableData, CFRetained, CFString};
    use objc2_core_graphics::CGImage;
    use objc2_foundation::NSError;
    use objc2_image_io::CGImageDestination;
    use objc2_screen_capture_kit::{
        SCContentFilter, SCScreenshotManager, SCShareableContent, SCStreamConfiguration,
    };

    const CAPTURE_TIMEOUT: Duration = Duration::from_secs(5);

    if AnyClass::get(c"SCScreenshotManager").is_none() {
        return Err("SCScreenshotManager is unavailable (requires macOS 14 or newer)".into());
    }

    let window_id = root.window_id;
    let (sender, receiver) = mpsc::sync_channel::<Result<CFRetained<CGImage>, String>>(1);
    let shareable_content_handler = RcBlock::new(
        move |content: *mut SCShareableContent, error: *mut NSError| {
            if let Some(error) = unsafe { error.as_ref() } {
                let _ = sender.send(Err(format!("failed to get shareable content: {error}")));
                return;
            }
            let Some(content) = (unsafe { content.as_ref() }) else {
                let _ = sender.send(Err("ScreenCaptureKit returned no shareable content".into()));
                return;
            };

            let windows = unsafe { content.windows() };
            let Some(window) = windows
                .to_vec()
                .into_iter()
                .find(|window| unsafe { window.windowID() } == window_id)
            else {
                let _ = sender.send(Err(format!(
                    "window {window_id} was not present in ScreenCaptureKit shareable content"
                )));
                return;
            };

            let filter = unsafe {
                SCContentFilter::initWithDesktopIndependentWindow(SCContentFilter::alloc(), &window)
            };
            let frame = unsafe { window.frame() };
            let backing_scale = f64::from(unsafe { filter.pointPixelScale() });
            let width = (frame.size.width * backing_scale).round();
            let height = (frame.size.height * backing_scale).round();
            if !width.is_finite() || !height.is_finite() || width < 1.0 || height < 1.0 {
                let _ = sender.send(Err(format!(
                    "invalid ScreenCaptureKit dimensions {width}x{height} for window {window_id}"
                )));
                return;
            }

            let configuration = unsafe { SCStreamConfiguration::new() };
            unsafe {
                configuration.setWidth(width as usize);
                configuration.setHeight(height as usize);
                configuration.setShowsCursor(false);
            }

            let image_sender = sender.clone();
            let image_handler = RcBlock::new(move |image: *mut CGImage, error: *mut NSError| {
                let result = if let Some(error) = unsafe { error.as_ref() } {
                    Err(format!("failed to capture image: {error}"))
                } else if let Some(image) = NonNull::new(image) {
                    Ok(unsafe { CFRetained::retain(image) })
                } else {
                    Err("ScreenCaptureKit returned no image".into())
                };
                let _ = image_sender.send(result);
            });
            unsafe {
                SCScreenshotManager::captureImageWithFilter_configuration_completionHandler(
                    &filter,
                    &configuration,
                    Some(&image_handler),
                );
            }
        },
    );

    unsafe {
        SCShareableContent::getShareableContentWithCompletionHandler(&shareable_content_handler);
    }
    let image = receiver
        .recv_timeout(CAPTURE_TIMEOUT)
        .map_err(|error| match error {
            mpsc::RecvTimeoutError::Timeout => {
                "ScreenCaptureKit capture timed out after 5 seconds".to_owned()
            }
            mpsc::RecvTimeoutError::Disconnected => {
                "ScreenCaptureKit capture callback disconnected".to_owned()
            }
        })??;
    let data = CFMutableData::new(None, 0)
        .ok_or_else(|| "failed to allocate PNG destination data".to_owned())?;
    let png_type = CFString::from_static_str("public.png");
    let destination = unsafe { CGImageDestination::with_data(&data, &png_type, 1, None) }
        .ok_or_else(|| "failed to create PNG image destination".to_owned())?;
    unsafe {
        destination.add_image(&image, None);
        if !destination.finalize() {
            return Err("failed to finalize ScreenCaptureKit PNG".into());
        }
    }
    Ok(data.to_vec())
}

fn capture_window_cli(root: &RootInfo) -> Result<Vec<u8>, BackendError> {
    let path = std::env::temp_dir().join(format!(
        "tcode-computer-use-{}-{}.png",
        root.window_id,
        uuid::Uuid::new_v4()
    ));
    let result = tcode_services::process::command("screencapture")
        .arg("-x")
        .arg("-l")
        .arg(root.window_id.to_string())
        .arg("-t")
        .arg("png")
        .arg(&path)
        .status()
        .map_err(|error| {
            BackendError::new(
                BackendErrorCode::CaptureFailed,
                format!("failed to spawn screencapture: {error}"),
            )
        })
        .and_then(|status| {
            if status.success() {
                std::fs::read(&path).map_err(|error| {
                    BackendError::new(
                        BackendErrorCode::CaptureFailed,
                        format!("failed to read captured PNG: {error}"),
                    )
                })
            } else {
                Err(BackendError::new(
                    BackendErrorCode::CaptureFailed,
                    format!("screencapture exited with status {status}"),
                ))
            }
        });
    let _ = std::fs::remove_file(path);
    result
}
