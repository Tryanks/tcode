//! Lifecycle for app-owned Git worktrees.
//!
//! Callers provide only the canonical project root and session identity. This
//! module owns every correlated Git identity: base revision, target path, and
//! collision-safe branch name.

use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::git::run_git;

const WORKTREE_SEED_LIMIT_BYTES: u64 = 512 * 1024 * 1024;
const WORKTREES_DIR_ENV: &str = "TCODE_WORKTREES_DIR";

/// A fresh directory may belong to another running tcode process or store.
/// One hour is deliberately conservative: startup recovery favors preserving
/// potentially live work over promptly reclaiming crash residue.
const ORPHAN_MIN_AGE: Duration = Duration::from_secs(60 * 60);

/// Result of copying the paths selected by `.worktreeinclude`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorktreeSeedSummary {
    pub manifest_found: bool,
    pub copied_files: usize,
    pub skipped: Vec<String>,
    pub limit_reached: bool,
}

/// The complete identity of a newly provisioned worktree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProvisionedWorktree {
    pub path: PathBuf,
    pub branch: String,
    pub base: String,
    pub seed_summary: WorktreeSeedSummary,
}

/// Provisioning failed before the lifecycle could be completed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProvisionError {
    NotRepositoryRoot { path: PathBuf },
    Git(String),
}

impl std::fmt::Display for ProvisionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotRepositoryRoot { path } => {
                write!(formatter, "{} is not a Git repository root", path.display())
            }
            Self::Git(error) => formatter.write_str(error),
        }
    }
}

impl std::error::Error for ProvisionError {}

/// Raw process detail from removal and seeding operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeError(String);

impl std::fmt::Display for WorktreeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for WorktreeError {}

impl From<WorktreeError> for ProvisionError {
    fn from(error: WorktreeError) -> Self {
        Self::Git(error.0)
    }
}

/// Result of checking the app-owned worktree directory for orphaned sessions.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CleanupSummary {
    pub removed: Vec<PathBuf>,
    pub skipped: Vec<PathBuf>,
}

/// How a clean dedicated-worktree branch was integrated into its original checkout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeBackOutcome {
    FastForward,
    MergeCommit,
}

/// A merge-back refusal or failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeBackError {
    WorktreeMissing,
    DirtyWorktree,
    DestinationDetached,
    DirtyDestination,
    DivergedConflict,
    Git(String),
}

