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
