use serde_json::{Value, json};
use std::{
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};
use tcode_core::{pull_request::PullRequestKey, session::ReviewSide};
use tcode_protocol::{
    PullRequestFileText, PullRequestMedia, PullRequestPatch, PullRequestReviewAnchor,
    PullRequestViewedState,
};
use tcode_services::github::{GitHubApi, GitHubError, pull_request_reads::PullRequestReads};

#[path = "support/github.rs"]
#[allow(dead_code)] // tests/github.rs drives the fixture by hand as well.
mod fixture;
use fixture::{Exchange, Fixture, Server, Store};

const BASE: &str = "1111111111111111111111111111111111111111";
const HEAD: &str = "2222222222222222222222222222222222222222";

/// One request as GitHub would see it.
#[derive(Debug, Clone)]
struct Seen {
    line: String,
    host: String,
    accept: String,
    authorization: Option<String>,
    body: String,
}
impl Seen {
    fn from(exchange: &Exchange) -> Self {
        let header = |name: &str| {
            exchange.request.lines().find_map(|line| {
                let (key, value) = line.split_once(':')?;
                key.eq_ignore_ascii_case(name)
                    .then(|| value.trim().to_owned())
            })
        };
        Self {
            line: exchange.request.lines().next().unwrap().to_owned(),
            host: header("host").unwrap_or_default(),
            accept: header("accept").unwrap_or_default(),
            authorization: header("authorization"),
            body: String::from_utf8_lossy(&exchange.body).into_owned(),
        }
    }
    fn operation(&self) -> Option<String> {
        let query: Value = serde_json::from_str(&self.body).ok()?;
        let text = query["query"].as_str()?;
        Some(
            text.split_whitespace()
                .nth(1)?
                .split('(')
                .next()?
                .to_owned(),
        )
    }
    fn variables(&self) -> Value {
        serde_json::from_str::<Value>(&self.body).unwrap()["variables"].clone()
    }
}

type Log = Arc<Mutex<Vec<Seen>>>;

/// Answers each request with `answer(seen)` and keeps what it saw.
fn serve(
    fixture: Fixture,
    answer: impl Fn(&Seen) -> (u16, String, Vec<u8>) + Send + 'static,
) -> (Server, Log) {
    let log: Log = Arc::default();
    let seen = log.clone();
    let server = fixture.serve(move |exchange| {
        let request = Seen::from(&exchange);
        let (status, headers, body) = answer(&request);
        seen.lock().unwrap().push(request);
        exchange.reply(status, &headers, &body);
    });
    (server, log)
}

fn json_reply(value: Value) -> (u16, String, Vec<u8>) {
    (200, String::new(), value.to_string().into_bytes())
}

fn key() -> PullRequestKey {
    PullRequestKey::new("github.com", "octo/repo", 7)
}

fn reads(fixture: &Fixture, store: &Store) -> Arc<PullRequestReads> {
    PullRequestReads::new(GitHubApi::new(
        store.credentials(&[("GH_TOKEN", "fixture-token")]),
        fixture.builder(),
    ))
}

fn revisions(changed_files: u64) -> (u16, String, Vec<u8>) {
    json_reply(json!({
        "number": 7,
        "base": {"sha": BASE},
        "head": {"sha": HEAD},
        "changed_files": changed_files,
        "merged_at": null,
    }))
}

const WHOLE_DIFF: &str = "diff --git a/src/old name.rs b/src/new name.rs
similarity index 90%
rename from src/old name.rs
rename to src/new name.rs
index 3333333..4444444 100644
--- a/src/old name.rs
+++ b/src/new name.rs
@@ -1,3 +1,3 @@
 fn main() {
-    old();
+    new();
 }
diff --git a/docs/gone.md b/docs/gone.md
deleted file mode 100644
index 5555555..0000000
--- a/docs/gone.md
+++ /dev/null
@@ -1,2 +0,0 @@
-# Gone
--- a removed line that looks like a header
diff --git a/assets/logo.png b/assets/logo.png
index 6666666..7777777 100644
Binary files a/assets/logo.png and b/assets/logo.png differ
";