impl std::fmt::Display for MergeBackError {
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

impl std::error::Error for MergeBackError {}

/// Provision `session_id` from the branch currently checked out at `root`.
///
/// `root` must itself be the canonical main repository root. The target path,
/// requested branch, collision suffix, base revision, seeding, and rollback are
/// all owned by this module.
pub fn provision(root: &Path, session_id: &str) -> Result<ProvisionedWorktree, ProvisionError> {
    let canonical_root = root
        .canonicalize()
        .map_err(|error| ProvisionError::Git(error.to_string()))?;
    let git_root = run_git(&canonical_root, &["rev-parse", "--show-toplevel"])
        .map(|path| PathBuf::from(path.trim()))
        .and_then(|path| {
            path.canonicalize()
                .map_err(|error| format!("cannot resolve repository root: {error}"))
        });
    if git_root.as_ref() != Ok(&canonical_root) {
        return Err(ProvisionError::NotRepositoryRoot {
            path: canonical_root,
        });
    }
    let base = run_git(
        &canonical_root,
        &["symbolic-ref", "--quiet", "--short", "HEAD"],
    )
    .or_else(|_| run_git(&canonical_root, &["rev-parse", "HEAD"]))
    .map_err(ProvisionError::Git)?
    .trim()
    .to_string();
    provision_at(
        &canonical_root,
        session_id,
        &base,
        &worktrees_root(),
        WORKTREE_SEED_LIMIT_BYTES,
    )
    .map_err(Into::into)
}

fn provision_at(
    root: &Path,
    session_id: &str,
    base: &str,
    worktrees_root: &Path,
    seed_limit: u64,
) -> Result<ProvisionedWorktree, WorktreeError> {
    let path = worktrees_root.join(session_id);
    let requested_branch = format!("tcode/{session_id}");
    let seed_plan = read_seed_plan(root)?;
    std::fs::create_dir_all(worktrees_root).map_err(io_error)?;
    let branch = available_branch(root, &requested_branch)?;
    if registered_worktree_path(root, &path)?.is_some() {
        return Err(WorktreeError(format!(
            "worktree target is already registered: {}",
            path.display()
        )));
    }
    if path.exists() {
        prune(root)?;
    }

    let mut output = add(root, &path, &branch, base)?;
    if !output.status.success() && registered_worktree_path(root, &path)?.is_none() {
        prune(root)?;
        output = add(root, &path, &branch, base)?;
    }
    if !output.status.success() {
        return Err(WorktreeError(
            String::from_utf8_lossy(&output.stderr).trim().to_string(),
        ));
    }

    let seed_summary = match seed(&path, seed_plan, seed_limit) {
        Ok(summary) => summary,
        Err(error) => {
            if let Err(cleanup_error) = remove(root, &path) {
                log::warn!(
                    "failed to remove worktree after seeding failed at {}: {cleanup_error}",
                    path.display()
                );
            }
            return Err(error);
        }
    };
    Ok(ProvisionedWorktree {
        path,
        branch,
        base: base.to_string(),
        seed_summary,
    })
}

fn add(
    root: &Path,
    path: &Path,
    branch: &str,
    base: &str,
) -> Result<std::process::Output, WorktreeError> {
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
        .map_err(io_error)
}

fn available_branch(root: &Path, requested: &str) -> Result<String, WorktreeError> {
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
            .map_err(io_error)?;
        if !status.success() {
            return Ok(candidate);
        }
    }
    unreachable!("the branch suffix space cannot be exhausted in practice")
}

