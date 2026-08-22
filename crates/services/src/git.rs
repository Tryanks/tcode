//! App-owned Git process and filesystem infrastructure.

use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};

use agent::{FileChange, FileChangeKind};
use tcode_core::git::{GitAction, GitStatus, parse_status};
pub use tcode_protocol::{GitDiffResult, GitDiffScope, GitFileText};

const MAX_RAW_DIFF_BYTES: usize = 200 * 1024;
const MAX_FILE_TEXT_BYTES: u64 = 512 * 1024;
const WORKTREE_SEED_LIMIT_BYTES: u64 = 512 * 1024 * 1024;

fn working_tree_diff_args(ignore_whitespace: bool) -> Vec<String> {
    let mut args = vec!["diff".into(), "HEAD".into()];
    if ignore_whitespace {
        args.push("-w".into());
    }
    args.push("--".into());
    args
}

fn merge_base_args(base: &str) -> Vec<String> {
    vec!["merge-base".into(), base.into(), "HEAD".into()]
}

fn branch_diff_args(merge_base: &str, ignore_whitespace: bool) -> Vec<String> {
    let mut args = vec!["diff".into(), format!("{merge_base}...HEAD")];
    if ignore_whitespace {
        args.push("-w".into());
    }
    args.push("--".into());
    args
}

struct ParsedFileChange {
    change: FileChange,
    old_path: Option<String>,
    new_path: Option<String>,
}

fn git_output(cwd: &Path, args: &[String]) -> Result<std::process::Output, String> {
    crate::process::command("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .map_err(|error| error.to_string())
}

fn append_capped(raw: &mut Vec<u8>, bytes: &[u8], truncated: &mut bool) {
    let remaining = MAX_RAW_DIFF_BYTES.saturating_sub(raw.len());
    raw.extend_from_slice(&bytes[..bytes.len().min(remaining)]);
    *truncated |= bytes.len() > remaining;
}

fn patch_path(value: &str, side_prefix: Option<&str>) -> Option<String> {
    let value = value.trim_end_matches('\t');
    let value = value
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .unwrap_or(value);
    if value == "/dev/null" {
        return None;
    }
    Some(
        side_prefix
            .and_then(|prefix| value.strip_prefix(prefix))
            .unwrap_or(value)
            .to_string(),
    )
}

fn split_git_patch(raw: &str, cwd: &Path, repo_prefix: &str) -> Vec<ParsedFileChange> {
    let mut sections = Vec::new();
    let mut current = String::new();
    for line in raw.lines() {
        if line.starts_with("diff --git ") && !current.is_empty() {
            sections.push(std::mem::take(&mut current));
        }
        if !current.is_empty() || line.starts_with("diff --git ") {
            current.push_str(line);
            current.push('\n');
        }
    }
    if !current.is_empty() {
        sections.push(current);
    }
    sections
        .into_iter()
        .filter_map(|patch| {
            let old_path = patch
                .lines()
                .find_map(|line| {
                    line.strip_prefix("rename from ")
                        .and_then(|path| patch_path(path, None))
                })
                .or_else(|| {
                    patch.lines().find_map(|line| {
                        line.strip_prefix("--- ")
                            .and_then(|path| patch_path(path, Some("a/")))
                    })
                });
            let new_path = patch
                .lines()
                .find_map(|line| {
                    line.strip_prefix("rename to ")
                        .and_then(|path| patch_path(path, None))
                })
                .or_else(|| {
                    patch.lines().find_map(|line| {
                        line.strip_prefix("+++ ")
                            .and_then(|path| patch_path(path, Some("b/")))
                    })
                });
            let path = new_path.as_deref().or(old_path.as_deref())?;
            let cwd_path = path.strip_prefix(repo_prefix).unwrap_or(path);
            Some(ParsedFileChange {
                change: FileChange {
                    path: cwd.join(cwd_path).to_string_lossy().to_string(),
                    kind: if old_path.is_none() {
                        FileChangeKind::Create
                    } else if new_path.is_none() {
                        FileChangeKind::Delete
                    } else if patch.lines().any(|line| line.starts_with("rename to ")) {
                        FileChangeKind::Rename
                    } else {
                        FileChangeKind::Modify
                    },
                    diff: Some(patch),
                },
                old_path,
                new_path,
            })
        })
        .collect()
}

fn repo_prefix(cwd: &Path) -> String {
    let args = vec!["rev-parse".into(), "--show-prefix".into()];
    git_output(cwd, &args)
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|prefix| prefix.trim().to_string())
        .unwrap_or_default()
}

fn repo_root_path(path: &str, prefix: &str) -> String {
    if prefix.is_empty() || path.starts_with(prefix) {
        path.to_string()
    } else {
        format!("{prefix}{path}")
    }
}

fn read_disk_text(path: &Path) -> Option<String> {
    let metadata = std::fs::metadata(path).ok()?;
    if metadata.len() > MAX_FILE_TEXT_BYTES {
        return None;
    }
    std::fs::read_to_string(path).ok()
}