#[test]
fn whole_diff_keeps_renames_deletions_and_binaries_and_reads_text_at_a_revision() {
    let fixture = Fixture::new();
    let store = Store::new();
    let reads = reads(&fixture, &store);
    let (_server, log) = serve(fixture, |seen| {
        match (
            seen.line.split_whitespace().nth(1).unwrap(),
            seen.accept.as_str(),
        ) {
            ("/repos/octo/repo/pulls/7", "application/vnd.github.diff") => {
                (200, String::new(), WHOLE_DIFF.as_bytes().to_vec())
            }
            ("/repos/octo/repo/pulls/7", _) => revisions(3),
            (path, "application/vnd.github.raw") => match path {
                p if p == format!("/repos/octo/repo/contents/src/new%20name.rs?ref={HEAD}") => {
                    (200, String::new(), b"fn main() {\n    new();\n}\n".to_vec())
                }
                p if p.starts_with("/repos/octo/repo/contents/assets/logo.png") => {
                    (200, String::new(), b"\x89PNG\r\n\x1a\n\0\0".to_vec())
                }
                p if p.starts_with("/repos/octo/repo/contents/big.txt") => {
                    (200, String::new(), vec![b'a'; 1024 * 1024 + 1])
                }
                _ => (404, String::new(), b"{\"message\":\"Not Found\"}".to_vec()),
            },
            _ => (500, String::new(), Vec::new()),
        }
    });

    let files = reads.files(&key(), None).unwrap().value;
    assert_eq!((files.base.as_str(), files.head.as_str()), (BASE, HEAD));
    assert!(files.complete && files.next_page.is_none());
    let summary: Vec<_> = files
        .files
        .iter()
        .map(|file| {
            (
                file.path.as_str(),
                file.previous_path.as_deref(),
                file.kind,
                file.additions,
                file.deletions,
            )
        })
        .collect();
    use agent::FileChangeKind::*;
    assert_eq!(
        summary,
        vec![
            ("src/new name.rs", Some("src/old name.rs"), Rename, 1, 1),
            ("docs/gone.md", None, Delete, 0, 2),
            ("assets/logo.png", None, Modify, 0, 0),
        ]
    );
    let PullRequestPatch::Hunks(hunks) = &files.files[0].patch else {
        panic!("a renamed file keeps its hunks")
    };
    assert!(hunks.starts_with("@@ -1,3 +1,3 @@\n") && hunks.ends_with(" }\n"));
    assert_eq!(files.files[2].patch, PullRequestPatch::Binary);

    assert_eq!(
        *reads
            .file_text(&key(), HEAD, "src/new name.rs")
            .unwrap()
            .value,
        PullRequestFileText::Text("fn main() {\n    new();\n}\n".into())
    );
    assert_eq!(
        *reads
            .file_text(&key(), HEAD, "assets/logo.png")
            .unwrap()
            .value,
        PullRequestFileText::Binary
    );
    assert_eq!(
        *reads.file_text(&key(), HEAD, "big.txt").unwrap().value,
        PullRequestFileText::Oversized
    );
    assert_eq!(
        *reads
            .file_text(&key(), BASE, "src/new name.rs")
            .unwrap()
            .value,
        PullRequestFileText::Missing,
        "a side the revision does not have is missing, not a failure"
    );
    let before = log.lock().unwrap().len();
    for (revision, path) in [
        ("main", "src/new name.rs"),
        (HEAD, "../../../user"),
        (HEAD, "src//x"),
        (HEAD, ""),
    ] {
        assert_eq!(
            reads.file_text(&key(), revision, path).unwrap_err(),
            GitHubError::InvalidInput
        );
    }
    assert_eq!(
        log.lock().unwrap().len(),
        before,
        "a branch name or a path leaving the repository never reaches GitHub"
    );
    let log = log.lock().unwrap();
    assert!(
        log.iter()
            .all(|seen| seen.authorization.as_deref() == Some("Bearer fixture-token"))
    );
}

fn listed(name: &str, status: &str, changes: (u64, u64), patch: Option<&str>) -> Value {
    let mut row = json!({
        "filename": name,
        "status": status,
        "additions": changes.0,
        "deletions": changes.1,
        "sha": "8888888888888888888888888888888888888888",
    });
    if status == "renamed" {
        row["previous_filename"] = json!(format!("old/{name}"));
    }
    if let Some(patch) = patch {
        row["patch"] = json!(patch);
    }
    row
}

