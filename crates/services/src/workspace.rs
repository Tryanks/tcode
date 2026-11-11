//! Workspace file/folder listing for the `@`-mention popover.
//!
//! Lists `git ls-files` (cached + untracked, gitignore-respected) when the
//! session cwd is in a git work tree; otherwise walks the directory, skipping
//! common build and dependency directories.

use std::collections::{BTreeSet, VecDeque};
use std::path::Path;
pub use tcode_protocol::{PathEntry, WorkspaceListing};

/// Entry cap for both listing paths: the listing is one reply, which is one
/// line of the transport, and a large tree's names would not fit in it. A
/// listing that reaches it says so.
const MAX_ENTRIES: usize = 8000;
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
/// `smol::unblock`. Git's error, or why `cwd` could not be read, when there is
/// nothing to list.
pub fn list_workspace(cwd: &Path) -> Result<WorkspaceListing, String> {
    if crate::git::is_git_repo(cwd)? {
        let text = crate::git::run_git(
            cwd,
            &[
                "ls-files",
                "-z",
                "--cached",
                "--others",
                "--exclude-standard",
            ],
        )?;
        return Ok(entries_from_relpaths(&text));
    }
    list_from_walk(cwd)
}

/// Parse NUL-separated relative file paths (`-z`, which leaves a path with
/// non-ASCII or special characters unquoted) into file + derived-folder
/// entries.
fn entries_from_relpaths(text: &str) -> WorkspaceListing {
    let mut files: Vec<PathEntry> = Vec::new();
    let mut dirs: BTreeSet<String> = BTreeSet::new();
    let mut lines = text.split('\0').filter(|rel| !rel.is_empty());
    let mut truncated = false;
    for rel in lines.by_ref() {
        if files.len() + dirs.len() >= MAX_ENTRIES {
            truncated = true;
            break;
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
    }
    let mut entries: Vec<PathEntry> = dirs
        .into_iter()
        .map(|d| PathEntry::from_rel(d, true))
        .collect();
    entries.extend(files);
    WorkspaceListing {
        entries,
        truncated,
        unreadable: Vec::new(),
    }
}

/// Walk a workspace that is not a git work tree, shallowest folders first.
fn list_from_walk(cwd: &Path) -> Result<WorkspaceListing, String> {
    let mut listing = WorkspaceListing {
        entries: Vec::new(),
        truncated: false,
        unreadable: Vec::new(),
    };
    let mut pending = VecDeque::from([String::new()]);
    while let Some(dir) = pending.pop_front() {
        let path = cwd.join(&dir);
        let read = match std::fs::read_dir(&path) {
            Ok(read) => read,
            Err(error) if dir.is_empty() => return Err(format!("{}: {error}", path.display())),
            Err(error) => {
                listing
                    .unreadable
                    .push(format!("{}: {error}", path.display()));
                continue;
            }
        };
        for entry in read {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    listing
                        .unreadable
                        .push(format!("{}: {error}", path.display()));
                    continue;
                }
            };
            let name = entry.file_name().to_string_lossy().into_owned();
            if SKIP_DIRS.contains(&name.as_str()) {
                continue;
            }
            if listing.entries.len() >= MAX_ENTRIES {
                listing.truncated = true;
                return Ok(listing);
            }
            let rel_path = if dir.is_empty() {
                name
            } else {
                format!("{dir}/{name}")
            };
            let is_dir = match entry.file_type() {
                Ok(kind) => kind.is_dir(),
                Err(error) => {
                    listing
                        .unreadable
                        .push(format!("{}: {error}", entry.path().display()));
                    false
                }
            };
            if is_dir {
                pending.push_back(rel_path.clone());
            }
            listing.entries.push(PathEntry::from_rel(rel_path, is_dir));
        }
    }
    Ok(listing)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_listing_respects_gitignore_and_falls_back_to_filtered_filesystem_walk() {
        let root = std::env::temp_dir().join(format!("tcode-workspace-{}", uuid::Uuid::new_v4()));
        for directory in ["src/ui", "node_modules", "target"] {
            std::fs::create_dir_all(root.join(directory)).unwrap();
        }
        for file in [
            "src/main.rs",
            "src/ui/composer.rs",
            "README.md",
            "node_modules/dependency.js",
            "target/output",
        ] {
            std::fs::write(root.join(file), "fixture").unwrap();
        }
        for git in [false, true] {
            if git {
                let output = crate::process::command("git")
                    .args(["-c", "init.templateDir=", "init", "-b", "main"])
                    .current_dir(&root)
                    .output()
                    .unwrap();
                assert!(
                    output.status.success(),
                    "{}",
                    String::from_utf8_lossy(&output.stderr)
                );
                std::fs::write(
                    root.join(".gitignore"),
                    "node_modules/\ntarget/\n.gitignore\n",
                )
                .unwrap();
            }
            let mut entries = list_workspace(&root).unwrap().entries;
            entries.sort_by(|left, right| left.rel_path.cmp(&right.rel_path));
            assert_eq!(
                entries
                    .iter()
                    .map(|entry| (entry.rel_path.as_str(), entry.is_dir))
                    .collect::<Vec<_>>(),
                [
                    ("README.md", false),
                    ("src", true),
                    ("src/main.rs", false),
                    ("src/ui", true),
                    ("src/ui/composer.rs", false),
                ],
                "git={git}"
            );
            let composer = entries
                .iter()
                .find(|entry| entry.rel_path == "src/ui/composer.rs")
                .unwrap();
            assert_eq!(composer.basename, "composer.rs");
            assert_eq!(composer.parent, "src/ui");
        }
        let _ = std::fs::remove_dir_all(&root);
    }
}
