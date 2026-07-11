//! Workspace file/folder listing for the `@`-mention popover.
//!
//! Prefers `git ls-files` (cached + untracked, gitignore-respected) when the
//! session cwd is a git repo; otherwise falls back to a bounded recursive walk
//! that skips the usual noise directories. No external crates — a plain
//! `std::process`/`std::fs` implementation (the `ignore` crate is intentionally
//! avoided per the no-new-deps constraint).

use std::collections::BTreeSet;
use std::path::Path;

/// One listable workspace entry (relative to the session cwd, `/`-separated).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathEntry {
    /// Path relative to the workspace root, using `/` separators.
    pub rel_path: String,
    /// Final path component.
    pub basename: String,
    /// Parent path (`rel_path` without the basename), possibly empty.
    pub parent: String,
    pub is_dir: bool,
}

impl PathEntry {
    fn from_rel(rel_path: String, is_dir: bool) -> Self {
        let (parent, basename) = match rel_path.rfind('/') {
            Some(i) => (rel_path[..i].to_string(), rel_path[i + 1..].to_string()),
            None => (String::new(), rel_path.clone()),
        };
        Self {
            rel_path,
            basename,
            parent,
            is_dir,
        }
    }
}

/// Caps for the fallback walk (git listing is naturally bounded by the repo).
const MAX_ENTRIES: usize = 8000;
const MAX_DEPTH: usize = 8;
/// Directories the fallback walk never descends into.
const SKIP_DIRS: &[&str] = &[
    ".git",
    "node_modules",
    "target",
    "dist",
    "build",
    ".next",
    ".venv",
    "venv",
    "__pycache__",
    ".idea",
    ".cache",
];

/// List workspace files and folders under `cwd`. Blocking — call via
/// `smol::unblock`. Empty when `cwd` is unreadable.
pub fn list_workspace(cwd: &Path) -> Vec<PathEntry> {
    if let Some(entries) = list_from_git(cwd) {
        return entries;
    }
    list_from_walk(cwd)
}

/// `git ls-files --cached --others --exclude-standard`, then synthesize the set
/// of parent directories as folder entries. Returns `None` when not a repo or
/// git is unavailable.
fn list_from_git(cwd: &Path) -> Option<Vec<PathEntry>> {
    let output = crate::process::command("git")
        .args(["ls-files", "--cached", "--others", "--exclude-standard"])
        .current_dir(cwd)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    Some(entries_from_relpaths(&text))
}

/// Parse newline-separated relative file paths into file + derived-folder entries.
fn entries_from_relpaths(text: &str) -> Vec<PathEntry> {
    let mut files: Vec<PathEntry> = Vec::new();
    let mut dirs: BTreeSet<String> = BTreeSet::new();
    for line in text.lines() {
        let rel = line.trim();
        if rel.is_empty() {
            continue;
        }
        // Accumulate every ancestor directory of this file.
        let mut acc = String::new();
        let parts: Vec<&str> = rel.split('/').collect();
        for part in &parts[..parts.len().saturating_sub(1)] {
            if !acc.is_empty() {
                acc.push('/');
            }
            acc.push_str(part);
            dirs.insert(acc.clone());
        }
        files.push(PathEntry::from_rel(rel.to_string(), false));
        if files.len() + dirs.len() >= MAX_ENTRIES {
            break;
        }
    }
    let mut entries: Vec<PathEntry> = dirs
        .into_iter()
        .map(|d| PathEntry::from_rel(d, true))
        .collect();
    entries.extend(files);
    entries
}

