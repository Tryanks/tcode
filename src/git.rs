//! Git quick-actions support (ported from T3's `GitActionsControl.logic.ts`,
//! `GitWorkflowService.ts` and `GitVcsDriverCore.ts`, trimmed to the local,
//! single-SCM subset — no PR/MR, no publish-to-provider wizard).
//!
//! This module is deliberately split into a *pure* core (status parsing, the
//! adaptive quick-action state machine, path-spec selection, slug and prompt
//! builders — all unit-tested against fixtures) and a thin *IO* layer
//! (`std::process::Command` wrappers) driven from background tasks in
//! [`crate::app`].

use std::collections::HashSet;
use std::path::Path;

// ---------------------------------------------------------------------------
// Status model
// ---------------------------------------------------------------------------

/// One changed file in the working tree (staged and/or unstaged), with its
/// combined line delta (0/0 for untracked files with no numstat).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitFileEntry {
    pub path: String,
    pub insertions: u32,
    pub deletions: u32,
}

/// A snapshot of a repository's state, driving the adaptive quick-action
/// button. Mirrors the subset of T3's `VcsStatusResult` we act on.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GitStatus {
    /// `cwd` is inside a git working tree.
    pub is_repo: bool,
    /// `HEAD` resolves to a commit (false for a fresh `git init` with no commit).
    pub has_commits: bool,
    /// Detached HEAD (no current branch).
    pub detached: bool,
    /// Current branch name (`None` when detached / no branch).
    pub branch: Option<String>,
    /// The branch is the repo's default ref (`origin/HEAD`, else main/master).
    pub is_default_branch: bool,
    /// The working tree has staged or unstaged changes (or untracked files).
    pub has_working_tree_changes: bool,
    /// An `origin` remote is configured.
    pub has_origin_remote: bool,
    /// The current branch has a configured upstream.
    pub has_upstream: bool,
    /// Commits ahead of upstream.
    pub ahead: u32,
    /// Commits behind upstream.
    pub behind: u32,
    /// Changed files (for the commit dialog list).
    pub changed_files: Vec<GitFileEntry>,
}

impl GitStatus {
    /// The branch diverged from its upstream (both ahead and behind) — a
    /// fast-forward pull is impossible, so Pull is offered disabled.
    pub fn diverged(&self) -> bool {
        self.ahead > 0 && self.behind > 0
    }
}

// ---------------------------------------------------------------------------
// Quick-action state machine
// ---------------------------------------------------------------------------

/// An executable git operation behind the quick-action button / dropdown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitAction {
    /// Open the commit dialog, then `git commit` the selected files.
    Commit,
    /// Open the commit dialog, then commit and `git push`.
    CommitPush,
    /// `git push` existing local commits.
    Push,
    /// `git pull --ff-only`.
    Pull,
    /// `git push -u origin <branch>` (set upstream for a branch that has none).
    PublishBranch,
    /// `git init` in a non-repo cwd.
    InitializeGit,
}

impl GitAction {
    /// Whether triggering this action opens the commit dialog first.
    pub fn opens_commit_dialog(self) -> bool {
        matches!(self, GitAction::Commit | GitAction::CommitPush)
    }
}

/// The resolved state of the adaptive quick-action button. `action` is `None`
/// when the button is a disabled status hint. `label_key`/`hint_key` are i18n
/// keys resolved by the UI (`rust_i18n::t!`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuickAction {
    pub action: Option<GitAction>,
    pub label_key: &'static str,
    pub hint_key: Option<&'static str>,
    pub disabled: bool,
}

impl QuickAction {
    fn run(action: GitAction, label_key: &'static str) -> Self {
        Self { action: Some(action), label_key, hint_key: None, disabled: false }
    }

    fn hint(label_key: &'static str, hint_key: &'static str) -> Self {
        Self { action: None, label_key, hint_key: Some(hint_key), disabled: true }
    }
}

