//! A checkout's repository and branch on a host, from its git remotes, for hosts whose remote
//! URLs each kind reads in its own way.

use super::Repository;
use std::path::Path;

fn git(cwd: &Path, args: &[&str]) -> Option<String> {
    crate::github::repository::git_read(cwd, args)
}

/// The fetch remotes of the checkout that `remote` reads as a repository, by remote name.
fn remotes(cwd: &Path, remote: impl Fn(&str) -> Option<Repository>) -> Vec<(String, Repository)> {
    git(cwd, &["remote", "-v"])
        .unwrap_or_default()
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let name = parts.next()?;
            let url = parts.next()?;
            (parts.next()? == "(fetch)").then_some(())?;
            Some((name.to_owned(), remote(url)?))
        })
        .collect()
}

/// The checkout's repository: `upstream`, then `origin`, then the first remote `remote` reads.
pub(crate) fn resolve(
    cwd: &Path,
    remote: impl Fn(&str) -> Option<Repository>,
) -> Option<Repository> {
    let remotes = remotes(cwd, remote);
    ["upstream", "origin"]
        .iter()
        .find_map(|name| remotes.iter().find(|(remote, _)| remote == name))
        .or_else(|| remotes.first())
        .map(|(_, repository)| repository.clone())
}

/// The checked-out branch and where it is pushed: the repository and branch a pull request from
/// it names as its source. A branch that tracks nothing has no pull request yet.
pub(crate) struct Branch {
    pub(crate) branch: String,
    pub(crate) source: Repository,
    pub(crate) source_branch: String,
}

pub(crate) fn branch(cwd: &Path, remote: impl Fn(&str) -> Option<Repository>) -> Option<Branch> {
    let branch = git(cwd, &["symbolic-ref", "--short", "HEAD"])?;
    let pushed_to = git(cwd, &["config", &format!("branch.{branch}.remote")])?;
    let merge = git(cwd, &["config", &format!("branch.{branch}.merge")])?;
    let source = remotes(cwd, remote)
        .into_iter()
        .find(|(name, _)| *name == pushed_to)?
        .1;
    Some(Branch {
        source,
        source_branch: merge
            .strip_prefix("refs/heads/")
            .unwrap_or(&merge)
            .to_owned(),
        branch,
    })
}
