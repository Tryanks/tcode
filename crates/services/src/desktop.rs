use std::io;
use std::path::Path;

pub fn open_in_zed(cwd: &Path) -> io::Result<()> {
    let child = crate::process::command("zed").arg(cwd).spawn()?;
    drop(child);
    Ok(())
}

#[cfg(target_os = "macos")]
pub fn capture_screen_region(region: &str) -> Result<Vec<u8>, String> {
    let path = std::env::temp_dir().join(format!("tcode-preview-{}.png", uuid::Uuid::new_v4()));
    let result = crate::process::command("screencapture")
        .arg("-x")
        .arg("-R")
        .arg(region)
        .arg(&path)
        .status()
        .map_err(|err| format!("failed to run screencapture: {err}"))
        .and_then(|status| {
            if status.success() {
                std::fs::read(&path).map_err(|err| format!("failed to read screenshot: {err}"))
            } else {
                Err("screencapture failed".to_string())
            }
        });
    let _ = std::fs::remove_file(path);
    result
}
