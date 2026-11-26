//! A repository's checkouts as a move carries them: each checkout's
//! uncommitted state kept as two commits under `refs/tcode/move/<move>/…`
//! without touching its index, HEAD, branch or stash; everything in one
//! bundle; and on the other machine a repository whose `git status` reads as
//! the source's did.
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Where a move keeps its snapshot refs in the repository it leaves.
pub const REFS_ROOT: &str = "refs/tcode/move";

/// What a checkout had checked out.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "content", rename_all = "snake_case")]
pub enum Head {
    /// On a branch (its short name).
    Branch(String),
    /// On a commit, kept under the snapshot ref `head`.
    Detached { head_ref: String },
    /// On a branch with no commit yet.
    Unborn(String),
}

/// One checkout's state as its two snapshot commits hold it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckoutSnapshot {
    pub head: Head,
    /// A commit on HEAD whose tree is the checkout's index.
    pub index_ref: String,
    /// A commit on HEAD whose tree is its working tree, untracked files Git
    /// does not ignore included.
    pub worktree_ref: String,
}

/// One `git` in `cwd`, as a move runs it: no optional locks, so no status
/// refresh rewrites an index it only reads, and an identity of its own for the
/// commits it makes, so a machine without one configured can still make them.
fn git(cwd: &Path) -> std::process::Command {
    let mut command = crate::process::command("git");
    command
        .current_dir(cwd)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_AUTHOR_NAME", "Tcode")
        .env("GIT_AUTHOR_EMAIL", "tcode@localhost")
        .env("GIT_COMMITTER_NAME", "Tcode")
        .env("GIT_COMMITTER_EMAIL", "tcode@localhost")
        // A fixed date makes the snapshot commits the same whenever they are
        // made from the same state.
        .env("GIT_AUTHOR_DATE", "@0 +0000")
        .env("GIT_COMMITTER_DATE", "@0 +0000");
    command
}

