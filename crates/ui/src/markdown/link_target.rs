use std::{
    cell::RefCell,
    collections::HashMap,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum LinkTarget {
    Web(String),
    Local(PathBuf),
}

impl LinkTarget {
    pub(super) fn tooltip_text(&self) -> String {
        match self {
            Self::Web(url) => url.clone(),
            Self::Local(path) => path.display().to_string(),
        }
    }
}

#[derive(Default)]
pub(super) struct LinkTargetCache {
    entries: RefCell<HashMap<String, LinkTarget>>,
}

impl LinkTargetCache {
    pub(super) fn resolve(&self, url: &str, base_dir: Option<&Path>) -> LinkTarget {
        if let Some(target) = self.entries.borrow().get(url) {
            return target.clone();
        }
        let target = resolve_link(url, base_dir);
        self.entries
            .borrow_mut()
            .insert(url.to_string(), target.clone());
        target
    }

    pub(super) fn clear(&mut self) {
        self.entries.get_mut().clear();
    }
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
                resolve_link(target, Some(&root)),
                LinkTarget::Web(target.into()),
                "{target}"
            );
        }
        for target in [
            "src/lib.rs",
            "src/lib.rs:42",
            "src/lib.rs:42:7",
            "src/lib.rs#L12",
            "src/lib.rs:42#L12",
        ] {
            assert_eq!(
                resolve_link(target, Some(&root)),
                LinkTarget::Local(file.clone()),
                "{target}"
            );
        }
        assert_eq!(
            resolve_link(file.to_str().unwrap(), None),
            LinkTarget::Local(file)
        );
        assert_eq!(
            resolve_link("src/lib.rs", None),
            LinkTarget::Web("src/lib.rs".into())
        );
        assert_eq!(
            resolve_link("file:///does-not-need-to-exist", None),
            LinkTarget::Local(PathBuf::from("/does-not-need-to-exist"))
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