/// Resolve the primary quick-action for `status`. `is_busy` is true while an
/// action is already running (the button is disabled with an in-progress hint).
///
/// Ported/trimmed from T3's `resolveQuickAction` — the PR/MR and
/// publish-repository branches collapse to `PublishBranch` (push `-u`) and the
/// disabled hints.
pub fn quick_action(status: &GitStatus, is_busy: bool) -> QuickAction {
    if is_busy {
        return QuickAction::hint("git.action.commit", "git.hint.in_progress");
    }
    if !status.is_repo {
        return QuickAction::run(GitAction::InitializeGit, "git.action.init");
    }
    if status.detached {
        return QuickAction::hint("git.action.commit", "git.hint.detached");
    }
    let dirty = status.has_working_tree_changes;
    // A fresh repo with no commit yet: the only thing to do is the first commit.
    if !status.has_commits {
        if dirty {
            return QuickAction::run(GitAction::Commit, "git.action.commit");
        }
        return QuickAction::hint("git.action.commit", "git.hint.no_commits");
    }
    if dirty {
        if status.has_upstream {
            return QuickAction::run(GitAction::CommitPush, "git.action.commit_push");
        }
        return QuickAction::run(GitAction::Commit, "git.action.commit");
    }
    // Clean working tree.
    if !status.has_upstream {
        if status.has_origin_remote && status.branch.is_some() {
            return QuickAction::run(GitAction::PublishBranch, "git.action.publish");
        }
        return QuickAction::hint("git.action.commit", "git.hint.no_remote");
    }
    if status.diverged() {
        return QuickAction::hint("git.action.pull", "git.hint.diverged");
    }
    if status.behind > 0 {
        return QuickAction::run(GitAction::Pull, "git.action.pull");
    }
    if status.ahead > 0 {
        return QuickAction::run(GitAction::Push, "git.action.push");
    }
    QuickAction::hint("git.action.commit", "git.hint.up_to_date")
}

/// One entry of the quick-action dropdown (the applicable subset for `status`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuItem {
    pub action: GitAction,
    pub label_key: &'static str,
    pub disabled: bool,
    pub hint_key: Option<&'static str>,
}

/// Build the quick-action dropdown items for `status`. Always offers the
/// actions that make sense for the repo shape, disabling the inapplicable ones
/// with a reason (T3's `buildMenuItems` + the exact disabled hints).
pub fn menu_items(status: &GitStatus, is_busy: bool) -> Vec<MenuItem> {
    if !status.is_repo {
        return vec![MenuItem {
            action: GitAction::InitializeGit,
            label_key: "git.action.init",
            disabled: is_busy,
            hint_key: None,
        }];
    }

    let mut items = Vec::new();
    let dirty = status.has_working_tree_changes;
    let can_commit = !is_busy && dirty && !status.detached;
    items.push(MenuItem {
        action: GitAction::Commit,
        label_key: "git.action.commit",
        disabled: !can_commit,
        hint_key: (!can_commit).then(|| commit_disabled_hint(status)),
    });

    // Push: needs commits ahead of a configured upstream, clean tree.
    let can_push = !is_busy
        && status.has_upstream
        && status.ahead > 0
        && !status.behind_blocks_push()
        && !status.detached;
    items.push(MenuItem {
        action: GitAction::Push,
        label_key: "git.action.push",
        disabled: !can_push,
        hint_key: (!can_push).then(|| push_disabled_hint(status)),
    });

    // Pull: behind a configured upstream, fast-forwardable.
    let can_pull = !is_busy && status.has_upstream && status.behind > 0 && !status.diverged();
    if status.has_upstream {
        items.push(MenuItem {
            action: GitAction::Pull,
            label_key: "git.action.pull",
            disabled: !can_pull,
            hint_key: (!can_pull).then(|| pull_disabled_hint(status)),
        });
    }

    // Publish branch: no upstream yet but an origin remote exists.
    if !status.has_upstream && status.has_origin_remote {
        let can_publish = !is_busy && status.branch.is_some();
        items.push(MenuItem {
            action: GitAction::PublishBranch,
            label_key: "git.action.publish",
            disabled: !can_publish,
            hint_key: (!can_publish).then_some("git.hint.detached"),
        });
    }

    items
}

