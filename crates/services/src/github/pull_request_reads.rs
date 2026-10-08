//! What a client reads of a linked pull request: its files, file text at a revision, the
//! conversation, the account's viewed files, and media the conversation points at.

use super::{
    GitHubApi, GitHubError, RequestOptions, RestRequest,
    api::{Authentication, DEADLINE_CAP},
    graphql::{self, AliasItem, Document, Variables},
    media,
    read_cache::{ReadCache, ReadKey},
    repository::Repository,
};
use agent::FileChangeKind;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, Mutex},
    time::Duration,
};
use tcode_core::{pull_request::PullRequestKey, session::ReviewSide};
use tcode_protocol::{
    PullRequestActor, PullRequestComment, PullRequestConversation, PullRequestFile,
    PullRequestFileText, PullRequestFiles, PullRequestMedia, PullRequestPatch, PullRequestReaction,
    PullRequestReviewAnchor, PullRequestReviewState, PullRequestReviewThread,
    PullRequestThreadReplies, PullRequestViewedFiles, PullRequestViewedState,
};

const READ_TTL: Duration = Duration::from_secs(60);
/// A merged pull request changes no more, short of an edited comment.
const MERGED_TTL: Duration = Duration::from_secs(600);
const VIEWED_TTL: Duration = Duration::from_secs(15);
/// Text at a commit never changes; the bound is only on how long it holds memory.
const TEXT_TTL: Duration = Duration::from_secs(600);
const DIFF_BYTES: usize = 8 * 1024 * 1024;
const FILE_BYTES: usize = 1024 * 1024;
const FILES_PER_PAGE: usize = 100;
/// Past this many requests a list is reported incomplete rather than read on.
const MAX_PAGES: usize = 10;
const VIEWED_PAGES: usize = 5;
const EMPTY_BLOB: &str = "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391";

const ACTOR: &str = "author { login avatarUrl }";
const REACTIONS: &str = "reactionGroups { content viewerHasReacted reactors { totalCount } }";

#[derive(Debug, Clone)]
struct Revisions {
    base: String,
    head: String,
    changed_files: u64,
}

pub struct PullRequestReads {
    api: Arc<GitHubApi>,
    revisions: ReadCache<Revisions>,
    files: ReadCache<PullRequestFiles>,
    texts: ReadCache<PullRequestFileText>,
    conversations: ReadCache<PullRequestConversation>,
    replies: ReadCache<PullRequestThreadReplies>,
    viewed: ReadCache<PullRequestViewedFiles>,
    /// Node ids for the viewed mutation, per pull request and account.
    node_ids: Mutex<HashMap<(PullRequestKey, String), String>>,
}

struct Reader<'a> {
    api: &'a GitHubApi,
    key: &'a PullRequestKey,
    repository: Repository,
    account: String,
    options: RequestOptions,
}