#[test]
fn a_refused_whole_diff_pages_the_changed_files_and_names_withheld_patches() {
    let fixture = Fixture::new();
    let store = Store::new();
    let reads = reads(&fixture, &store);
    let pages = Arc::new(Mutex::new(true));
    let pages_answer = pages.clone();
    let (_server, log) = serve(fixture, move |seen| {
        let path = seen.line.split_whitespace().nth(1).unwrap();
        if seen.accept == "application/vnd.github.diff" {
            return (
                406,
                String::new(),
                br#"{"message":"Sorry, the diff exceeded the maximum number of files (300)."}"#
                    .to_vec(),
            );
        }
        if !*pages_answer.lock().unwrap() && path.contains("/files?") {
            return (500, String::new(), b"{}".to_vec());
        }
        match path {
            "/repos/octo/repo/pulls/7" => revisions(101),
            "/repos/octo/repo/pulls/7/files?per_page=100&page=1" => {
                let mut rows = vec![
                    listed("huge.json", "modified", (40_000, 2), None),
                    listed("image.png", "added", (0, 0), None),
                    listed("moved.rs", "renamed", (0, 0), None),
                ];
                rows.extend((3..100).map(|index| {
                    listed(
                        &format!("src/{index}.rs"),
                        "modified",
                        (1, 0),
                        Some("@@ -1 +1,2 @@\n a\n+b"),
                    )
                }));
                json_reply(Value::Array(rows))
            }
            "/repos/octo/repo/pulls/7/files?per_page=100&page=2" => json_reply(json!([listed(
                "last.rs",
                "removed",
                (0, 3),
                Some("@@ -1,3 +0,0 @@\n-a\n-b\n-c")
            )])),
            _ => (500, String::new(), Vec::new()),
        }
    });

    let first = reads.files(&key(), None).unwrap().value;
    assert_eq!(first.files.len(), 100);
    assert_eq!(
        (first.next_page, first.complete),
        (Some(2), false),
        "a full page names the next one"
    );
    assert_eq!(first.files[0].patch, PullRequestPatch::Oversized);
    assert_eq!(first.files[1].patch, PullRequestPatch::Binary);
    assert_eq!(
        (
            &first.files[2].patch,
            first.files[2].previous_path.as_deref()
        ),
        (
            &PullRequestPatch::Hunks(String::new()),
            Some("old/moved.rs")
        )
    );
    let second = reads.files(&key(), Some(2)).unwrap().value;
    assert_eq!(second.files[0].kind, agent::FileChangeKind::Delete);
    assert_eq!((second.next_page, second.complete), (None, true));
    assert_eq!(
        log.lock()
            .unwrap()
            .iter()
            .filter(|seen| seen.accept == "application/vnd.github.diff")
            .count(),
        1,
        "a page carries on from the files walk without asking for the whole diff again"
    );

    // When the pages fail too, the refusal that explains the missing diff is reported.
    reads.invalidate(&key());
    *pages.lock().unwrap() = false;
    assert!(matches!(
        reads.files(&key(), None).unwrap_err(),
        GitHubError::Response { status: 406, .. }
    ));
}