fn read_git_text(cwd: &Path, revision: &str, path: &str) -> Option<String> {
    let args = vec!["show".into(), format!("{revision}:{path}")];
    let output = git_output(cwd, &args).ok()?;
    if !output.status.success() || output.stdout.len() as u64 > MAX_FILE_TEXT_BYTES {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}

fn load_file_texts(
    cwd: &Path,
    scope: GitDiffScope,
    base_revision: Option<&str>,
    prefix: &str,
    parsed: &[ParsedFileChange],
) -> Vec<GitFileText> {
    parsed
        .iter()
        .map(|parsed| {
            let old_path = parsed
                .old_path
                .as_deref()
                .map(|path| repo_root_path(path, prefix));
            let new_path = parsed
                .new_path
                .as_deref()
                .map(|path| repo_root_path(path, prefix));
            match scope {
                GitDiffScope::WorkingTree => GitFileText {
                    old: old_path
                        .as_deref()
                        .and_then(|path| read_git_text(cwd, "HEAD", path)),
                    new: new_path
                        .as_deref()
                        .and_then(|_| read_disk_text(Path::new(&parsed.change.path))),
                },
                GitDiffScope::Branch => GitFileText {
                    old: base_revision.and_then(|revision| {
                        old_path
                            .as_deref()
                            .and_then(|path| read_git_text(cwd, revision, path))
                    }),
                    new: new_path
                        .as_deref()
                        .and_then(|path| read_git_text(cwd, "HEAD", path)),
                },
                GitDiffScope::Unknown => GitFileText::default(),
            }
        })
        .collect()
}

fn git_branches(cwd: &Path) -> (Vec<String>, Option<String>) {
    let args = vec![
        "for-each-ref".into(),
        "--format=%(refname:short)".into(),
        "refs/heads".into(),
    ];
    let mut branches = git_output(cwd, &args)
        .ok()
        .filter(|output| output.status.success())
        .map(|output| parse_branch_list(&String::from_utf8_lossy(&output.stdout)))
        .unwrap_or_default();
    let origin_args = vec![
        "symbolic-ref".into(),
        "--quiet".into(),
        "--short".into(),
        "refs/remotes/origin/HEAD".into(),
    ];
    let origin_default = git_output(cwd, &origin_args)
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    if let Some(origin) = &origin_default
        && !branches.contains(origin)
    {
        branches.push(origin.clone());
    }
    let default = ["main", "master"]
        .into_iter()
        .find(|candidate| branches.iter().any(|branch| branch == candidate))
        .map(str::to_string)
        .or(origin_default)
        .or_else(|| branches.first().cloned());
    (branches, default)
}

pub fn load_git_diff(
    cwd: &Path,
    scope: GitDiffScope,
    base: Option<&str>,
    ignore_whitespace: bool,
) -> GitDiffResult {
    let (branches, default_base) = git_branches(cwd);
    let mut base_revision = None;
    let args = match scope {
        GitDiffScope::WorkingTree => working_tree_diff_args(ignore_whitespace),
        GitDiffScope::Branch => {
            let base = base.or(default_base.as_deref()).unwrap_or("HEAD");
            let merge_base = match git_output(cwd, &merge_base_args(base)) {
                Ok(output) if output.status.success() => {
                    String::from_utf8_lossy(&output.stdout).trim().to_string()
                }
                Ok(output) => {
                    return GitDiffResult {
                        error: Some(String::from_utf8_lossy(&output.stderr).trim().to_string()),
                        branches,
                        default_base,
                        ..GitDiffResult::default()
                    };
                }
                Err(error) => {
                    return GitDiffResult {
                        error: Some(error),
                        branches,
                        default_base,
                        ..GitDiffResult::default()
                    };
                }
            };
            let args = branch_diff_args(&merge_base, ignore_whitespace);
            base_revision = Some(merge_base);
            args
        }
        GitDiffScope::Unknown => {
            return GitDiffResult {
                error: Some("unknown git diff scope".into()),
                branches,
                default_base,
                ..GitDiffResult::default()
            };
        }
    };
    let output = match git_output(cwd, &args) {
        Ok(output) if output.status.success() => output,
        Ok(output) => {
            return GitDiffResult {
                error: Some(String::from_utf8_lossy(&output.stderr).trim().to_string()),
                branches,
                default_base,
                ..GitDiffResult::default()
            };
        }
        Err(error) => {
            return GitDiffResult {
                error: Some(error),
                branches,
                default_base,
                ..GitDiffResult::default()
            };
        }
    };
    let mut raw = Vec::new();
    let mut truncated = false;
    append_capped(&mut raw, &output.stdout, &mut truncated);
    if scope == GitDiffScope::WorkingTree && !truncated {
        let untracked_args = vec![
            "ls-files".into(),
            "--others".into(),
            "--exclude-standard".into(),
            "-z".into(),
        ];
        if let Ok(untracked) = git_output(cwd, &untracked_args)
            && untracked.status.success()
        {
            for path in untracked
                .stdout
                .split(|byte| *byte == 0)
                .filter(|p| !p.is_empty())
            {
                let path = String::from_utf8_lossy(path).to_string();
                let args = vec!["diff".into(), "--no-index".into(), "/dev/null".into(), path];
                if let Ok(output) = git_output(cwd, &args)
                    && (output.status.success() || output.status.code() == Some(1))
                {
                    append_capped(&mut raw, &output.stdout, &mut truncated);
                }
                if truncated {
                    break;
                }
            }
        }
    }
    let raw = String::from_utf8_lossy(&raw);
    let prefix = repo_prefix(cwd);
    let parsed = split_git_patch(&raw, cwd, &prefix);
    let texts = load_file_texts(cwd, scope, base_revision.as_deref(), &prefix, &parsed);
    let changes = parsed.into_iter().map(|parsed| parsed.change).collect();
    GitDiffResult {
        changes,
        texts,
        truncated,
        error: None,
        branches,
        default_base,
    }
}

/// Read the current git branch (or short detached-HEAD sha) for `cwd`, if it is
/// a git repository. Reads `.git/HEAD` directly (no git process); returns None
/// when `cwd` is not a repo. Worktrees/submodules (`.git` is a file) are treated
/// as non-repos here — the below-card branch row simply hides.
pub fn read_git_branch(cwd: &Path) -> Option<String> {
    let head = std::fs::read_to_string(cwd.join(".git").join("HEAD")).ok()?;
    let head = head.trim();
    if let Some(reference) = head.strip_prefix("ref: ") {
        // e.g. "refs/heads/feature/x" -> "feature/x"
        let name = reference.strip_prefix("refs/heads/").unwrap_or(reference);
        (!name.is_empty()).then(|| name.to_string())
    } else if !head.is_empty() {
        // Detached HEAD: show the short commit sha.
        Some(head.chars().take(7).collect())
    } else {
        None
    }
}

/// Parse `git for-each-ref` output into a list of branch names (blank lines
/// dropped, whitespace trimmed).
fn parse_branch_list(output: &str) -> Vec<String> {
    output
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}

/// List local git branches for `cwd` (empty when not a repo / git fails).
pub fn list_git_branches(cwd: &Path) -> Vec<String> {
    let output = crate::process::command("git")
        .args(["for-each-ref", "refs/heads", "--format=%(refname:short)"])
        .current_dir(cwd)
        .output();
    match output {
        Ok(out) if out.status.success() => parse_branch_list(&String::from_utf8_lossy(&out.stdout)),
        _ => Vec::new(),
    }
}

/// Why a `git checkout` was refused.
#[derive(Debug, PartialEq, Eq)]
pub enum CheckoutError {
    /// The working tree has uncommitted changes.
    Dirty,
    /// git failed (spawn error or non-zero checkout).
    Git(String),
}

/// Check out `branch` in `cwd` iff the working tree is clean.
pub fn checkout_if_clean(cwd: &Path, branch: &str) -> Result<(), CheckoutError> {
    let status = crate::process::command("git")
        .args(["status", "--porcelain"])
        .current_dir(cwd)
        .output()
        .map_err(|e| CheckoutError::Git(format!("git status failed: {e}")))?;
    if !status.status.success() {
        let stderr = String::from_utf8_lossy(&status.stderr);
        return Err(CheckoutError::Git(format!(
            "git status failed: {}",
            stderr.trim()
        )));
    }
    if !status.stdout.is_empty() {
        return Err(CheckoutError::Dirty);
    }
    let checkout = crate::process::command("git")
        .args(["checkout", branch])
        .current_dir(cwd)
        .output()
        .map_err(|e| CheckoutError::Git(format!("git checkout failed: {e}")))?;
    if !checkout.status.success() {
        let stderr = String::from_utf8_lossy(&checkout.stderr);
        return Err(CheckoutError::Git(format!(
            "git checkout failed: {}",
            stderr.trim()
        )));
    }
    Ok(())
}

/// Raw process detail from a git worktree operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitWorktreeError(String);

impl std::fmt::Display for GitWorktreeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for GitWorktreeError {}

/// Result of copying the paths selected by `.worktreeinclude`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorktreeSeedSummary {
    pub manifest_found: bool,
    pub copied_files: usize,
    pub skipped: Vec<String>,
    pub limit_reached: bool,
}

/// A newly-created dedicated worktree and the branch Git actually assigned it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreatedGitWorktree {
    pub path: PathBuf,
    pub branch: String,
    pub seed: WorktreeSeedSummary,
}

/// Result of checking the app-owned worktree directory for orphaned sessions.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorktreeCleanupSummary {
    pub removed: Vec<PathBuf>,
    pub skipped: Vec<PathBuf>,
}

/// How a clean dedicated-worktree branch was integrated into its original
/// checkout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeWorktreeOutcome {
    FastForward,
    MergeCommit,
}

/// A merge-back refusal or failure. Preconditions are deliberately represented
/// separately so callers can localize the actionable cases without parsing Git
/// output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeWorktreeError {
    WorktreeMissing,
    DirtyWorktree,
    DestinationDetached,
    DirtyDestination,
    DivergedConflict,
    Git(String),
}

impl std::fmt::Display for MergeWorktreeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::WorktreeMissing => formatter.write_str("worktree is missing"),
            Self::DirtyWorktree => formatter.write_str("worktree has uncommitted changes"),
            Self::DestinationDetached => formatter.write_str("destination is not on a branch"),
            Self::DirtyDestination => formatter.write_str("destination has uncommitted changes"),
            Self::DivergedConflict => formatter.write_str("branches diverged and conflict"),
            Self::Git(error) => formatter.write_str(error),
        }
    }
}

impl std::error::Error for MergeWorktreeError {}