impl Reader<'_> {
    fn read_key(&self, read: impl Into<String>) -> ReadKey {
        ReadKey {
            pull_request: self.key.clone(),
            account: self.account.clone(),
            read: read.into(),
        }
    }

    fn rest_path(&self, rest: &str) -> String {
        format!(
            "/repos/{}/{}/{rest}",
            self.repository.owner, self.repository.name
        )
    }

    fn long_read(&self, operation: &'static str, body_limit: usize) -> RequestOptions {
        RequestOptions {
            operation,
            timeout: DEADLINE_CAP,
            body_limit,
            ..self.options.clone()
        }
    }

    fn query(
        &self,
        operation: &'static str,
        query: String,
        extra: impl IntoIterator<Item = (&'static str, Value)>,
    ) -> Result<Value, GitHubError> {
        let mut variables = BTreeMap::from([
            ("owner".to_owned(), json!(self.repository.owner)),
            ("name".to_owned(), json!(self.repository.name)),
            ("number".to_owned(), json!(self.key.number)),
        ]);
        variables.extend(
            extra
                .into_iter()
                .map(|(name, value)| (name.to_owned(), value)),
        );
        let options = RequestOptions {
            operation,
            ..self.options.clone()
        };
        self.api
            .graphql(&self.key.host, &Document { query, variables }, &options)?
            .json()
    }

    fn revisions(&self) -> Result<(Revisions, Duration), GitHubError> {
        let raw: Value = self
            .api
            .rest(
                &self.key.host,
                RestRequest::get(&self.rest_path(&format!("pulls/{}", self.key.number))),
                &RequestOptions {
                    operation: "PullRequestRevisions",
                    ..self.options.clone()
                },
            )?
            .json()?;
        let sha = |side: &str| {
            raw[side]["sha"]
                .as_str()
                .filter(|sha| is_revision(sha))
                .map(str::to_owned)
                .ok_or(GitHubError::InvalidResponse)
        };
        let revisions = Revisions {
            base: sha("base")?,
            head: sha("head")?,
            changed_files: raw["changed_files"].as_u64().unwrap_or(0),
        };
        let ttl = if raw["merged_at"].is_string() {
            MERGED_TTL
        } else {
            READ_TTL
        };
        Ok((revisions, ttl))
    }

    /// GitHub refuses a whole diff past its own limits, and this read cuts one past 8 MiB; both
    /// read the files a page at a time instead. A failing page reports the original refusal.
    fn whole_diff(&self, revisions: &Revisions) -> Result<PullRequestFiles, GitHubError> {
        let path = self.rest_path(&format!("pulls/{}", self.key.number));
        let answer = self.api.rest(
            &self.key.host,
            RestRequest {
                accept: Some("application/vnd.github.diff"),
                ..RestRequest::get(&path)
            },
            &self.long_read("PullRequestDiff", DIFF_BYTES),
        );
        match answer {
            Ok(response) if !response.truncated => Ok(PullRequestFiles {
                base: revisions.base.clone(),
                head: revisions.head.clone(),
                files: diff_files(&String::from_utf8_lossy(&response.body))
                    .ok_or(GitHubError::InvalidResponse)?,
                next_page: None,
                complete: true,
            }),
            Ok(_) => self.files_page(revisions, 1),
            Err(refusal @ GitHubError::Response { .. }) => {
                self.files_page(revisions, 1).map_err(|_| refusal)
            }
            Err(error) => Err(error),
        }
    }

    fn files_page(
        &self,
        revisions: &Revisions,
        page: u32,
    ) -> Result<PullRequestFiles, GitHubError> {
        let path = self.rest_path(&format!(
            "pulls/{}/files?per_page={FILES_PER_PAGE}&page={page}",
            self.key.number
        ));
        let rows: Vec<Value> = self
            .api
            .rest(
                &self.key.host,
                RestRequest::get(&path),
                &self.long_read("PullRequestFiles", DIFF_BYTES),
            )?
            .json()?;
        let next_page = (rows.len() >= FILES_PER_PAGE).then_some(page + 1);
        let listed = (page as u64 - 1) * FILES_PER_PAGE as u64 + rows.len() as u64;
        Ok(PullRequestFiles {
            base: revisions.base.clone(),
            head: revisions.head.clone(),
            files: rows.iter().filter_map(listed_file).collect(),
            next_page,
            complete: next_page.is_none() && listed >= revisions.changed_files,
        })
    }

    fn file_text(&self, revision: &str, path: &str) -> Result<PullRequestFileText, GitHubError> {
        let encoded = path
            .split('/')
            .map(percent_encode)
            .collect::<Vec<_>>()
            .join("/");
        let answer = self.api.rest(
            &self.key.host,
            RestRequest {
                accept: Some("application/vnd.github.raw"),
                ..RestRequest::get(&self.rest_path(&format!("contents/{encoded}?ref={revision}")))
            },
            &self.long_read("PullRequestFileText", FILE_BYTES),
        );
        Ok(match answer {
            Ok(response) if response.truncated => PullRequestFileText::Oversized,
            Ok(response) if response.body.contains(&0) => PullRequestFileText::Binary,
            Ok(response) => String::from_utf8(response.body)
                .map_or(PullRequestFileText::Binary, PullRequestFileText::Text),
            Err(GitHubError::NotFound) => PullRequestFileText::Missing,
            Err(error) => return Err(error),
        })
    }

    fn conversation(&self) -> Result<(PullRequestConversation, Duration), GitHubError> {
        let comment = format!("id body createdAt lastEditedAt url {ACTOR} {REACTIONS}");
        let query = format!(
            "query PullRequestConversation($owner: String!, $name: String!, $number: Int!, $head: Boolean!, $withComments: Boolean!, $commentsAfter: String, $withReviews: Boolean!, $reviewsAfter: String) {{ repository(owner: $owner, name: $name) {{ pullRequest(number: $number) {{ ... on PullRequest @include(if: $head) {{ {comment} mergedAt }} comments(first: 100, after: $commentsAfter) @include(if: $withComments) {{ pageInfo {{ hasNextPage endCursor }} nodes {{ {comment} }} }} reviews(first: 100, after: $reviewsAfter) @include(if: $withReviews) {{ pageInfo {{ hasNextPage endCursor }} nodes {{ {comment} state submittedAt }} }} }} }} }}"
        );
        let mut description = None;
        let mut merged = false;
        let mut comments = Vec::new();
        let (mut more_comments, mut more_reviews) = (true, true);
        let (mut comments_after, mut reviews_after) = (Value::Null, Value::Null);
        for page in 0..MAX_PAGES {
            let response = self.query(
                "PullRequestConversation",
                query.clone(),
                [
                    ("head", json!(page == 0)),
                    ("withComments", json!(more_comments)),
                    ("commentsAfter", comments_after.clone()),
                    ("withReviews", json!(more_reviews)),
                    ("reviewsAfter", reviews_after.clone()),
                ],
            )?;
            let pr = &response["data"]["repository"]["pullRequest"];
            if pr.is_null() {
                return Err(GitHubError::NotFound);
            }
            if page == 0 {
                merged = pr["mergedAt"].is_string();
                description = Some(comment_from(pr, None).ok_or(GitHubError::InvalidResponse)?);
            }
            if more_comments {
                comments.extend(nodes(&pr["comments"]).filter_map(|raw| comment_from(raw, None)));
                comments_after = next_cursor(&pr["comments"]).map_or(Value::Null, Value::String);
                more_comments = !comments_after.is_null();
            }
            if more_reviews {
                comments.extend(nodes(&pr["reviews"]).filter_map(review_from));
                reviews_after = next_cursor(&pr["reviews"]).map_or(Value::Null, Value::String);
                more_reviews = !reviews_after.is_null();
            }
            if !more_comments && !more_reviews {
                break;
            }
        }
        let mut complete = !more_comments && !more_reviews;
        comments.sort_by(|left, right| left.created_at.cmp(&right.created_at));

        let thread_query = format!(
            "query PullRequestReviewThreads($owner: String!, $name: String!, $number: Int!, $cursor: String) {{ repository(owner: $owner, name: $name) {{ pullRequest(number: $number) {{ reviewThreads(first: 100, after: $cursor) {{ pageInfo {{ hasNextPage endCursor }} nodes {{ id isResolved isOutdated path line startLine originalLine originalStartLine diffSide startDiffSide comments(first: 10) {{ totalCount pageInfo {{ hasNextPage endCursor }} nodes {{ {comment} commit {{ oid }} originalCommit {{ oid }} }} }} }} }} }} }} }}"
        );
        let mut threads = Vec::new();
        let mut cursor = Value::Null;
        for page in 1..=MAX_PAGES {
            let response = self.query(
                "PullRequestReviewThreads",
                thread_query.clone(),
                [("cursor", cursor.clone())],
            )?;
            let connection = &response["data"]["repository"]["pullRequest"]["reviewThreads"];
            threads.extend(nodes(connection).filter_map(thread_from));
            match next_cursor(connection) {
                Some(next) if page < MAX_PAGES => cursor = Value::String(next),
                Some(_) => complete = false,
                None => break,
            }
        }
        Ok((
            PullRequestConversation {
                description: description.ok_or(GitHubError::InvalidResponse)?,
                comments,
                threads,
                complete,
                account: super::digest(&self.account)[..16].to_owned(),
            },
            if merged { MERGED_TTL } else { READ_TTL },
        ))
    }

    fn thread_replies(
        &self,
        thread: &str,
        after: &str,
    ) -> Result<PullRequestThreadReplies, GitHubError> {
        let query = format!(
            "query PullRequestThreadReplies($owner: String!, $name: String!, $number: Int!, $thread: ID!, $cursor: String) {{ repository(owner: $owner, name: $name) {{ pullRequest(number: $number) {{ id }} }} node(id: $thread) {{ ... on PullRequestReviewThread {{ pullRequest {{ id }} comments(first: 100, after: $cursor) {{ pageInfo {{ hasNextPage endCursor }} nodes {{ id body createdAt lastEditedAt url {ACTOR} {REACTIONS} }} }} }} }} }}"
        );
        let response = self.query(
            "PullRequestThreadReplies",
            query,
            [("thread", json!(thread)), ("cursor", json!(after))],
        )?;
        let data = &response["data"];
        // A thread id names any thread on GitHub; only this pull request's are read here.
        let owner = data["repository"]["pullRequest"]["id"].as_str();
        if owner.is_none() || data["node"]["pullRequest"]["id"].as_str() != owner {
            return Err(GitHubError::NotFound);
        }
        let connection = &data["node"]["comments"];
        Ok(PullRequestThreadReplies {
            comments: nodes(connection)
                .filter_map(|raw| comment_from(raw, None))
                .collect(),
            after: next_cursor(connection),
        })
    }

    fn viewed_files(&self) -> Result<(PullRequestViewedFiles, Option<String>), GitHubError> {
        let query = "query PullRequestViewedFiles($owner: String!, $name: String!, $number: Int!, $after: String) { repository(owner: $owner, name: $name) { pullRequest(number: $number) { id files(first: 100, after: $after) { pageInfo { hasNextPage endCursor } nodes { path viewerViewedState } } } } }";
        let read = graphql::pages(
            None,
            Some(VIEWED_PAGES),
            |after, _| {
                self.query(
                    "PullRequestViewedFiles",
                    query.to_owned(),
                    [("after", json!(after))],
                )
            },
            |page| next_cursor(&page["data"]["repository"]["pullRequest"]["files"]),
            |_| false,
        )?;
        let node_id = read.pages.first().and_then(|page| {
            page["data"]["repository"]["pullRequest"]["id"]
                .as_str()
                .map(str::to_owned)
        });
        if node_id.is_none() {
            return Err(GitHubError::NotFound);
        }
        let files = read
            .pages
            .iter()
            .flat_map(|page| nodes(&page["data"]["repository"]["pullRequest"]["files"]))
            .filter_map(|node| {
                let state = match node["viewerViewedState"].as_str()? {
                    "VIEWED" => PullRequestViewedState::Viewed,
                    "DISMISSED" => PullRequestViewedState::Dismissed,
                    _ => PullRequestViewedState::Unviewed,
                };
                Some((node["path"].as_str()?.to_owned(), state))
            })
            .collect();
        Ok((
            PullRequestViewedFiles {
                files,
                complete: !read.truncated,
            },
            node_id,
        ))
    }
}