fn porcelain_worktree_paths(root: &Path) -> Result<Vec<PathBuf>, WorktreeError> {
    let output = crate::process::command("git")
        .current_dir(root)
        .args(["worktree", "list", "--porcelain"])
        .output()
        .map_err(io_error)?;
    if !output.status.success() {
        return Err(WorktreeError(
            String::from_utf8_lossy(&output.stderr).trim().to_string(),
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.strip_prefix("worktree "))
        .map(PathBuf::from)
        .collect())
}

fn registered_worktree_path(root: &Path, path: &Path) -> Result<Option<PathBuf>, WorktreeError> {
    Ok(porcelain_worktree_paths(root)?
        .into_iter()
        .find(|registered| same_existing_path(registered, path)))
}

fn main_worktree_root(path: &Path) -> Result<PathBuf, WorktreeError> {
    porcelain_worktree_paths(path)?
        .into_iter()
        .next()
        .ok_or_else(|| WorktreeError("git worktree list returned no entries".into()))
}

fn same_existing_path(left: &Path, right: &Path) -> bool {
    left == right
        || matches!(
            (left.canonicalize(), right.canonicalize()),
            (Ok(left), Ok(right)) if left == right
        )
}

fn prune(root: &Path) -> Result<(), WorktreeError> {
    let output = crate::process::command("git")
        .current_dir(root)
        .args(["worktree", "prune"])
        .output()
        .map_err(io_error)?;
    if output.status.success() {
        Ok(())
    } else {
        Err(WorktreeError(
            String::from_utf8_lossy(&output.stderr).trim().to_string(),
        ))
    }
}

/// Remove `worktree` from `root`. Missing paths are successful and stale Git
/// metadata is pruned, making removal idempotent.
pub fn remove(root: &Path, worktree: &Path) -> Result<(), WorktreeError> {
    if !worktree.exists() {
        return prune(root);
    }
    let registered =
        registered_worktree_path(root, worktree)?.unwrap_or_else(|| worktree.to_path_buf());
    let output = crate::process::command("git")
        .current_dir(root)
        .args([
            "worktree",
            "remove",
            "--force",
            &registered.to_string_lossy(),
        ])
        .output()
        .map_err(io_error)?;
    if output.status.success() {
        Ok(())
    } else {
        Err(WorktreeError(
            String::from_utf8_lossy(&output.stderr).trim().to_string(),
        ))
    }
}

/// Integrate `branch` from `worktree` into the branch checked out at
/// `destination`. Both trees must be clean; conflicts are aborted.
pub fn merge_back(
    destination: &Path,
    worktree: &Path,
    branch: &str,
) -> Result<MergeBackOutcome, MergeBackError> {
    if !worktree.is_dir() {
        return Err(MergeBackError::WorktreeMissing);
    }
    if !tree_is_clean(worktree)? {
        return Err(MergeBackError::DirtyWorktree);
    }
    run_git(destination, &["symbolic-ref", "--quiet", "--short", "HEAD"])
        .map_err(|_| MergeBackError::DestinationDetached)?;
    if !tree_is_clean(destination)? {
        return Err(MergeBackError::DirtyDestination);
    }

    let ancestor = crate::process::command("git")
        .args(["merge-base", "--is-ancestor", "HEAD", branch])
        .current_dir(destination)
        .status()
        .map_err(|error| MergeBackError::Git(error.to_string()))?;
    if ancestor.success() {
        run_git(destination, &["merge", "--ff-only", branch]).map_err(MergeBackError::Git)?;
        return Ok(MergeBackOutcome::FastForward);
    }
    if ancestor.code() != Some(1) {
        return Err(MergeBackError::Git(
            "git merge-base --is-ancestor failed".into(),
        ));
    }

    match run_git(destination, &["merge", "--no-ff", "--no-edit", branch]) {
        Ok(_) => Ok(MergeBackOutcome::MergeCommit),
        Err(error) => {
            let conflicted = destination.join(".git/MERGE_HEAD").exists()
                || run_git(destination, &["diff", "--name-only", "--diff-filter=U"])
                    .is_ok_and(|paths| !paths.trim().is_empty());
            if conflicted {
                run_git(destination, &["merge", "--abort"]).map_err(MergeBackError::Git)?;
                Err(MergeBackError::DivergedConflict)
            } else {
                Err(MergeBackError::Git(error))
            }
        }
    }
}

fn tree_is_clean(cwd: &Path) -> Result<bool, MergeBackError> {
    run_git(cwd, &["status", "--porcelain"])
        .map(|status| status.trim().is_empty())
        .map_err(MergeBackError::Git)
}

fn worktrees_root() -> PathBuf {
    std::env::var_os(WORKTREES_DIR_ENV)
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap_or_else(std::env::temp_dir)
                .join(".tcode")
                .join("worktrees")
        })
}

/// Remove old app-owned worktrees whose directory names are not known session ids.
///
/// Tests and isolated processes may set `TCODE_WORKTREES_DIR`; production falls
/// back to `~/.tcode/worktrees`. Fresh unknown entries are presumed live and
/// preserved for at least [`ORPHAN_MIN_AGE`].
pub fn cleanup_orphans(known_session_ids: &HashSet<String>) -> CleanupSummary {
    cleanup_orphans_at(
        &worktrees_root(),
        known_session_ids,
        SystemTime::now(),
        ORPHAN_MIN_AGE,
    )
}