fn conversation_reply(seen: &Seen) -> (u16, String, Vec<u8>) {
    let author =
        json!({"login": "octocat", "avatarUrl": "https://avatars.githubusercontent.com/u/1"});
    let comment = |id: &str, body: &str, at: &str| {
        json!({
            "id": id, "body": body, "createdAt": at, "lastEditedAt": null,
            "url": format!("https://github.com/octo/repo/pull/7#{id}"), "author": author,
            "reactionGroups": [
                {"content": "THUMBS_UP", "viewerHasReacted": true, "reactors": {"totalCount": 2}},
                {"content": "HEART", "viewerHasReacted": false, "reactors": {"totalCount": 0}},
            ],
            "commit": null,
            "originalCommit": {"oid": BASE},
            "diffHunk": "@@ -8,5 +8,5 @@\n a\n b\n-c\n+d\n e\n f",
        })
    };
    match seen.operation().as_deref() {
        Some("PullRequestConversation") => {
            json_reply(json!({"data": {"repository": {"pullRequest": {
                "id": "PR_7", "body": "Screenshot: ![shot](https://github.com/user-attachments/assets/abc-123)\n<video src=\"https://github.com/user-attachments/assets/vid-1\">\n![](https://github.com/user-attachments/assets/loop) ![](https://github.com/user-attachments/assets/huge) ![](https://github.com/user-attachments/assets/wide) ![](https://github.com/user-attachments/assets/page)",
                "createdAt": "2026-10-01T00:00:00Z", "lastEditedAt": null,
                "url": "https://github.com/octo/repo/pull/7", "author": author, "reactionGroups": [],
                "mergedAt": null,
                "comments": {"pageInfo": {"hasNextPage": false, "endCursor": null},
                    "nodes": [comment("IC_2", "second", "2026-10-03T00:00:00Z")]},
                "reviews": {"pageInfo": {"hasNextPage": false, "endCursor": null}, "nodes": [
                    {"id": "PRR_1", "body": "", "state": "COMMENTED", "submittedAt": "2026-10-02T00:00:00Z",
                     "createdAt": "2026-10-02T00:00:00Z", "url": "", "author": author, "reactionGroups": []},
                    {"id": "PRR_2", "body": "", "state": "APPROVED", "submittedAt": "2026-10-02T12:00:00Z",
                     "createdAt": "2026-10-02T12:00:00Z", "url": "", "author": author, "reactionGroups": []},
                ]},
            }}}}))
        }
        Some("PullRequestReviewThreads") => {
            let after = &seen.variables()["cursor"];
            if after.is_null() {
                json_reply(
                    json!({"data": {"repository": {"pullRequest": {"reviewThreads": {
                        "pageInfo": {"hasNextPage": false, "endCursor": null},
                        "nodes": [{
                            "id": "PRRT_outdated", "isResolved": true, "isOutdated": true,
                            "path": "src/lib.rs", "line": null, "startLine": null,
                            "originalLine": 12, "originalStartLine": 10,
                            "diffSide": "RIGHT", "startDiffSide": "RIGHT",
                            "comments": {"totalCount": 12,
                                "pageInfo": {"hasNextPage": true, "endCursor": "C10"},
                                "nodes": [comment("RC_1", "These three lines ![x](https://github.com/user-attachments/assets/in-thread)", "2026-10-02T01:00:00Z")]},
                        }],
                    }}}}}),
                )
            } else {
                json_reply(
                    json!({"data": {"repository": {"pullRequest": {"reviewThreads": {
                        "pageInfo": {"hasNextPage": false, "endCursor": null}, "nodes": [],
                    }}}}}),
                )
            }
        }
        Some("PullRequestThreadReplies") => {
            let thread = seen.variables()["thread"].as_str().unwrap().to_owned();
            json_reply(json!({"data": {
                "repository": {"pullRequest": {"id": "PR_7"}},
                "node": {
                    "pullRequest": {"id": if thread == "PRRT_outdated" { "PR_7" } else { "PR_other" }},
                    "comments": {"pageInfo": {"hasNextPage": false, "endCursor": null},
                        "nodes": [comment("RC_11", "eleventh", "2026-10-04T00:00:00Z")]},
                },
            }}))
        }
        _ => (500, String::new(), Vec::new()),
    }
}