impl GitStatus {
    fn behind_blocks_push(&self) -> bool {
        self.behind > 0
    }
}

fn commit_disabled_hint(status: &GitStatus) -> &'static str {
    if status.detached {
        "git.hint.detached"
    } else {
        "git.hint.no_changes"
    }
}

fn push_disabled_hint(status: &GitStatus) -> &'static str {
    if status.detached {
        "git.hint.detached"
    } else if !status.has_upstream {
        "git.hint.no_upstream"
    } else if status.diverged() {
        "git.hint.diverged"
    } else if status.behind > 0 {
        "git.hint.behind"
    } else {
        "git.hint.no_ahead"
    }
}

fn pull_disabled_hint(status: &GitStatus) -> &'static str {
    if status.diverged() {
        "git.hint.diverged"
    } else {
        "git.hint.up_to_date"
    }
}

// ---------------------------------------------------------------------------
// Path-spec selection
// ---------------------------------------------------------------------------

/// The path-spec to stage for a commit given the changed files and the set of
/// user-*excluded* (unchecked) paths.
///
/// Returns `None` when nothing is excluded (stage everything: `git add -A`),
/// otherwise `Some(included)` — the checked subset, staged explicitly so
/// unchecked files are left out of the commit. Ported from T3's
/// `selectedFiles`/`filePaths` handling in `GitActionsControl.tsx`.
pub fn included_paths(all: &[GitFileEntry], excluded: &HashSet<String>) -> Option<Vec<String>> {
    if excluded.is_empty() {
        return None;
    }
    Some(
        all.iter()
            .filter(|f| !excluded.contains(&f.path))
            .map(|f| f.path.clone())
            .collect(),
    )
}

// ---------------------------------------------------------------------------
// Slug / feature-branch name
// ---------------------------------------------------------------------------

/// Sanitize an arbitrary string into a lowercase git ref fragment (T3's
/// `sanitizeBranchFragment`): strip quotes, collapse separators, cap at 48
/// chars. Falls back to `"update"` when empty.
pub fn sanitize_branch_fragment(raw: &str) -> String {
    let is_valid = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '/' | '_' | '-');
    let is_edge_trim = |c: char| matches!(c, '.' | '/' | '_' | '-' | ' ' | '\t' | '\n' | '\r');

    // Trim, lowercase, drop quotes, then trim separator-ish edges.
    let lowered: String = raw
        .trim()
        .to_lowercase()
        .chars()
        .filter(|c| !matches!(c, '\'' | '"' | '`'))
        .collect();
    let normalized = lowered.trim_matches(is_edge_trim);

    // Replace runs of invalid chars with a single '-'.
    let mut out = String::with_capacity(normalized.len());
    let mut in_invalid_run = false;
    for ch in normalized.chars() {
        if is_valid(ch) {
            out.push(ch);
            in_invalid_run = false;
        } else if !in_invalid_run {
            out.push('-');
            in_invalid_run = true;
        }
    }

    // Collapse runs of '/' and of '-' (underscores are preserved, as in T3).
    let mut collapsed = String::with_capacity(out.len());
    let mut prev: Option<char> = None;
    for ch in out.chars() {
        if matches!(ch, '/' | '-') && prev == Some(ch) {
            continue;
        }
        collapsed.push(ch);
        prev = Some(ch);
    }

    let trimmed = collapsed.trim_matches(|c| matches!(c, '.' | '/' | '_' | '-'));
    let capped: String = trimmed.chars().take(48).collect();
    let capped = capped.trim_end_matches(|c| matches!(c, '.' | '/' | '_' | '-'));
    if capped.is_empty() {
        "update".to_string()
    } else {
        capped.to_string()
    }
}

