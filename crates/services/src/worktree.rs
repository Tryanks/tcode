//! Lifecycle for app-owned Git worktrees.
//!
//! Callers name the requesting checkout, a name and a base. This module owns
//! every correlated Git identity: target path, collision-safe branch name,
//! resolved base, and which branch removal may delete.

use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime};

use sha2::{Digest as _, Sha256};

use crate::git::run_git;
use crate::store::SessionStore;

const WORKTREE_SEED_LIMIT_BYTES: u64 = 512 * 1024 * 1024;

/// Removal deletes only branches in this namespace: Tcode created them.
const BRANCH_PREFIX: &str = "tcode/";

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

/// Provision a worktree called `name` for the repository `checkout` belongs to,
/// in the data dir `data_dir`, and seed it from `checkout`.
///
/// `checkout` is the top level of the main checkout or of one of its linked
/// worktrees. `base` is the branch or commit to start from; `None` takes
/// whatever `checkout` has checked out now. The worktree lands in
/// `<data_dir>/worktrees/<repository>/<name>` on the branch `tcode/<name>`,
/// both made safe from `name` and the branch suffixed on collision.
pub fn provision(
    checkout: &Path,
    name: &str,
    base: Option<&str>,
    data_dir: &Path,
) -> Result<ProvisionedWorktree, ProvisionError> {
    let checkout = checkout
        .canonicalize()
        .map_err(|error| ProvisionError::Git(error.to_string()))?;
    let top_level = run_git(&checkout, &["rev-parse", "--show-toplevel"])
        .map(|path| PathBuf::from(path.trim()))
        .and_then(|path| {
            path.canonicalize()
                .map_err(|error| format!("cannot resolve repository root: {error}"))
        });
    if top_level.as_ref() != Ok(&checkout) {
        return Err(ProvisionError::NotRepositoryRoot { path: checkout });
    }
    let base = match base {
        Some(base) => base.to_string(),
        None => run_git(&checkout, &["symbolic-ref", "--quiet", "--short", "HEAD"])
            .or_else(|_| run_git(&checkout, &["rev-parse", "HEAD"]))
            .map_err(ProvisionError::Git)?
            .trim()
            .to_string(),
    };
    let bucket = worktrees_root(data_dir).join(repository_key(&checkout)?);
    provision_at(
        &checkout,
        &path_safe(name),
        &base,
        &bucket,
        WORKTREE_SEED_LIMIT_BYTES,
    )
    .map_err(Into::into)
}

/// One directory per repository: its directory name for people, and a hash of
/// its common Git dir so that every checkout of one repository shares it and
/// two repositories with the same name do not.
fn repository_key(checkout: &Path) -> Result<String, WorktreeError> {
    let common = run_git(checkout, &["rev-parse", "--git-common-dir"]).map_err(WorktreeError)?;
    let common = checkout
        .join(common.trim())
        .canonicalize()
        .map_err(|error| WorktreeError(format!("cannot resolve the Git directory: {error}")))?;
    let directory = if common.file_name().is_some_and(|name| name == ".git") {
        common.parent().and_then(Path::file_name)
    } else {
        common.file_name()
    }
    .map(|name| name.to_string_lossy())
    .unwrap_or_default();
    let directory = directory.strip_suffix(".git").unwrap_or(&directory);
    let digest = Sha256::digest(common.as_os_str().as_encoded_bytes());
    let hash: String = digest[..4]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    Ok(format!("{}-{hash}", path_safe(directory)))
}

/// `name` as one path segment that is also a valid branch-name component.
fn path_safe(name: &str) -> String {
    let safe: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let safe = safe.trim_matches('-');
    if safe.is_empty() {
        "worktree".to_string()
    } else {
        safe.to_string()
    }
}