impl PullRequestReads {
    pub fn new(api: Arc<GitHubApi>) -> Arc<Self> {
        Arc::new(Self {
            api,
            revisions: ReadCache::new(1024 * 1024),
            files: ReadCache::new(32 * 1024 * 1024),
            texts: ReadCache::new(16 * 1024 * 1024),
            conversations: ReadCache::new(16 * 1024 * 1024),
            replies: ReadCache::new(4 * 1024 * 1024),
            viewed: ReadCache::new(4 * 1024 * 1024),
            node_ids: Mutex::new(HashMap::new()),
        })
    }

    /// Captures the credential once, so every request of a read is the same account's.
    fn reader<'a>(&'a self, key: &'a PullRequestKey) -> Result<Reader<'a>, GitHubError> {
        let repository = Repository::from_key(key).ok_or(GitHubError::InvalidInput)?;
        let credential = self.api.credentials().get(&key.host)?;
        Ok(Reader {
            api: &self.api,
            key,
            repository,
            account: credential.fingerprint.clone(),
            options: RequestOptions {
                authentication: Authentication::Pinned(credential),
                interactive: true,
                ..Default::default()
            },
        })
    }

    /// The whole diff, or with `page` one page of the changed files once the whole diff was
    /// refused.
    pub fn files(
        &self,
        key: &PullRequestKey,
        page: Option<u32>,
    ) -> Result<Arc<PullRequestFiles>, GitHubError> {
        if page == Some(0) {
            return Err(GitHubError::InvalidInput);
        }
        let reader = self.reader(key)?;
        let revisions =
            self.revisions
                .read(reader.read_key("revisions"), || reader.revisions(), |_| 128)?;
        self.files.read(
            reader.read_key(format!("files {page:?}")),
            || {
                let files = match page {
                    None => reader.whole_diff(&revisions)?,
                    Some(page) => reader.files_page(&revisions, page)?,
                };
                Ok((files, READ_TTL))
            },
            |files| {
                files
                    .files
                    .iter()
                    .map(|file| {
                        file.path.len()
                            + match &file.patch {
                                PullRequestPatch::Hunks(hunks) => hunks.len(),
                                _ => 0,
                            }
                    })
                    .sum()
            },
        )
    }

