use std::path::{Path, PathBuf};
use tcode_core::pull_request::PullRequestKey;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Repository {
    pub host: String,
    pub owner: String,
    pub name: String,
}

impl Repository {
    pub fn key(&self, number: u64) -> PullRequestKey {
        PullRequestKey::new(&self.host, &format!("{}/{}", self.owner, self.name), number)
    }
    pub fn from_key(key: &PullRequestKey) -> Option<Self> {
        let repository = selector(&key.repository, &key.host)?;
        (repository.host == key.host).then_some(repository)
    }
    pub fn url(&self, number: u64) -> String {
        format!(
            "https://{}/{}/{}/pull/{number}",
            self.host, self.owner, self.name
        )
    }
}

fn component(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
}

pub fn selector(value: &str, default_host: &str) -> Option<Repository> {
    let value = value.trim().trim_end_matches('/');
    let value = value
        .strip_prefix("https://")
        .or_else(|| value.strip_prefix("http://"))
        .unwrap_or(value);
    let value = value.strip_suffix(".git").unwrap_or(value);
    let parts: Vec<_> = value.split('/').collect();
    let (host, owner, name) = match parts.as_slice() {
        [owner, name] => (default_host, *owner, *name),
        [host, owner, name] => (*host, *owner, *name),
        _ => return None,
    };
    Some(Repository {
        host: super::normalize_host(host).ok()?,
        owner: component(owner).then(|| owner.to_ascii_lowercase())?,
        name: component(name).then(|| name.to_ascii_lowercase())?,
    })
}

pub fn pull_request_url(value: &str) -> Option<(PullRequestKey, String)> {
    let value = value.trim();
    let rest = value
        .strip_prefix("https://")
        .or_else(|| value.strip_prefix("http://"))?;
    let parts: Vec<_> = rest.split('/').collect();
    let [host, owner, name, "pull", number, ..] = parts.as_slice() else {
        return None;
    };
    let number: u64 = number.parse().ok()?;
    if number == 0 {
        return None;
    }
    let repository = selector(&format!("{host}/{owner}/{name}"), host)?;
    Some((repository.key(number), repository.url(number)))
}

fn remote(value: &str) -> Option<(Repository, String)> {
    let ssh = value.starts_with("ssh://") || value.starts_with("git@");
    let value = value
        .strip_prefix("ssh://")
        .or_else(|| value.strip_prefix("https://"))
        .or_else(|| value.strip_prefix("http://"))
        .unwrap_or(value);
    let value = value.rsplit_once('@').map_or(value, |(_, rest)| rest);
    let value = if value
        .split_once('/')
        .is_some_and(|(host, _)| host.ends_with(":443") || host.ends_with(":22"))
    {
        value.replacen(":443", "", 1).replacen(":22", "", 1)
    } else {
        value.replacen(':', "/", 1)
    };
    let (host, path) = value.split_once('/')?;
    let literal_host = host.to_ascii_lowercase();
    let host = if ssh && !host.contains('.') && host.to_ascii_lowercase().contains("github") {
        "github.com"
    } else {
        host
    };
    if !host
        .to_ascii_lowercase()
        .split('.')
        .any(|label| label == "github")
    {
        return None;
    }
    Some((selector(&format!("{host}/{path}"), host)?, literal_host))
}

pub fn git_read(cwd: &Path, args: &[&str]) -> Option<String> {
    let output = crate::process::command("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn fetch_remotes(cwd: &Path) -> Vec<(String, String)> {
    git_read(cwd, &["remote", "-v"])
        .unwrap_or_default()
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let name = parts.next()?;
            let url = parts.next()?;
            if parts.next()? != "(fetch)" {
                return None;
            }
            Some((name.to_owned(), url.to_owned()))
        })
        .collect()
}

fn remotes(cwd: &Path) -> Vec<(String, Repository)> {
    fetch_remotes(cwd)
        .into_iter()
        .filter_map(|(name, url)| Some((name, remote(&url)?.0)))
        .collect()
}