/// The path a session's dedicated worktree lives at (`~/.tcode/worktrees/<id>`),
/// falling back to a temp dir when the home directory is unknown.
pub fn worktree_path_for(session_id: &str) -> PathBuf {
    worktrees_root().join(session_id)
}

fn worktrees_root() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join(".tcode")
        .join("worktrees")
}

/// Create a dedicated worktree at `path` branching `branch` from `base`, run
/// from the project checkout `root`.
///
/// If `branch` already exists, a numeric suffix is appended (starting at `-2`)
/// rather than reusing mutable state left by an earlier session. An existing,
/// unregistered target path is treated as crash residue: Git's worktree records
/// are pruned before creation is retried once.
///
/// After Git materializes tracked files, paths selected by the optional
/// `.worktreeinclude` are copied (never linked) from `root`. Existing destination
/// files are not overwritten. The list is user-controlled and may copy secrets
/// such as `.env.local` into the app-owned `~/.tcode/worktrees` directory.
pub fn create_git_worktree(
    root: &Path,
    path: &Path,
    branch: &str,
    base: &str,
) -> Result<CreatedGitWorktree, GitWorktreeError> {
    create_git_worktree_with_seed_limit(root, path, branch, base, WORKTREE_SEED_LIMIT_BYTES)
}

fn create_git_worktree_with_seed_limit(
    root: &Path,
    path: &Path,
    branch: &str,
    base: &str,
    seed_limit: u64,
) -> Result<CreatedGitWorktree, GitWorktreeError> {
    let seed_plan = read_worktree_seed_plan(root)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| GitWorktreeError(error.to_string()))?;
    }
    let actual_branch = available_worktree_branch(root, branch)?;
    if path_is_registered_worktree(root, path)? {
        return Err(GitWorktreeError(format!(
            "worktree target is already registered: {}",
            path.display()
        )));
    }
    if path.exists() {
        prune_worktrees(root)?;
    }

    let mut out = add_worktree(root, path, &actual_branch, base)?;
    if !out.status.success() && !path_is_registered_worktree(root, path)? {
        prune_worktrees(root)?;
        out = add_worktree(root, path, &actual_branch, base)?;
    }
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(GitWorktreeError(stderr.trim().to_string()));
    }

    let seed = match seed_worktree(path, seed_plan, seed_limit) {
        Ok(summary) => summary,
        Err(error) => {
            if let Err(cleanup_error) = remove_git_worktree(root, path) {
                log::warn!(
                    "failed to remove worktree after seeding failed at {}: {cleanup_error}",
                    path.display()
                );
            }
            return Err(error);
        }
    };
    Ok(CreatedGitWorktree {
        path: path.to_path_buf(),
        branch: actual_branch,
        seed,
    })
}

fn add_worktree(
    root: &Path,
    path: &Path,
    branch: &str,
    base: &str,
) -> Result<std::process::Output, GitWorktreeError> {
    crate::process::command("git")
        .current_dir(root)
        .args([
            "worktree",
            "add",
            "-b",
            branch,
            &path.to_string_lossy(),
            base,
        ])
        .output()
        .map_err(|error| GitWorktreeError(error.to_string()))
}

fn available_worktree_branch(root: &Path, requested: &str) -> Result<String, GitWorktreeError> {
    for suffix in 1_u64.. {
        let candidate = if suffix == 1 {
            requested.to_string()
        } else {
            format!("{requested}-{suffix}")
        };
        let status = crate::process::command("git")
            .current_dir(root)
            .args([
                "show-ref",
                "--verify",
                "--quiet",
                &format!("refs/heads/{candidate}"),
            ])
            .status()
            .map_err(|error| GitWorktreeError(error.to_string()))?;
        if !status.success() {
            return Ok(candidate);
        }
    }
    unreachable!("the branch suffix space is finite but cannot be exhausted in practice")
}

fn path_is_registered_worktree(root: &Path, path: &Path) -> Result<bool, GitWorktreeError> {
    Ok(registered_worktree_path(root, path)?.is_some())
}

