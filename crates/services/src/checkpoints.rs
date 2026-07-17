//! Per-turn git checkpoints (Group B, ported from T3's `CheckpointStore.ts`).
//!
//! Before each user turn is dispatched in a git-repo cwd, tcode snapshots the
//! working tree into a hidden ref **without touching the index or worktree**:
//!
//! ```text
//! GIT_INDEX_FILE=<tmp> git add -A          # stage the whole worktree into a scratch index
//! GIT_INDEX_FILE=<tmp> git write-tree      # -> <tree>
//! git commit-tree <tree> [-p HEAD] -m …    # -> <commit>
//! git update-ref refs/tcode/checkpoints/<session>/<turn> <commit>
//! ```
//!
//! Reverting restores the worktree to a checkpoint commit (T3 semantics from
//! `GitVcsDriver.ts` — `git restore --source <commit> --worktree --staged -- .`,
//! then `git clean -fd -- .` to drop files created after the checkpoint, then
//! `git reset --quiet -- .` so the restored content shows as working-tree
//! changes rather than staged), after which the caller truncates the JSONL log
//! and deletes the now-orphaned newer checkpoint refs.

use std::path::Path;

use agent::{FileChange, FileChangeKind};

/// The ref name for one checkpoint (`refs/tcode/checkpoints/<session>/<turn>`).
fn checkpoint_ref(session_id: &str, turn: usize) -> String {
    format!("refs/tcode/checkpoints/{session_id}/{turn}")
}

/// The `refs/tcode/checkpoints/<session>/` prefix for a session's checkpoints.
fn checkpoint_ref_prefix(session_id: &str) -> String {
    format!("refs/tcode/checkpoints/{session_id}/")
}

/// Whether `cwd` is inside a git working tree (checkpoints only apply there).
pub fn is_git_repo(cwd: &Path) -> bool {
    crate::process::command("git")
        .args(["rev-parse", "--is-inside-work-tree"])
        .current_dir(cwd)
        .output()
        .map(|out| out.status.success() && String::from_utf8_lossy(&out.stdout).trim() == "true")
        .unwrap_or(false)
}