fn run(mut command: std::process::Command, what: &str) -> Result<String, String> {
    let output = command
        .output()
        .map_err(|error| format!("git {what}: {error}"))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        Err(format!(
            "git {what} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

fn run_args<S: AsRef<OsStr>>(cwd: &Path, args: &[S]) -> Result<String, String> {
    let what = args
        .iter()
        .map(|arg| arg.as_ref().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(" ");
    let mut command = git(cwd);
    command.args(args);
    run(command, &what)
}

/// The value `rev-parse` gives for `args`, or `None` where it names nothing.
fn verify(cwd: &Path, name: &str) -> Result<Option<String>, String> {
    let output = git(cwd)
        .args(["rev-parse", "--quiet", "--verify", name])
        .output()
        .map_err(|error| format!("git rev-parse: {error}"))?;
    Ok(output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned()))
}

/// A path Git reports for `dir`, made absolute.
fn git_path(dir: &Path, name: &str) -> Result<PathBuf, String> {
    let path = run_args(dir, &["rev-parse", "--git-path", name])?;
    let path = PathBuf::from(path.trim());
    Ok(if path.is_absolute() {
        path
    } else {
        dir.join(path)
    })
}

/// Whether `dir` is the top level of a Git checkout: the main one or a linked
/// worktree.
pub fn is_checkout_root(dir: &Path) -> bool {
    let Ok(top) = run_args(dir, &["rev-parse", "--show-toplevel"]) else {
        return false;
    };
    match (Path::new(top.trim()).canonicalize(), dir.canonicalize()) {
        (Ok(top), Ok(dir)) => top == dir,
        _ => false,
    }
}

/// Whether `dir` is in the middle of a merge, rebase, cherry-pick or revert,
/// or has unmerged index entries: a state its snapshot could not hold.
pub fn interrupted(dir: &Path) -> Result<bool, String> {
    for head in ["MERGE_HEAD", "CHERRY_PICK_HEAD", "REVERT_HEAD"] {
        if git_path(dir, head)?.exists() {
            return Ok(true);
        }
    }
    for state in ["rebase-merge", "rebase-apply"] {
        if git_path(dir, state)?.exists() {
            return Ok(true);
        }
    }
    Ok(!run_args(dir, &["ls-files", "--unmerged"])?
        .trim()
        .is_empty())
}

/// Whether `dir`'s index or working tree differs from its HEAD.
pub fn dirty(dir: &Path) -> Result<bool, String> {
    Ok(!run_args(dir, &["status", "--porcelain"])?.trim().is_empty())
}

/// How many local branches the repository of `dir` has.
pub fn branch_count(dir: &Path) -> Result<u32, String> {
    Ok(
        run_args(dir, &["for-each-ref", "--format=x", "refs/heads"])?
            .lines()
            .count() as u32,
    )
}

/// The bytes the repository of `dir` keeps its objects in.
pub fn object_bytes(dir: &Path) -> Result<u64, String> {
    let counts = run_args(dir, &["count-objects", "-v"])?;
    Ok(counts
        .lines()
        .filter_map(|line| {
            let (key, value) = line.split_once(": ")?;
            matches!(key, "size" | "size-pack")
                .then(|| value.trim().parse::<u64>().ok())
                .flatten()
        })
        .sum::<u64>()
        * 1024)
}

/// Keep the state of the checkout `dir` under `prefix` (`refs/tcode/move/<move>/<label>`):
/// a commit of its index and one of its working tree, both on HEAD, made from
/// copies of its index in `scratch` so the checkout's own is never written.
pub fn snapshot(dir: &Path, prefix: &str, scratch: &Path) -> Result<CheckoutSnapshot, String> {
    let head_commit = verify(dir, "HEAD^{commit}")?;
    let branch = git(dir)
        .args(["symbolic-ref", "--quiet", "HEAD"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .and_then(|name| name.strip_prefix("refs/heads/").map(str::to_owned));
    let head = match (&branch, &head_commit) {
        (Some(branch), Some(_)) => Head::Branch(branch.clone()),
        (Some(branch), None) => Head::Unborn(branch.clone()),
        (None, Some(commit)) => {
            let head_ref = format!("{prefix}/head");
            run_args(dir, &["update-ref", head_ref.as_str(), commit.as_str()])?;
            Head::Detached { head_ref }
        }
        (None, None) => return Err(format!("{} has no HEAD", dir.display())),
    };
    let index = git_path(dir, "index")?;
    std::fs::create_dir_all(scratch).map_err(|error| error.to_string())?;
    let copy = |name: &str| -> Result<PathBuf, String> {
        let path = scratch.join(name);
        match std::fs::copy(&index, &path) {
            Ok(_) => Ok(path),
            // A checkout that never had anything staged has no index yet.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(path),
            Err(error) => Err(format!("cannot copy {}: {error}", index.display())),
        }
    };
    let tree = |index: &Path| -> Result<String, String> {
        let mut command = git(dir);
        command.env("GIT_INDEX_FILE", index).arg("write-tree");
        Ok(run(command, "write-tree")?.trim().to_owned())
    };
    let commit = |tree: &str, message: &str| -> Result<String, String> {
        let mut command = git(dir);
        command.args(["commit-tree", "--no-gpg-sign", tree, "-m", message]);
        if let Some(parent) = &head_commit {
            command.args(["-p", parent]);
        }
        Ok(run(command, "commit-tree")?.trim().to_owned())
    };
    let staged = copy("index-staged")?;
    let index_commit = commit(&tree(&staged)?, "Tcode move: index")?;
    let working = copy("index-working")?;
    let mut add = git(dir);
    add.env("GIT_INDEX_FILE", &working).args(["add", "-A"]);
    run(add, "add -A")?;
    let worktree_commit = commit(&tree(&working)?, "Tcode move: working tree")?;
    for path in [&staged, &working] {
        let _ = std::fs::remove_file(path);
    }
    let index_ref = format!("{prefix}/index");
    let worktree_ref = format!("{prefix}/worktree");
    run_args(
        dir,
        &["update-ref", index_ref.as_str(), index_commit.as_str()],
    )?;
    run_args(
        dir,
        &[
            "update-ref",
            worktree_ref.as_str(),
            worktree_commit.as_str(),
        ],
    )?;
    Ok(CheckoutSnapshot {
        head,
        index_ref,
        worktree_ref,
    })
}

/// Every ref of the repository of `root` in one bundle at `out`.
pub fn bundle(root: &Path, out: &Path) -> Result<(), String> {
    let mut command = git(root);
    command
        .args(["bundle", "create", "--quiet"])
        .arg(out)
        .arg("--all");
    run(command, "bundle create").map(drop)
}

/// The repository settings a move carries: where its remotes are and which
/// remote branch each local branch follows. The rest of its configuration,
/// hooks included, stays.
pub fn carried_config(root: &Path) -> Result<Vec<(String, String)>, String> {
    let output = git(root)
        .args([
            "config",
            "--local",
            "--null",
            "--get-regexp",
            r"^(remote\..*\.(url|pushurl|fetch)|branch\..*\.(remote|merge))$",
        ])
        .output()
        .map_err(|error| format!("git config: {error}"))?;
    match output.status.code() {
        Some(0) => {}
        // Nothing matched.
        Some(1) => return Ok(Vec::new()),
        _ => {
            return Err(format!(
                "git config failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .split('\0')
        .filter_map(|entry| {
            let (key, value) = entry.split_once('\n')?;
            Some((key.to_owned(), value.to_owned()))
        })
        .collect())
}

/// Delete every ref under `prefix` in the repository of `root`; a repository
/// that is gone has none.
pub fn remove_refs(root: &Path, prefix: &str) -> Result<(), String> {
    if !root.is_dir() {
        return Ok(());
    }
    let listed = run_args(
        root,
        &["for-each-ref", "--format=%(refname)", &format!("{prefix}/")],
    )?;
    for name in listed.lines().filter(|name| !name.is_empty()) {
        run_args(root, &["update-ref", "-d", name])?;
    }
    Ok(())
}

/// Checkout filters stay off while a snapshot is put back: what the source's
/// clean filters made is what it held, and no filter may fetch anything.
const NO_FILTERS: [&str; 6] = [
    "-c",
    "filter.lfs.smudge=",
    "-c",
    "filter.lfs.process=",
    "-c",
    "filter.lfs.required=false",
];

/// Put a checkout's snapshot back in `dir`: its working tree, then its index.
fn put_back(dir: &Path, snapshot: &CheckoutSnapshot) -> Result<(), String> {
    let mut args: Vec<&str> = NO_FILTERS.to_vec();
    args.extend(["read-tree", "--reset", "-u", snapshot.worktree_ref.as_str()]);
    run_args(dir, &args)?;
    run_args(dir, &["read-tree", snapshot.index_ref.as_str()]).map(drop)
}

/// Point a checkout at what `head` names.
fn check_out_head(dir: &Path, head: &Head) -> Result<(), String> {
    match head {
        Head::Branch(branch) | Head::Unborn(branch) => run_args(
            dir,
            &["symbolic-ref", "HEAD", &format!("refs/heads/{branch}")],
        )
        .map(drop),
        Head::Detached { head_ref } => {
            let commit = verify(dir, &format!("{head_ref}^{{commit}}"))?
                .ok_or_else(|| format!("the bundle has no {head_ref}"))?;
            run_args(dir, &["update-ref", "--no-deref", "HEAD", commit.as_str()]).map(drop)
        }
    }
}

/// Make `root`, an empty directory, the repository the bundle at `bundle`
/// holds, with the root checkout as `snapshot` left it and `config` set.
/// Objects are checked as they arrive and no submodule is followed.
pub fn restore_root(
    bundle: &Path,
    root: &Path,
    snapshot: &CheckoutSnapshot,
    config: &[(String, String)],
) -> Result<(), String> {
    run_args(root, &["init", "--quiet"])?;
    for (key, value) in config {
        run_args(root, &["config", "--add", key.as_str(), value.as_str()])?;
    }
    let mut fetch = git(root);
    fetch
        .args([
            "-c",
            "fetch.fsckObjects=true",
            "-c",
            "transfer.fsckObjects=true",
            "-c",
            "fetch.recurseSubmodules=false",
            "fetch",
            "--quiet",
            "--no-recurse-submodules",
            "--no-write-fetch-head",
            // The new repository's unborn HEAD names a branch the bundle may
            // carry.
            "--update-head-ok",
        ])
        .arg(bundle)
        .arg("refs/*:refs/*");
    run(fetch, "fetch")?;
    check_out_head(root, &snapshot.head)?;
    put_back(root, snapshot)
}

/// Add the linked worktree `path` to the repository of `root`, on what
/// `snapshot` had checked out, with its state put back.
pub fn restore_worktree(
    root: &Path,
    path: &Path,
    snapshot: &CheckoutSnapshot,
) -> Result<(), String> {
    let path_arg = path.to_string_lossy().into_owned();
    match &snapshot.head {
        Head::Branch(branch) => run_args(
            root,
            &[
                "worktree",
                "add",
                "--quiet",
                "--no-checkout",
                path_arg.as_str(),
                branch.as_str(),
            ],
        )?,
        Head::Detached { head_ref } => run_args(
            root,
            &[
                "worktree",
                "add",
                "--quiet",
                "--no-checkout",
                "--detach",
                path_arg.as_str(),
                head_ref.as_str(),
            ],
        )?,
        Head::Unborn(branch) => {
            return Err(format!(
                "a worktree on {branch}, which has no commit, cannot be added"
            ));
        }
    };
    put_back(path, snapshot)
}
