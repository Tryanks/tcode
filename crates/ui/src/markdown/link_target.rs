use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum LinkTarget {
    Web(String),
    Local(PathBuf),
    /// `tcode-thread://<thread_id>#<position>`: an entry of a Tcode thread.
    Thread(OpenThreadAnchor),
}

/// Open a thread at the entry its anchor names, as a cited anchor link does.
#[derive(gpui::Action, Clone, Debug, PartialEq, Eq, serde::Deserialize)]
#[action(namespace = tcode_markdown, no_json)]
pub struct OpenThreadAnchor {
    pub thread_id: String,
    pub position: Option<u64>,
}

/// The scheme of a link to a Tcode thread.
const THREAD_SCHEME: &str = "tcode-thread://";

impl OpenThreadAnchor {
    /// The link that opens this anchor.
    pub fn url(&self) -> String {
        match self.position {
            Some(position) => format!("{THREAD_SCHEME}{}#{position}", self.thread_id),
            None => format!("{THREAD_SCHEME}{}", self.thread_id),
        }
    }
}

impl LinkTarget {
    pub(super) fn tooltip_text(&self) -> String {
        match self {
            Self::Web(url) => url.clone(),
            Self::Local(path) => path.display().to_string(),
            Self::Thread(anchor) => match anchor.position {
                Some(position) => format!("{}#{position}", anchor.thread_id),
                None => anchor.thread_id.clone(),
            },
        }
    }

    pub(super) fn open(
        self,
        image_host: crate::store::ImageHost,
        window: &mut gpui::Window,
        cx: &mut gpui::App,
    ) {
        match self {
            Self::Web(url) => cx.open_url(&url),
            Self::Local(path) => {
                crate::remote::open_or_download_host_path(Some(image_host), path, window, cx);
            }
            Self::Thread(anchor) => window.dispatch_action(Box::new(anchor), cx),
        }
    }
}

pub(super) fn resolve_link(url: &str, base_dir: Option<&Path>, remote: bool) -> LinkTarget {
    if let Some(anchor) = url.strip_prefix(THREAD_SCHEME) {
        let (thread_id, position) = match anchor.split_once('#') {
            Some((id, position)) => (id, position.parse().ok()),
            None => (anchor, None),
        };
        return LinkTarget::Thread(OpenThreadAnchor {
            thread_id: thread_id.to_owned(),
            position,
        });
    }
    if let Some(path) = parse_file_url(url, remote) {
        return LinkTarget::Local(path);
    }
    if url.contains("://") || url.starts_with("mailto:") {
        return LinkTarget::Web(url.to_string());
    }
    if remote {
        return resolve_remote_link(url, base_dir);
    }

    let candidates = path_candidates(url);
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

/// On a remote host (such as the browser client), the client cannot probe the
/// host's filesystem with `Path::exists`. Treat explicit filesystem paths — and
/// relative paths when a working directory is known — as host files.
fn resolve_remote_link(url: &str, base_dir: Option<&Path>) -> LinkTarget {
    if url.is_empty() || url.starts_with('#') || url.starts_with("//") || has_uri_scheme(url) {
        return LinkTarget::Web(url.to_string());
    }
    let stripped = strip_location_suffix(url);
    if is_host_absolute_or_home(&stripped) {
        return LinkTarget::Local(PathBuf::from(stripped));
    }
    if let Some(base_dir) = base_dir {
        return LinkTarget::Local(base_dir.join(stripped));
    }
    LinkTarget::Web(url.to_string())
}

fn parse_file_url(url: &str, remote: bool) -> Option<PathBuf> {
    let raw = url.strip_prefix("file://")?;
    let decoded = if let Ok(parsed) = url::Url::parse(url)
        && parsed.scheme() == "file"
        && let Ok(path) = percent_encoding::percent_decode_str(parsed.path()).decode_utf8()
    {
        let path = match parsed.host_str() {
            Some(host) if !host.eq_ignore_ascii_case("localhost") => format!("//{host}{path}"),
            _ => path.into_owned(),
        };
        let path = path
            .strip_prefix('/')
            .filter(|rest| is_windows_drive_path(rest))
            .unwrap_or(&path);
        path.to_owned()
    } else {
        raw.to_owned()
    };
    if !remote && Path::new(&decoded).exists() {
        return Some(PathBuf::from(decoded));
    }
    Some(PathBuf::from(strip_location_suffix(&decoded)))
}

fn path_candidates(url: &str) -> Vec<String> {
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
    candidates
}

fn strip_location_suffix(path: &str) -> String {
    let mut current = path.to_owned();
    loop {
        if let Some(stripped) = strip_line_fragment(&current) {
            current = stripped;
            continue;
        }
        if let Some(stripped) = strip_line_suffix(&current) {
            current = stripped;
            continue;
        }
        return current;
    }
}

fn is_windows_drive_path(path: &str) -> bool {
    let bytes = path.as_bytes();
    bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'/' | b'\\')
}