fn provision_at(
    root: &Path,
    name: &str,
    base: &str,
    worktrees_root: &Path,
    seed_limit: u64,
) -> Result<ProvisionedWorktree, WorktreeError> {
    let path = worktrees_root.join(name);
    let requested_branch = format!("{BRANCH_PREFIX}{name}");
    let seed_plan = read_seed_plan(root)?;
    std::fs::create_dir_all(worktrees_root).map_err(io_error)?;
    let branch = available_branch(root, &requested_branch)?;
    if registered_worktree(root, &path)?.is_some() {
        return Err(WorktreeError(format!(
            "worktree target is already registered: {}",
            path.display()
        )));
    }
    if path.exists() {
        prune(root)?;
    }

    let mut output = add(root, &path, &branch, base)?;
    if !output.status.success() && registered_worktree(root, &path)?.is_none() {
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

/// A worktree as `git worktree list` records it; `branch` is the short name of
/// the branch it has checked out.
#[derive(Debug)]
struct RegisteredWorktree {
    path: PathBuf,
    branch: Option<String>,
}

/// Every worktree of `root`'s repository, the main checkout first.
fn registered_worktrees(root: &Path) -> Result<Vec<RegisteredWorktree>, WorktreeError> {
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
    let mut worktrees: Vec<RegisteredWorktree> = Vec::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        if let Some(path) = line.strip_prefix("worktree ") {
            worktrees.push(RegisteredWorktree {
                path: PathBuf::from(path),
                branch: None,
            });
        } else if let Some(branch) = line.strip_prefix("branch refs/heads/")
            && let Some(worktree) = worktrees.last_mut()
        {
            worktree.branch = Some(branch.to_string());
        }
    }
    Ok(worktrees)
}

fn registered_worktree(
    root: &Path,
    path: &Path,
) -> Result<Option<RegisteredWorktree>, WorktreeError> {
    Ok(registered_worktrees(root)?
        .into_iter()
        .find(|registered| same_path(&registered.path, path)))
}

/// Whether `dir` lies inside `root`'s place in some checkout of its
/// repository: the same subdirectory of the main checkout or of any worktree
/// registered with it, wherever that worktree lives.
pub fn within_repository_worktree(root: &Path, dir: &Path) -> bool {
    let Ok(dir) = dir.canonicalize() else {
        return false;
    };
    let Ok(prefix) = run_git(root, &["rev-parse", "--show-prefix"]) else {
        return false;
    };
    let prefix = prefix.trim();
    registered_worktrees(root).is_ok_and(|worktrees| {
        worktrees.iter().any(|worktree| {
            worktree
                .path
                .canonicalize()
                .is_ok_and(|path| dir.starts_with(path.join(prefix)))
        })
    })
}

/// Whether two spellings name one path; a missing path is compared through
/// its parent, as Git records a worktree's path resolved.
fn same_path(left: &Path, right: &Path) -> bool {
    fn resolved(path: &Path) -> Option<PathBuf> {
        path.canonicalize().ok().or_else(|| {
            let parent = path.parent()?.canonicalize().ok()?;
            Some(parent.join(path.file_name()?))
        })
    }
    left == right
        || matches!((resolved(left), resolved(right)), (Some(left), Some(right)) if left == right)
}

/// Re-point the `.git` files of `worktrees` at `root` after the main
/// repository moved; a path that is not a registered worktree is reported by
/// git and skipped.
pub fn repair(root: &Path, worktrees: &[PathBuf]) -> Result<(), WorktreeError> {
    if worktrees.is_empty() {
        return Ok(());
    }
    let output = crate::process::command("git")
        .current_dir(root)
        .args(["worktree", "repair"])
        .args(worktrees)
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

/// Remove `worktree`, uncommitted work included, from the repository of
/// `root`, and its branch as [`delete_branch_without_own_commits`] allows.
/// Missing paths are successful and stale Git metadata is pruned, making
/// removal idempotent.
pub fn remove(root: &Path, worktree: &Path) -> Result<(), WorktreeError> {
    let registered = registered_worktree(root, worktree)?;
    if worktree.exists() {
        let path = registered
            .as_ref()
            .map_or(worktree, |registered| registered.path.as_path());
        remove_registered(root, path, true)?;
    } else {
        prune(root)?;
    }
    if let Some(branch) = registered.and_then(|registered| registered.branch) {
        delete_branch_without_own_commits(root, &branch);
    }
    Ok(())
}

fn remove_registered(root: &Path, path: &Path, force: bool) -> Result<(), WorktreeError> {
    let path = path.to_string_lossy();
    let mut args = vec!["worktree", "remove"];
    if force {
        args.push("--force");
    }
    args.push(&path);
    let output = crate::process::command("git")
        .current_dir(root)
        .args(args)
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

/// Delete the Tcode branch a removed worktree had checked out when every
/// commit on it is also reachable from another branch, a tag or a remote
/// branch: deleting it loses nothing. A branch with commits of its own is kept.
fn delete_branch_without_own_commits(root: &Path, branch: &str) {
    // `--exclude` takes a glob; a name that is not literal cannot be excluded.
    if !branch.starts_with(BRANCH_PREFIX) || branch.contains(['*', '?', '[', '\\']) {
        return;
    }
    let tip = format!("refs/heads/{branch}");
    let exclude = format!("--exclude={branch}");
    let own = run_git(
        root,
        &[
            "rev-list",
            "--max-count=1",
            &tip,
            "--not",
            "--tags",
            "--remotes",
            &exclude,
            "--branches",
        ],
    );
    match own {
        Ok(own) if own.trim().is_empty() => {
            if let Err(error) = run_git(root, &["branch", "-D", branch]) {
                log::warn!("keeping branch {branch}: {error}");
            }
        }
        Ok(_) => log::info!("keeping branch {branch}: it has commits of its own"),
        Err(error) => log::warn!("keeping branch {branch}: {error}"),
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

/// A data dir's worktrees live inside it: its sweep can only tell which ones
/// its own threads use or keep.
pub fn worktrees_root(data_dir: &Path) -> PathBuf {
    data_dir.join("worktrees")
}

/// Remove old, clean app-owned worktrees in `store`'s data dir that none of
/// its threads works in and that were not kept when their thread was deleted.
///
/// Fresh unknown entries are presumed live and preserved for at least
/// [`ORPHAN_MIN_AGE`]. Removal is never forced: a worktree may hold
/// uncommitted work. Nothing is removed when the store cannot be read.
pub fn cleanup_orphans(store: &SessionStore) -> CleanupSummary {
    let sessions = match store.read_file() {
        Ok(file) => file.sessions,
        Err(error) => {
            log::warn!("skipping orphaned worktree recovery: {error}");
            return CleanupSummary::default();
        }
    };
    let kept = match store.kept_worktrees() {
        Ok(kept) => kept,
        Err(error) => {
            log::warn!("skipping orphaned worktree recovery: {error}");
            return CleanupSummary::default();
        }
    };
    let known_ids = sessions.iter().map(|meta| meta.id.clone()).collect();
    let in_use: Vec<_> = sessions
        .into_iter()
        .map(|meta| meta.cwd)
        .chain(kept)
        .collect();
    cleanup_orphans_at(
        &worktrees_root(store.root()),
        &known_ids,
        &in_use,
        SystemTime::now(),
        ORPHAN_MIN_AGE,
    )
}

/// `in_use` holds paths that keep the worktree containing them: thread cwds
/// and kept worktrees. A worktree sits either in a repository's directory
/// (`<worktrees>/<repository>/<name>`) or, provisioned before those existed,
/// directly in `worktrees` under its thread's id.
fn cleanup_orphans_at(
    worktrees: &Path,
    known_session_ids: &HashSet<String>,
    in_use: &[PathBuf],
    now: SystemTime,
    minimum_age: Duration,
) -> CleanupSummary {
    let mut summary = CleanupSummary::default();
    for entry in directories(worktrees) {
        let candidates = if std::fs::symlink_metadata(entry.join(".git")).is_ok() {
            vec![entry]
        } else {
            directories(&entry)
        };
        for path in candidates {
            sweep(
                path,
                known_session_ids,
                in_use,
                now,
                minimum_age,
                &mut summary,
            );
        }
    }
    summary
}

/// The directories, links excluded, directly inside `parent`.
fn directories(parent: &Path) -> Vec<PathBuf> {
    let entries = match std::fs::read_dir(parent) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(error) => {
            log::warn!(
                "failed to scan {} for orphaned worktrees: {error}",
                parent.display()
            );
            return Vec::new();
        }
    };
    entries
        .flatten()
        .filter(|entry| {
            entry
                .file_type()
                .is_ok_and(|kind| kind.is_dir() && !kind.is_symlink())
        })
        .map(|entry| entry.path())
        .collect()
}

fn sweep(
    path: PathBuf,
    known_session_ids: &HashSet<String>,
    in_use: &[PathBuf],
    now: SystemTime,
    minimum_age: Duration,
    summary: &mut CleanupSummary,
) {
    let named_for_thread = path
        .file_name()
        .is_some_and(|name| known_session_ids.contains(name.to_string_lossy().as_ref()));
    if named_for_thread || in_use.iter().any(|used| used.starts_with(&path)) {
        return;
    }
    let modified = match std::fs::metadata(&path).and_then(|metadata| metadata.modified()) {
        Ok(modified) => modified,
        Err(error) => {
            log::warn!("leaving possible orphan at {}: {error}", path.display());
            summary.skipped.push(path);
            return;
        }
    };
    if !old_enough(modified, now, minimum_age) {
        log::info!("leaving fresh possible worktree at {}", path.display());
        summary.skipped.push(path);
        return;
    }
    let registered = match registered_worktree(&path, &path) {
        Ok(Some(registered)) => registered,
        Ok(None) => {
            log::warn!("leaving non-worktree directory at {}", path.display());
            summary.skipped.push(path);
            return;
        }
        Err(error) => {
            log::warn!(
                "failed to inspect possible orphan at {}: {error}",
                path.display()
            );
            summary.skipped.push(path);
            return;
        }
    };
    let main_checkout = match registered_worktrees(&path).map(|all| all.into_iter().next()) {
        Ok(Some(main)) => main.path,
        Ok(None) => {
            log::warn!(
                "git lists no main checkout for orphan at {}",
                path.display()
            );
            summary.skipped.push(path);
            return;
        }
        Err(error) => {
            log::warn!(
                "failed to resolve the main checkout for orphan at {}: {error}",
                path.display()
            );
            summary.skipped.push(path);
            return;
        }
    };
    match remove_registered(&main_checkout, &registered.path, false) {
        Ok(()) => {
            log::info!("removed orphaned tcode worktree {}", path.display());
            if let Some(branch) = registered.branch {
                delete_branch_without_own_commits(&main_checkout, &branch);
            }
            summary.removed.push(path);
        }
        Err(error) => {
            log::warn!("leaving possible orphan at {}: {error}", path.display());
            summary.skipped.push(path);
        }
    }
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
        run(&root, &["-c", "init.templateDir=", "init", "-b", "main"]);
        run(&root, &["config", "commit.gpgSign", "false"]);
        run(
            &root,
            &[
                "config",
                "core.hooksPath",
                temp.join("no-hooks").to_str().unwrap(),
            ],
        );
        run(&root, &["config", "core.autocrlf", "false"]);
        run(&root, &["config", "user.name", "tcode"]);
        run(&root, &["config", "user.email", "tcode@localhost"]);
        std::fs::write(root.join("tracked.txt"), "initial\n").unwrap();
        run(&root, &["add", "tracked.txt"]);
        run(&root, &["commit", "-m", "initial"]);
        (temp, root)
    }

    fn provision_for_test(root: &Path, name: &str, data_dir: &Path) -> ProvisionedWorktree {
        provision(root, name, Some("main"), data_dir).unwrap()
    }

    fn has_branch(root: &Path, branch: &str) -> bool {
        run_git(
            root,
            &["show-ref", "--verify", &format!("refs/heads/{branch}")],
        )
        .is_ok()
    }

    fn commit_file(root: &Path, path: &str, contents: &str, message: &str) {
        std::fs::write(root.join(path), contents).unwrap();
        run(root, &["add", path]);
        run(root, &["commit", "-m", message]);
    }

    #[test]
    fn repair_reconnects_worktrees_after_the_repository_moves() {
        let (temp, root) = scratch_repo("tcode-worktree-repair");
        let created = provision_for_test(&root, "session", &temp);
        let moved = temp.join("moved");
        std::fs::rename(&root, &moved).unwrap();
        assert!(run_git(&created.path, &["status", "--porcelain"]).is_err());

        repair(&moved, std::slice::from_ref(&created.path)).unwrap();
        run_git(&created.path, &["status", "--porcelain"]).unwrap();
        let registered = registered_worktrees(&moved).unwrap();
        assert_eq!(registered.len(), 2);
        assert!(same_path(&registered[0].path, &moved));
        assert!(same_path(&registered[1].path, &created.path));
        let _ = std::fs::remove_dir_all(temp);
    }

    #[test]
    fn provision_recovers_stale_paths_and_branch_collisions_and_removal_is_idempotent() {
        let (temp, root) = scratch_repo("tcode-worktree-lifecycle");
        let stale = worktrees_root(&temp)
            .join(repository_key(&root).unwrap())
            .join("session");
        std::fs::create_dir_all(&stale).unwrap();
        run(&root, &["branch", "tcode/session"]);
        let created = provision_for_test(&root, "session", &temp);
        assert_eq!(created.path, stale);
        assert_eq!(created.branch, "tcode/session-2");
        assert_eq!(created.base, "main");
        assert_eq!(created.seed_summary, WorktreeSeedSummary::default());
        assert_eq!(
            std::fs::read_to_string(created.path.join("tracked.txt")).unwrap(),
            "initial\n"
        );

        std::fs::create_dir(created.path.join("nested")).unwrap();
        std::fs::write(created.path.join("untracked.txt"), "force removal\n").unwrap();
        assert_eq!(remove(&root, &created.path.join("nested/..")), Ok(()));
        assert!(!created.path.exists());
        assert_eq!(remove(&root, &created.path), Ok(()));

        let external = provision_for_test(&root, "external", &temp);
        std::fs::remove_dir_all(&external.path).unwrap();
        assert_eq!(remove(&root, &external.path), Ok(()));
        assert_eq!(remove(&root, &external.path), Ok(()));
        std::fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn merge_back_integrates_clean_descendant_or_divergent_branches() {
        for diverged in [false, true] {
            let (temp, root) = scratch_repo("tcode-merge-back");
            let created = provision_for_test(&root, "feature", &temp);
            commit_file(&created.path, "feature.txt", "feature\n", "feature");
            if diverged {
                commit_file(&root, "destination.txt", "destination\n", "destination");
            }
            let expected = if diverged {
                MergeBackOutcome::MergeCommit
            } else {
                MergeBackOutcome::FastForward
            };
            assert_eq!(
                merge_back(&root, &created.path, &created.branch),
                Ok(expected)
            );
            assert_eq!(
                std::fs::read_to_string(root.join("feature.txt")).unwrap(),
                "feature\n"
            );
            if diverged {
                assert_eq!(
                    std::fs::read_to_string(root.join("destination.txt")).unwrap(),
                    "destination\n"
                );
                let parents = run_git(&root, &["show", "-s", "--format=%P", "HEAD"]).unwrap();
                assert_eq!(parents.split_whitespace().count(), 2);
            } else {
                assert_eq!(
                    run_git(&root, &["rev-parse", "HEAD"]).unwrap(),
                    run_git(&created.path, &["rev-parse", "HEAD"]).unwrap()
                );
            }
            remove(&root, &created.path).unwrap();
            std::fs::remove_dir_all(temp).unwrap();
        }
    }

    #[test]
    fn merge_back_refuses_either_dirty_side_without_changing_files() {
        for dirty_destination in [false, true] {
            let (temp, root) = scratch_repo("tcode-merge-dirty");
            let created = provision_for_test(&root, "dirty", &temp);
            commit_file(&created.path, "feature.txt", "feature\n", "feature");
            let (dirty, expected) = if dirty_destination {
                (&root, MergeBackError::DirtyDestination)
            } else {
                (&created.path, MergeBackError::DirtyWorktree)
            };
            std::fs::write(dirty.join("tracked.txt"), "uncommitted\n").unwrap();
            assert_eq!(
                merge_back(&root, &created.path, &created.branch),
                Err(expected)
            );
            assert_eq!(
                std::fs::read_to_string(dirty.join("tracked.txt")).unwrap(),
                "uncommitted\n"
            );
            assert!(!root.join("feature.txt").exists());
            std::fs::remove_dir_all(temp).unwrap();
        }
    }

    #[test]
    fn merge_back_aborts_conflict_and_restores_clean_destination() {
        let (temp, root) = scratch_repo("tcode-merge-back-conflict-test");
        let created = provision_for_test(&root, "conflict", &temp);
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
        let created = provision_for_test(&root, "seed", &temp);
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
    fn seed_rejects_traversal_before_creating_a_worktree() {
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
    fn removal_deletes_the_branch_only_when_it_has_no_commits_of_its_own() {
        let (temp, root) = scratch_repo("tcode-worktree-branch-removal");
        let untouched = provision_for_test(&root, "untouched", &temp);
        let worked = provision_for_test(&root, "worked", &temp);
        commit_file(&worked.path, "feature.txt", "feature\n", "feature");
        let vanished = provision_for_test(&root, "vanished", &temp);
        std::fs::remove_dir_all(&vanished.path).unwrap();

        for created in [&untouched, &worked, &vanished] {
            remove(&root, &created.path).unwrap();
        }
        assert!(!has_branch(&root, &untouched.branch));
        assert!(!has_branch(&root, &vanished.branch));
        assert!(has_branch(&root, &worked.branch));
        let _ = std::fs::remove_dir_all(temp);
    }

    #[test]
    fn worktrees_of_one_repository_share_its_directory_and_only_its() {
        let (temp, root) = scratch_repo("tcode-worktree-bucket");
        let (other_temp, same_name) = scratch_repo("tcode-worktree-bucket-same-name");
        assert_eq!(root.file_name(), same_name.file_name());
        let first = provision_for_test(&root, "first", &temp);
        let bucket = first.path.parent().unwrap();

        // A subagent of a thread that already works in a worktree asks from there.
        let nested = provision(&first.path, "nested", None, &temp).unwrap();
        assert_eq!(nested.path.parent(), Some(bucket));
        assert_eq!(nested.base, first.branch);
        assert!(registered_worktree(&root, &nested.path).unwrap().is_some());
        std::fs::create_dir(first.path.join("sub")).unwrap();
        assert!(matches!(
            provision(&first.path.join("sub"), "inner", None, &temp),
            Err(ProvisionError::NotRepositoryRoot { .. })
        ));

        let escaped = provision(&root, "../escape", None, &temp).unwrap();
        assert_eq!(escaped.path.parent(), Some(bucket));

        let other = provision_for_test(&same_name, "first", &temp);
        assert_ne!(other.path.parent(), Some(bucket));
        assert!(
            registered_worktree(&same_name, &other.path)
                .unwrap()
                .is_some()
        );
        let _ = std::fs::remove_dir_all(temp);
        let _ = std::fs::remove_dir_all(other_temp);
    }

    #[test]
    fn orphan_cleanup_uses_age_and_ownership_in_either_layout_and_leaves_unregistered_directories()
    {
        let (temp, root) = scratch_repo("tcode-worktree-orphan-age-test");
        let worktrees = worktrees_root(&temp);
        let orphan = provision_for_test(&root, "orphan", &temp).path;
        let modified = std::fs::metadata(&orphan).unwrap().modified().unwrap();
        let kept = provision_for_test(&root, "kept", &temp).path;
        // A deleted source whose fork still runs in its worktree.
        let forked = provision_for_test(&root, "deleted-source", &temp).path;
        // Unknown, but holding uncommitted work: removal is never forced.
        let dirty = provision_for_test(&root, "kept-on-delete", &temp).path;
        std::fs::write(dirty.join("notes.txt"), "uncommitted\n").unwrap();
        // Provisioned before worktrees were grouped by repository.
        let legacy = |id: &str| {
            let path = worktrees.join(id);
            let branch = format!("tcode/{id}");
            run(
                &root,
                &[
                    "worktree",
                    "add",
                    "-b",
                    &branch,
                    path.to_str().unwrap(),
                    "main",
                ],
            );
            path
        };
        let legacy_in_use = legacy("legacy-thread");
        let legacy_orphan = legacy("legacy-orphan");
        let known = HashSet::from(["kept".to_string()]);
        let cwds = [forked.clone(), legacy_in_use.clone()];

        let fresh = cleanup_orphans_at(&worktrees, &known, &cwds, modified, ORPHAN_MIN_AGE);
        assert!(fresh.removed.is_empty());
        let mut skipped = fresh.skipped;
        skipped.sort();
        let mut expected = [dirty.clone(), legacy_orphan.clone(), orphan.clone()];
        expected.sort();
        assert_eq!(skipped, expected);
        assert!(orphan.exists());

        let later = || SystemTime::now() + ORPHAN_MIN_AGE + Duration::from_secs(1);
        let old = cleanup_orphans_at(&worktrees, &known, &cwds, later(), ORPHAN_MIN_AGE);
        let mut removed = old.removed;
        removed.sort();
        let mut expected = [legacy_orphan.clone(), orphan.clone()];
        expected.sort();
        assert_eq!(removed, expected);
        assert_eq!(old.skipped.as_slice(), std::slice::from_ref(&dirty));
        assert!(!orphan.exists() && !legacy_orphan.exists());
        assert!(!has_branch(&root, "tcode/orphan"));
        assert!(!has_branch(&root, "tcode/legacy-orphan"));
        assert!(kept.join("tracked.txt").exists());
        assert!(forked.join("tracked.txt").exists());
        assert!(legacy_in_use.join("tracked.txt").exists());
        assert!(has_branch(&root, "tcode/legacy-thread"));
        assert!(dirty.join("notes.txt").exists());
        let unrelated = kept.parent().unwrap().join("unregistered");
        std::fs::create_dir(&unrelated).unwrap();
        let summary = cleanup_orphans_at(&worktrees, &known, &cwds, later(), ORPHAN_MIN_AGE);
        assert!(summary.removed.is_empty());
        let mut skipped = summary.skipped;
        skipped.sort();
        let mut expected = [dirty, unrelated.clone()];
        expected.sort();
        assert_eq!(skipped, expected);
        assert!(unrelated.exists());
        let _ = std::fs::remove_dir_all(temp);
    }
}
