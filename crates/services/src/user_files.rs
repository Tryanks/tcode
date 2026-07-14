use std::io;
use std::path::{Path, PathBuf};

pub fn read_bytes(path: &Path) -> io::Result<Vec<u8>> {
    std::fs::read(path)
}

pub fn remove_file(path: &Path) -> io::Result<()> {
    std::fs::remove_file(path)
}

pub fn is_directory(path: &Path) -> bool {
    path.is_dir()
}

pub fn relativize_to_workspace(path: &str, cwd: &Path) -> String {
    let canonical_cwd = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    let path_buf = Path::new(path);
    path_buf
        .strip_prefix(cwd)
        .or_else(|_| path_buf.strip_prefix(&canonical_cwd))
        .map(|relative| relative.to_string_lossy().to_string())
        .unwrap_or_else(|_| path.to_string())
}

/// Return the directory used to persist attachments for a session.
pub fn attachment_dir(data_root: &Path, session_id: &str) -> PathBuf {
    data_root.join("attachments").join(session_id)
}

/// Persist attachment bytes under the session's attachment directory.
pub fn save_attachment(
    data_root: &Path,
    session_id: &str,
    bytes: &[u8],
    ext: &str,
) -> io::Result<PathBuf> {
    let dir = attachment_dir(data_root, session_id);
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{}.{ext}", uuid::Uuid::new_v4()));
    std::fs::write(&path, bytes)?;
    Ok(path)
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
    fn attachment_bytes_path_and_extension() {
        let root = temp_dir("attachment");
        let bytes = b"\0attachment\xffbytes";

        let path = save_attachment(&root, "session-123", bytes, "png").unwrap();

        assert_eq!(
            path.parent(),
            Some(attachment_dir(&root, "session-123").as_path())
        );
        assert_eq!(path.extension().and_then(|ext| ext.to_str()), Some("png"));
        assert_eq!(
            uuid::Uuid::parse_str(path.file_stem().unwrap().to_str().unwrap())
                .unwrap()
                .get_version(),
            Some(uuid::Version::Random)
        );
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        std::fs::remove_dir_all(root).unwrap();
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

    #[test]
    fn file_lifecycle_and_relativization() {
        let root = temp_dir("lifecycle");
        let workspace = root.join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let path = workspace.join("exact.bin");
        let bytes = b"\0exact\xffbytes";
        std::fs::write(&path, bytes).unwrap();

        assert!(is_directory(&workspace));
        assert!(!is_directory(&path));
        assert_eq!(read_bytes(&path).unwrap(), bytes);
        assert_eq!(
            relativize_to_workspace(path.to_str().unwrap(), &workspace),
            "exact.bin"
        );
        if let Ok(canonical_path) = path.canonicalize() {
            assert_eq!(
                relativize_to_workspace(canonical_path.to_str().unwrap(), &workspace),
                "exact.bin"
            );
        }
        let outside = root.join("outside.bin");
        assert_eq!(
            relativize_to_workspace(outside.to_str().unwrap(), &workspace),
            outside.to_string_lossy()
        );

        remove_file(&path).unwrap();
        assert!(!path.exists());
        std::fs::remove_dir_all(root).unwrap();
    }
}