#[test]
fn conversation_keeps_an_outdated_multiline_thread_and_pages_its_replies_within_the_pull_request() {
    let fixture = Fixture::new();
    let store = Store::new();
    let reads = reads(&fixture, &store);
    let (_server, log) = serve(fixture, conversation_reply);

    let conversation = reads.conversation(&key()).unwrap().value;
    assert!(conversation.complete);
    assert!(conversation.description.body.starts_with("Screenshot:"));
    assert_eq!(
        conversation
            .comments
            .iter()
            .map(|comment| comment.id.as_str())
            .collect::<Vec<_>>(),
        vec!["PRR_2", "IC_2"],
        "a bodiless comment review is the threads' wrapper; an approval is the event itself"
    );
    assert_eq!(conversation.comments[1].reactions.len(), 1);
    assert!(conversation.comments[1].reactions[0].viewer_reacted);
    let thread = &conversation.threads[0];
    assert_eq!(thread.id, "PRRT_outdated");
    assert!(thread.outdated && thread.resolved);
    assert_eq!(
        thread.anchor,
        Some(PullRequestReviewAnchor {
            revision: BASE.into(),
            path: "src/lib.rs".into(),
            side: ReviewSide::New,
            start_line: 10,
            end_line: 12,
        }),
        "an outdated thread keeps the lines it was left on, at the commit it was left on"
    );
    assert_eq!(
        thread.diff_hunk.as_deref(),
        Some("-c\n+d\n e\n f"),
        "the excerpt is the hunk's last lines"
    );
    assert_eq!(
        (thread.total_comments, thread.replies_after.as_deref()),
        (12, Some("C10"))
    );

    let replies = reads
        .thread_replies(&key(), "PRRT_outdated", "C10")
        .unwrap()
        .value;
    assert_eq!(replies.comments[0].id, "RC_11");
    assert_eq!(
        reads
            .thread_replies(&key(), "PRRT_elsewhere", "C10")
            .unwrap_err(),
        GitHubError::NotFound,
        "a thread of another pull request is not read through this one"
    );
    let log = log.lock().unwrap();
    let replies = log
        .iter()
        .find(|seen| seen.operation().as_deref() == Some("PullRequestThreadReplies"))
        .unwrap();
    assert_eq!(replies.variables()["cursor"], "C10");
}

#[test]
fn a_thread_list_past_ten_pages_is_reported_incomplete() {
    let fixture = Fixture::new();
    let store = Store::new();
    let reads = reads(&fixture, &store);
    let (_server, log) = serve(fixture, |seen| {
        if seen.operation().as_deref() == Some("PullRequestReviewThreads") {
            // Every page names a fresh next page.
            let page = seen.variables()["cursor"]
                .as_str()
                .unwrap_or("0")
                .to_owned();
            return json_reply(
                json!({"data": {"repository": {"pullRequest": {"reviewThreads": {
                    "pageInfo": {"hasNextPage": true, "endCursor": format!("{page}+")}, "nodes": [],
                }}}}}),
            );
        }
        conversation_reply(seen)
    });
    let conversation = reads.conversation(&key()).unwrap().value;
    assert!(!conversation.complete);
    assert_eq!(
        log.lock()
            .unwrap()
            .iter()
            .filter(|seen| seen.operation().as_deref() == Some("PullRequestReviewThreads"))
            .count(),
        10
    );
}

fn viewed_page(after: &Value) -> (u16, String, Vec<u8>) {
    let page = after
        .as_str()
        .map_or(0, |cursor| cursor.parse::<u32>().unwrap());
    let nodes: Vec<_> = (0..100)
        .map(|index| {
            json!({
                "path": format!("file-{page}-{index}"),
                "viewerViewedState": match index % 3 { 0 => "VIEWED", 1 => "DISMISSED", _ => "UNVIEWED" },
            })
        })
        .collect();
    json_reply(
        json!({"data": {"repository": {"pullRequest": {"id": "PR_node_7", "files": {
            "pageInfo": {"hasNextPage": true, "endCursor": (page + 1).to_string()},
            "nodes": nodes,
        }}}}}),
    )
}