pub fn resolve(cwd: &Path) -> Option<Repository> {
    let default_host = std::env::var("GH_HOST").unwrap_or_else(|_| "github.com".into());
    if let Ok(value) = std::env::var("GH_REPO")
        && let Some(repository) = selector(&value, &default_host)
    {
        return Some(repository);
    }
    let fetch = fetch_remotes(cwd);
    let remotes: Vec<_> = fetch
        .iter()
        .filter_map(|(name, url)| Some((name.clone(), remote(url)?.0)))
        .collect();
    let marks = git_read(
        cwd,
        &["config", "--get-regexp", "^remote\\..*\\.gh-resolved$"],
    )
    .unwrap_or_default();
    let marked: Vec<_> = marks
        .lines()
        .filter_map(|line| {
            let (key, value) = line.split_once(char::is_whitespace)?;
            let name = key.strip_prefix("remote.")?.strip_suffix(".gh-resolved")?;
            let (_, repository) = remotes.iter().find(|(n, _)| n == name)?;
            if value.trim() == "base" {
                Some(repository.clone())
            } else {
                selector(value, &repository.host)
            }
        })
        .collect();
    let host = remotes
        .iter()
        .find(|(name, _)| name == "origin")
        .or_else(|| remotes.first())
        .map(|(_, repository)| repository.host.as_str())
        .unwrap_or(&default_host);
    if marked.len() == 1
        && fetch
            .iter()
            .all(|(_, url)| remote(url).is_some_and(|(_, literal_host)| literal_host == host))
    {
        return marked.into_iter().next();
    }
    remotes
        .iter()
        .filter(|(_, repository)| repository.host == host)
        .min_by_key(|(name, _)| {
            ["upstream", "github", "origin"]
                .iter()
                .position(|rank| name.eq_ignore_ascii_case(rank))
                .unwrap_or(99)
        })
        .map(|(_, repository)| repository.clone())
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BranchHead {
    pub cwd: PathBuf,
    pub branch: String,
    pub repository: Repository,
    pub head_owner: String,
    pub head_branch: String,
    pub local_identity: String,
    pub default_branch: bool,
}

/// A local answer for unpublished branches avoids a host request entirely.
pub fn branch_head(cwd: &Path) -> Option<BranchHead> {
    let cwd = cwd.canonicalize().ok()?;
    let branch = git_read(&cwd, &["symbolic-ref", "--short", "HEAD"])?;
    let repository = resolve(&cwd)?;
    let refs = git_read(
        &cwd,
        &[
            "for-each-ref",
            "--format=%(refname)%00%(upstream:remotename)%00%(upstream:remoteref)",
            &format!("refs/heads/{branch}"),
        ],
    )?;
    let line = refs
        .lines()
        .find(|line| line.split('\0').next() == Some(format!("refs/heads/{branch}").as_str()))?;
    let parts: Vec<_> = line.split('\0').collect();
    let remotes = remotes(&cwd);
    let saved = parts.get(1).copied().unwrap_or_default();
    let saved_ref = parts.get(2).copied().unwrap_or_default();
    if !saved.is_empty() && saved_ref.is_empty() {
        return None;
    }
    let pushed = git_read(
        &cwd,
        &["for-each-ref", "--format=%(refname)", "refs/remotes"],
    )?;
    let saved_repository = remotes
        .iter()
        .find(|(name, _)| name == saved)
        .map(|(_, repo)| repo);
    let default_head = remotes
        .iter()
        .find(|(_, repo)| repo == &repository)
        .and_then(|(name, _)| {
            git_read(
                &cwd,
                &[
                    "symbolic-ref",
                    "--short",
                    &format!("refs/remotes/{name}/HEAD"),
                ],
            )
        })
        .and_then(|head| {
            head.split_once('/')
                .map(|(_, branch)| format!("refs/heads/{branch}"))
        });
    let tracks_base = default_head.as_ref().map_or(
        saved_ref == "refs/heads/main" || saved_ref == "refs/heads/master",
        |head| head == saved_ref,
    ) && saved_ref != format!("refs/heads/{branch}")
        && saved_repository.is_some_and(|repo| repo == &repository);
    let remote_name = if !saved.is_empty() && !tracks_base {
        saved.to_string()
    } else {
        remotes
            .iter()
            .filter(|(name, _)| {
                pushed
                    .lines()
                    .any(|r| r == format!("refs/remotes/{name}/{branch}"))
            })
            .min_by_key(|(name, _)| {
                if name == saved {
                    0
                } else if name == "origin" {
                    1
                } else {
                    2
                }
            })
            .or_else(|| {
                if pushed.lines().any(|line| !line.ends_with("/HEAD")) {
                    return None;
                }
                remotes
                    .iter()
                    .find(|(name, _)| name == "origin")
                    .or_else(|| remotes.first())
            })?
            .0
            .clone()
    };
    let head_repository = remotes
        .iter()
        .find(|(name, _)| name == &remote_name)?
        .1
        .clone();
    if head_repository.host != repository.host {
        return None;
    }
    let head_branch = if tracks_base || saved.is_empty() {
        branch.clone()
    } else {
        saved_ref
            .strip_prefix("refs/heads/")
            .unwrap_or(&branch)
            .to_string()
    };
    let local_identity = format!(
        "{}\0{}\0{}\0{}",
        refs,
        pushed,
        git_read(&cwd, &["remote", "-v"])?,
        git_read(
            &cwd,
            &["config", "--get-regexp", "^remote\\..*\\.gh-resolved$"]
        )
        .unwrap_or_default()
    );
    let default_branch = git_read(
        &cwd,
        &[
            "symbolic-ref",
            "--short",
            &format!("refs/remotes/{remote_name}/HEAD"),
        ],
    )
    .map(|head| head == format!("{remote_name}/{branch}"))
    .unwrap_or(branch == "main" || branch == "master");
    Some(BranchHead {
        cwd,
        branch,
        repository,
        head_owner: head_repository.owner,
        head_branch,
        local_identity,
        default_branch,
    })
}