/// The feature-branch name for the default-branch safeguard "create a feature
/// branch" path: `tcode/<slug>` derived from the commit subject.
pub fn feature_branch_name(subject: &str) -> String {
    format!("tcode/{}", sanitize_branch_fragment(subject))
}

// ---------------------------------------------------------------------------
// AI commit-message prompt + sanitizer
// ---------------------------------------------------------------------------

/// Max chars of the patch fed to the model (keeps the headless call cheap).
const COMMIT_PATCH_MAX: usize = 8_000;

/// Build the plain-text prompt for the headless commit-message generation
/// (`claude -p`). Includes the diff stat and a truncated patch.
pub fn build_commit_prompt(stat: &str, patch: &str) -> String {
    let patch = if patch.len() > COMMIT_PATCH_MAX {
        let mut cut = COMMIT_PATCH_MAX;
        while !patch.is_char_boundary(cut) {
            cut -= 1;
        }
        format!("{}\n… (patch truncated)", &patch[..cut])
    } else {
        patch.to_string()
    };
    format!(
        "You write concise git commit messages.\n\
         Write a single conventional commit message for the staged changes below.\n\
         Rules:\n\
         - subject line must be imperative mood, at most 72 characters, no trailing period\n\
         - optionally add a short body after a blank line\n\
         - output ONLY the commit message text — no explanation, no code fences\n\
         \n\
         Diff stat:\n{stat}\n\
         \n\
         Patch:\n{patch}\n",
    )
}

/// Sanitize a model-produced commit message: strip surrounding code fences,
/// trim, and cap the subject line at 72 chars. Empty output falls back to the
/// caller (which re-generates), so an empty string is returned as-is.
pub fn sanitize_commit_message(raw: &str) -> String {
    let mut text = raw.trim();
    // Strip a leading/trailing ``` fence block if present.
    if let Some(rest) = text.strip_prefix("```") {
        // Drop the fence's info line.
        let rest = rest.splitn(2, '\n').nth(1).unwrap_or("");
        text = rest.trim_end().strip_suffix("```").unwrap_or(rest).trim();
    }
    let mut lines = text.lines();
    let Some(subject_raw) = lines.next() else {
        return String::new();
    };
    let subject: String = subject_raw.trim().trim_end_matches('.').chars().take(72).collect();
    let subject = subject.trim().to_string();
    let body: Vec<&str> = lines.collect();
    let body = body.join("\n");
    let body = body.trim();
    if subject.is_empty() {
        return String::new();
    }
    if body.is_empty() {
        subject
    } else {
        format!("{subject}\n\n{body}")
    }
}

// ---------------------------------------------------------------------------
// Status parsing (pure)
// ---------------------------------------------------------------------------