fn is_host_absolute_or_home(path: &str) -> bool {
    path.starts_with('/')
        || path.starts_with("~/")
        || path.starts_with("~\\")
        || is_windows_drive_path(path)
}

fn has_uri_scheme(url: &str) -> bool {
    if is_windows_drive_path(url) {
        return false;
    }
    let Some((scheme, _rest)) = url.split_once(':') else {
        return false;
    };
    let mut bytes = scheme.bytes();
    let Some(first) = bytes.next() else {
        return false;
    };
    first.is_ascii_alphabetic()
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'-' | b'.'))
        // `README.md:42` has a `:line` suffix rather than a URI scheme.
        && strip_line_suffix(url).is_none()
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
    let (without_fragment, rest) = path.rsplit_once("#L")?;
    let valid = if let Some((start, end)) = rest.split_once('-') {
        let end = end.strip_prefix('L').unwrap_or(end);
        !start.is_empty()
            && !end.is_empty()
            && start.bytes().all(|byte| byte.is_ascii_digit())
            && end.bytes().all(|byte| byte.is_ascii_digit())
    } else {
        !rest.is_empty() && rest.bytes().all(|byte| byte.is_ascii_digit())
    };
    valid.then(|| without_fragment.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_resolve_existing_files_and_locations_without_rewriting_web_targets() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "tcode-markdown-links-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir_all(root.join("src")).unwrap();
        let file = root.join("src/lib.rs");
        std::fs::write(&file, "test").unwrap();

        for target in [
            "https://example.com/docs#L12",
            "custom://resource",
            "mailto:hello@example.com",
            "missing/file.rs:42",
            "src/lib.rs:line",
            "src/lib.rs#Lx",
        ] {
            assert_eq!(
                resolve_link(target, Some(&root), false),
                LinkTarget::Web(target.into()),
                "{target}"
            );
        }
        for target in [
            "src/lib.rs",
            "src/lib.rs:42",
            "src/lib.rs:42:7",
            "src/lib.rs#L12",
            "src/lib.rs#L12-L24",
            "src/lib.rs:42#L12",
        ] {
            assert_eq!(
                resolve_link(target, Some(&root), false),
                LinkTarget::Local(file.clone()),
                "{target}"
            );
        }
        assert_eq!(
            resolve_link(file.to_str().unwrap(), None, false),
            LinkTarget::Local(file)
        );
        assert_eq!(
            resolve_link("src/lib.rs", None, false),
            LinkTarget::Web("src/lib.rs".into())
        );
        assert_eq!(
            resolve_link("file:///does-not-need-to-exist", None, false),
            LinkTarget::Local(PathBuf::from("/does-not-need-to-exist"))
        );
        assert_eq!(
            resolve_link("file:///tmp/my%20report.pdf#L10-L20", None, false),
            LinkTarget::Local(PathBuf::from("/tmp/my report.pdf"))
        );
        assert_eq!(
            resolve_link("file:///tmp/src/lib.rs:42:7", None, true),
            LinkTarget::Local(PathBuf::from("/tmp/src/lib.rs"))
        );
        for (target, expected) in [
            ("/host/output/report.pdf", "/host/output/report.pdf"),
            ("/host/src/lib.rs:42:7", "/host/src/lib.rs"),
            ("~/downloads/archive.tar.gz", "~/downloads/archive.tar.gz"),
            ("src/lib.rs#L10-L20", "/repo/src/lib.rs"),
            ("README.md:12", "/repo/README.md"),
        ] {
            assert_eq!(
                resolve_link(target, Some(Path::new("/repo")), true),
                LinkTarget::Local(PathBuf::from(expected)),
                "{target}"
            );
        }
        for target in [
            "https://example.com/docs",
            "mailto:hello@example.com",
            "tel:+123456789",
            "//example.com/file.pdf",
            "#section-1",
        ] {
            assert_eq!(
                resolve_link(target, Some(Path::new("/repo")), true),
                LinkTarget::Web(target.into()),
                "{target}"
            );
        }
        for (target, position) in [
            ("tcode-thread://t-1#42", Some(42)),
            ("tcode-thread://t-1", None),
        ] {
            assert_eq!(
                resolve_link(target, None, false),
                LinkTarget::Thread(OpenThreadAnchor {
                    thread_id: "t-1".into(),
                    position,
                }),
                "{target}"
            );
        }
        let _ = std::fs::remove_dir_all(root);
    }
}
