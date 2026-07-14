//! Git presentation with core-owned semantics and services-owned I/O.

pub use tcode_core::git::{
    GitAction, GitFileEntry, GitHint, GitStatus, MenuItem, QuickAction, build_commit_prompt,
    feature_branch_name, included_paths, menu_items, parse_status, quick_action,
    sanitize_branch_fragment, sanitize_commit_message,
};

pub fn git_action_label_key(action: GitAction) -> &'static str {
    match action {
        GitAction::Commit => "git.action.commit",
        GitAction::CommitPush => "git.action.commit_push",
        GitAction::Push => "git.action.push",
        GitAction::Pull => "git.action.pull",
        GitAction::PublishBranch => "git.action.publish",
        GitAction::InitializeGit => "git.action.init",
    }
}

pub fn git_hint_key(hint: GitHint) -> &'static str {
    match hint {
        GitHint::InProgress => "git.hint.in_progress",
        GitHint::Detached => "git.hint.detached",
        GitHint::NoCommits => "git.hint.no_commits",
        GitHint::NoRemote => "git.hint.no_remote",
        GitHint::Diverged => "git.hint.diverged",
        GitHint::UpToDate => "git.hint.up_to_date",
        GitHint::NoChanges => "git.hint.no_changes",
        GitHint::NoUpstream => "git.hint.no_upstream",
        GitHint::Behind => "git.hint.behind",
        GitHint::NoAhead => "git.hint.no_ahead",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_and_hint_keys_preserve_the_presentation_vocabulary() {
        assert_eq!(git_action_label_key(GitAction::Commit), "git.action.commit");
        assert_eq!(
            git_action_label_key(GitAction::CommitPush),
            "git.action.commit_push"
        );
        assert_eq!(git_action_label_key(GitAction::Push), "git.action.push");
        assert_eq!(git_action_label_key(GitAction::Pull), "git.action.pull");
        assert_eq!(
            git_action_label_key(GitAction::PublishBranch),
            "git.action.publish"
        );
        assert_eq!(
            git_action_label_key(GitAction::InitializeGit),
            "git.action.init"
        );
        assert_eq!(git_hint_key(GitHint::InProgress), "git.hint.in_progress");
        assert_eq!(git_hint_key(GitHint::Detached), "git.hint.detached");
        assert_eq!(git_hint_key(GitHint::NoCommits), "git.hint.no_commits");
        assert_eq!(git_hint_key(GitHint::NoRemote), "git.hint.no_remote");
        assert_eq!(git_hint_key(GitHint::Diverged), "git.hint.diverged");
        assert_eq!(git_hint_key(GitHint::UpToDate), "git.hint.up_to_date");
        assert_eq!(git_hint_key(GitHint::NoChanges), "git.hint.no_changes");
        assert_eq!(git_hint_key(GitHint::NoUpstream), "git.hint.no_upstream");
        assert_eq!(git_hint_key(GitHint::Behind), "git.hint.behind");
        assert_eq!(git_hint_key(GitHint::NoAhead), "git.hint.no_ahead");
    }
}