fn cleanup_orphans_at(
    worktrees: &Path,
    known_session_ids: &HashSet<String>,
    now: SystemTime,
    minimum_age: Duration,
) -> CleanupSummary {
    let mut summary = CleanupSummary::default();
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
        let modified = match entry.metadata().and_then(|metadata| metadata.modified()) {
            Ok(modified) => modified,
            Err(error) => {
                log::warn!("leaving possible orphan at {}: {error}", path.display());
                summary.skipped.push(path);
                continue;
            }
        };
        if !old_enough(modified, now, minimum_age) {
            log::info!("leaving fresh possible worktree at {}", path.display());
            summary.skipped.push(path);
            continue;
        }
        let registered = match registered_worktree_path(&path, &path) {
            Ok(Some(registered)) => registered,
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
        let output = crate::process::command("git")
            .current_dir(&removal_root)
            .args([
                "worktree",
                "remove",
                "--force",
                &registered.to_string_lossy(),
            ])
            .output();
        match output {
            Ok(output) if output.status.success() => {
                log::info!("removed orphaned tcode worktree {}", path.display());
                summary.removed.push(path);
            }
            Ok(output) => {
                log::warn!(
                    "leaving possible orphan at {}: {}",
                    path.display(),
                    String::from_utf8_lossy(&output.stderr).trim()
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

fn old_enough(modified: SystemTime, now: SystemTime, minimum_age: Duration) -> bool {
    now.duration_since(modified)
        .is_ok_and(|age| age > minimum_age)
}

#[derive(Debug)]
struct SeedPlan {
    manifest_found: bool,
    root: PathBuf,
    entries: Vec<(String, PathBuf, PathBuf)>,
    missing: Vec<String>,
}

fn read_seed_plan(root: &Path) -> Result<SeedPlan, WorktreeError> {
    let canonical_root = root.canonicalize().map_err(|error| {
        WorktreeError(format!(
            "cannot resolve repository root {}: {error}",
            root.display()
        ))
    })?;
    let manifest = root.join(".worktreeinclude");
    let contents = match std::fs::read_to_string(&manifest) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(SeedPlan {
                manifest_found: false,
                root: canonical_root,
                entries: Vec::new(),
                missing: Vec::new(),
            });
        }
        Err(error) => {
            return Err(WorktreeError(format!(
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
            return Err(WorktreeError(format!(
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
                return Err(WorktreeError(format!(
                    "cannot resolve .worktreeinclude entry {entry:?}: {error}"
                )));
            }
        };
        if !canonical_source.starts_with(&canonical_root) {
            return Err(WorktreeError(format!(
                ".worktreeinclude entry {entry:?} resolves outside the repository"
            )));
        }
        entries.push((entry.to_string(), canonical_source, relative));
    }
    Ok(SeedPlan {
        manifest_found: true,
        root: canonical_root,
        entries,
        missing,
    })
}

fn seed(worktree: &Path, plan: SeedPlan, limit: u64) -> Result<WorktreeSeedSummary, WorktreeError> {
    let canonical_worktree = worktree.canonicalize().map_err(|error| {
        WorktreeError(format!(
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
) -> Result<(), WorktreeError> {
    let canonical_source = source.canonicalize().map_err(|error| {
        WorktreeError(format!(
            "cannot resolve seed source {}: {error}",
            source.display()
        ))
    })?;
    if !canonical_source.starts_with(source_root) {
        return Err(WorktreeError(format!(
            ".worktreeinclude path {display:?} resolves outside the repository"
        )));
    }
    let metadata = std::fs::metadata(&canonical_source).map_err(|error| {
        WorktreeError(format!(
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
            std::fs::create_dir(destination).map_err(io_error)?;
        }
        let children = std::fs::read_dir(&canonical_source).map_err(io_error)?;
        for child in children {
            let child = child.map_err(io_error)?;
            let name = child.file_name();
            copy_seed_entry(
                source_root,
                destination_root,
                &child.path(),
                &destination.join(&name),
                &format!("{display}/{}", name.to_string_lossy()),
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
        std::fs::create_dir_all(parent).map_err(io_error)?;
    }
    std::fs::copy(&canonical_source, destination).map_err(io_error)?;
    *copied_bytes += metadata.len();
    summary.copied_files += 1;
    Ok(())
}

fn destination_has_unsafe_ancestor(
    destination_root: &Path,
    destination: &Path,
) -> Result<bool, WorktreeError> {
    let relative = destination.strip_prefix(destination_root).map_err(|_| {
        WorktreeError(format!(
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
            Err(error) => return Err(io_error(error)),
        }
    }
    Ok(false)
}

fn io_error(error: std::io::Error) -> WorktreeError {
    WorktreeError(error.to_string())
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
        run(&root, &["config", "user.name", "tcode"]);
        run(&root, &["config", "user.email", "tcode@localhost"]);
        std::fs::write(root.join("tracked.txt"), "initial\n").unwrap();
        run(&root, &["add", "tracked.txt"]);
        run(&root, &["commit", "-m", "initial"]);
        (temp, root)
    }

    fn provision_for_test(root: &Path, session_id: &str, worktrees: &Path) -> ProvisionedWorktree {
        provision_at(
            root,
            session_id,
            "main",
            worktrees,
            WORKTREE_SEED_LIMIT_BYTES,
        )
        .unwrap()
    }

    fn commit_file(root: &Path, path: &str, contents: &str, message: &str) {
        std::fs::write(root.join(path), contents).unwrap();
        run(root, &["add", path]);
        run(root, &["commit", "-m", message]);
    }

    #[test]
    fn provision_returns_derived_identity_without_caller_computation() {
        let (temp, root) = scratch_repo("tcode-worktree-provision-test");
        let worktrees = temp.join("owned-worktrees");
        let created = provision_for_test(&root, "session-identity", &worktrees);

        assert_eq!(created.path, worktrees.join("session-identity"));
        assert_eq!(created.branch, "tcode/session-identity");
        assert_eq!(created.base, "main");
        assert_eq!(created.seed_summary, WorktreeSeedSummary::default());

        remove(&root, &created.path).unwrap();
        let _ = std::fs::remove_dir_all(temp);
    }

    #[test]
    fn worktree_create_and_remove_round_trip() {
        let (temp, root) = scratch_repo("tcode-worktree-round-trip-test");
        let created = provision_for_test(&root, "round-trip", &temp.join("worktrees"));
        std::fs::write(created.path.join("untracked.txt"), "force removal\n").unwrap();
        assert_eq!(remove(&root, &created.path), Ok(()));
        assert!(!created.path.exists());
        let _ = std::fs::remove_dir_all(temp);
    }

    #[test]
    fn merge_back_fast_forwards_descendant() {
        let (temp, root) = scratch_repo("tcode-merge-back-ff-test");
        let created = provision_for_test(&root, "ff", &temp.join("worktrees"));
        commit_file(&created.path, "feature.txt", "feature\n", "feature");

        assert_eq!(
            merge_back(&root, &created.path, &created.branch),
            Ok(MergeBackOutcome::FastForward)
        );
        assert_eq!(
            run_git(&root, &["rev-parse", "HEAD"]).unwrap(),
            run_git(&created.path, &["rev-parse", "HEAD"]).unwrap()
        );
        remove(&root, &created.path).unwrap();
        let _ = std::fs::remove_dir_all(temp);
    }

    #[test]
    fn merge_back_creates_merge_commit_for_clean_divergence() {
        let (temp, root) = scratch_repo("tcode-merge-back-diverged-test");
        let created = provision_for_test(&root, "diverged", &temp.join("worktrees"));
        commit_file(&created.path, "feature.txt", "feature\n", "feature");
        commit_file(&root, "destination.txt", "destination\n", "destination");

        assert_eq!(
            merge_back(&root, &created.path, &created.branch),
            Ok(MergeBackOutcome::MergeCommit)
        );
        let parents = run_git(&root, &["show", "-s", "--format=%P", "HEAD"]).unwrap();
        assert_eq!(parents.split_whitespace().count(), 2);
        remove(&root, &created.path).unwrap();
        let _ = std::fs::remove_dir_all(temp);
    }

    #[test]
    fn merge_back_refuses_dirty_worktree() {
        let (temp, root) = scratch_repo("tcode-merge-back-dirty-worktree-test");
        let created = provision_for_test(&root, "dirty-worktree", &temp.join("worktrees"));
        std::fs::write(created.path.join("tracked.txt"), "dirty\n").unwrap();
        assert_eq!(
            merge_back(&root, &created.path, &created.branch),
            Err(MergeBackError::DirtyWorktree)
        );
        let _ = std::fs::remove_dir_all(temp);
    }

    #[test]
    fn merge_back_refuses_dirty_destination() {
        let (temp, root) = scratch_repo("tcode-merge-back-dirty-destination-test");
        let created = provision_for_test(&root, "dirty-destination", &temp.join("worktrees"));
        commit_file(&created.path, "feature.txt", "feature\n", "feature");
        std::fs::write(root.join("tracked.txt"), "dirty\n").unwrap();
        assert_eq!(
            merge_back(&root, &created.path, &created.branch),
            Err(MergeBackError::DirtyDestination)
        );
        let _ = std::fs::remove_dir_all(temp);
    }

    #[test]
    fn merge_back_aborts_conflict_and_restores_clean_destination() {
        let (temp, root) = scratch_repo("tcode-merge-back-conflict-test");
        let created = provision_for_test(&root, "conflict", &temp.join("worktrees"));
        commit_file(&created.path, "tracked.txt", "worktree\n", "worktree edit");
        commit_file(&root, "tracked.txt", "destination\n", "destination edit");
        assert_eq!(
            merge_back(&root, &created.path, &created.branch),
            Err(MergeBackError::DivergedConflict)
        );
        assert!(
            run_git(&root, &["status", "--porcelain"])
                .unwrap()
                .trim()
                .is_empty()
        );
        assert!(!root.join(".git/MERGE_HEAD").exists());
        remove(&root, &created.path).unwrap();
        let _ = std::fs::remove_dir_all(temp);
    }

    #[test]
    fn removal_accepts_a_canonical_equivalent_path_spelling() {
        let (temp, root) = scratch_repo("tcode-worktree-equivalent-path-test");
        let created = provision_for_test(&root, "equivalent", &temp.join("worktrees"));
        std::fs::create_dir(created.path.join("nested")).unwrap();
        let alternate = created.path.join("nested").join("..");
        assert!(same_existing_path(&alternate, &created.path));
        assert_eq!(remove(&root, &alternate), Ok(()));
        assert!(!created.path.exists());
        let _ = std::fs::remove_dir_all(temp);
    }

    #[test]
    fn seeding_copies_selected_paths_without_overwriting() {
        let (temp, root) = scratch_repo("tcode-worktree-seed-test");
        std::fs::write(root.join(".env.local"), "TOKEN=test-only\n").unwrap();
        std::fs::create_dir(root.join("ignored-config")).unwrap();
        std::fs::write(root.join("ignored-config/settings.json"), "{}\n").unwrap();
        std::fs::write(
            root.join(".worktreeinclude"),
            "# local build inputs\n.env.local\nignored-config\nmissing.file\ntracked.txt\n",
        )
        .unwrap();
        let created = provision_for_test(&root, "seed", &temp.join("worktrees"));
        assert_eq!(created.seed_summary.copied_files, 2);
        assert_eq!(
            created.seed_summary.skipped,
            ["missing.file", "tracked.txt"]
        );
        assert_eq!(
            std::fs::read_to_string(created.path.join(".env.local")).unwrap(),
            "TOKEN=test-only\n"
        );
        assert_eq!(
            std::fs::read_to_string(created.path.join("tracked.txt")).unwrap(),
            "initial\n"
        );
        remove(&root, &created.path).unwrap();
        let _ = std::fs::remove_dir_all(temp);
    }

    #[test]
    fn seed_rejects_traversal_and_rolls_back() {
        let (temp, root) = scratch_repo("tcode-worktree-seed-traversal-test");
        std::fs::write(root.join(".worktreeinclude"), "../outside\n").unwrap();
        let worktrees = temp.join("worktrees");
        let error = provision_at(
            &root,
            "traversal",
            "main",
            &worktrees,
            WORKTREE_SEED_LIMIT_BYTES,
        )
        .unwrap_err();
        assert!(error.to_string().contains("cannot contain '..'"));
        assert!(!worktrees.join("traversal").exists());
        let _ = std::fs::remove_dir_all(temp);
    }

    #[test]
    fn seed_stops_at_total_size_limit() {
        let (temp, root) = scratch_repo("tcode-worktree-seed-limit-test");
        std::fs::write(root.join("first.bin"), b"1234").unwrap();
        std::fs::write(root.join("second.bin"), b"5678").unwrap();
        std::fs::write(root.join(".worktreeinclude"), "first.bin\nsecond.bin\n").unwrap();
        let created = provision_at(&root, "limit", "main", &temp.join("worktrees"), 4).unwrap();
        assert_eq!(created.seed_summary.copied_files, 1);
        assert!(created.seed_summary.limit_reached);
        assert_eq!(created.seed_summary.skipped, ["second.bin"]);
        remove(&root, &created.path).unwrap();
        let _ = std::fs::remove_dir_all(temp);
    }

    #[test]
    fn orphan_cleanup_preserves_fresh_and_removes_old_unknown_worktree() {
        let (temp, root) = scratch_repo("tcode-worktree-orphan-age-test");
        let worktrees = temp.join("worktrees");
        let orphan = provision_for_test(&root, "orphan", &worktrees).path;
        let modified = std::fs::metadata(&orphan).unwrap().modified().unwrap();
        let known = HashSet::new();

        let fresh = cleanup_orphans_at(&worktrees, &known, modified, ORPHAN_MIN_AGE);
        assert!(fresh.removed.is_empty());
        assert_eq!(fresh.skipped.as_slice(), std::slice::from_ref(&orphan));
        assert!(orphan.exists());

        let old = cleanup_orphans_at(
            &worktrees,
            &known,
            modified + ORPHAN_MIN_AGE + Duration::from_secs(1),
            ORPHAN_MIN_AGE,
        );
        assert_eq!(old.removed.as_slice(), std::slice::from_ref(&orphan));
        assert!(old.skipped.is_empty());
        assert!(!orphan.exists());
        let _ = std::fs::remove_dir_all(temp);
    }

    #[test]
    fn orphan_cleanup_keeps_known_worktree() {
        let (temp, root) = scratch_repo("tcode-worktree-known-test");
        let worktrees = temp.join("worktrees");
        let kept = provision_for_test(&root, "kept", &worktrees).path;
        let modified = std::fs::metadata(&kept).unwrap().modified().unwrap();
        let known = HashSet::from(["kept".to_string()]);
        let summary = cleanup_orphans_at(
            &worktrees,
            &known,
            modified + ORPHAN_MIN_AGE + Duration::from_secs(1),
            ORPHAN_MIN_AGE,
        );
        assert_eq!(summary, CleanupSummary::default());
        remove(&root, &kept).unwrap();
        let _ = std::fs::remove_dir_all(temp);
    }

    #[test]
    fn provision_recovers_an_unregistered_stale_path() {
        let (temp, root) = scratch_repo("tcode-worktree-stale-path-test");
        let worktrees = temp.join("worktrees");
        std::fs::create_dir_all(worktrees.join("stale")).unwrap();
        let created = provision_for_test(&root, "stale", &worktrees);
        assert!(created.path.join("tracked.txt").exists());
        remove(&root, &created.path).unwrap();
        let _ = std::fs::remove_dir_all(temp);
    }

    #[test]
    fn branch_collision_uses_numeric_suffix() {
        let (temp, root) = scratch_repo("tcode-worktree-branch-collision-test");
        run(&root, &["branch", "tcode/collision"]);
        let created = provision_for_test(&root, "collision", &temp.join("worktrees"));
        assert_eq!(created.branch, "tcode/collision-2");
        remove(&root, &created.path).unwrap();
        let _ = std::fs::remove_dir_all(temp);
    }

    #[test]
    fn removal_is_idempotent_after_external_deletion() {
        let (temp, root) = scratch_repo("tcode-worktree-remove-idempotent-test");
        let created = provision_for_test(&root, "remove", &temp.join("worktrees"));
        std::fs::remove_dir_all(&created.path).unwrap();
        assert_eq!(remove(&root, &created.path), Ok(()));
        assert_eq!(remove(&root, &created.path), Ok(()));
        let _ = std::fs::remove_dir_all(temp);
    }
}