fn registered_worktree_path(root: &Path, path: &Path) -> Result<Option<PathBuf>, GitWorktreeError> {
    let out = crate::process::command("git")
        .current_dir(root)
        .args(["worktree", "list", "--porcelain"])
        .output()
        .map_err(|error| GitWorktreeError(error.to_string()))?;
    if !out.status.success() {
        return Err(GitWorktreeError(
            String::from_utf8_lossy(&out.stderr).trim().to_string(),
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| line.strip_prefix("worktree "))
        .map(PathBuf::from)
        .find(|registered| paths_refer_to_same_existing_path(registered, path)))
}

/// The main working tree of the repository `path` belongs to: the first
/// `worktree ` entry in porcelain output. Removal must run from here — on
/// Windows, `git worktree remove` fails when the process cwd is inside the
/// worktree being removed.
fn main_worktree_root(path: &Path) -> Result<PathBuf, GitWorktreeError> {
    let out = crate::process::command("git")
        .current_dir(path)
        .args(["worktree", "list", "--porcelain"])
        .output()
        .map_err(|error| GitWorktreeError(error.to_string()))?;
    if !out.status.success() {
        return Err(GitWorktreeError(
            String::from_utf8_lossy(&out.stderr).trim().to_string(),
        ));
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find_map(|line| line.strip_prefix("worktree "))
        .map(PathBuf::from)
        .ok_or_else(|| GitWorktreeError("git worktree list returned no entries".into()))
}

fn paths_refer_to_same_existing_path(left: &Path, right: &Path) -> bool {
    if left == right {
        return true;
    }
    match (left.canonicalize(), right.canonicalize()) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

fn prune_worktrees(root: &Path) -> Result<(), GitWorktreeError> {
    let out = crate::process::command("git")
        .current_dir(root)
        .args(["worktree", "prune"])
        .output()
        .map_err(|error| GitWorktreeError(error.to_string()))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(GitWorktreeError(
            String::from_utf8_lossy(&out.stderr).trim().to_string(),
        ))
    }
}

/// Remove the worktree at `path` (force), run from the project checkout `root`.
/// A path already removed outside tcode is treated as success and stale Git
/// metadata is pruned.
pub fn remove_git_worktree(root: &Path, path: &Path) -> Result<(), GitWorktreeError> {
    if !path.exists() {
        return prune_worktrees(root);
    }
    let registered_path =
        registered_worktree_path(root, path)?.unwrap_or_else(|| path.to_path_buf());
    let out = crate::process::command("git")
        .current_dir(root)
        .args([
            "worktree",
            "remove",
            "--force",
            &registered_path.to_string_lossy(),
        ])
        .output()
        .map_err(|e| GitWorktreeError(e.to_string()))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(GitWorktreeError(stderr.trim().to_string()));
    }
    Ok(())
}

/// Integrate a dedicated worktree branch into the branch currently checked out
/// at `destination`. Both trees must be clean. A descendant is fast-forwarded;
/// otherwise Git may create a merge commit. Conflicts are always aborted, and
/// this function never checks out, resets, or removes either tree.
pub fn merge_worktree_back(
    destination: &Path,
    worktree: &Path,
    branch: &str,
) -> Result<MergeWorktreeOutcome, MergeWorktreeError> {
    if !worktree.is_dir() {
        return Err(MergeWorktreeError::WorktreeMissing);
    }
    if !git_tree_is_clean(worktree)? {
        return Err(MergeWorktreeError::DirtyWorktree);
    }
    run_git(destination, &["symbolic-ref", "--quiet", "--short", "HEAD"])
        .map_err(|_| MergeWorktreeError::DestinationDetached)?;
    if !git_tree_is_clean(destination)? {
        return Err(MergeWorktreeError::DirtyDestination);
    }

    let ancestor = crate::process::command("git")
        .args(["merge-base", "--is-ancestor", "HEAD", branch])
        .current_dir(destination)
        .status()
        .map_err(|error| MergeWorktreeError::Git(error.to_string()))?;
    if ancestor.success() {
        run_git(destination, &["merge", "--ff-only", branch]).map_err(MergeWorktreeError::Git)?;
        return Ok(MergeWorktreeOutcome::FastForward);
    }
    if ancestor.code() != Some(1) {
        return Err(MergeWorktreeError::Git(
            "git merge-base --is-ancestor failed".into(),
        ));
    }

    match run_git(destination, &["merge", "--no-ff", "--no-edit", branch]) {
        Ok(_) => Ok(MergeWorktreeOutcome::MergeCommit),
        Err(error) => {
            let conflicted = destination.join(".git/MERGE_HEAD").exists()
                || run_git(destination, &["diff", "--name-only", "--diff-filter=U"])
                    .is_ok_and(|paths| !paths.trim().is_empty());
            if conflicted {
                run_git(destination, &["merge", "--abort"]).map_err(MergeWorktreeError::Git)?;
                Err(MergeWorktreeError::DivergedConflict)
            } else {
                Err(MergeWorktreeError::Git(error))
            }
        }
    }
}

fn git_tree_is_clean(cwd: &Path) -> Result<bool, MergeWorktreeError> {
    run_git(cwd, &["status", "--porcelain"])
        .map(|status| status.trim().is_empty())
        .map_err(MergeWorktreeError::Git)
}

#[derive(Debug)]
struct WorktreeSeedPlan {
    manifest_found: bool,
    root: PathBuf,
    entries: Vec<(String, PathBuf, PathBuf)>,
    missing: Vec<String>,
}

fn read_worktree_seed_plan(root: &Path) -> Result<WorktreeSeedPlan, GitWorktreeError> {
    let canonical_root = root.canonicalize().map_err(|error| {
        GitWorktreeError(format!(
            "cannot resolve repository root {}: {error}",
            root.display()
        ))
    })?;
    let manifest = root.join(".worktreeinclude");
    let contents = match std::fs::read_to_string(&manifest) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(WorktreeSeedPlan {
                manifest_found: false,
                root: canonical_root,
                entries: Vec::new(),
                missing: Vec::new(),
            });
        }
        Err(error) => {
            return Err(GitWorktreeError(format!(
                "cannot read {}: {error}",
                manifest.display()
            )));
        }
    };
    let mut entries = Vec::new();
    let mut missing = Vec::new();
    for (index, raw) in contents.lines().enumerate() {
        let entry = raw.trim();
        if entry.is_empty() || entry.starts_with('#') {
            continue;
        }
        let relative = PathBuf::from(entry);
        if relative.is_absolute()
            || relative.components().any(|component| {
                matches!(
                    component,
                    Component::ParentDir | Component::RootDir | Component::Prefix(_)
                )
            })
        {
            return Err(GitWorktreeError(format!(
                "invalid .worktreeinclude entry on line {}: {entry:?} must be relative and cannot contain '..'",
                index + 1
            )));
        }
        let source = root.join(&relative);
        let canonical_source = match source.canonicalize() {
            Ok(path) => path,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                log::info!("skipping missing .worktreeinclude entry {entry:?}");
                missing.push(entry.to_string());
                continue;
            }
            Err(error) => {
                return Err(GitWorktreeError(format!(
                    "cannot resolve .worktreeinclude entry {entry:?}: {error}"
                )));
            }
        };
        if !canonical_source.starts_with(&canonical_root) {
            return Err(GitWorktreeError(format!(
                ".worktreeinclude entry {entry:?} resolves outside the repository"
            )));
        }
        entries.push((entry.to_string(), canonical_source, relative));
    }
    Ok(WorktreeSeedPlan {
        manifest_found: true,
        root: canonical_root,
        entries,
        missing,
    })
}

fn seed_worktree(
    worktree: &Path,
    plan: WorktreeSeedPlan,
    limit: u64,
) -> Result<WorktreeSeedSummary, GitWorktreeError> {
    let canonical_worktree = worktree.canonicalize().map_err(|error| {
        GitWorktreeError(format!(
            "cannot resolve new worktree {}: {error}",
            worktree.display()
        ))
    })?;
    let mut summary = WorktreeSeedSummary {
        manifest_found: plan.manifest_found,
        skipped: plan.missing,
        ..WorktreeSeedSummary::default()
    };
    let mut copied_bytes = 0_u64;
    let mut visited_directories = HashSet::new();
    for (display, source, relative) in plan.entries {
        copy_seed_entry(
            &plan.root,
            &canonical_worktree,
            &source,
            &canonical_worktree.join(relative),
            &display,
            limit,
            &mut copied_bytes,
            &mut visited_directories,
            &mut summary,
        )?;
    }
    if summary.limit_reached {
        log::warn!(
            "worktree seed limit reached for {}; skipped: {}",
            worktree.display(),
            summary.skipped.join(", ")
        );
    }
    Ok(summary)
}

#[allow(clippy::too_many_arguments)]
fn copy_seed_entry(
    source_root: &Path,
    destination_root: &Path,
    source: &Path,
    destination: &Path,
    display: &str,
    limit: u64,
    copied_bytes: &mut u64,
    visited_directories: &mut HashSet<PathBuf>,
    summary: &mut WorktreeSeedSummary,
) -> Result<(), GitWorktreeError> {
    let canonical_source = source.canonicalize().map_err(|error| {
        GitWorktreeError(format!(
            "cannot resolve seed source {}: {error}",
            source.display()
        ))
    })?;
    if !canonical_source.starts_with(source_root) {
        return Err(GitWorktreeError(format!(
            ".worktreeinclude path {display:?} resolves outside the repository"
        )));
    }
    let metadata = std::fs::metadata(&canonical_source).map_err(|error| {
        GitWorktreeError(format!(
            "cannot inspect seed source {}: {error}",
            source.display()
        ))
    })?;
    if destination_has_unsafe_ancestor(destination_root, destination)? {
        summary.skipped.push(display.to_string());
        return Ok(());
    }
    if metadata.is_dir() {
        if !visited_directories.insert(canonical_source.clone()) {
            summary.skipped.push(display.to_string());
            return Ok(());
        }
        if let Ok(destination_metadata) = std::fs::symlink_metadata(destination) {
            if !destination_metadata.is_dir() || destination_metadata.file_type().is_symlink() {
                summary.skipped.push(display.to_string());
                return Ok(());
            }
        } else {
            std::fs::create_dir(destination).map_err(|error| {
                GitWorktreeError(format!(
                    "cannot create seed directory {}: {error}",
                    destination.display()
                ))
            })?;
        }
        let children = std::fs::read_dir(&canonical_source).map_err(|error| {
            GitWorktreeError(format!(
                "cannot read seed directory {}: {error}",
                canonical_source.display()
            ))
        })?;
        for child in children {
            let child = child.map_err(|error| GitWorktreeError(error.to_string()))?;
            let name = child.file_name();
            let child_display = format!("{display}/{}", name.to_string_lossy());
            copy_seed_entry(
                source_root,
                destination_root,
                &child.path(),
                &destination.join(name),
                &child_display,
                limit,
                copied_bytes,
                visited_directories,
                summary,
            )?;
        }
        return Ok(());
    }
    if std::fs::symlink_metadata(destination).is_ok() {
        summary.skipped.push(display.to_string());
        return Ok(());
    }
    if summary.limit_reached || copied_bytes.saturating_add(metadata.len()) > limit {
        summary.limit_reached = true;
        summary.skipped.push(display.to_string());
        return Ok(());
    }
    if let Some(parent) = destination.parent() {
        std::fs::create_dir_all(parent).map_err(|error| GitWorktreeError(error.to_string()))?;
    }
    std::fs::copy(&canonical_source, destination).map_err(|error| {
        GitWorktreeError(format!(
            "cannot seed {} into {}: {error}",
            canonical_source.display(),
            destination.display()
        ))
    })?;
    *copied_bytes += metadata.len();
    summary.copied_files += 1;
    Ok(())
}