/// Parse `git status --porcelain=2 --branch` output plus the numstat map into
/// the local part of a [`GitStatus`]. `default_branch` is the short name from
/// `origin/HEAD` (or `None`); `has_origin_remote` from `git remote`.
///
/// The caller fills `is_repo` (true here) — this parser assumes a repo.
pub fn parse_status(
    porcelain: &str,
    numstat: &[(String, u32, u32)],
    default_branch: Option<&str>,
    has_origin_remote: bool,
) -> GitStatus {
    let mut branch: Option<String> = None;
    let mut detached = false;
    let mut has_commits = true;
    let mut has_upstream = false;
    let mut ahead = 0u32;
    let mut behind = 0u32;
    let mut has_working_tree_changes = false;
    let mut paths: Vec<String> = Vec::new();

    for line in porcelain.lines() {
        if let Some(value) = line.strip_prefix("# branch.oid ") {
            if value.trim() == "(initial)" {
                has_commits = false;
            }
        } else if let Some(value) = line.strip_prefix("# branch.head ") {
            let value = value.trim();
            if value == "(detached)" {
                detached = true;
                branch = None;
            } else {
                branch = Some(value.to_string());
            }
        } else if let Some(value) = line.strip_prefix("# branch.upstream ") {
            has_upstream = !value.trim().is_empty();
        } else if let Some(value) = line.strip_prefix("# branch.ab ") {
            let (a, b) = parse_branch_ab(value.trim());
            ahead = a;
            behind = b;
        } else if !line.starts_with('#') && !line.trim().is_empty() {
            has_working_tree_changes = true;
            if let Some(path) = parse_porcelain_path(line) {
                if !paths.contains(&path) {
                    paths.push(path);
                }
            }
        }
    }

    let numstat_map: std::collections::HashMap<&str, (u32, u32)> =
        numstat.iter().map(|(p, i, d)| (p.as_str(), (*i, *d))).collect();
    paths.sort();
    let changed_files = paths
        .into_iter()
        .map(|path| {
            let (insertions, deletions) = numstat_map.get(path.as_str()).copied().unwrap_or((0, 0));
            GitFileEntry { path, insertions, deletions }
        })
        .collect();

    let is_default_branch = match (&branch, default_branch) {
        (Some(b), Some(d)) => b == d,
        (Some(b), None) => b == "main" || b == "master",
        _ => false,
    };

    GitStatus {
        is_repo: true,
        has_commits,
        detached,
        branch,
        is_default_branch,
        has_working_tree_changes,
        has_origin_remote,
        has_upstream,
        ahead,
        behind,
        changed_files,
    }
}

/// Parse `+A -B` from a `# branch.ab` value.
fn parse_branch_ab(value: &str) -> (u32, u32) {
    let mut ahead = 0;
    let mut behind = 0;
    for token in value.split_whitespace() {
        if let Some(rest) = token.strip_prefix('+') {
            ahead = rest.parse().unwrap_or(0);
        } else if let Some(rest) = token.strip_prefix('-') {
            behind = rest.parse().unwrap_or(0);
        }
    }
    (ahead, behind)
}

