use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum LinkTarget {
    Web(String),
    Local(PathBuf),
}

pub(super) fn resolve_link(url: &str, base_dir: Option<&Path>) -> LinkTarget {
    if let Some(path) = url.strip_prefix("file://") {
        return LinkTarget::Local(PathBuf::from(path));
    }
    if url.contains("://") || url.starts_with("mailto:") {
        return LinkTarget::Web(url.to_string());
    }

    let mut candidates = vec![url.to_string()];
    let mut index = 0;
    while index < candidates.len() {
        let candidate = &candidates[index];
        for stripped in [strip_line_suffix(candidate), strip_line_fragment(candidate)]
            .into_iter()
            .flatten()
        {
            if !candidates.contains(&stripped) {
                candidates.push(stripped);
            }
        }
        index += 1;
    }

    for candidate in candidates {
        let candidate = expand_home(&candidate);
        let resolved = if candidate.is_absolute() {
            Some(candidate)
        } else {
            base_dir.map(|base_dir| base_dir.join(candidate))
        };
        if let Some(path) = resolved
            && path.exists()
        {
            return LinkTarget::Local(path);
        }
    }

    LinkTarget::Web(url.to_string())
}

fn expand_home(path: &str) -> PathBuf {
    let Some(rest) = path.strip_prefix("~/") else {
        return PathBuf::from(path);
    };
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join(rest))
        .unwrap_or_else(|| PathBuf::from(path))
}

fn strip_line_suffix(path: &str) -> Option<String> {
    let (without_last, last) = path.rsplit_once(':')?;
    if last.is_empty() || !last.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    if let Some((without_line, line)) = without_last.rsplit_once(':')
        && !line.is_empty()
        && line.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Some(without_line.to_string());
    }
    Some(without_last.to_string())
}

fn strip_line_fragment(path: &str) -> Option<String> {
    let (without_fragment, line) = path.rsplit_once("#L")?;
    (!line.is_empty() && line.bytes().all(|byte| byte.is_ascii_digit()))
        .then(|| without_fragment.to_string())
}

#[cfg(test)]
mod tests {
    use std::{
        fs, process,
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::*;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock should be after Unix epoch")
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "tcode-markdown-link-target-{}-{nonce}",
                process::id()
            ));
            fs::create_dir_all(&path).expect("create temporary test directory");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }

        fn create_file(&self, relative: &str) -> PathBuf {
            let path = self.0.join(relative);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).expect("create parent directory");
            }
            fs::write(&path, b"test").expect("create temporary test file");
            path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn classifies_web_schemes_and_mailto() {
        assert_eq!(
            resolve_link("https://example.com/docs", None),
            LinkTarget::Web("https://example.com/docs".to_string())
        );
        assert_eq!(
            resolve_link("custom://resource", None),
            LinkTarget::Web("custom://resource".to_string())
        );
        assert_eq!(
            resolve_link("mailto:hello@example.com", None),
            LinkTarget::Web("mailto:hello@example.com".to_string())
        );
    }

    #[test]
    fn file_scheme_is_always_local() {
        assert_eq!(
            resolve_link("file:///tmp/does-not-need-to-exist", None),
            LinkTarget::Local(PathBuf::from("/tmp/does-not-need-to-exist"))
        );
    }

    #[test]
    fn resolves_existing_absolute_and_relative_paths() {
        let temp = TempDir::new();
        let absolute = temp.create_file("absolute.md");
        let relative = temp.create_file("src/main.rs");

        assert_eq!(
            resolve_link(absolute.to_str().expect("UTF-8 temp path"), None),
            LinkTarget::Local(absolute)
        );
        assert_eq!(
            resolve_link("src/main.rs", Some(temp.path())),
            LinkTarget::Local(relative)
        );
        assert_eq!(
            resolve_link("src/main.rs", None),
            LinkTarget::Web("src/main.rs".to_string())
        );
    }

    #[test]
    fn expands_leading_home_directory() {
        let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
            return;
        };
        assert_eq!(resolve_link("~/", None), LinkTarget::Local(home));
    }

    #[test]
    fn strips_line_and_column_suffixes_for_existing_paths() {
        let temp = TempDir::new();
        let path = temp.create_file("src/lib.rs");

        assert_eq!(
            resolve_link("src/lib.rs:42", Some(temp.path())),
            LinkTarget::Local(path.clone())
        );
        assert_eq!(
            resolve_link("src/lib.rs:42:7", Some(temp.path())),
            LinkTarget::Local(path)
        );
    }

    #[test]
    fn strips_line_fragment_for_existing_path() {
        let temp = TempDir::new();
        let path = temp.create_file("README.md");

        assert_eq!(
            resolve_link("README.md#L12", Some(temp.path())),
            LinkTarget::Local(path)
        );
    }

    #[test]
    fn nonexistent_path_falls_back_to_original_web_target() {
        let temp = TempDir::new();
        assert_eq!(
            resolve_link("missing/file.rs:42", Some(temp.path())),
            LinkTarget::Web("missing/file.rs:42".to_string())
        );
    }
}