fn destination_has_unsafe_ancestor(
    destination_root: &Path,
    destination: &Path,
) -> Result<bool, GitWorktreeError> {
    let relative = destination.strip_prefix(destination_root).map_err(|_| {
        GitWorktreeError(format!(
            "seed destination escapes the worktree: {}",
            destination.display()
        ))
    })?;
    let mut current = destination_root.to_path_buf();
    let Some(parent) = relative.parent() else {
        return Ok(false);
    };
    for component in parent.components() {
        current.push(component);
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                return Ok(true);
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(GitWorktreeError(error.to_string())),
        }
    }
    Ok(false)
}

/// Remove app-owned worktrees whose directory name is not a known session id.
/// Symlinks and directories that are not valid linked Git worktrees are left in
/// place. Only immediate children of `~/.tcode/worktrees` are considered.
pub fn cleanup_orphaned_worktrees(known_session_ids: &HashSet<String>) -> WorktreeCleanupSummary {
    cleanup_orphaned_worktrees_at(&worktrees_root(), known_session_ids)
}

fn cleanup_orphaned_worktrees_at(
    worktrees: &Path,
    known_session_ids: &HashSet<String>,
) -> WorktreeCleanupSummary {
    let mut summary = WorktreeCleanupSummary::default();
    let entries = match std::fs::read_dir(worktrees) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return summary,
        Err(error) => {
            log::warn!(
                "failed to scan {} for orphaned worktrees: {error}",
                worktrees.display()
            );
            return summary;
        }
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let session_id = entry.file_name().to_string_lossy().into_owned();
        let is_directory = entry
            .file_type()
            .is_ok_and(|kind| kind.is_dir() && !kind.is_symlink());
        if known_session_ids.contains(&session_id) || !is_directory {
            continue;
        }
        let registered_path = match registered_worktree_path(&path, &path) {
            Ok(Some(registered_path)) => registered_path,
            Ok(None) => {
                log::warn!("leaving non-worktree directory at {}", path.display());
                summary.skipped.push(path);
                continue;
            }
            Err(error) => {
                log::warn!(
                    "failed to inspect possible orphan at {}: {error}",
                    path.display()
                );
                summary.skipped.push(path);
                continue;
            }
        };
        // Run removal from the main checkout: Windows cannot delete a
        // directory that is the git process's cwd.
        let removal_root = match main_worktree_root(&path) {
            Ok(root) => root,
            Err(error) => {
                log::warn!(
                    "failed to resolve the main checkout for orphan at {}: {error}",
                    path.display()
                );
                summary.skipped.push(path);
                continue;
            }
        };
        let out = crate::process::command("git")
            .current_dir(&removal_root)
            .args([
                "worktree",
                "remove",
                "--force",
                &registered_path.to_string_lossy(),
            ])
            .output();
        match out {
            Ok(out) if out.status.success() => {
                log::info!("removed orphaned tcode worktree {}", path.display());
                summary.removed.push(path);
            }
            Ok(out) => {
                log::warn!(
                    "leaving possible orphan at {}: {}",
                    path.display(),
                    String::from_utf8_lossy(&out.stderr).trim()
                );
                summary.skipped.push(path);
            }
            Err(error) => {
                log::warn!("leaving possible orphan at {}: {error}", path.display());
                summary.skipped.push(path);
            }
        }
    }
    summary
}

