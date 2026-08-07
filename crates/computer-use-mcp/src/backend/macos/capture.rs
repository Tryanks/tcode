use super::super::{BackendError, BackendErrorCode, RootInfo};

pub(super) fn capture_window(root: &RootInfo) -> Result<Vec<u8>, BackendError> {
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