/// Run a git subcommand in `cwd`, optionally against a scratch index file
/// (`GIT_INDEX_FILE`). A deterministic identity is injected so `commit-tree`
/// works even when the environment has no configured git user.
fn run_git(cwd: &Path, args: &[&str], index: Option<&Path>) -> Result<String, String> {
    let mut cmd = crate::process::command("git");
    cmd.args(args)
        .current_dir(cwd)
        .env("GIT_AUTHOR_NAME", "tcode")
        .env("GIT_AUTHOR_EMAIL", "tcode@localhost")
        .env("GIT_COMMITTER_NAME", "tcode")
        .env("GIT_COMMITTER_EMAIL", "tcode@localhost");
    if let Some(index) = index {
        cmd.env("GIT_INDEX_FILE", index);
    }
    let out = cmd
        .output()
        .map_err(|err| format!("git {}: {err}", args.join(" ")))?;
    if !out.status.success() {
        return Err(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Compute the net changes made by one turn from its checkpoint tree to the
/// next checkpoint tree, or to the current worktree when there is no later
/// checkpoint. Only the supplied paths are included.
pub fn net_turn_file_changes(
    cwd: &Path,
    session_id: &str,
    base_turn: usize,
    target_turn: Option<usize>,
    paths: &[String],
) -> Result<Vec<FileChange>, String> {
    if paths.is_empty() {
        return Ok(Vec::new());
    }

    let base = checkpoint_ref(session_id, base_turn);
    if !checkpoint_ref_exists(cwd, session_id, base_turn) {
        return Err(format!("checkpoint ref does not exist: {base}"));
    }
    let target = target_turn.map(|turn| checkpoint_ref(session_id, turn));
    if let Some(turn) = target_turn
        && !checkpoint_ref_exists(cwd, session_id, turn)
    {
        return Err(format!(
            "checkpoint ref does not exist: {}",
            checkpoint_ref(session_id, turn)
        ));
    }

    let mut args = vec![
        "diff".to_string(),
        "--no-color".to_string(),
        "--no-ext-diff".to_string(),
        "--find-renames".to_string(),
        base,
    ];
    if let Some(target) = target {
        args.push(target);
    }
    args.push("--".to_string());
    for path in paths {
        let path = Path::new(path);
        let relative = path.strip_prefix(cwd).unwrap_or(path);
        args.push(relative.to_string_lossy().to_string());
    }

    let args_ref: Vec<&str> = args.iter().map(String::as_str).collect();
    let raw = run_git(cwd, &args_ref, None)?;
    let mut changes = split_git_patch(&raw, cwd);

    // `git diff <tree>` intentionally ignores untracked worktree files. Agent
    // creates are nevertheless named in `paths`, so add those explicitly when
    // the target is the worktree and the base tree has no such path.
    if target_turn.is_none() {
        for path in paths {
            let path = Path::new(path);
            let relative = path.strip_prefix(cwd).unwrap_or(path);
            let absolute = cwd.join(relative);
            if changes
                .iter()
                .any(|change| Path::new(&change.path) == absolute)
                || !absolute.is_file()
            {
                continue;
            }
            let tree_path = format!(
                "{}:{}",
                checkpoint_ref(session_id, base_turn),
                relative.display()
            );
            if run_git(cwd, &["cat-file", "-e", &tree_path], None).is_ok() {
                continue;
            }
            let output = crate::process::command("git")
                .args(["diff", "--no-color", "--no-index", "--", "/dev/null"])
                .arg(relative)
                .current_dir(cwd)
                .output()
                .map_err(|err| format!("git diff --no-index: {err}"))?;
            if !output.status.success() && output.status.code() != Some(1) {
                return Err(format!(
                    "git diff --no-index failed: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                ));
            }
            changes.extend(split_git_patch(
                &String::from_utf8_lossy(&output.stdout),
                cwd,
            ));
        }
    }
    Ok(changes)
}

fn split_git_patch(raw: &str, cwd: &Path) -> Vec<FileChange> {
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
            let old_null = patch.lines().any(|line| line == "--- /dev/null")
                || patch.lines().any(|line| line.starts_with("new file mode "));
            let new_null = patch.lines().any(|line| line == "+++ /dev/null")
                || patch
                    .lines()
                    .any(|line| line.starts_with("deleted file mode "));
            let path = patch
                .lines()
                .find_map(|line| line.strip_prefix("rename to "))
                .or_else(|| patch.lines().find_map(|line| line.strip_prefix("+++ b/")))
                .or_else(|| patch.lines().find_map(|line| line.strip_prefix("--- a/")))
                .or_else(|| {
                    patch
                        .lines()
                        .find_map(|line| line.strip_prefix("diff --git "))
                        .and_then(|header| header.rsplit_once(" b/").map(|(_, path)| path))
                })?;
            Some(FileChange {
                path: cwd.join(path).to_string_lossy().to_string(),
                kind: if old_null {
                    FileChangeKind::Create
                } else if new_null {
                    FileChangeKind::Delete
                } else if patch.lines().any(|line| line.starts_with("rename to ")) {
                    FileChangeKind::Rename
                } else {
                    FileChangeKind::Modify
                },
                diff: Some(patch),
            })
        })
        .collect()
}

/// Snapshot `cwd`'s working tree into `refs/tcode/checkpoints/<session>/<turn>`
/// without disturbing the index or worktree. Returns the snapshot commit sha.
pub fn create_checkpoint(cwd: &Path, session_id: &str, turn: usize) -> Result<String, String> {
    let has_head = run_git(cwd, &["rev-parse", "--verify", "--quiet", "HEAD"], None)
        .map(|sha| !sha.trim().is_empty())
        .unwrap_or(false);
    let tmp_index = std::env::temp_dir().join(format!(
        "tcode-ckpt-index-{session_id}-{}",
        uuid::Uuid::new_v4()
    ));
    // Seed the scratch index from HEAD (when present) then stage the whole
    // worktree over it, so `write-tree` captures the current tree exactly.
    let staged = (|| {
        if has_head {
            run_git(cwd, &["read-tree", "HEAD"], Some(&tmp_index))?;
        }
        run_git(cwd, &["add", "-A", "--", "."], Some(&tmp_index))?;
        run_git(cwd, &["write-tree"], Some(&tmp_index))
    })();
    let _ = std::fs::remove_file(&tmp_index);
    let tree = staged?.trim().to_string();
    if tree.is_empty() {
        return Err("git write-tree returned an empty tree oid".into());
    }

    let message = format!("tcode checkpoint {session_id}/{turn}");
    let commit = run_git(cwd, &["commit-tree", &tree, "-m", &message], None)?
        .trim()
        .to_string();
    if commit.is_empty() {
        return Err("git commit-tree returned an empty commit oid".into());
    }

    run_git(
        cwd,
        &["update-ref", &checkpoint_ref(session_id, turn), &commit],
        None,
    )?;
    Ok(commit)
}

/// Restore `cwd`'s worktree to a checkpoint commit (T3 semantics: `git restore
/// --source <commit> --worktree --staged`, then `git clean -fd` to drop files
/// created after the checkpoint, then `git reset` so restored content is shown
/// as unstaged changes).
pub fn restore_checkpoint(cwd: &Path, commit: &str) -> Result<(), String> {
    run_git(
        cwd,
        &[
            "restore",
            "--source",
            commit,
            "--worktree",
            "--staged",
            "--",
            ".",
        ],
        None,
    )?;
    run_git(cwd, &["clean", "-fd", "--", "."], None)?;
    // Reset the index back to HEAD when the repo has one, so the restored tree
    // reads as working-tree changes rather than staged content.
    if run_git(cwd, &["rev-parse", "--verify", "--quiet", "HEAD"], None)
        .map(|sha| !sha.trim().is_empty())
        .unwrap_or(false)
    {
        run_git(cwd, &["reset", "--quiet", "--", "."], None)?;
    }
    Ok(())
}

/// Delete every checkpoint ref for `session_id` whose turn is `>= from_turn`.
/// Best-effort: individual deletions are ignored on error.
pub fn delete_checkpoint_refs_from(cwd: &Path, session_id: &str, from_turn: usize) {
    let prefix = checkpoint_ref_prefix(session_id);
    let Ok(out) = run_git(cwd, &["for-each-ref", "--format=%(refname)", &prefix], None) else {
        return;
    };
    for refname in out.lines().map(str::trim).filter(|line| !line.is_empty()) {
        let keep = refname
            .strip_prefix(&prefix)
            .and_then(|turn| turn.parse::<usize>().ok())
            .is_some_and(|turn| turn < from_turn);
        if !keep {
            let _ = run_git(cwd, &["update-ref", "-d", refname], None);
        }
    }
}

/// Delete all checkpoint refs for a session (used when a session is permanently
/// deleted). Best-effort.
pub fn delete_all_checkpoint_refs(cwd: &Path, session_id: &str) {
    delete_checkpoint_refs_from(cwd, session_id, 0);
}

/// Delete the checkpoint refs for the given `turns` of a session (used to cap
/// how many checkpoints a session keeps). Best-effort: individual deletions
/// are ignored on error.
pub fn delete_checkpoint_turns(cwd: &Path, session_id: &str, turns: &[usize]) {
    for turn in turns {
        let _ = run_git(
            cwd,
            &["update-ref", "-d", &checkpoint_ref(session_id, *turn)],
            None,
        );
    }
}

/// Whether a checkpoint ref currently exists (test helper).
#[doc(hidden)]
pub fn checkpoint_ref_exists(cwd: &Path, session_id: &str, turn: usize) -> bool {
    run_git(
        cwd,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &checkpoint_ref(session_id, turn),
        ],
        None,
    )
    .map(|sha| !sha.trim().is_empty())
    .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    fn scratch_repo() -> PathBuf {
        let root = std::env::temp_dir().join(format!("tcode-ckpt-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        run_git(&root, &["init", "-q"], None).unwrap();
        // A deterministic identity so `commit` works in a bare CI environment.
        run_git(&root, &["config", "user.name", "tcode"], None).unwrap();
        run_git(&root, &["config", "user.email", "tcode@localhost"], None).unwrap();
        // Windows' git defaults to core.autocrlf=true, which would restore
        // "original\n" as "original\r\n". That is correct git behavior; pin it
        // off here so these fixtures assert checkpoint semantics, not line endings.
        run_git(&root, &["config", "core.autocrlf", "false"], None).unwrap();
        root
    }

    fn commit_all(root: &Path, message: &str) {
        run_git(root, &["add", "-A"], None).unwrap();
        run_git(root, &["commit", "-q", "-m", message], None).unwrap();
    }

    fn diff_stats(diff: &str) -> (u32, u32) {
        diff.lines().fold((0, 0), |(added, deleted), line| {
            if line.starts_with("+++") || line.starts_with("---") {
                (added, deleted)
            } else if line.starts_with('+') {
                (added + 1, deleted)
            } else if line.starts_with('-') {
                (added, deleted + 1)
            } else {
                (added, deleted)
            }
        })
    }

    #[test]
    fn net_turn_diff_uses_checkpoint_trees_and_restricts_paths() {
        let root = scratch_repo();
        fs::write(root.join("tracked.txt"), "alpha\nbeta\ngamma\n").unwrap();
        fs::write(root.join("outside.txt"), "outside base\n").unwrap();
        commit_all(&root, "seed");
        create_checkpoint(&root, "sess", 1).unwrap();

        // Multiple overlapping rewrites of the same lines. The intermediate
        // fragments would total 4 additions / 4 deletions, but only the final
        // tree relative to checkpoint 1 is the turn's true result.
        fs::write(root.join("tracked.txt"), "alpha\nintermediate\ngamma\n").unwrap();
        fs::write(root.join("tracked.txt"), "alpha\nsecond pass\ngamma\n").unwrap();
        fs::write(root.join("tracked.txt"), "alpha\nfinal\ngamma\n").unwrap();
        fs::write(root.join("created.txt"), "created in turn\n").unwrap();
        fs::write(root.join("outside.txt"), "outside changed\n").unwrap();
        create_checkpoint(&root, "sess", 2).unwrap();

        let paths = vec![
            root.join("tracked.txt").to_string_lossy().to_string(),
            root.join("created.txt").to_string_lossy().to_string(),
        ];
        let changes = net_turn_file_changes(&root, "sess", 1, Some(2), &paths).unwrap();
        assert_eq!(changes.len(), 2);
        assert!(
            changes
                .iter()
                .all(|change| !change.path.ends_with("outside.txt"))
        );
        let tracked = changes
            .iter()
            .find(|change| change.path.ends_with("tracked.txt"))
            .unwrap();
        let expected = run_git(
            &root,
            &[
                "diff",
                "--no-color",
                &checkpoint_ref("sess", 1),
                &checkpoint_ref("sess", 2),
                "--",
                "tracked.txt",
            ],
            None,
        )
        .unwrap();
        assert_eq!(
            diff_stats(tracked.diff.as_deref().unwrap()),
            diff_stats(&expected)
        );
        assert_ne!(diff_stats(tracked.diff.as_deref().unwrap()), (4, 4));
        assert_eq!(
            changes
                .iter()
                .find(|change| change.path.ends_with("created.txt"))
                .unwrap()
                .kind,
            FileChangeKind::Create
        );

        // With no next checkpoint, compare the base tree to the worktree.
        fs::write(root.join("tracked.txt"), "alpha\nworktree\ngamma\n").unwrap();
        let worktree = net_turn_file_changes(
            &root,
            "sess",
            2,
            None,
            &[root.join("tracked.txt").to_string_lossy().to_string()],
        )
        .unwrap();
        assert_eq!(worktree.len(), 1);
        assert_eq!(worktree[0].kind, FileChangeKind::Modify);
        assert_eq!(diff_stats(worktree[0].diff.as_deref().unwrap()), (1, 1));

        fs::write(root.join("worktree-created.txt"), "new after checkpoint\n").unwrap();
        let worktree_create = net_turn_file_changes(
            &root,
            "sess",
            2,
            None,
            &[root
                .join("worktree-created.txt")
                .to_string_lossy()
                .to_string()],
        )
        .unwrap();
        assert_eq!(worktree_create.len(), 1);
        assert_eq!(worktree_create[0].kind, FileChangeKind::Create);
        assert_eq!(
            diff_stats(worktree_create[0].diff.as_deref().unwrap()),
            (1, 0)
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn create_records_ref_and_restore_reverts_tracked_files() {
        let root = scratch_repo();
        fs::write(root.join("a.txt"), "original\n").unwrap();
        commit_all(&root, "seed");

        // Snapshot the clean tree as the turn-0 checkpoint.
        let commit = create_checkpoint(&root, "sess", 0).unwrap();
        assert!(!commit.is_empty());
        assert!(checkpoint_ref_exists(&root, "sess", 0));

        // The "turn" mutates a tracked file and adds a new untracked file.
        fs::write(root.join("a.txt"), "changed by agent\n").unwrap();
        fs::write(root.join("b.txt"), "new file\n").unwrap();

        // Reverting restores the tracked file and (T3 `git clean -fd` semantics)
        // removes files created after the checkpoint.
        restore_checkpoint(&root, &commit).unwrap();
        assert_eq!(
            fs::read_to_string(root.join("a.txt")).unwrap(),
            "original\n"
        );
        assert!(
            !root.join("b.txt").exists(),
            "restore should clean files created after the checkpoint (git clean -fd)"
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn restore_recreates_files_the_turn_deleted() {
        let root = scratch_repo();
        fs::write(root.join("keep.txt"), "hi\n").unwrap();
        commit_all(&root, "seed");
        let commit = create_checkpoint(&root, "sess", 0).unwrap();

        // The turn deletes a tracked file; restore brings it back.
        fs::remove_file(root.join("keep.txt")).unwrap();
        assert!(!root.join("keep.txt").exists());
        restore_checkpoint(&root, &commit).unwrap();
        assert_eq!(fs::read_to_string(root.join("keep.txt")).unwrap(), "hi\n");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn delete_refs_from_removes_only_newer_checkpoints() {
        let root = scratch_repo();
        fs::write(root.join("f.txt"), "0\n").unwrap();
        commit_all(&root, "seed");
        for turn in 0..3 {
            fs::write(root.join("f.txt"), format!("{turn}\n")).unwrap();
            create_checkpoint(&root, "sess", turn).unwrap();
        }
        assert!(checkpoint_ref_exists(&root, "sess", 0));
        assert!(checkpoint_ref_exists(&root, "sess", 1));
        assert!(checkpoint_ref_exists(&root, "sess", 2));

        // Reverting to turn 1 drops refs for turns >= 1, keeps turn 0.
        delete_checkpoint_refs_from(&root, "sess", 1);
        assert!(checkpoint_ref_exists(&root, "sess", 0));
        assert!(!checkpoint_ref_exists(&root, "sess", 1));
        assert!(!checkpoint_ref_exists(&root, "sess", 2));

        // Deleting all removes the remainder.
        delete_all_checkpoint_refs(&root, "sess");
        assert!(!checkpoint_ref_exists(&root, "sess", 0));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn checkpoint_works_in_a_fresh_repo_without_head() {
        // A repo with no commits yet (no HEAD) still snapshots as an orphan.
        let root = scratch_repo();
        fs::write(root.join("draft.txt"), "wip\n").unwrap();
        let commit = create_checkpoint(&root, "sess", 0).unwrap();
        assert!(!commit.is_empty());
        assert!(checkpoint_ref_exists(&root, "sess", 0));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn is_git_repo_detects_non_repo() {
        let plain = std::env::temp_dir().join(format!("tcode-plain-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&plain).unwrap();
        assert!(!is_git_repo(&plain));
        let repo = scratch_repo();
        assert!(is_git_repo(&repo));
        let _ = fs::remove_dir_all(&plain);
        let _ = fs::remove_dir_all(&repo);
    }
}