    /// A file's text at a commit of the pull request's repository.
    pub fn file_text(
        &self,
        key: &PullRequestKey,
        revision: &str,
        path: &str,
    ) -> Result<Arc<PullRequestFileText>, GitHubError> {
        if !is_revision(revision) || !is_repository_path(path) {
            return Err(GitHubError::InvalidInput);
        }
        let reader = self.reader(key)?;
        self.texts.read(
            reader.read_key(format!("text {revision} {path}")),
            || Ok((reader.file_text(revision, path)?, TEXT_TTL)),
            |text| match text {
                PullRequestFileText::Text(text) => text.len(),
                _ => 0,
            },
        )
    }

    pub fn conversation(
        &self,
        key: &PullRequestKey,
    ) -> Result<Arc<PullRequestConversation>, GitHubError> {
        let reader = self.reader(key)?;
        self.conversations.read(
            reader.read_key("conversation"),
            || reader.conversation(),
            conversation_bytes,
        )
    }

    pub fn thread_replies(
        &self,
        key: &PullRequestKey,
        thread: &str,
        after: &str,
    ) -> Result<Arc<PullRequestThreadReplies>, GitHubError> {
        let reader = self.reader(key)?;
        self.replies.read(
            reader.read_key(format!("replies {thread} {after}")),
            || Ok((reader.thread_replies(thread, after)?, READ_TTL)),
            |replies| replies.comments.iter().map(comment_bytes).sum(),
        )
    }