#[test]
fn viewed_files_stop_at_five_pages_and_marking_rereads_only_the_viewed_state() {
    let fixture = Fixture::new();
    let store = Store::new();
    let reads = reads(&fixture, &store);
    let (_server, log) = serve(fixture, |seen| match seen.operation().as_deref() {
        Some("PullRequestViewedFiles") => viewed_page(&seen.variables()["after"]),
        Some("SetPullRequestFilesViewed") => json_reply(json!({"data": {
            "f0": {"clientMutationId": null}, "f1": {"clientMutationId": null},
        }})),
        _ => conversation_reply(seen),
    });
    let count = |operation: &str| {
        log.lock()
            .unwrap()
            .iter()
            .filter(|seen| seen.operation().as_deref() == Some(operation))
            .count()
    };

    let viewed = reads.viewed_files(&key()).unwrap().value;
    assert_eq!(viewed.files.len(), 500);
    assert!(!viewed.complete, "a sixth page is unknown, not unviewed");
    assert_eq!(
        viewed.files[..3]
            .iter()
            .map(|(_, state)| *state)
            .collect::<Vec<_>>(),
        vec![
            PullRequestViewedState::Viewed,
            PullRequestViewedState::Dismissed,
            PullRequestViewedState::Unviewed
        ]
    );
    reads.conversation(&key()).unwrap();
    assert_eq!(
        (
            count("PullRequestViewedFiles"),
            count("PullRequestConversation")
        ),
        (5, 1)
    );

    reads
        .set_viewed(&key(), &["a.rs".into(), "b/c d.rs".into()], true)
        .unwrap();
    let mutation = log
        .lock()
        .unwrap()
        .iter()
        .find(|seen| seen.operation().as_deref() == Some("SetPullRequestFilesViewed"))
        .cloned()
        .unwrap();
    let body: Value = serde_json::from_str(&mutation.body).unwrap();
    let document = body["query"].as_str().unwrap();
    assert!(document.starts_with("mutation SetPullRequestFilesViewed("));
    assert!(document.contains(
        "f1: markFileAsViewed(input: { pullRequestId: $pullRequestId, path: $f1_path })"
    ));
    assert_eq!(
        body["variables"],
        json!({"pullRequestId": "PR_node_7", "f0_path": "a.rs", "f1_path": "b/c d.rs"}),
        "paths travel as variables and the node id comes from the viewed read"
    );
    assert_eq!(count("PullRequestNodeId"), 0);

    reads.viewed_files(&key()).unwrap();
    reads.conversation(&key()).unwrap();
    assert_eq!(
        (
            count("PullRequestViewedFiles"),
            count("PullRequestConversation")
        ),
        (10, 1),
        "marking a file reads the viewed state again and leaves the conversation shared"
    );
}

#[test]
fn reads_are_shared_in_flight_and_within_their_ttl_but_never_a_failure_or_across_accounts() {
    let fixture = Fixture::new();
    let store = Store::new();
    let credentials = store.credentials(&[]);
    store
        .store
        .set_github_token("github.com", Some("first-account"))
        .unwrap();
    let reads = PullRequestReads::new(GitHubApi::new(credentials.clone(), fixture.builder()));
    let fail = Arc::new(Mutex::new(false));
    let failing = fail.clone();
    let (_server, log) = serve(fixture, move |seen| {
        // Slow enough that concurrent readers overlap the first read.
        thread::sleep(Duration::from_millis(100));
        if *failing.lock().unwrap() {
            return (502, String::new(), b"{}".to_vec());
        }
        match seen.operation().as_deref() {
            Some("PullRequestViewedFiles") => {
                json_reply(json!({"data": {"repository": {"pullRequest": {
                    "id": "PR_node_7",
                    "files": {"pageInfo": {"hasNextPage": false, "endCursor": null}, "nodes": []},
                }}}}))
            }
            _ => conversation_reply(seen),
        }
    });
    let count = |operation: &str| {
        log.lock()
            .unwrap()
            .iter()
            .filter(|seen| seen.operation().as_deref() == Some(operation))
            .count()
    };

    thread::scope(|scope| {
        let readers: Vec<_> = (0..4)
            .map(|_| scope.spawn(|| reads.conversation(&key()).unwrap().value))
            .collect();
        for reader in readers {
            reader.join().unwrap();
        }
    });
    assert_eq!(
        count("PullRequestConversation"),
        1,
        "one read in flight serves all"
    );
    reads.conversation(&key()).unwrap();
    assert_eq!(count("PullRequestConversation"), 1, "shared within the TTL");

    reads.invalidate(&key());
    *fail.lock().unwrap() = true;
    assert!(reads.conversation(&key()).is_err());
    *fail.lock().unwrap() = false;
    reads.conversation(&key()).unwrap();
    assert_eq!(
        count("PullRequestConversation"),
        3,
        "a failure is kept for no one; the next reader asks again"
    );

    let first = reads.conversation(&key()).unwrap().value.account.clone();
    store
        .store
        .set_github_token("github.com", Some("second-account"))
        .unwrap();
    let second = reads.conversation(&key()).unwrap().value.account.clone();
    assert_eq!(
        count("PullRequestConversation"),
        4,
        "another account reads its own"
    );
    assert_ne!(
        first, second,
        "media a client keys by the account misses after a switch"
    );
    assert_eq!(
        log.lock().unwrap().last().unwrap().authorization.as_deref(),
        Some("Bearer second-account")
    );

    let viewed = reads.viewed_files(&key()).unwrap();
    reads.viewed_files(&key()).unwrap();
    assert_eq!(count("PullRequestViewedFiles"), 1);
    let left = |at: std::time::SystemTime| {
        at.duration_since(std::time::SystemTime::now())
            .unwrap_or_default()
            .as_secs_f64()
            .round()
    };
    assert_eq!(
        (
            left(viewed.expires_at),
            left(reads.conversation(&key()).unwrap().expires_at)
        ),
        (15., 60.),
        "each answer tells its reader when to ask again"
    );
    thread::sleep(Duration::from_secs(15));
    reads.viewed_files(&key()).unwrap();
    assert_eq!(
        count("PullRequestViewedFiles"),
        2,
        "viewed state is shared for fifteen seconds"
    );
    reads.conversation(&key()).unwrap();
    assert_eq!(
        count("PullRequestConversation"),
        4,
        "the conversation outlives the viewed state's fifteen seconds"
    );
}

