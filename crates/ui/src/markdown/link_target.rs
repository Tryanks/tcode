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

    /// Open the target; `host` is the host a file it names is on.
    pub(super) fn open(
        self,
        host: crate::store::ImageHost,
        window: &mut gpui::Window,
        cx: &mut gpui::App,
    ) {
        match self {
            Self::Web(url) => cx.open_url(&url),
            Self::Local(path) => crate::host_file::open(host, &path, window, cx),
            Self::Thread(anchor) => window.dispatch_action(Box::new(anchor), cx),
        }
    }
}

/// `elsewhere`: the files are on a host this client is not on.
pub(super) fn resolve_link(url: &str, base_dir: Option<&Path>, elsewhere: bool) -> LinkTarget {
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
    if let Some(path) = file_url_path(url, elsewhere) {
        return LinkTarget::Local(path);
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

    // Another machine's files cannot be looked for from here, so what is
    // written as a path is taken as one, without the location it ends with,
    // and is absolute by that machine's rules, or under its home, which it
    // expands. What has a scheme of its own (`data:`) is not written as a
    // path.
    if elsewhere {
        let path = candidates.pop().unwrap_or_default();
        return if path.starts_with('/')
            || path.starts_with("~/")
            || path.starts_with("~\\")
            || crate::remote::windows_drive_path(&path)
        {
            LinkTarget::Local(PathBuf::from(path))
        } else if let Some(base_dir) = base_dir.filter(|_| url::Url::parse(&path).is_err()) {
            LinkTarget::Local(base_dir.join(path))
        } else {
            LinkTarget::Web(url.to_string())
        };
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

/// The file a `file://` URL names, percent-decoded and without the location
/// it ends with, unless a file of that very name is here to be found.
fn file_url_path(url: &str, elsewhere: bool) -> Option<PathBuf> {
    let raw = url.strip_prefix("file://")?;
    let decoded = if let Ok(parsed) = url::Url::parse(url)
        && let Ok(path) = percent_encoding::percent_decode_str(parsed.path()).decode_utf8()
    {
        let path = match parsed.host_str() {
            Some(host) if !host.eq_ignore_ascii_case("localhost") => format!("//{host}{path}"),
            _ => path.into_owned(),
        };
        // `file:///C:/app` names `C:/app`.
        match path.strip_prefix('/') {
            Some(drive) if crate::remote::windows_drive_path(drive) => drive.to_owned(),
            _ => path,
        }
    } else {
        raw.to_owned()
    };
    if !elsewhere && Path::new(&decoded).exists() {
        return Some(PathBuf::from(decoded));
    }
    let mut path = decoded;
    while let Some(stripped) = strip_line_fragment(&path).or_else(|| strip_line_suffix(&path)) {
        path = stripped;
    }
    Some(PathBuf::from(path))
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
    let (without_fragment, lines) = path.rsplit_once("#L")?;
    // `#L12`, or the range `#L12-L24` or `#L12-24`.
    let (first, last) = match lines.split_once('-') {
        Some((first, last)) => (first, Some(last.strip_prefix('L').unwrap_or(last))),
        None => (lines, None),
    };
    let number = |line: &str| !line.is_empty() && line.bytes().all(|byte| byte.is_ascii_digit());
    (number(first) && last.is_none_or(number)).then(|| without_fragment.to_string())
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
        // A host elsewhere: none of these is on this client to be found.
        for (target, path) in [
            ("/srv/app/out/report.pdf", "/srv/app/out/report.pdf"),
            ("/srv/app/src/lib.rs:42:7", "/srv/app/src/lib.rs"),
            ("file:///srv/app/src/lib.rs:42:7", "/srv/app/src/lib.rs"),
            ("~/downloads/archive.tar.gz", "~/downloads/archive.tar.gz"),
            ("src/lib.rs#L12", "/srv/app/src/lib.rs"),
            ("C:\\app\\src\\lib.rs:42", "C:\\app\\src\\lib.rs"),
        ] {
            assert_eq!(
                resolve_link(target, Some(Path::new("/srv/app")), true),
                LinkTarget::Local(PathBuf::from(path)),
                "{target}"
            );
        }
        for target in ["https://example.com/docs#L12", "data:text/plain,hello"] {
            assert_eq!(
                resolve_link(target, Some(&root), true),
                LinkTarget::Web(target.into()),
                "{target}"
            );
        }
        let _ = std::fs::remove_dir_all(root);
    }
}