    pub fn viewed_files(
        &self,
        key: &PullRequestKey,
    ) -> Result<Arc<PullRequestViewedFiles>, GitHubError> {
        let reader = self.reader(key)?;
        self.viewed.read(
            reader.read_key("viewed"),
            || {
                let (files, node_id) = reader.viewed_files()?;
                if let Some(node_id) = node_id {
                    self.node_ids
                        .lock()
                        .unwrap()
                        .insert((key.clone(), reader.account.clone()), node_id);
                }
                Ok((files, VIEWED_TTL))
            },
            |viewed| viewed.files.iter().map(|(path, _)| path.len() + 8).sum(),
        )
    }

    /// Marks or unmarks files as viewed in one request; the viewed files are read again after,
    /// whether it succeeded or not.
    pub fn set_viewed(
        &self,
        key: &PullRequestKey,
        paths: &[String],
        viewed: bool,
    ) -> Result<(), GitHubError> {
        if paths.is_empty() {
            return Ok(());
        }
        let reader = self.reader(key)?;
        self.viewed.invalidate(key);
        let result = self.mark(&reader, paths, viewed);
        self.viewed.invalidate(key);
        result
    }

    fn mark(&self, reader: &Reader<'_>, paths: &[String], viewed: bool) -> Result<(), GitHubError> {
        let cached = self
            .node_ids
            .lock()
            .unwrap()
            .get(&(reader.key.clone(), reader.account.clone()))
            .cloned();
        let node_id = match cached {
            Some(node_id) => node_id,
            None => reader
                .query(
                    "PullRequestNodeId",
                    "query PullRequestNodeId($owner: String!, $name: String!, $number: Int!) { repository(owner: $owner, name: $name) { pullRequest(number: $number) { id } } }".into(),
                    [],
                )?["data"]["repository"]["pullRequest"]["id"]
                .as_str()
                .ok_or(GitHubError::NotFound)?
                .to_owned(),
        };
        let items: Vec<_> = paths
            .iter()
            .enumerate()
            .map(|(index, path)| AliasItem {
                key: index,
                variables: Variables::from([("path".into(), ("String!".into(), json!(path)))]),
            })
            .collect();
        let field = if viewed {
            "markFileAsViewed"
        } else {
            "unmarkFileAsViewed"
        };
        let document = graphql::aliases(
            "mutation",
            "SetPullRequestFilesViewed",
            "f",
            &items,
            &Variables::from([("pullRequestId".into(), ("ID!".into(), json!(node_id)))]),
            |v| {
                format!(
                    "{field}(input: {{ pullRequestId: $pullRequestId, path: {} }}) {{ clientMutationId }}",
                    v["path"]
                )
            },
            |fields| fields,
        )
        .ok_or(GitHubError::InvalidInput)?;
        self.api.graphql(
            &reader.key.host,
            &document,
            &RequestOptions {
                operation: "SetPullRequestFilesViewed",
                ..reader.options.clone()
            },
        )?;
        Ok(())
    }