fn png(width: u32, height: u32) -> Vec<u8> {
    let mut bytes = std::io::Cursor::new(Vec::new());
    image::RgbaImage::new(width, height)
        .write_to(&mut bytes, image::ImageFormat::Png)
        .unwrap();
    bytes.into_inner()
}

#[test]
fn media_is_read_only_from_github_assets_the_conversation_names_and_keeps_the_token_on_github() {
    use tcode_services::github::media::asset_url;
    for (source, expected) in [
        (
            "https://github.com/user-attachments/assets/abc-123",
            Some("https://github.com/user-attachments/assets/abc-123"),
        ),
        (
            "https://www.github.com/octo/repo/assets/42/legacy-id",
            Some("https://github.com/octo/repo/assets/42/legacy-id"),
        ),
        (
            "https://github.com/octo/repo/blob/main/docs/shot.png?raw=true",
            Some("https://raw.githubusercontent.com/octo/repo/main/docs/shot.png"),
        ),
        (
            "https://RAW.githubusercontent.com:443/octo/repo/main/a.png",
            Some("https://raw.githubusercontent.com/octo/repo/main/a.png"),
        ),
        ("http://github.com/user-attachments/assets/abc", None),
        ("https://github.com/octo/repo/pull/7", None),
        ("https://avatars.githubusercontent.com/u/1", None),
        (
            "https://private-user-images.githubusercontent.com/1/2.png?jwt=expired",
            None,
        ),
        ("https://example.com/user-attachments/assets/abc", None),
    ] {
        assert_eq!(
            asset_url(source).as_ref().map(url::Url::as_str),
            expected,
            "{source}"
        );
    }

    let fixture = Fixture::new();
    let store = Store::new();
    let reads = reads(&fixture, &store);
    let (_server, log) = serve(fixture, move |seen| {
        let path = seen.line.split_whitespace().nth(1).unwrap();
        let redirect = |to: &str| (302, format!("Location: {to}\r\n"), Vec::new());
        match (seen.host.as_str(), path) {
            ("api.github.com", _) => conversation_reply(seen),
            ("github.com", "/user-attachments/assets/abc-123") => {
                redirect("https://objects.githubusercontent.com/signed/abc?X-Amz-Signature=s")
            }
            ("objects.githubusercontent.com", "/signed/abc?X-Amz-Signature=s") => (
                200,
                "Content-Type: image/png\r\nETag: \"v1\"\r\n".into(),
                png(3, 2),
            ),
            ("github.com", "/user-attachments/assets/vid-1") => {
                (200, "Content-Type: video/mp4\r\n".into(), vec![0; 64])
            }
            ("github.com", "/user-attachments/assets/in-thread") => {
                redirect("http://objects.githubusercontent.com/plain")
            }
            ("github.com", "/user-attachments/assets/loop") => {
                redirect("https://objects.githubusercontent.com/hop/1")
            }
            ("objects.githubusercontent.com", hop) if hop.starts_with("/hop/") => {
                let next = hop[5..].parse::<u32>().unwrap() + 1;
                redirect(&format!("/hop/{next}"))
            }
            ("github.com", "/user-attachments/assets/huge") => (
                200,
                "Content-Type: image/png\r\n".into(),
                vec![0; tcode_protocol::MAX_PULL_REQUEST_MEDIA_BYTES + 1],
            ),
            ("github.com", "/user-attachments/assets/wide") => {
                (200, "Content-Type: image/png\r\n".into(), png(9000, 1))
            }
            ("avatars.githubusercontent.com", "/u/1") => {
                (200, "Content-Type: image/png\r\n".into(), png(2, 2))
            }
            ("github.com", "/user-attachments/assets/page") => (
                200,
                "Content-Type: text/html\r\n".into(),
                b"<html></html>".to_vec(),
            ),
            _ => (404, String::new(), Vec::new()),
        }
    });
    let media = |url: &str| reads.media(&key(), url, None);
    let asset = |id: &str| format!("https://github.com/user-attachments/assets/{id}");

    let PullRequestMedia::Image {
        bytes,
        mime,
        validator,
        ..
    } = media(&asset("abc-123")).unwrap()
    else {
        panic!("an uploaded screenshot is an image")
    };
    assert_eq!((bytes, mime.as_str()), (png(3, 2), "image/png"));
    assert_eq!(validator.as_deref(), Some("\"v1\""));
    {
        let log = log.lock().unwrap();
        let asset = log.iter().find(|seen| seen.host == "github.com").unwrap();
        let object = log
            .iter()
            .find(|seen| seen.host == "objects.githubusercontent.com")
            .unwrap();
        assert_eq!(asset.authorization.as_deref(), Some("Bearer fixture-token"));
        assert_eq!(
            object.authorization, None,
            "the signed hop never sees the token"
        );
    }
    assert!(matches!(
        media("https://avatars.githubusercontent.com/u/1").unwrap(),
        PullRequestMedia::Image { .. }
    ));
    assert_eq!(
        log.lock().unwrap().last().unwrap().authorization,
        None,
        "an author's avatar is public and read without the token"
    );
    assert_eq!(
        media("https://avatars.githubusercontent.com/u/2").unwrap_err(),
        GitHubError::InvalidInput,
        "an avatar no author of the pull request has is not read"
    );
    assert_eq!(
        media(&asset("vid-1")).unwrap(),
        PullRequestMedia::External {
            mime: "video/mp4".into()
        }
    );
    assert_eq!(
        media(&asset("in-thread")).unwrap_err(),
        GitHubError::InvalidResponse,
        "a redirect off HTTPS is not followed, for media a review thread names"
    );
    let before = log.lock().unwrap().len();
    assert_eq!(
        media(&asset("loop")).unwrap_err(),
        GitHubError::InvalidResponse
    );
    assert_eq!(
        log.lock().unwrap().len() - before,
        4,
        "three redirects are followed and no fourth"
    );
    assert_eq!(
        media(&asset("huge")).unwrap_err(),
        GitHubError::BodyTooLarge
    );
    assert_eq!(
        media(&asset("wide")).unwrap_err(),
        GitHubError::BodyTooLarge
    );
    assert_eq!(
        media(&asset("page")).unwrap_err(),
        GitHubError::UnsupportedMedia
    );
    let before = log.lock().unwrap().len();
    assert_eq!(
        media(&asset("not-mentioned")).unwrap_err(),
        GitHubError::InvalidInput,
        "only media the pull request names is read"
    );
    assert_eq!(
        media("https://example.com/user-attachments/assets/abc-123").unwrap_err(),
        GitHubError::InvalidInput
    );
    assert_eq!(log.lock().unwrap().len(), before);
}