pub fn run_git(cwd: &Path, args: &[&str]) -> Result<String, String> {
    let out = crate::process::command("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .map_err(|e| format!("git {}: {e}", args.join(" ")))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

pub fn is_git_repo(cwd: &Path) -> bool {
    run_git(cwd, &["rev-parse", "--is-inside-work-tree"]).is_ok_and(|out| out.trim() == "true")
}

pub fn read_status(cwd: &Path) -> GitStatus {
    if !is_git_repo(cwd) {
        return GitStatus::default();
    }
    let porcelain = run_git(cwd, &["status", "--porcelain=2", "--branch"]).unwrap_or_default();
    let numstat = read_numstat(cwd);
    let has_origin_remote = run_git(cwd, &["remote"])
        .map(|out| out.lines().any(|line| line.trim() == "origin"))
        .unwrap_or(false);
    let default_branch = run_git(cwd, &["symbolic-ref", "refs/remotes/origin/HEAD"])
        .ok()
        .map(|value| {
            value
                .trim()
                .trim_start_matches("refs/remotes/origin/")
                .to_string()
        })
        .filter(|value| !value.is_empty());
    parse_status(
        &porcelain,
        &numstat,
        default_branch.as_deref(),
        has_origin_remote,
    )
}

fn read_numstat(cwd: &Path) -> Vec<(String, u32, u32)> {
    let mut out = Vec::new();
    for args in [
        ["diff", "--numstat"].as_slice(),
        ["diff", "--cached", "--numstat"].as_slice(),
    ] {
        if let Ok(text) = run_git(cwd, args) {
            for line in text.lines() {
                let mut columns = line.split('\t');
                let (Some(insertions), Some(deletions), Some(path)) =
                    (columns.next(), columns.next(), columns.next())
                else {
                    continue;
                };
                out.push((
                    path.to_string(),
                    insertions.parse().unwrap_or(0),
                    deletions.parse().unwrap_or(0),
                ));
            }
        }
    }
    out
}

pub fn commit_diff_context(cwd: &Path, included: Option<&[String]>) -> (String, String) {
    let has_head = run_git(cwd, &["rev-parse", "--verify", "HEAD"]).is_ok();
    let pathspec: Vec<&str> = match included {
        Some(paths) if !paths.is_empty() => {
            let mut values = vec!["--"];
            values.extend(paths.iter().map(String::as_str));
            values
        }
        _ => Vec::new(),
    };
    let mut stat_args = vec!["diff", "--stat"];
    let mut patch_args = vec!["diff", "--no-ext-diff", "--patch", "--minimal"];
    if has_head {
        stat_args.push("HEAD");
        patch_args.push("HEAD");
    }
    stat_args.extend_from_slice(&pathspec);
    patch_args.extend_from_slice(&pathspec);
    let mut stat = run_git(cwd, &stat_args).unwrap_or_default();
    if stat.trim().is_empty() {
        stat = run_git(cwd, &["status", "--short"]).unwrap_or_default();
    }
    let patch = run_git(cwd, &patch_args).unwrap_or_default();
    (stat, patch)
}

pub fn run_claude_headless(
    binary: Option<&Path>,
    cwd: &Path,
    prompt: &str,
) -> Result<String, String> {
    let bin = binary
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_else(|| "claude".to_string());
    let out = crate::process::command(&bin)
        .arg("-p")
        .arg(prompt)
        .current_dir(cwd)
        .output()
        .map_err(|e| format!("failed to run {bin}: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "{bin} -p failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

pub fn perform_action(
    cwd: &Path,
    action: GitAction,
    message: Option<&str>,
    included: Option<&[String]>,
    feature_branch: Option<&str>,
    current_branch: Option<&str>,
) -> Result<String, String> {
    match action {
        GitAction::InitializeGit => Ok(run_git(cwd, &["init"])?.trim().to_string()),
        GitAction::Commit | GitAction::CommitPush => {
            if let Some(branch) = feature_branch {
                run_git(cwd, &["checkout", "-b", branch])?;
            }
            stage_for_commit(cwd, included)?;
            let message = message.unwrap_or("").trim();
            if message.is_empty() {
                return Err("empty commit message".to_string());
            }
            let mut transcript = run_git(cwd, &["commit", "-m", message])?.trim().to_string();
            if action == GitAction::CommitPush {
                let push = run_git(cwd, &["push"])?;
                transcript.push('\n');
                transcript.push_str(push.trim());
            }
            Ok(transcript)
        }
        GitAction::Push => Ok(run_git(cwd, &["push"])?.trim().to_string()),
        GitAction::Pull => Ok(run_git(cwd, &["pull", "--ff-only"])?.trim().to_string()),
        GitAction::PublishBranch => {
            let branch = feature_branch
                .or(current_branch)
                .ok_or_else(|| "no current branch to publish".to_string())?;
            Ok(run_git(cwd, &["push", "-u", "origin", branch])?
                .trim()
                .to_string())
        }
    }
}

pub fn stage_for_commit(cwd: &Path, included: Option<&[String]>) -> Result<(), String> {
    match included {
        Some(paths) if !paths.is_empty() => {
            let _ = run_git(cwd, &["reset", "-q"]);
            let mut args = vec!["add", "-A", "--"];
            args.extend(paths.iter().map(String::as_str));
            run_git(cwd, &args).map(|_| ())
        }
        _ => run_git(cwd, &["add", "-A"]).map(|_| ()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(root: &Path, args: &[&str]) {
        let output = crate::process::command("git")
            .args(args)
            .current_dir(root)
            .env("GIT_AUTHOR_NAME", "tcode")
            .env("GIT_AUTHOR_EMAIL", "tcode@localhost")
            .env("GIT_COMMITTER_NAME", "tcode")
            .env("GIT_COMMITTER_EMAIL", "tcode@localhost")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    fn scratch_repo(prefix: &str) -> (PathBuf, PathBuf) {
        let temp = std::env::temp_dir().join(format!("{prefix}-{}", uuid::Uuid::new_v4()));
        let root = temp.join("repo");
        std::fs::create_dir_all(&root).unwrap();
        run(&root, &["init", "-b", "main"]);
        run(&root, &["config", "core.autocrlf", "false"]);
        std::fs::write(root.join("tracked.txt"), "initial\n").unwrap();
        run(&root, &["add", "tracked.txt"]);
        run(&root, &["commit", "-m", "initial"]);
        (temp, root)
    }

    #[test]
    fn diff_command_shapes() {
        assert_eq!(working_tree_diff_args(false), ["diff", "HEAD", "--"]);
        assert_eq!(working_tree_diff_args(true), ["diff", "HEAD", "-w", "--"]);
        assert_eq!(merge_base_args("main"), ["merge-base", "main", "HEAD"]);
        assert_eq!(
            branch_diff_args("abc123", false),
            ["diff", "abc123...HEAD", "--"]
        );
        assert_eq!(
            branch_diff_args("abc123", true),
            ["diff", "abc123...HEAD", "-w", "--"]
        );
    }

    #[test]
    fn working_tree_and_branch_diff_round_trip() {
        let root = std::env::temp_dir().join(format!("tcode-diff-scope-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let git = |args: &[&str]| {
            let output = crate::process::command("git")
                .args(args)
                .current_dir(&root)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            output
        };
        git(&["init"]);
        git(&["config", "user.email", "diff-test@example.invalid"]);
        git(&["config", "user.name", "Diff Test"]);
        std::fs::write(root.join("tracked.txt"), "before\n").unwrap();
        git(&["add", "tracked.txt"]);
        git(&["commit", "-m", "base"]);
        let base = String::from_utf8(git(&["branch", "--show-current"]).stdout)
            .unwrap()
            .trim()
            .to_string();
        git(&["checkout", "-b", "feature"]);
        std::fs::write(root.join("tracked.txt"), "after\n").unwrap();
        std::fs::write(root.join("untracked.txt"), "new\n").unwrap();

        let working = load_git_diff(&root, GitDiffScope::WorkingTree, None, false);
        assert!(working.error.is_none());
        assert_eq!(working.changes.len(), 2);
        assert_eq!(working.texts.len(), working.changes.len());
        assert!(working.changes.iter().any(|change| {
            change.path.ends_with("untracked.txt") && change.kind == FileChangeKind::Create
        }));
        let tracked_index = working
            .changes
            .iter()
            .position(|change| change.path.ends_with("tracked.txt"))
            .unwrap();
        assert_eq!(
            working.texts[tracked_index].old.as_deref(),
            Some("before\n")
        );
        assert_eq!(working.texts[tracked_index].new.as_deref(), Some("after\n"));
        let untracked_index = working
            .changes
            .iter()
            .position(|change| change.path.ends_with("untracked.txt"))
            .unwrap();
        assert!(working.texts[untracked_index].old.is_none());
        assert_eq!(working.texts[untracked_index].new.as_deref(), Some("new\n"));
        git(&["add", "."]);
        git(&["commit", "-m", "feature changes"]);
        let branch = load_git_diff(&root, GitDiffScope::Branch, Some(&base), false);
        assert!(branch.error.is_none());
        assert_eq!(branch.changes.len(), 2);
        assert_eq!(branch.texts.len(), branch.changes.len());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn diff_texts_handle_created_and_deleted_files() {
        let (temp, root) = scratch_repo("tcode-diff-file-text-test");
        std::fs::remove_file(root.join("tracked.txt")).unwrap();
        std::fs::write(root.join("created.txt"), "created\n").unwrap();

        let result = load_git_diff(&root, GitDiffScope::WorkingTree, None, false);

        assert!(result.error.is_none());
        assert_eq!(result.texts.len(), result.changes.len());
        let deleted_index = result
            .changes
            .iter()
            .position(|change| change.path.ends_with("tracked.txt"))
            .unwrap();
        assert_eq!(result.changes[deleted_index].kind, FileChangeKind::Delete);
        assert_eq!(
            result.texts[deleted_index].old.as_deref(),
            Some("initial\n")
        );
        assert!(result.texts[deleted_index].new.is_none());
        let created_index = result
            .changes
            .iter()
            .position(|change| change.path.ends_with("created.txt"))
            .unwrap();
        assert_eq!(result.changes[created_index].kind, FileChangeKind::Create);
        assert!(result.texts[created_index].old.is_none());
        assert_eq!(
            result.texts[created_index].new.as_deref(),
            Some("created\n")
        );

        let _ = std::fs::remove_dir_all(temp);
    }

    #[test]
    fn whitespace_only_changes_can_be_ignored() {
        let (temp, root) = scratch_repo("tcode-diff-whitespace-test");
        std::fs::write(root.join("tracked.txt"), "  initial  \n").unwrap();

        let normal = load_git_diff(&root, GitDiffScope::WorkingTree, None, false);
        let ignored = load_git_diff(&root, GitDiffScope::WorkingTree, None, true);

        assert_eq!(normal.changes.len(), 1);
        assert_eq!(normal.texts.len(), normal.changes.len());
        assert!(ignored.changes.is_empty());
        assert_eq!(ignored.texts.len(), ignored.changes.len());

        let _ = std::fs::remove_dir_all(temp);
    }

    #[test]
    fn diff_texts_support_paths_with_spaces() {
        let (temp, root) = scratch_repo("tcode-diff-spaced-path-test");
        std::fs::write(root.join("new file.txt"), "new\n").unwrap();

        let result = load_git_diff(&root, GitDiffScope::WorkingTree, None, false);

        let index = result
            .changes
            .iter()
            .position(|change| change.path.ends_with("new file.txt"))
            .unwrap();
        assert!(result.texts[index].old.is_none());
        assert_eq!(result.texts[index].new.as_deref(), Some("new\n"));
        assert_eq!(result.texts.len(), result.changes.len());

        let _ = std::fs::remove_dir_all(temp);
    }

    #[test]
    fn rename_uses_the_old_path_for_base_text() {
        let (temp, root) = scratch_repo("tcode-diff-rename-test");
        run(&root, &["mv", "tracked.txt", "renamed.txt"]);

        let result = load_git_diff(&root, GitDiffScope::WorkingTree, None, false);

        assert_eq!(result.changes.len(), 1);
        assert_eq!(result.changes[0].kind, FileChangeKind::Rename);
        assert!(result.changes[0].path.ends_with("renamed.txt"));
        assert_eq!(result.texts[0].old.as_deref(), Some("initial\n"));
        assert_eq!(result.texts[0].new.as_deref(), Some("initial\n"));
        assert_eq!(result.texts.len(), result.changes.len());

        let _ = std::fs::remove_dir_all(temp);
    }

    #[test]
    fn diff_texts_map_subdirectory_cwd_paths_to_repo_root() {
        let (temp, root) = scratch_repo("tcode-diff-subdirectory-test");
        let subdirectory = root.join("subdirectory");
        std::fs::create_dir(&subdirectory).unwrap();
        std::fs::write(subdirectory.join("nested.txt"), "before\n").unwrap();
        run(&root, &["add", "subdirectory/nested.txt"]);
        run(&root, &["commit", "-m", "add nested file"]);
        std::fs::write(subdirectory.join("nested.txt"), "after\n").unwrap();

        let result = load_git_diff(&subdirectory, GitDiffScope::WorkingTree, None, false);

        assert_eq!(result.changes.len(), 1);
        assert_eq!(
            Path::new(&result.changes[0].path),
            subdirectory.join("nested.txt")
        );
        assert_eq!(result.texts[0].old.as_deref(), Some("before\n"));
        assert_eq!(result.texts[0].new.as_deref(), Some("after\n"));
        assert_eq!(result.texts.len(), result.changes.len());

        let _ = std::fs::remove_dir_all(temp);
    }

    #[test]
    fn branch_list_parser_filters_blank_lines() {
        let out = "main\nfeature/x\n\n  \nrelease-1.0\n";
        assert_eq!(
            parse_branch_list(out),
            vec![
                "main".to_string(),
                "feature/x".to_string(),
                "release-1.0".to_string()
            ]
        );
    }

    #[test]
    fn read_git_branch_reads_head() {
        let root = std::env::temp_dir().join(format!("tcode-branch-test-{}", uuid::Uuid::new_v4()));
        let git = root.join(".git");
        std::fs::create_dir_all(&git).unwrap();

        // A .git dir with no HEAD file yet is treated as no branch.
        assert_eq!(read_git_branch(&root), None);

        // Symbolic ref -> short branch name.
        std::fs::write(git.join("HEAD"), "ref: refs/heads/feature/x\n").unwrap();
        assert_eq!(read_git_branch(&root), Some("feature/x".into()));

        // Detached HEAD -> short sha.
        std::fs::write(git.join("HEAD"), "0123456789abcdef\n").unwrap();
        assert_eq!(read_git_branch(&root), Some("0123456".into()));

        // Non-repo directory.
        let plain = std::env::temp_dir().join(format!("tcode-plain-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&plain).unwrap();
        assert_eq!(read_git_branch(&plain), None);

        let _ = std::fs::remove_dir_all(root);
        let _ = std::fs::remove_dir_all(plain);
    }

    #[test]
    fn checkout_refuses_dirty_worktree() {
        let (temp, root) = scratch_repo("tcode-checkout-dirty-test");
        run(&root, &["branch", "feature"]);
        std::fs::write(root.join("tracked.txt"), "dirty\n").unwrap();

        assert_eq!(
            checkout_if_clean(&root, "feature"),
            Err(CheckoutError::Dirty)
        );
        assert_eq!(read_git_branch(&root), Some("main".into()));

        let _ = std::fs::remove_dir_all(temp);
    }

    #[test]
    fn checkout_switches_clean_worktree() {
        let (temp, root) = scratch_repo("tcode-checkout-clean-test");
        run(&root, &["branch", "feature"]);

        assert_eq!(checkout_if_clean(&root, "feature"), Ok(()));
        assert_eq!(read_git_branch(&root), Some("feature".into()));

        let _ = std::fs::remove_dir_all(temp);
    }

    #[test]
    fn worktree_create_and_remove_round_trip() {
        let (temp, root) = scratch_repo("tcode-worktree-round-trip-test");
        let path = temp.join("nested").join("worktree");

        let created = create_git_worktree(&root, &path, "tcode/test", "main").unwrap();
        assert_eq!(created.path, path);
        assert_eq!(created.branch, "tcode/test");
        assert_eq!(created.seed, WorktreeSeedSummary::default());
        assert!(path.is_dir());
        std::fs::write(path.join("untracked.txt"), "force removal\n").unwrap();
        assert_eq!(remove_git_worktree(&root, &path), Ok(()));
        assert!(!path.exists());

        let _ = std::fs::remove_dir_all(temp);
    }

    fn commit_file(root: &Path, path: &str, contents: &str, message: &str) {
        std::fs::write(root.join(path), contents).unwrap();
        run(root, &["add", path]);
        run(root, &["commit", "-m", message]);
    }

    #[test]
    fn merge_worktree_back_fast_forwards_descendant() {
        let (temp, root) = scratch_repo("tcode-merge-back-ff-test");
        let worktree = temp.join("worktree");
        create_git_worktree(&root, &worktree, "tcode/ff", "main").unwrap();
        commit_file(&worktree, "feature.txt", "feature\n", "feature");

        assert_eq!(
            merge_worktree_back(&root, &worktree, "tcode/ff"),
            Ok(MergeWorktreeOutcome::FastForward)
        );
        assert_eq!(
            run_git(&root, &["rev-parse", "HEAD"]).unwrap(),
            run_git(&worktree, &["rev-parse", "HEAD"]).unwrap()
        );
        remove_git_worktree(&root, &worktree).unwrap();
        let _ = std::fs::remove_dir_all(temp);
    }

    #[test]
    fn merge_worktree_back_creates_merge_commit_for_clean_divergence() {
        let (temp, root) = scratch_repo("tcode-merge-back-diverged-test");
        let worktree = temp.join("worktree");
        create_git_worktree(&root, &worktree, "tcode/diverged", "main").unwrap();
        commit_file(&worktree, "feature.txt", "feature\n", "feature");
        commit_file(&root, "destination.txt", "destination\n", "destination");

        assert_eq!(
            merge_worktree_back(&root, &worktree, "tcode/diverged"),
            Ok(MergeWorktreeOutcome::MergeCommit)
        );
        let parents = run_git(&root, &["show", "-s", "--format=%P", "HEAD"]).unwrap();
        assert_eq!(parents.split_whitespace().count(), 2);
        remove_git_worktree(&root, &worktree).unwrap();
        let _ = std::fs::remove_dir_all(temp);
    }

    #[test]
    fn merge_worktree_back_refuses_dirty_worktree() {
        let (temp, root) = scratch_repo("tcode-merge-back-dirty-worktree-test");
        let worktree = temp.join("worktree");
        create_git_worktree(&root, &worktree, "tcode/dirty-worktree", "main").unwrap();
        std::fs::write(worktree.join("tracked.txt"), "dirty\n").unwrap();

        assert_eq!(
            merge_worktree_back(&root, &worktree, "tcode/dirty-worktree"),
            Err(MergeWorktreeError::DirtyWorktree)
        );
        let _ = std::fs::remove_dir_all(temp);
    }

    #[test]
    fn merge_worktree_back_refuses_dirty_destination() {
        let (temp, root) = scratch_repo("tcode-merge-back-dirty-destination-test");
        let worktree = temp.join("worktree");
        create_git_worktree(&root, &worktree, "tcode/dirty-destination", "main").unwrap();
        commit_file(&worktree, "feature.txt", "feature\n", "feature");
        std::fs::write(root.join("tracked.txt"), "dirty\n").unwrap();

        assert_eq!(
            merge_worktree_back(&root, &worktree, "tcode/dirty-destination"),
            Err(MergeWorktreeError::DirtyDestination)
        );
        let _ = std::fs::remove_dir_all(temp);
    }

    #[test]
    fn merge_worktree_back_aborts_conflict_and_restores_clean_destination() {
        let (temp, root) = scratch_repo("tcode-merge-back-conflict-test");
        let worktree = temp.join("worktree");
        create_git_worktree(&root, &worktree, "tcode/conflict", "main").unwrap();
        commit_file(&worktree, "tracked.txt", "worktree\n", "worktree edit");
        commit_file(&root, "tracked.txt", "destination\n", "destination edit");

        assert_eq!(
            merge_worktree_back(&root, &worktree, "tcode/conflict"),
            Err(MergeWorktreeError::DivergedConflict)
        );
        assert!(
            run_git(&root, &["status", "--porcelain"])
                .unwrap()
                .trim()
                .is_empty()
        );
        assert!(!root.join(".git/MERGE_HEAD").exists());
        remove_git_worktree(&root, &worktree).unwrap();
        let _ = std::fs::remove_dir_all(temp);
    }

    #[test]
    fn worktree_removal_accepts_a_canonical_equivalent_path_spelling() {
        let (temp, root) = scratch_repo("tcode-worktree-equivalent-path-test");
        let path = temp.join("worktree");
        create_git_worktree(&root, &path, "tcode/equivalent-path", "main").unwrap();
        std::fs::create_dir(path.join("nested")).unwrap();
        let alternate_spelling = path.join("nested").join("..");

        assert_ne!(alternate_spelling, path);
        assert!(paths_refer_to_same_existing_path(
            &alternate_spelling,
            &path
        ));
        assert_eq!(remove_git_worktree(&root, &alternate_spelling), Ok(()));
        assert!(!path.exists());

        let _ = std::fs::remove_dir_all(temp);
    }

    #[test]
    fn worktree_seeds_selected_file_and_directory_without_overwriting() {
        let (temp, root) = scratch_repo("tcode-worktree-seed-test");
        std::fs::write(root.join(".env.local"), "TOKEN=test-only\n").unwrap();
        std::fs::create_dir(root.join("ignored-config")).unwrap();
        std::fs::write(root.join("ignored-config/settings.json"), "{}\n").unwrap();
        std::fs::write(
            root.join(".worktreeinclude"),
            "# local build inputs\n.env.local\nignored-config\nmissing.file\ntracked.txt\n",
        )
        .unwrap();
        let path = temp.join("seeded");

        let created = create_git_worktree(&root, &path, "tcode/seed", "main").unwrap();

        assert_eq!(created.seed.copied_files, 2);
        assert!(!created.seed.limit_reached);
        assert_eq!(created.seed.skipped, ["missing.file", "tracked.txt"]);
        assert_eq!(
            std::fs::read_to_string(path.join(".env.local")).unwrap(),
            "TOKEN=test-only\n"
        );
        assert_eq!(
            std::fs::read_to_string(path.join("ignored-config/settings.json")).unwrap(),
            "{}\n"
        );
        assert_eq!(
            std::fs::read_to_string(path.join("tracked.txt")).unwrap(),
            "initial\n"
        );

        remove_git_worktree(&root, &path).unwrap();
        let _ = std::fs::remove_dir_all(temp);
    }

    #[test]
    fn worktree_seed_rejects_traversal() {
        let (temp, root) = scratch_repo("tcode-worktree-seed-traversal-test");
        std::fs::write(root.join(".worktreeinclude"), "../outside\n").unwrap();
        let path = temp.join("worktree");

        let error = create_git_worktree(&root, &path, "tcode/traversal", "main").unwrap_err();

        assert!(error.to_string().contains("cannot contain '..'"));
        assert!(!path.exists());
        let _ = std::fs::remove_dir_all(temp);
    }

    #[test]
    fn worktree_seed_stops_at_total_size_limit() {
        let (temp, root) = scratch_repo("tcode-worktree-seed-limit-test");
        std::fs::write(root.join("first.bin"), b"1234").unwrap();
        std::fs::write(root.join("second.bin"), b"5678").unwrap();
        std::fs::write(root.join(".worktreeinclude"), "first.bin\nsecond.bin\n").unwrap();
        let path = temp.join("worktree");

        let created =
            create_git_worktree_with_seed_limit(&root, &path, "tcode/limit", "main", 4).unwrap();

        assert_eq!(created.seed.copied_files, 1);
        assert!(created.seed.limit_reached);
        assert_eq!(created.seed.skipped, ["second.bin"]);
        assert_eq!(std::fs::read(path.join("first.bin")).unwrap(), b"1234");
        assert!(!path.join("second.bin").exists());

        remove_git_worktree(&root, &path).unwrap();
        let _ = std::fs::remove_dir_all(temp);
    }

    #[test]
    fn orphan_cleanup_removes_only_unknown_session_worktrees() {
        let (temp, root) = scratch_repo("tcode-worktree-orphan-test");
        let worktrees = temp.join("owned-worktrees");
        let orphan = worktrees.join("orphan");
        let kept = worktrees.join("kept");
        create_git_worktree(&root, &orphan, "tcode/orphan", "main").unwrap();
        create_git_worktree(&root, &kept, "tcode/kept", "main").unwrap();
        let known = HashSet::from(["kept".to_string()]);

        let summary = cleanup_orphaned_worktrees_at(&worktrees, &known);

        assert_eq!(summary.removed.as_slice(), std::slice::from_ref(&orphan));
        assert!(summary.skipped.is_empty());
        assert!(!orphan.exists());
        assert!(kept.exists());
        remove_git_worktree(&root, &kept).unwrap();
        let _ = std::fs::remove_dir_all(temp);
    }

    #[test]
    fn worktree_creation_recovers_an_unregistered_stale_path() {
        let (temp, root) = scratch_repo("tcode-worktree-stale-path-test");
        let path = temp.join("worktree");
        std::fs::create_dir(&path).unwrap();

        let created = create_git_worktree(&root, &path, "tcode/stale", "main").unwrap();

        assert_eq!(created.path, path);
        assert!(created.path.join("tracked.txt").exists());
        remove_git_worktree(&root, &created.path).unwrap();
        let _ = std::fs::remove_dir_all(temp);
    }

    #[test]
    fn worktree_branch_collision_uses_numeric_suffix() {
        let (temp, root) = scratch_repo("tcode-worktree-branch-collision-test");
        run(&root, &["branch", "tcode/collision"]);
        let path = temp.join("worktree");

        let created = create_git_worktree(&root, &path, "tcode/collision", "main").unwrap();

        assert_eq!(created.branch, "tcode/collision-2");
        remove_git_worktree(&root, &path).unwrap();
        let _ = std::fs::remove_dir_all(temp);
    }

    #[test]
    fn worktree_removal_is_idempotent_after_external_deletion() {
        let (temp, root) = scratch_repo("tcode-worktree-remove-idempotent-test");
        let path = temp.join("worktree");
        create_git_worktree(&root, &path, "tcode/remove", "main").unwrap();

        std::fs::remove_dir_all(&path).unwrap();
        assert_eq!(remove_git_worktree(&root, &path), Ok(()));
        assert_eq!(remove_git_worktree(&root, &path), Ok(()));

        let _ = std::fs::remove_dir_all(temp);
    }

    #[test]
    fn worktree_path_uses_tcode_layout() {
        let session_id = format!("layout-test-{}", uuid::Uuid::new_v4());
        assert_eq!(
            worktree_path_for(&session_id),
            dirs::home_dir()
                .unwrap_or_else(std::env::temp_dir)
                .join(".tcode")
                .join("worktrees")
                .join(session_id)
        );
    }
}