/// Extract the file path from a porcelain=2 changed-entry line.
fn parse_porcelain_path(line: &str) -> Option<String> {
    let mut chars = line.chars();
    let kind = chars.next()?;
    match kind {
        // Untracked / ignored: "? path" or "! path".
        '?' | '!' => Some(line[2..].trim().to_string()),
        // Ordinary "1 ..." (8 metadata fields then path) or rename "2 ..."
        // (9 fields then "path\torig").
        '1' | '2' => {
            let skip = if kind == '1' { 8 } else { 9 };
            let rest = line.splitn(skip + 1, ' ').nth(skip)?;
            // Rename entries encode "path\torig"; take the new path.
            let path = rest.split('\t').next().unwrap_or(rest);
            Some(path.trim().to_string())
        }
        // Unmerged "u ..." (10 fields then path).
        'u' => {
            let rest = line.splitn(11, ' ').nth(10)?;
            Some(rest.trim().to_string())
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// IO layer
// ---------------------------------------------------------------------------

/// Run `git <args>` in `cwd`, returning stdout on success or a
/// `git <cmd> failed: <stderr>` error on non-zero exit / spawn failure.
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

/// Whether `cwd` is inside a git working tree.
pub fn is_git_repo(cwd: &Path) -> bool {
    crate::checkpoints::is_git_repo(cwd)
}

/// Read a full [`GitStatus`] for `cwd` (runs several git commands; call from a
/// background task).
pub fn read_status(cwd: &Path) -> GitStatus {
    if !is_git_repo(cwd) {
        return GitStatus::default();
    }
    let porcelain = run_git(cwd, &["status", "--porcelain=2", "--branch"]).unwrap_or_default();
    let numstat = read_numstat(cwd);
    let has_origin_remote = run_git(cwd, &["remote"])
        .map(|out| out.lines().any(|l| l.trim() == "origin"))
        .unwrap_or(false);
    let default_branch = run_git(cwd, &["symbolic-ref", "refs/remotes/origin/HEAD"])
        .ok()
        .map(|s| s.trim().trim_start_matches("refs/remotes/origin/").to_string())
        .filter(|s| !s.is_empty());
    parse_status(&porcelain, &numstat, default_branch.as_deref(), has_origin_remote)
}

/// Combined staged + unstaged numstat for `cwd` (path, insertions, deletions).
fn read_numstat(cwd: &Path) -> Vec<(String, u32, u32)> {
    let mut out = Vec::new();
    for args in [
        ["diff", "--numstat"].as_slice(),
        ["diff", "--cached", "--numstat"].as_slice(),
    ] {
        if let Ok(text) = run_git(cwd, args) {
            for line in text.lines() {
                let mut cols = line.split('\t');
                let (Some(ins), Some(del), Some(path)) = (cols.next(), cols.next(), cols.next())
                else {
                    continue;
                };
                let ins = ins.parse().unwrap_or(0);
                let del = del.parse().unwrap_or(0);
                out.push((path.to_string(), ins, del));
            }
        }
    }
    out
}

/// The diff stat + patch used to seed the AI commit-message generation, scoped
/// to `included` paths when given. Uses `HEAD`-relative diffs so tracked
/// changes are captured without staging.
pub fn commit_diff_context(cwd: &Path, included: Option<&[String]>) -> (String, String) {
    let has_head = run_git(cwd, &["rev-parse", "--verify", "HEAD"]).is_ok();

    let pathspec: Vec<&str> = match included {
        Some(paths) if !paths.is_empty() => {
            let mut e = vec!["--"];
            e.extend(paths.iter().map(String::as_str));
            e
        }
        _ => Vec::new(),
    };

    let mut stat_args: Vec<&str> = vec!["diff", "--stat"];
    let mut patch_args: Vec<&str> = vec!["diff", "--no-ext-diff", "--patch", "--minimal"];
    if has_head {
        stat_args.push("HEAD");
        patch_args.push("HEAD");
    }
    stat_args.extend_from_slice(&pathspec);
    patch_args.extend_from_slice(&pathspec);

    let mut stat = run_git(cwd, &stat_args).unwrap_or_default();
    if stat.trim().is_empty() {
        // Fall back to the porcelain short status (e.g. only untracked files).
        stat = run_git(cwd, &["status", "--short"]).unwrap_or_default();
    }
    let patch = run_git(cwd, &patch_args).unwrap_or_default();
    (stat, patch)
}

/// Run `claude -p <prompt>` headlessly in `cwd` and return its stdout. Used to
/// generate a commit message (provider fixed to Claude for v1). `binary` is the
/// configured claude path, falling back to `claude` on `PATH`.
pub fn run_claude_headless(binary: Option<&Path>, cwd: &Path, prompt: &str) -> Result<String, String> {
    let bin = binary
        .map(|p| p.to_string_lossy().into_owned())
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

/// Execute a resolved [`GitAction`] synchronously (call from a background task),
/// returning a combined success transcript or an error detail (for the toast).
///
/// `feature_branch` — when set, a `git checkout -b <name>` runs first (the
/// default-branch safeguard's "create a feature branch" path). `current_branch`
/// is the branch to publish for [`GitAction::PublishBranch`].
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
            if let Some(fb) = feature_branch {
                run_git(cwd, &["checkout", "-b", fb])?;
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
            Ok(run_git(cwd, &["push", "-u", "origin", branch])?.trim().to_string())
        }
    }
}

/// Stage the selected paths for a commit: `git add -A` for everything, or
/// `git reset` + `git add -A -- <included>` for a subset (T3 semantics).
pub fn stage_for_commit(cwd: &Path, included: Option<&[String]>) -> Result<(), String> {
    match included {
        Some(paths) if !paths.is_empty() => {
            // Unstage everything first so only the selected subset is committed.
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

    fn dirty_upstream() -> GitStatus {
        GitStatus {
            is_repo: true,
            has_commits: true,
            has_working_tree_changes: true,
            has_upstream: true,
            has_origin_remote: true,
            branch: Some("feature/x".into()),
            ..Default::default()
        }
    }

    #[test]
    fn quick_action_non_repo_is_init() {
        let s = GitStatus::default();
        let qa = quick_action(&s, false);
        assert_eq!(qa.action, Some(GitAction::InitializeGit));
        assert!(!qa.disabled);
    }

    #[test]
    fn quick_action_busy_is_disabled() {
        let qa = quick_action(&dirty_upstream(), true);
        assert!(qa.disabled);
        assert_eq!(qa.hint_key, Some("git.hint.in_progress"));
    }

    #[test]
    fn quick_action_dirty_with_upstream_is_commit_push() {
        assert_eq!(quick_action(&dirty_upstream(), false).action, Some(GitAction::CommitPush));
    }

    #[test]
    fn quick_action_dirty_without_upstream_is_commit() {
        let s = GitStatus { has_upstream: false, ..dirty_upstream() };
        assert_eq!(quick_action(&s, false).action, Some(GitAction::Commit));
    }

    #[test]
    fn quick_action_clean_ahead_is_push() {
        let s = GitStatus {
            has_working_tree_changes: false,
            ahead: 2,
            ..dirty_upstream()
        };
        assert_eq!(quick_action(&s, false).action, Some(GitAction::Push));
    }

    #[test]
    fn quick_action_behind_is_pull() {
        let s = GitStatus {
            has_working_tree_changes: false,
            behind: 3,
            ..dirty_upstream()
        };
        assert_eq!(quick_action(&s, false).action, Some(GitAction::Pull));
    }

    #[test]
    fn quick_action_diverged_is_disabled_with_reason() {
        let s = GitStatus {
            has_working_tree_changes: false,
            ahead: 1,
            behind: 1,
            ..dirty_upstream()
        };
        let qa = quick_action(&s, false);
        assert!(qa.disabled);
        assert_eq!(qa.hint_key, Some("git.hint.diverged"));
    }

    #[test]
    fn quick_action_clean_no_upstream_with_remote_is_publish() {
        let s = GitStatus {
            has_working_tree_changes: false,
            has_upstream: false,
            has_origin_remote: true,
            ..dirty_upstream()
        };
        assert_eq!(quick_action(&s, false).action, Some(GitAction::PublishBranch));
    }

    #[test]
    fn quick_action_detached_is_disabled() {
        let s = GitStatus { is_repo: true, detached: true, ..Default::default() };
        let qa = quick_action(&s, false);
        assert!(qa.disabled);
        assert_eq!(qa.hint_key, Some("git.hint.detached"));
    }

    #[test]
    fn quick_action_no_commits_dirty_is_commit() {
        let s = GitStatus {
            is_repo: true,
            has_commits: false,
            has_working_tree_changes: true,
            ..Default::default()
        };
        assert_eq!(quick_action(&s, false).action, Some(GitAction::Commit));
    }

    #[test]
    fn menu_items_disable_with_reasons() {
        // Clean, no upstream, with remote: Commit disabled (no changes),
        // Push disabled (no upstream), Publish enabled.
        let s = GitStatus {
            is_repo: true,
            has_commits: true,
            has_origin_remote: true,
            branch: Some("main".into()),
            ..Default::default()
        };
        let items = menu_items(&s, false);
        let commit = items.iter().find(|i| i.action == GitAction::Commit).unwrap();
        assert!(commit.disabled && commit.hint_key == Some("git.hint.no_changes"));
        let push = items.iter().find(|i| i.action == GitAction::Push).unwrap();
        assert!(push.disabled && push.hint_key == Some("git.hint.no_upstream"));
        let publish = items.iter().find(|i| i.action == GitAction::PublishBranch).unwrap();
        assert!(!publish.disabled);
    }

    #[test]
    fn included_paths_none_when_nothing_excluded() {
        let all = vec![
            GitFileEntry { path: "a.rs".into(), insertions: 1, deletions: 0 },
            GitFileEntry { path: "b.rs".into(), insertions: 0, deletions: 2 },
        ];
        assert_eq!(included_paths(&all, &HashSet::new()), None);
    }

    #[test]
    fn included_paths_excludes_unchecked() {
        let all = vec![
            GitFileEntry { path: "a.rs".into(), insertions: 1, deletions: 0 },
            GitFileEntry { path: "b.rs".into(), insertions: 0, deletions: 2 },
            GitFileEntry { path: "c.rs".into(), insertions: 3, deletions: 3 },
        ];
        let excluded: HashSet<String> = ["b.rs".to_string()].into_iter().collect();
        assert_eq!(
            included_paths(&all, &excluded),
            Some(vec!["a.rs".to_string(), "c.rs".to_string()])
        );
    }

    #[test]
    fn slug_generation() {
        assert_eq!(sanitize_branch_fragment("Add: Feature!! Foo"), "add-feature-foo");
        // Underscores are preserved (T3 semantics); separator edges are trimmed.
        assert_eq!(sanitize_branch_fragment("  --Weird__Name--  "), "weird__name");
        assert_eq!(sanitize_branch_fragment("feat/thing bar"), "feat/thing-bar");
        assert_eq!(sanitize_branch_fragment("feat//a///b"), "feat/a/b");
        assert_eq!(sanitize_branch_fragment("***"), "update");
        assert_eq!(feature_branch_name("Fix the parser"), "tcode/fix-the-parser");
        assert_eq!(
            feature_branch_name("feat: add toast system!"),
            "tcode/feat-add-toast-system"
        );
    }

    #[test]
    fn commit_message_sanitizer() {
        assert_eq!(sanitize_commit_message("feat: do the thing."), "feat: do the thing");
        assert_eq!(
            sanitize_commit_message("```\nfix: bug\n\nbody line\n```"),
            "fix: bug\n\nbody line"
        );
        let long = "x".repeat(100);
        assert_eq!(sanitize_commit_message(&long).len(), 72);
        assert_eq!(sanitize_commit_message("   "), "");
    }

    #[test]
    fn parse_status_dirty_ahead_behind() {
        let porcelain = "\
# branch.oid abc123
# branch.head feature/x
# branch.upstream origin/feature/x
# branch.ab +2 -1
1 .M N... 100644 100644 100644 abc def src/app.rs
? new_file.txt
";
        let numstat = vec![("src/app.rs".to_string(), 5, 3)];
        let s = parse_status(porcelain, &numstat, Some("main"), true);
        assert!(s.has_commits && !s.detached);
        assert_eq!(s.branch.as_deref(), Some("feature/x"));
        assert!(s.has_upstream && s.has_working_tree_changes);
        assert_eq!((s.ahead, s.behind), (2, 1));
        assert!(!s.is_default_branch);
        assert_eq!(s.changed_files.len(), 2);
        let app = s.changed_files.iter().find(|f| f.path == "src/app.rs").unwrap();
        assert_eq!((app.insertions, app.deletions), (5, 3));
    }

    #[test]
    fn parse_status_initial_and_default_branch() {
        let porcelain = "\
# branch.oid (initial)
# branch.head main
";
        let s = parse_status(porcelain, &[], None, false);
        assert!(!s.has_commits);
        assert!(s.is_default_branch, "main is default when no origin/HEAD");
        assert!(!s.has_upstream);
    }

    #[test]
    fn parse_status_detached() {
        let porcelain = "# branch.oid abc\n# branch.head (detached)\n";
        let s = parse_status(porcelain, &[], None, false);
        assert!(s.detached);
        assert_eq!(s.branch, None);
    }
}
