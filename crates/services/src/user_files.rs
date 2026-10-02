use std::io;
use std::path::{Path, PathBuf};

/// Return the directory used to persist attachments for a session.
pub fn attachment_dir(data_root: &Path, session_id: &str) -> PathBuf {
    attachments_root(data_root).join(session_id)
}

/// The directory every session's attachment directory lives under; a client
/// may only save attachments below it.
pub fn attachments_root(data_root: &Path) -> PathBuf {
    data_root.join("attachments")
}

/// Save plan markdown to the lowest unused numbered plan file in the workspace.
pub fn save_plan_to_workspace(cwd: &Path, markdown: &str) -> io::Result<PathBuf> {
    let mut n = 1;
    let path = loop {
        let candidate = cwd.join(format!("PLAN-{n}.md"));
        if !candidate.exists() {
            break candidate;
        }
        n += 1;
        if n > 9999 {
            break cwd.join("PLAN.md");
        }
    };
    std::fs::write(&path, markdown)?;
    Ok(path)
}

/// Save plan markdown to Downloads, falling back to the workspace and then `.`.
pub fn save_plan_download(
    filename: &str,
    markdown: &str,
    fallback_cwd: Option<&Path>,
) -> io::Result<PathBuf> {
    save_plan_download_with_dir(
        filename,
        markdown,
        dirs::download_dir().as_deref(),
        fallback_cwd,
    )
}

fn save_plan_download_with_dir(
    filename: &str,
    markdown: &str,
    download_dir: Option<&Path>,
    fallback_cwd: Option<&Path>,
) -> io::Result<PathBuf> {
    let dir = download_dir
        .or(fallback_cwd)
        .unwrap_or_else(|| Path::new("."));
    let path = dir.join(filename);
    std::fs::write(&path, markdown)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!("tcode-user-files-{label}-{}", uuid::Uuid::new_v4()))
    }

    #[test]
    fn workspace_uses_next_plan_number_without_overwriting() {
        let cwd = temp_dir("workspace");
        std::fs::create_dir_all(&cwd).unwrap();

        let first = save_plan_to_workspace(&cwd, "first plan").unwrap();
        let second = save_plan_to_workspace(&cwd, "second plan").unwrap();

        assert_eq!(first, cwd.join("PLAN-1.md"));
        assert_eq!(second, cwd.join("PLAN-2.md"));
        assert_eq!(std::fs::read_to_string(first).unwrap(), "first plan");
        assert_eq!(std::fs::read_to_string(second).unwrap(), "second plan");
        std::fs::remove_dir_all(cwd).unwrap();
    }

    #[test]
    fn download_prefers_explicit_dir_then_falls_back_with_exact_bytes() {
        let root = temp_dir("download");
        let downloads = root.join("downloads");
        let fallback = root.join("fallback");
        std::fs::create_dir_all(&downloads).unwrap();
        std::fs::create_dir_all(&fallback).unwrap();

        let preferred = save_plan_download_with_dir(
            "preferred.md",
            "preferred\nmarkdown\0",
            Some(&downloads),
            Some(&fallback),
        )
        .unwrap();
        let fallback_path =
            save_plan_download_with_dir("fallback.md", "fallback\nmarkdown", None, Some(&fallback))
                .unwrap();

        assert_eq!(preferred, downloads.join("preferred.md"));
        assert_eq!(fallback_path, fallback.join("fallback.md"));
        assert_eq!(std::fs::read(preferred).unwrap(), b"preferred\nmarkdown\0");
        assert_eq!(std::fs::read(fallback_path).unwrap(), b"fallback\nmarkdown");
        std::fs::remove_dir_all(root).unwrap();
    }
}
