use super::*;
#[test]
fn merge_or_close_detection_matches_words_in_the_raw_command() {
    for command in [
        "gh pr merge 12 --squash",
        "cd repo && gh pr close 3",
        "(gh  pr\tmerge)",
        "glab mr merge 4",
    ] {
        assert!(merges_or_closes(command), "{command}");
    }
    for command in [
        "gh pr view 12",
        "ugh pr merge",
        "gh pr merged",
        "gh prmerge",
        "echo gh pr",
    ] {
        assert!(!merges_or_closes(command), "{command}");
    }
}

/// The model reads this text once per registration: a configured host adds its name, and its
/// CLI where it has one, once for the hosts that share it.
#[test]
fn the_model_reads_the_hosts_settings_configure() {
    let github = hosts_in([HostKind::Github]);
    assert_eq!(host_names(&github), "GitHub");
    let all = hosts_in([HostKind::Gitea, HostKind::Forgejo]);
    assert_eq!(host_names(&all), "GitHub or Forgejo or Gitea");
    assert!(linking_instructions(&all).contains("This applies to gh, gh stack, tea, other CLIs"));
    assert!(merges_or_closes("tea pr merge 3"));
    assert!(merges_or_closes("glab mr close 3"));
    let gitlab = hosts_in([HostKind::Gitlab]);
    assert_eq!(host_names(&gitlab), "GitHub or GitLab");
    assert!(
        linking_instructions(&gitlab).contains("This applies to gh, gh stack, glab, other CLIs")
    );
    // Bitbucket has no CLI to name, so the list stays GitHub's.
    let bitbucket = hosts_in([HostKind::Bitbucket]);
    assert_eq!(host_names(&bitbucket), "GitHub or Bitbucket");
    assert!(linking_instructions(&bitbucket).contains("This applies to gh, gh stack, other CLIs"));
    // `az repos pr` has no merge or close verb, so no command of it is taken for one.
    let azure = hosts_in([HostKind::Azure]);
    assert_eq!(host_names(&azure), "GitHub or Azure DevOps");
    assert!(
        linking_instructions(&azure).contains("This applies to gh, gh stack, az repos, other CLIs")
    );
    assert!(!merges_or_closes(
        "az repos pr update --id 3 --status completed"
    ));
}

/// What Add host accepts, and what the host's validation of a settings write accepts: a
/// GitHub host is a host name alone; a Forgejo or Gitea server may carry a port and a mount.
#[test]
fn each_kind_names_its_hosts_by_its_own_rule() {
    assert_eq!(
        HostKind::Forgejo.authority(" HTTPS://Git.Acme.test:3000/forge/ "),
        Ok("git.acme.test:3000/forge".to_owned())
    );
    assert_eq!(
        HostKind::Github.authority("github.example.com:8443"),
        Err(HostRefusal::PortOrPath)
    );
    assert_eq!(HostKind::Gitea.authority("  "), Err(HostRefusal::Blank));
    assert_eq!(
        HostKind::Gitlab.authority("https://Code.Acme.test:8443/"),
        Ok("code.acme.test:8443".to_owned())
    );
    assert_eq!(
        HostKind::Gitlab.authority("code.acme.test/gitlab"),
        Err(HostRefusal::Path)
    );
    assert_eq!(HostKind::detect("gitlab.com"), Some(HostKind::Gitlab));
    assert_eq!(
        HostKind::detect("gitlab.acme.test:8443"),
        Some(HostKind::Gitlab)
    );
    for invalid in ["a b", "git.acme.test:0", "git.acme.test/../x", "-x.test"] {
        assert_eq!(
            HostKind::Gitea.authority(invalid),
            Err(HostRefusal::Invalid),
            "{invalid}"
        );
    }
    assert_eq!(HostKind::detect("codeberg.org"), Some(HostKind::Forgejo));
    assert_eq!(
        HostKind::detect("gitea.acme.test:3000/x"),
        Some(HostKind::Gitea)
    );
    assert_eq!(HostKind::detect("ghost.acme.test"), None);
    // Bitbucket Cloud is bitbucket.org alone; a `bitbucket` label is Data Center's.
    assert_eq!(
        HostKind::Bitbucket.authority("https://Bitbucket.org/"),
        Ok("bitbucket.org".to_owned())
    );
    for other in [
        "bitbucket.acme.test",
        "bitbucket.org:8443",
        "bitbucket.org/x",
    ] {
        assert_eq!(
            HostKind::Bitbucket.authority(other),
            Err(HostRefusal::Fixed),
            "{other}"
        );
    }
    assert_eq!(HostKind::detect("bitbucket.org"), Some(HostKind::Bitbucket));
    assert_eq!(HostKind::detect("bitbucket.acme.test"), None);
    // An Azure DevOps host is an organization, however it is written.
    for organization in [
        "Fabrikam",
        "https://dev.azure.com/Fabrikam/",
        "dev.azure.com/fabrikam/Project",
        "fabrikam.visualstudio.com",
    ] {
        assert_eq!(
            HostKind::Azure.authority(organization),
            Ok("dev.azure.com/fabrikam".to_owned()),
            "{organization}"
        );
    }
    assert_eq!(
        HostKind::Azure.authority("https://dev.azure.com/"),
        Err(HostRefusal::Blank)
    );
    for invalid in [
        "git.acme.test",
        "fab rikam",
        "-fabrikam",
        "dev.azure.com/a.b",
    ] {
        assert_eq!(
            HostKind::Azure.authority(invalid),
            Err(HostRefusal::Invalid),
            "{invalid}"
        );
    }
    assert_eq!(
        HostKind::detect("dev.azure.com/fabrikam"),
        Some(HostKind::Azure)
    );
    assert_eq!(AZURE.display_name("dev.azure.com/fabrikam"), "Azure DevOps");
    assert_eq!(
        FORGEJO.display_name("git.acme.test:3000/forge"),
        "git.acme.test"
    );
    assert_eq!(GITHUB.display_name("github.example.com"), "GitHub");
}

