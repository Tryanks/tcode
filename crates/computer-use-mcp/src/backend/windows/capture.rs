use std::io::ErrorKind;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use uiautomation::UIElement;

use super::super::{BackendError, BackendErrorCode};

static NEXT_CAPTURE_ID: AtomicU64 = AtomicU64::new(1);

pub(super) fn capture_element(element: &UIElement) -> Result<Vec<u8>, BackendError> {
    let screenshot = element.screenshot().map_err(|error| {
        capture_error(format!("uiautomation element screenshot failed: {error}"))
    })?;
    let temporary = TemporaryPng::new()?;
    screenshot
        .save_png(&temporary.path)
        .map_err(|error| capture_error(format!("uiautomation PNG encoding failed: {error}")))?;
    std::fs::read(&temporary.path)
        .map_err(|error| capture_error(format!("reading encoded screenshot failed: {error}")))
}

struct TemporaryPng {
    directory: PathBuf,
    path: PathBuf,
}

impl TemporaryPng {
    fn new() -> Result<Self, BackendError> {
        let base = std::env::temp_dir();
        for _ in 0..100 {
            let id = NEXT_CAPTURE_ID.fetch_add(1, Ordering::Relaxed);
            let directory = base.join(format!("tcode-uia-capture-{}-{id}", std::process::id()));
            match std::fs::create_dir(&directory) {
                Ok(()) => {
                    let path = directory.join("capture.png");
                    return Ok(Self { directory, path });
                }
                Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(capture_error(format!(
                        "creating screenshot temporary directory failed: {error}"
                    )));
                }
            }
        }
        Err(capture_error(
            "could not allocate a unique screenshot temporary directory",
        ))
    }
}

impl Drop for TemporaryPng {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
        let _ = std::fs::remove_dir(&self.directory);
    }
}

fn capture_error(message: impl Into<String>) -> BackendError {
    BackendError::new(BackendErrorCode::CaptureFailed, message)
}
