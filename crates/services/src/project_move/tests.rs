use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use agent::ProviderKind;
use tcode_core::project::{OwnedWorktree, Project, SessionMeta, WorktreeInfo};

use super::*;

fn git_in(dir: &Path, args: &[&str]) -> String {
    let output = crate::process::command("git")
        .args(args)
        .current_dir(dir)
        // The test reads; a status refresh must not rewrite what it compares.
        .env("GIT_OPTIONAL_LOCKS", "0")
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
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// What `git status` says of a checkout, its branch line included.
fn status(dir: &Path) -> String {
    git_in(
        dir,
        &[
            "status",
            "--porcelain=v2",
            "--branch",
            "--untracked-files=all",
        ],
    )
}

/// Everything of a checkout the move must leave as it was: its status, the
/// bytes of its index and HEAD, and the stash.
fn untouched(dir: &Path) -> (String, Vec<u8>, Vec<u8>, String) {
    let path = |name: &str| {
        let path = PathBuf::from(git_in(dir, &["rev-parse", "--git-path", name]).trim());
        if path.is_absolute() {
            path
        } else {
            dir.join(path)
        }
    };
    (
        status(dir),
        std::fs::read(path("index")).unwrap(),
        std::fs::read(path("HEAD")).unwrap(),
        git_in(dir, &["stash", "list", "--format=%H %s"]),
    )
}

/// Staged, unstaged, untracked and deleted files in `dir`.
fn dirty(dir: &Path, tag: &str) {
    std::fs::write(dir.join("staged.txt"), format!("staged {tag}\n")).unwrap();
    git_in(dir, &["add", "staged.txt"]);
    std::fs::write(
        dir.join("staged.txt"),
        format!("staged then changed {tag}\n"),
    )
    .unwrap();
    std::fs::write(dir.join("tracked.txt"), format!("unstaged {tag}\n")).unwrap();
    std::fs::write(dir.join(format!("untracked-{tag}.txt")), "new\n").unwrap();
    std::fs::remove_file(dir.join("gone.txt")).unwrap();
    git_in(dir, &["rm", "--quiet", "--cached", "removed.txt"]);
}

/// A move's parts, built on one machine from a repository with a branch
/// worktree and a detached one, staged and committed on another: each
/// checkout there reads in `git status` as it did here, on the same branch
/// or commit, while every checkout here keeps its status, index, HEAD and
/// stash byte for byte and no snapshot ref is left behind.
#[test]
fn a_moved_repository_reads_as_it_did_and_the_source_is_left_as_it_was() {
    let temp = std::env::temp_dir().join(format!("tcode-move-git-{}", uuid::Uuid::new_v4()));
    let root = temp.join("source").join("app");
    std::fs::create_dir_all(&root).unwrap();
    git_in(
        &root,
        &["-c", "init.templateDir=", "init", "--quiet", "-b", "main"],
    );
    git_in(&root, &["config", "commit.gpgSign", "false"]);
    git_in(&root, &["config", "core.autocrlf", "false"]);
    git_in(
        &root,
        &["remote", "add", "origin", "https://example.invalid/app.git"],
    );
    for name in ["tracked.txt", "gone.txt", "removed.txt"] {
        std::fs::write(root.join(name), format!("{name}\n")).unwrap();
    }
    std::fs::write(root.join(".gitignore"), "secret.env\n").unwrap();
    std::fs::write(root.join(".worktreeinclude"), "secret.env\n").unwrap();
    git_in(&root, &["add", "."]);
    git_in(&root, &["commit", "--quiet", "-m", "initial"]);
    std::fs::write(root.join("tracked.txt"), "stashed\n").unwrap();
    git_in(&root, &["stash", "--quiet"]);
    std::fs::write(root.join("secret.env"), "TOKEN=1\n").unwrap();

    let source_data = temp.join("source-data");
    let branch = crate::worktree::provision(&root, "feature", None, &source_data).unwrap();
    let detached = source_data.join("worktrees").join("detached");
    git_in(
        &root,
        &[
            "worktree",
            "add",
            "--quiet",
            "--detach",
            detached.to_str().unwrap(),
            "HEAD",
        ],
    );
    for (dir, tag) in [
        (&root, "root"),
        (&branch.path, "branch"),
        (&detached, "detached"),
    ] {
        dirty(dir, tag);
    }

    let mut on_branch = SessionMeta::new(ProviderKind::Codex, branch.path.clone(), None);
    on_branch.worktree = Some(WorktreeInfo {
        root_project_path: root.clone(),
        base: "main".into(),
        branch: branch.branch.clone(),
    });
    let mut elsewhere = SessionMeta::new(ProviderKind::ClaudeCode, root.clone(), None);
    elsewhere.other_worktrees.push(OwnedWorktree {
        path: detached.clone(),
        info: WorktreeInfo {
            root_project_path: root.clone(),
            base: "main".into(),
            branch: String::new(),
        },
    });
    let threads = vec![on_branch.clone(), elsewhere.clone()];
    let checkouts = [&root, &branch.path, &detached];
    let before: Vec<_> = checkouts.iter().map(|dir| untouched(dir)).collect();

    let store = SessionStore::open_at(source_data.clone()).unwrap();
    let out = outgoing_dir(&source_data, "move-1");
    let parts = prepare(
        &store,
        &out,
        &Outgoing {
            move_id: "move-1",
            project_name: "app",
            root: &root,
            include_ignored: true,
            threads: &threads,
        },
    )
    .unwrap();
    assert!(parts.iter().any(|part| part.name == IGNORED_TAR));

    let target_data = temp.join("target-data");
    let target_store = SessionStore::open_at(target_data.clone()).unwrap();
    let staged = Staged {
        project_name: "app".into(),
        root: String::new(),
        git: true,
        thread_ids: threads.iter().map(|meta| meta.id.clone()).collect(),
        parts: parts.clone(),
    };
    for (name, held) in stage_begin(&target_data, "move-1", &staged).unwrap() {
        let bytes = std::fs::read(out.join(&name)).unwrap();
        for chunk in bytes[held as usize..].chunks(CHUNK_BYTES) {
            let at = std::fs::metadata(staging_dir(&target_data, "move-1").join(&name))
                .unwrap()
                .len();
            stage_chunk(&target_data, "move-1", &name, at, chunk).unwrap();
        }
    }
    let target_root = temp.join("target").join("app");
    let project = Project {
        id: "arrived".into(),
        name: "app".into(),
        root: target_root.clone(),
        quick_chat: false,
        icon_path: None,
        permission_defaults: BTreeMap::new(),
        created_at: 0,
    };
    let arrived = arrive(
        &target_store,
        &Arrival {
            move_id: "move-1",
            root: &target_root,
            project,
            from_machine: "source",
            from_machine_name: "Source",
            to_machine: "target",
            profile: &|_| None,
            now_ms: 0,
        },
    )
    .unwrap();

    let branch_there = arrived
        .iter()
        .find(|meta| meta.id == on_branch.id)
        .unwrap()
        .cwd
        .clone();
    let detached_there = arrived
        .iter()
        .find(|meta| meta.id == elsewhere.id)
        .unwrap()
        .other_worktrees[0]
        .path
        .clone();
    assert_eq!(
        branch_there,
        crate::worktree::worktree_path(&target_root, "feature", &target_data).unwrap()
    );
    for (here, there) in [
        (&root, &target_root),
        (&branch.path, &branch_there),
        (&detached, &detached_there),
    ] {
        assert_eq!(status(there), status(here), "{}", here.display());
    }
    assert_eq!(
        std::fs::read_to_string(target_root.join("secret.env")).unwrap(),
        "TOKEN=1\n"
    );
    assert_eq!(
        git_in(&target_root, &["config", "remote.origin.url"]).trim(),
        "https://example.invalid/app.git"
    );
    assert!(git_in(&target_root, &["for-each-ref", "refs/tcode"]).is_empty());

    end_outgoing(&source_data, "move-1", &root).unwrap();
    let after: Vec<_> = checkouts.iter().map(|dir| untouched(dir)).collect();
    assert_eq!(after, before);
    assert!(git_in(&root, &["for-each-ref", "refs/tcode"]).is_empty());
    assert!(!out.exists());

    drop((store, target_store));
    let _ = std::fs::remove_dir_all(&temp);
}