fn link(number: u64, state: Option<PullRequestState>, draft: bool) -> ThreadPullRequestLink {
    ThreadPullRequestLink {
        key: PullRequestKey::new("github.com", "sample/project", number),
        url: format!("https://github.com/sample/project/pull/{number}"),
        source: PullRequestSource::Manual,
        linked_at: Some(number),
        snapshot: state.map(|state| PullRequestSnapshot {
            state,
            title: "Change".into(),
            head_branch: format!("layer-{number}"),
            base_branch: "main".into(),
            is_draft: draft,
            updated_at: Some("2026-10-08T00:00:00Z".into()),
            synced_at: 1,
            closed_at: None,
            merged_at: None,
            author: None,
            additions: Some(0),
            deletions: Some(0),
            changed_files: Some(0),
            review_decision: None,
            checks_state: None,
            mergeability: Mergeability::Unknown,
        }),
        stack: PullRequestStackState::Unknown,
        sync_error: None,
        watch: None,
    }
}

/// v0.4.0 stored a stack with a `url`, the API address GitHub's REST stack carries.
#[test]
fn a_stack_stored_with_its_api_url_still_reads() {
    let stored = serde_json::json!({
        "key": {"host": "github.com", "repository": "sample/project", "number": 2},
        "source": "manual",
        "stack": {"status": "native", "stack": {
            "id": "4021", "number": 3,
            "url": "https://api.github.com/repos/sample/project/stacks/3",
            "base": "main",
            "layers": [{"url": "https://github.com/sample/project/pull/2", "number": 2,
                        "head_branch": "layer-2", "state": "open"}]
        }}
    });
    let link: ThreadPullRequestLink = serde_json::from_value(stored).unwrap();
    let PullRequestStackState::Native(stack) = link.stack else {
        panic!("a native stack");
    };
    assert_eq!((stack.number, stack.layers.len()), (3, 1));
}

#[test]
fn unlinking_removes_a_link_and_tombstones_only_a_stack_layer() {
    let stack = PullRequestStack {
        id: "stack".into(),
        number: 1,
        base: "main".into(),
        layers: [2, 3]
            .map(|number| PullRequestStackLayer {
                url: format!("https://github.com/sample/project/pull/{number}"),
                number,
                head_branch: format!("layer-{number}"),
                state: PullRequestState::Open,
            })
            .to_vec(),
    };
    let mut carrier = link(2, None, false);
    carrier.stack = PullRequestStackState::Native(stack);
    let mut layer = link(3, None, false);
    layer.source = PullRequestSource::Stack;
    let mut links = vec![link(1, None, false), carrier, layer];
    let key = |number| PullRequestKey::new("github.com", "sample/project", number);
    for number in [1, 2, 3] {
        assert!(unlink_pull_request(&mut links, &key(number)));
    }
    assert!(!unlink_pull_request(&mut links, &key(1)), "already gone");
    assert_eq!(
        links
            .iter()
            .map(|link| (link.key.number, link.source))
            .collect::<Vec<_>>(),
        [
            (2, PullRequestSource::Dismissed),
            (3, PullRequestSource::Dismissed)
        ],
        "the stack's layers stay dismissed so its sync does not relink them"
    );
    assert!(link_pull_request(
        &mut links,
        key(1),
        "https://github.com/sample/project/pull/1".into(),
        PullRequestSource::Manual,
        2,
        false,
    ));
    assert!(!link_pull_request(
        &mut links,
        key(3),
        "https://github.com/sample/project/pull/3".into(),
        PullRequestSource::Stack,
        2,
        false,
    ));
}