    /// Media is read only where the pull request's conversation points at it, so this is not a
    /// way to read anything else the account can see.
    pub fn media(
        &self,
        key: &PullRequestKey,
        url: &str,
        validator: Option<&str>,
    ) -> Result<PullRequestMedia, GitHubError> {
        let conversation = self.conversation(key)?;
        let mentioned = conversation_bodies(&conversation).any(|body| body.contains(url))
            || self
                .replies
                .held(key)
                .iter()
                .flat_map(|replies| replies.comments.iter())
                .any(|comment| comment.body.contains(url));
        if !mentioned {
            return Err(GitHubError::InvalidInput);
        }
        media::fetch(&self.api, url, validator)
    }

    /// Drops every answer about the pull request, for a change the sync observed.
    pub fn invalidate(&self, key: &PullRequestKey) {
        self.revisions.invalidate(key);
        self.files.invalidate(key);
        self.conversations.invalidate(key);
        self.replies.invalidate(key);
        self.viewed.invalidate(key);
    }
}

fn conversation_bodies(conversation: &PullRequestConversation) -> impl Iterator<Item = &str> {
    std::iter::once(&conversation.description)
        .chain(&conversation.comments)
        .chain(
            conversation
                .threads
                .iter()
                .flat_map(|thread| thread.comments.iter()),
        )
        .map(|comment| comment.body.as_str())
}

