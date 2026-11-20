//! The collaboration templates a host holds: the file the app ships with, and the one last
//! fetched from the repository, kept in the data directory.

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tcode_core::collaboration::{TEMPLATES_URL, Templates};

/// The file the fetched templates are kept in.
pub fn cache_path(data_dir: &Path) -> PathBuf {
    data_dir.join("collaboration-templates.json")
}

/// The templates a start begins with: those last fetched, where they are newer than the
/// ones the app ships with.
pub fn stored(data_dir: &Path) -> Templates {
    let bundled = Templates::bundled();
    match std::fs::read_to_string(cache_path(data_dir)) {
        Ok(text) => match Templates::parse(&text) {
            Ok(kept) if kept.updated_at > bundled.updated_at => kept,
            Ok(_) => bundled,
            Err(error) => {
                log::warn!("the kept collaboration templates are unreadable: {error}");
                bundled
            }
        },
        Err(_) => bundled,
    }
}

/// The templates file the repository publishes, as its text and read.
pub fn fetch() -> Result<(String, Templates), String> {
    let failed = |error: &dyn std::fmt::Display| format!("{TEMPLATES_URL}: {error}");
    let response = ureq::get(TEMPLATES_URL)
        .header("User-Agent", "tcode")
        .config()
        .timeout_global(Some(Duration::from_secs(30)))
        .build()
        .call()
        .map_err(|error| failed(&error))?;
    let mut text = String::new();
    response
        .into_body()
        .into_reader()
        .read_to_string(&mut text)
        .map_err(|error| failed(&error))?;
    let templates = Templates::parse(&text).map_err(|error| failed(&error))?;
    Ok((text, templates))
}