#[test]
fn badge_distinguishes_unknown_drafts_terminal_and_related_links() {
    use PullRequestBadgeState as Badge;
    use PullRequestState as State;
    let cases = [
        (vec![], None),
        (vec![link(1, None, false)], Some((Badge::Unknown, 1, false))),
        (
            vec![
                link(1, Some(State::Merged), false),
                link(2, Some(State::Open), true),
            ],
            Some((Badge::Open, 2, false)),
        ),
        (
            vec![
                link(1, Some(State::Open), true),
                link(2, Some(State::Open), true),
            ],
            Some((Badge::Draft, 2, false)),
        ),
        (
            vec![link(1, Some(State::Merged), false), link(2, None, false)],
            Some((Badge::Open, 2, false)),
        ),
        (
            vec![
                link(1, Some(State::Merged), false),
                link(2, Some(State::Merged), false),
            ],
            Some((Badge::Merged, 2, false)),
        ),
        (
            vec![
                link(1, Some(State::Merged), false),
                link(2, Some(State::Closed), false),
            ],
            Some((Badge::Closed, 2, false)),
        ),
    ];
    for (links, expected) in cases {
        assert_eq!(badge(&links), expected);
    }
    let mut links = vec![
        link(1, Some(State::Open), false),
        link(2, Some(State::Open), false),
        link(3, Some(State::Open), false),
    ];
    links[1].snapshot.as_mut().unwrap().base_branch = "layer-1".into();
    links[2].snapshot.as_mut().unwrap().base_branch = "layer-2".into();
    assert_eq!(badge(&links), Some((Badge::Open, 3, true)));
    assert_eq!(
        groups(&links)[0]
            .links
            .iter()
            .map(|link| link.key.number)
            .collect::<Vec<_>>(),
        vec![1, 2, 3]
    );
    links[1].source = PullRequestSource::Dismissed;
    assert_eq!(badge(&links), Some((Badge::Open, 2, false)));
    assert!(is_pull_request_url(
        "https://github.com/sample/project/pull/123/files"
    ));
    assert!(is_pull_request_url(
        "https://github.com/sample/project/pull/123?view=1#issuecomment-7"
    ));
    assert!(is_pull_request_url(
        "https://git.acme.test:3000/forge/sample/project/pulls/4/files"
    ));
    assert!(is_pull_request_url(
        "https://code.acme.test:8443/group/sub/project/-/merge_requests/4/diffs"
    ));
    assert!(is_pull_request_url(
        "https://bitbucket.org/sample/project/pull-requests/9/diff"
    ));
    for ordinary in [
        "https://github.com/sample/project/issues/123",
        "https://github.com/sample/project/pull/0",
        "https://gitlab.com/project/-/merge_requests/4",
        "https://bitbucket.acme.test/sample/project/pull-requests/9",
        "https://example.test",
        "#123",
    ] {
        assert!(!is_pull_request_url(ordinary));
    }
}

#[test]
fn a_layer_waits_only_for_the_write_moving_it_now() {
    let operation = |kind| PullRequestStackOperation {
        host: "github.com".into(),
        repository: "sample/project".into(),
        stack: 50,
        started_at: 1,
        kind,
    };
    let rebase = |step: StackRebaseStep| {
        vec![StackRebaseLayer {
            number: 2,
            branch: "layer-2".into(),
            step,
        }]
    };
    let unconfirmed = |checked| StackOperationKind::MergeUnconfirmed {
        id: "op".into(),
        target: 3,
        layers: vec![2, 3],
        checked,
    };
    let merging = operation(StackOperationKind::Merging {
        id: "op".into(),
        target: 3,
        layers: vec![2, 3],
        adopted: false,
    });
    assert!(
        merging.covers(3) && !merging.covers(4),
        "the layers above stay free"
    );
    assert!(operation(unconfirmed(false)).covers(2));
    assert!(
        !operation(unconfirmed(true)).covers(2),
        "a sync read the target still open"
    );
    assert!(
        operation(StackOperationKind::Rebasing {
            layers: rebase(StackRebaseStep::Rebasing)
        })
        .covers(2)
    );
    assert!(
        !operation(StackOperationKind::RebaseEnded {
            layers: rebase(StackRebaseStep::Failed {
                reason: StackRebaseFailure::Conflict
            })
        })
        .covers(2),
        "a stopped rebase is read, not waited for"
    );
}