/// Bounded recursive walk fallback (non-git workspaces).
fn list_from_walk(cwd: &Path) -> Vec<PathEntry> {
    let mut entries = Vec::new();
    let mut stack: Vec<(std::path::PathBuf, usize)> = vec![(cwd.to_path_buf(), 0)];
    while let Some((dir, depth)) = stack.pop() {
        if depth > MAX_DEPTH || entries.len() >= MAX_ENTRIES {
            continue;
        }
        let Ok(read) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in read.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') && depth == 0 && name != ".git" {
                // keep dotfiles at other depths, but skip a leading `.` dir only
                // when it is in SKIP_DIRS (handled below).
            }
            if SKIP_DIRS.contains(&name.as_str()) {
                continue;
            }
            let path = entry.path();
            let Ok(rel) = path.strip_prefix(cwd) else {
                continue;
            };
            let rel_path = rel.to_string_lossy().replace('\\', "/");
            let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
            entries.push(PathEntry::from_rel(rel_path, is_dir));
            if is_dir {
                stack.push((path, depth + 1));
            }
            if entries.len() >= MAX_ENTRIES {
                break;
            }
        }
    }
    entries
}

/// A ranked filter over workspace entries (case-insensitive), capped at `limit`.
/// Basename matches rank above path-only matches; a basename prefix match ranks
/// first. Empty query returns the first `limit` entries.
pub fn filter_entries<'a>(
    entries: &'a [PathEntry],
    query: &str,
    limit: usize,
) -> Vec<&'a PathEntry> {
    if query.is_empty() {
        return entries.iter().take(limit).collect();
    }
    let q = query.to_lowercase();
    let mut scored: Vec<(u8, usize, &PathEntry)> = Vec::new();
    for entry in entries {
        let base = entry.basename.to_lowercase();
        let path = entry.rel_path.to_lowercase();
        let rank = if base.starts_with(&q) {
            0
        } else if base.contains(&q) {
            1
        } else if path.contains(&q) {
            2
        } else {
            continue;
        };
        scored.push((rank, entry.rel_path.len(), entry));
    }
    scored.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)).then(a.2.rel_path.cmp(&b.2.rel_path)));
    scored.into_iter().take(limit).map(|(_, _, e)| e).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_folders_from_git_paths() {
        let entries = entries_from_relpaths("src/main.rs\nsrc/ui/composer.rs\nREADME.md\n");
        // Folders src, src/ui come first (dirs before files), then files.
        assert!(entries.iter().any(|e| e.rel_path == "src" && e.is_dir));
        assert!(entries.iter().any(|e| e.rel_path == "src/ui" && e.is_dir));
        assert!(entries
            .iter()
            .any(|e| e.rel_path == "src/ui/composer.rs" && !e.is_dir));
        let readme = entries.iter().find(|e| e.rel_path == "README.md").unwrap();
        assert_eq!(readme.basename, "README.md");
        assert_eq!(readme.parent, "");
    }

    #[test]
    fn parent_and_basename_split() {
        let e = PathEntry::from_rel("a/b/c.rs".to_string(), false);
        assert_eq!(e.basename, "c.rs");
        assert_eq!(e.parent, "a/b");
    }

    #[test]
    fn filter_ranks_basename_prefix_first() {
        let entries = vec![
            PathEntry::from_rel("src/composer.rs".into(), false),
            PathEntry::from_rel("docs/decompose.md".into(), false),
            PathEntry::from_rel("src/ui/composer_trigger.rs".into(), false),
        ];
        let out = filter_entries(&entries, "compo", 10);
        // `composer.rs` (basename prefix) ranks above `decompose.md` (basename
        // contains) which ranks above nothing here.
        assert_eq!(out[0].rel_path, "src/composer.rs");
        assert!(out.iter().any(|e| e.rel_path == "src/ui/composer_trigger.rs"));
        assert!(out.iter().any(|e| e.rel_path == "docs/decompose.md"));
    }

    #[test]
    fn empty_query_returns_capped_prefix() {
        let entries: Vec<PathEntry> = (0..10)
            .map(|i| PathEntry::from_rel(format!("f{i}.txt"), false))
            .collect();
        assert_eq!(filter_entries(&entries, "", 3).len(), 3);
    }
}