fn comment_bytes(comment: &PullRequestComment) -> usize {
    comment.body.len() + comment.id.len() + 256
}

fn conversation_bytes(conversation: &PullRequestConversation) -> usize {
    std::iter::once(&conversation.description)
        .chain(&conversation.comments)
        .chain(
            conversation
                .threads
                .iter()
                .flat_map(|thread| thread.comments.iter()),
        )
        .map(comment_bytes)
        .sum()
}

fn is_revision(value: &str) -> bool {
    matches!(value.len(), 40 | 64)
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// A path the contents endpoint reads as a file in the repository; dot segments would leave it.
fn is_repository_path(path: &str) -> bool {
    !path.is_empty()
        && path
            .split('/')
            .all(|segment| !matches!(segment, "" | "." | ".."))
}

fn percent_encode(segment: &str) -> String {
    segment
        .bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
                (byte as char).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect()
}

fn nodes(connection: &Value) -> impl Iterator<Item = &Value> {
    connection["nodes"].as_array().into_iter().flatten()
}

fn next_cursor(connection: &Value) -> Option<String> {
    connection["pageInfo"]["endCursor"]
        .as_str()
        .filter(|_| connection["pageInfo"]["hasNextPage"].as_bool() == Some(true))
        .map(str::to_owned)
}

fn comment_from(
    raw: &Value,
    review_state: Option<PullRequestReviewState>,
) -> Option<PullRequestComment> {
    let text = |field: &str| raw[field].as_str().map(str::to_owned);
    Some(PullRequestComment {
        id: text("id")?,
        author: raw["author"]["login"]
            .as_str()
            .map(|login| PullRequestActor {
                login: login.to_owned(),
                avatar_url: raw["author"]["avatarUrl"].as_str().map(str::to_owned),
            }),
        body: text("body").unwrap_or_default(),
        created_at: text("submittedAt").or_else(|| text("createdAt"))?,
        edited_at: text("lastEditedAt"),
        url: text("url").filter(|url| !url.is_empty()),
        review_state,
        reactions: raw["reactionGroups"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|group| {
                let count = group["reactors"]["totalCount"].as_u64()?;
                (count > 0).then(|| PullRequestReaction {
                    content: group["content"].as_str().unwrap_or_default().to_owned(),
                    count,
                    viewer_reacted: group["viewerHasReacted"].as_bool().unwrap_or(false),
                })
            })
            .collect(),
    })
}

/// A review with no body is kept only when its state is the event itself. GitHub also opens a
/// bodiless `COMMENTED` review around line comments, which the review threads carry.
fn review_from(raw: &Value) -> Option<PullRequestComment> {
    let state = match raw["state"].as_str()? {
        "APPROVED" => PullRequestReviewState::Approved,
        "CHANGES_REQUESTED" => PullRequestReviewState::ChangesRequested,
        "DISMISSED" => PullRequestReviewState::Dismissed,
        "PENDING" => PullRequestReviewState::Pending,
        _ => PullRequestReviewState::Commented,
    };
    if raw["body"].as_str().unwrap_or_default().trim().is_empty()
        && matches!(
            state,
            PullRequestReviewState::Commented | PullRequestReviewState::Pending
        )
    {
        return None;
    }
    comment_from(raw, Some(state))
}

fn thread_from(raw: &Value) -> Option<PullRequestReviewThread> {
    let path = raw["path"].as_str()?.to_owned();
    let comments: Vec<_> = nodes(&raw["comments"]).collect();
    let side = |field: &str| match raw[field].as_str() {
        Some("LEFT") => Some(ReviewSide::Old),
        Some("RIGHT") => Some(ReviewSide::New),
        _ => None,
    };
    let line = |field: &str| {
        raw[field]
            .as_u64()
            .and_then(|line| u32::try_from(line).ok())
    };
    // A current thread sits on the lines of the commit its comments are positioned on; an
    // outdated one keeps the lines as they were first commented on.
    let (end, start, commit) = match line("line") {
        Some(end) => (Some(end), line("startLine"), "commit"),
        None => (
            line("originalLine"),
            line("originalStartLine"),
            "originalCommit",
        ),
    };
    let anchor = end.and_then(|end_line| {
        Some(PullRequestReviewAnchor {
            revision: comments.first()?[commit]["oid"].as_str()?.to_owned(),
            path: path.clone(),
            side: side("diffSide")?,
            start_line: start.unwrap_or(end_line).min(end_line),
            end_line,
        })
    });
    Some(PullRequestReviewThread {
        id: raw["id"].as_str()?.to_owned(),
        path,
        resolved: raw["isResolved"].as_bool().unwrap_or(false),
        outdated: raw["isOutdated"].as_bool().unwrap_or(false),
        anchor,
        comments: comments
            .into_iter()
            .filter_map(|raw| comment_from(raw, None))
            .collect(),
        total_comments: raw["comments"]["totalCount"].as_u64().unwrap_or(0),
        replies_after: next_cursor(&raw["comments"]),
    })
}

fn listed_file(row: &Value) -> Option<PullRequestFile> {
    let path = row["filename"].as_str()?.to_owned();
    let status = row["status"].as_str()?;
    let kind = match status {
        "added" | "copied" => FileChangeKind::Create,
        "removed" => FileChangeKind::Delete,
        "renamed" => FileChangeKind::Rename,
        _ => FileChangeKind::Modify,
    };
    let additions = row["additions"].as_u64().unwrap_or(0);
    let deletions = row["deletions"].as_u64().unwrap_or(0);
    // The files API gives no reason for a missing patch: changed lines without one were
    // withheld for size, and a file with none is binary unless it is empty or only renamed.
    let patch = match row["patch"].as_str().filter(|patch| !patch.is_empty()) {
        Some(patch) => PullRequestPatch::Hunks(patch.to_owned()),
        None if additions + deletions > 0 => PullRequestPatch::Oversized,
        None if kind == FileChangeKind::Rename || row["sha"].as_str() == Some(EMPTY_BLOB) => {
            PullRequestPatch::Hunks(String::new())
        }
        None => PullRequestPatch::Binary,
    };
    Some(PullRequestFile {
        previous_path: matches!(status, "renamed" | "copied")
            .then(|| row["previous_filename"].as_str().map(str::to_owned))
            .flatten(),
        path,
        kind,
        additions,
        deletions,
        patch,
    })
}

fn diff_files(diff: &str) -> Option<Vec<PullRequestFile>> {
    agent::file_changes_from_unified_diff(diff)
        .ok()?
        .into_iter()
        .map(|change| {
            let section = change.diff.unwrap_or_default();
            let mut previous_path = None;
            let mut binary = false;
            let mut hunks_at = None;
            let mut offset = 0;
            for raw in section.split_inclusive('\n') {
                if raw.starts_with("@@") {
                    hunks_at = Some(offset);
                    break;
                }
                offset += raw.len();
                let line = raw.trim_end_matches('\n');
                if let Some(path) = line.strip_prefix("rename from ") {
                    previous_path = Some(path.to_owned());
                }
                binary |= line.starts_with("Binary files ") || line == "GIT binary patch";
            }
            let hunks = hunks_at.map_or("", |at| &section[at..]);
            let count = |sign: char| {
                hunks
                    .lines()
                    .filter(|line| line.starts_with(sign) && !line.starts_with("@@"))
                    .count() as u64
            };
            Some(PullRequestFile {
                previous_path: previous_path.filter(|_| change.kind == FileChangeKind::Rename),
                path: change.path,
                kind: change.kind,
                additions: count('+'),
                deletions: count('-'),
                patch: if binary {
                    PullRequestPatch::Binary
                } else {
                    PullRequestPatch::Hunks(hunks.to_owned())
                },
            })
        })
        .collect()
}
