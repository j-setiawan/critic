//! Pull request comment fetch and thread organization.

use crate::{
    domain::{
        IssueComment, PullRequestComment, PullRequestData, PullRequestSummary, PullReviewSummary,
        ReviewComment, ReviewThread,
    },
    github::errors::format_octocrab_error,
};
use octocrab::models::{CommentId, pulls};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use thiserror::Error;

/// Result type for pull request comment loading.
pub type Result<T> = std::result::Result<T, PullRequestCommentsError>;

/// Inline review comment payload staged before review submission.
#[derive(Debug, Clone)]
pub struct SubmitReviewComment {
    pub path: String,
    pub body: String,
    pub line: u64,
    pub side: pulls::Side,
    pub start_line: Option<u64>,
    pub start_side: Option<pulls::Side>,
}

/// Request payload for submitting a pull request review.
#[derive(Debug, Clone)]
pub struct SubmitPullRequestReviewRequest<'a> {
    pub owner: &'a str,
    pub repo: &'a str,
    pub pull_number: u64,
    pub event: &'a str,
    pub body: &'a str,
    pub comments: &'a [SubmitReviewComment],
    pub expected_head_sha: &'a str,
}

/// Errors for pull request comment loading and transformation.
#[derive(Debug, Error)]
pub enum PullRequestCommentsError {
    #[error("GitHub API request failed: {0}")]
    Octocrab(String),
    #[error("graphql response error: {0}")]
    GraphQlResponseError(String),
    #[error(
        "pull request was updated since last refresh (loaded {loaded_head_sha}, current {current_head_sha}); refresh before submitting"
    )]
    PullRequestUpdated {
        loaded_head_sha: String,
        current_head_sha: String,
    },
    #[error("unsupported review event: {0}")]
    InvalidReviewEvent(String),
    #[error("review event {event} produced unexpected state {state}")]
    UnexpectedReviewState { event: String, state: String },
}

impl From<octocrab::Error> for PullRequestCommentsError {
    fn from(error: octocrab::Error) -> Self {
        Self::Octocrab(format_octocrab_error(error))
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphQlPageInfo {
    has_next_page: bool,
    end_cursor: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphQlCommentNode {
    database_id: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphQlReviewComments {
    nodes: Vec<GraphQlCommentNode>,
    page_info: GraphQlPageInfo,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphQlReviewThreadNode {
    id: String,
    is_resolved: bool,
    comments: GraphQlReviewComments,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphQlReviewThreads {
    nodes: Vec<GraphQlReviewThreadNode>,
    page_info: GraphQlPageInfo,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphQlPullRequest {
    review_threads: GraphQlReviewThreads,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphQlRepository {
    pull_request: Option<GraphQlPullRequest>,
}

#[derive(Debug, Deserialize)]
struct GraphQlData {
    repository: Option<GraphQlRepository>,
}

#[derive(Debug, Deserialize)]
struct GraphQlResponse {
    data: Option<GraphQlData>,
    errors: Option<Vec<GraphQlError>>,
}

#[derive(Debug, Deserialize)]
struct GraphQlError {
    message: String,
}

const REVIEW_THREADS_RESOLUTION_QUERY: &str = r#"
query PullRequestReviewThreadResolution($owner: String!, $repo: String!, $pullNumber: Int!, $after: String) {
  repository(owner: $owner, name: $repo) {
    pullRequest(number: $pullNumber) {
      reviewThreads(first: 100, after: $after) {
        pageInfo {
          hasNextPage
          endCursor
        }
        nodes {
          id
          isResolved
          comments(first: 100) {
            pageInfo {
              hasNextPage
              endCursor
            }
            nodes {
              databaseId
            }
          }
        }
      }
    }
  }
}
"#;

const REVIEW_THREAD_COMMENTS_QUERY: &str = r#"
query PullRequestReviewThreadComments($threadId: ID!, $after: String) {
  node(id: $threadId) {
    ... on PullRequestReviewThread {
      comments(first: 100, after: $after) {
        pageInfo {
          hasNextPage
          endCursor
        }
        nodes {
          databaseId
        }
      }
    }
  }
}
"#;

/// Appends resolution fallback details to a debug log in the temp directory,
/// since the TUI owns the terminal and stderr is not visible while it runs.
fn log_resolution_fallback(
    owner: &str,
    repo: &str,
    pull_number: u64,
    error: &PullRequestCommentsError,
) -> String {
    use std::io::Write;

    let unix_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default();
    let path = std::env::temp_dir().join("critic-debug.log");
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = writeln!(
            file,
            "[{unix_secs}] {owner}/{repo}#{pull_number}: review thread resolution unavailable, \
             opening without resolved state: {error}"
        );
    }

    path.display().to_string()
}

/// Parses one page of the review thread resolution query, reporting GraphQL
/// errors and pinpointing which level of the response was null or malformed.
fn parse_review_threads_page(raw: serde_json::Value) -> Result<GraphQlReviewThreads> {
    let response: GraphQlResponse = serde_json::from_value(raw.clone()).map_err(|error| {
        PullRequestCommentsError::GraphQlResponseError(format!(
            "unexpected review thread response shape ({error}); {}",
            truncated_response_body(&raw)
        ))
    })?;

    if let Some(errors) = response.errors
        && !errors.is_empty()
    {
        let message = errors
            .into_iter()
            .map(|error| error.message)
            .collect::<Vec<_>>()
            .join("; ");
        return Err(PullRequestCommentsError::GraphQlResponseError(message));
    }

    response
        .data
        .and_then(|data| data.repository)
        .and_then(|repository| repository.pull_request)
        .map(|pull_request| pull_request.review_threads)
        .ok_or_else(|| {
            PullRequestCommentsError::GraphQlResponseError(format!(
                "missing review thread data in GraphQL response: {}; {}",
                missing_review_thread_level(&raw),
                truncated_response_body(&raw)
            ))
        })
}

/// Names the first null/absent level in the `data.repository.pullRequest` chain.
fn missing_review_thread_level(raw: &serde_json::Value) -> &'static str {
    let data = raw.get("data").filter(|value| !value.is_null());
    let Some(data) = data else {
        return "`data` is null or absent (possible rate limit or transient API failure)";
    };

    let repository = data.get("repository").filter(|value| !value.is_null());
    if repository.is_none() {
        return "`data.repository` is null (the token may not be able to see this repository)";
    }

    "`data.repository.pullRequest` is null (the token may not be able to see this pull request)"
}

/// Returns a bounded snippet of the raw response body for error reports.
fn truncated_response_body(raw: &serde_json::Value) -> String {
    const MAX_LEN: usize = 600;
    let mut body = raw.to_string();
    if body.len() > MAX_LEN {
        let mut end = MAX_LEN;
        while !body.is_char_boundary(end) {
            end -= 1;
        }
        body.truncate(end);
        body.push('…');
    }
    format!("raw response (truncated): {body}")
}

#[derive(Debug)]
struct BuildNode {
    comment: ReviewComment,
    replies: Vec<u64>,
}

/// Fetches all comment data for a pull request and returns merged review/issue entries.
pub async fn fetch_pull_request_data(
    client: &octocrab::Octocrab,
    pull: &PullRequestSummary,
) -> Result<PullRequestData> {
    let (
        changed_files_set,
        (review_threads, load_warning),
        issue_comments,
        review_summaries,
        pull_state,
    ) = tokio::try_join!(
        pull_request_file_paths(client, &pull.owner, &pull.repo, pull.number),
        list_review_comment_threads(client, &pull.owner, &pull.repo, pull.number),
        list_issue_comments(client, &pull.owner, &pull.repo, pull.number),
        list_pull_review_summary_comments(client, &pull.owner, &pull.repo, pull.number),
        async {
            client
                .pulls(&pull.owner, &pull.repo)
                .get(pull.number)
                .await
                .map_err(PullRequestCommentsError::from)
        },
    )?;

    let mut changed_files: Vec<String> = changed_files_set.into_iter().collect();
    changed_files.sort();

    let mut merged: Vec<(i64, PullRequestComment)> = review_threads
        .into_iter()
        .map(|thread| {
            (
                thread.comment.created_at.timestamp_millis(),
                PullRequestComment::ReviewThread(Box::new(thread)),
            )
        })
        .chain(issue_comments.into_iter().map(|comment| {
            (
                comment.created_at.timestamp_millis(),
                PullRequestComment::IssueComment(Box::new(comment)),
            )
        }))
        .chain(review_summaries.into_iter().map(|review| {
            (
                review
                    .submitted_at
                    .map(|value| value.timestamp_millis())
                    .unwrap_or_default(),
                PullRequestComment::ReviewSummary(Box::new(review)),
            )
        }))
        .collect();

    merged.sort_by_key(|entry| entry.0);

    Ok(PullRequestData {
        head_ref: pull_state.head.ref_field,
        base_ref: pull_state.base.ref_field,
        head_sha: pull_state.head.sha,
        base_sha: pull_state.base.sha,
        changed_files,
        comments: merged.into_iter().map(|(_, entry)| entry).collect(),
        load_warning,
    })
}

/// Replies to an existing review comment.
pub async fn reply_to_review_comment(
    client: &octocrab::Octocrab,
    owner: &str,
    repo: &str,
    pull_number: u64,
    comment_id: u64,
    body: &str,
) -> Result<()> {
    client
        .pulls(owner, repo)
        .reply_to_comment(pull_number, CommentId(comment_id), body.to_owned())
        .await?;

    Ok(())
}

/// Resolves or unresolves a review thread by GraphQL thread id.
pub async fn set_review_thread_resolved(
    client: &octocrab::Octocrab,
    thread_id: &str,
    resolved: bool,
) -> Result<()> {
    let query = if resolved {
        r#"
mutation ResolveReviewThread($threadId: ID!) {
  resolveReviewThread(input: {threadId: $threadId}) {
    thread { id }
  }
}
"#
    } else {
        r#"
mutation UnresolveReviewThread($threadId: ID!) {
  unresolveReviewThread(input: {threadId: $threadId}) {
    thread { id }
  }
}
"#
    };

    let response: serde_json::Value = client
        .graphql(&serde_json::json!({
            "query": query,
            "variables": {
                "threadId": thread_id,
            }
        }))
        .await?;

    if let Some(errors) = response.get("errors").and_then(|value| value.as_array())
        && !errors.is_empty()
    {
        let message = errors
            .iter()
            .filter_map(|value| value.get("message").and_then(|message| message.as_str()))
            .collect::<Vec<_>>()
            .join("; ");
        return Err(PullRequestCommentsError::GraphQlResponseError(message));
    }

    Ok(())
}

/// Submits a pull request review with `COMMENT`, `APPROVE`, or `REQUEST_CHANGES`.
pub async fn submit_pull_request_review(
    client: &octocrab::Octocrab,
    request: SubmitPullRequestReviewRequest<'_>,
) -> Result<()> {
    let SubmitPullRequestReviewRequest {
        owner,
        repo,
        pull_number,
        event,
        body,
        comments,
        expected_head_sha,
    } = request;

    let event = match event {
        "COMMENT" => pulls::ReviewAction::Comment,
        "APPROVE" => pulls::ReviewAction::Approve,
        "REQUEST_CHANGES" => pulls::ReviewAction::RequestChanges,
        other => {
            return Err(PullRequestCommentsError::InvalidReviewEvent(
                other.to_owned(),
            ));
        }
    };

    let pull = client.pulls(owner, repo).get(pull_number).await?;
    if pull.head.sha != expected_head_sha {
        return Err(PullRequestCommentsError::PullRequestUpdated {
            loaded_head_sha: expected_head_sha.to_owned(),
            current_head_sha: pull.head.sha,
        });
    }

    let route = format!("/repos/{owner}/{repo}/pulls/{pull_number}/reviews");
    let comments = comments
        .iter()
        .map(SubmitReviewCommentRequest::from)
        .collect::<Vec<_>>();
    let review: pulls::Review = client
        .post(
            route,
            Some(&serde_json::json!({
                "body": body,
                "event": event,
                "commit_id": expected_head_sha,
                "comments": comments,
            })),
        )
        .await?;

    let expected_state = match event {
        pulls::ReviewAction::Comment => Some(pulls::ReviewState::Commented),
        pulls::ReviewAction::Approve => Some(pulls::ReviewState::Approved),
        pulls::ReviewAction::RequestChanges => Some(pulls::ReviewState::ChangesRequested),
        _ => None,
    };

    if let Some(expected) = expected_state
        && review.state != Some(expected)
    {
        return Err(PullRequestCommentsError::UnexpectedReviewState {
            event: format!("{event:?}"),
            state: format!("{:?}", review.state),
        });
    }

    Ok(())
}

#[derive(Debug, Clone, Serialize)]
struct SubmitReviewCommentRequest {
    path: String,
    body: String,
    line: u64,
    side: pulls::Side,
    #[serde(skip_serializing_if = "Option::is_none")]
    start_line: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    start_side: Option<pulls::Side>,
}

impl From<&SubmitReviewComment> for SubmitReviewCommentRequest {
    fn from(value: &SubmitReviewComment) -> Self {
        Self {
            path: value.path.clone(),
            body: value.body.clone(),
            line: value.line,
            side: value.side,
            start_line: value.start_line,
            start_side: value.start_side,
        }
    }
}

async fn list_review_comment_threads(
    client: &octocrab::Octocrab,
    owner: &str,
    repo: &str,
    pull_number: u64,
) -> Result<(Vec<ReviewThread>, Option<String>)> {
    let first_page = client
        .pulls(owner, repo)
        .list_comments(Some(pull_number))
        .per_page(100)
        .send()
        .await?;
    let mapped = client.all_pages(first_page).await?;

    // Resolution state is a decoration on top of the REST comment data. If the
    // GraphQL resolution query fails, still open the pull request: fall back to
    // an empty map so every thread renders (as unresolved) instead of failing
    // the whole load. A concise warning is surfaced in the header; full details
    // are appended to a debug log for bug reports.
    let (resolved_by_comment_id, resolution_warning) =
        match review_thread_resolution_map(client, owner, repo, pull_number).await {
            Ok(map) => (map, None),
            Err(error) => {
                let log_path = log_resolution_fallback(owner, repo, pull_number, &error);
                let warning = format!(
                    "review thread resolved state unavailable; showing all threads as \
                     unresolved (details: {log_path})"
                );
                (HashMap::new(), Some(warning))
            }
        };
    let resolution_available = resolution_warning.is_none();

    let mut threads = build_review_threads(mapped);
    for thread in &mut threads {
        apply_thread_resolution(thread, &resolved_by_comment_id);
        // Only apply the "reply-less thread counts as resolved" heuristic when
        // resolution data actually loaded; otherwise it would hide every
        // single-comment thread behind the resolved filter.
        if resolution_available && thread.thread_id.is_none() && thread.replies.is_empty() {
            set_thread_resolved_without_id(thread);
        }
    }

    Ok((threads, resolution_warning))
}

async fn list_issue_comments(
    client: &octocrab::Octocrab,
    owner: &str,
    repo: &str,
    pull_number: u64,
) -> Result<Vec<IssueComment>> {
    let first_page = client
        .issues(owner, repo)
        .list_comments(pull_number)
        .per_page(100)
        .send()
        .await?;
    client.all_pages(first_page).await.map_err(Into::into)
}

async fn list_pull_review_summary_comments(
    client: &octocrab::Octocrab,
    owner: &str,
    repo: &str,
    pull_number: u64,
) -> Result<Vec<PullReviewSummary>> {
    let first_page = client
        .pulls(owner, repo)
        .list_reviews(pull_number)
        .per_page(100)
        .send()
        .await?;
    let reviews = client.all_pages(first_page).await?;

    Ok(reviews
        .into_iter()
        .filter(|review| {
            review
                .body
                .as_deref()
                .is_some_and(|body| !body.trim().is_empty())
        })
        .collect())
}

fn build_review_threads(comments: Vec<ReviewComment>) -> Vec<ReviewThread> {
    let mut nodes: HashMap<u64, BuildNode> = HashMap::with_capacity(comments.len());
    let mut parent_links: Vec<(u64, Option<u64>)> = Vec::with_capacity(comments.len());

    for comment in comments {
        let id = comment.id.into_inner();
        let parent_id = comment.in_reply_to_id.map(|parent| parent.into_inner());
        nodes.insert(
            id,
            BuildNode {
                comment,
                replies: Vec::new(),
            },
        );
        parent_links.push((id, parent_id));
    }

    let mut root_ids = Vec::new();
    for (id, parent_id) in parent_links {
        if let Some(parent_id) = parent_id {
            if let Some(parent) = nodes.get_mut(&parent_id) {
                parent.replies.push(id);
            } else {
                root_ids.push(id);
            }
        } else {
            root_ids.push(id);
        }
    }

    root_ids
        .into_iter()
        .filter_map(|id| materialize_thread(id, &mut nodes))
        .collect()
}

fn materialize_thread(id: u64, nodes: &mut HashMap<u64, BuildNode>) -> Option<ReviewThread> {
    let node = nodes.remove(&id)?;
    let replies = node
        .replies
        .iter()
        .copied()
        .filter_map(|reply_id| materialize_thread(reply_id, nodes))
        .collect();

    Some(ReviewThread {
        thread_id: None,
        is_resolved: false,
        comment: node.comment,
        replies,
    })
}

fn apply_thread_resolution(
    thread: &mut ReviewThread,
    resolved_by_comment_id: &HashMap<u64, (bool, String)>,
) {
    if let Some((is_resolved, thread_id)) = find_thread_resolution(thread, resolved_by_comment_id) {
        set_thread_resolution(thread, is_resolved, &thread_id);
    }
}

fn find_thread_resolution(
    thread: &ReviewThread,
    resolved_by_comment_id: &HashMap<u64, (bool, String)>,
) -> Option<(bool, String)> {
    if let Some((is_resolved, thread_id)) =
        resolved_by_comment_id.get(&thread.comment.id.into_inner())
    {
        return Some((*is_resolved, thread_id.clone()));
    }

    for reply in &thread.replies {
        if let Some(found) = find_thread_resolution(reply, resolved_by_comment_id) {
            return Some(found);
        }
    }

    None
}

fn set_thread_resolution(thread: &mut ReviewThread, is_resolved: bool, thread_id: &str) {
    thread.is_resolved = is_resolved;
    thread.thread_id = Some(thread_id.to_owned());
    for reply in &mut thread.replies {
        set_thread_resolution(reply, is_resolved, thread_id);
    }
}

fn set_thread_resolved_without_id(thread: &mut ReviewThread) {
    thread.is_resolved = true;
    for reply in &mut thread.replies {
        set_thread_resolved_without_id(reply);
    }
}

async fn review_thread_resolution_map(
    client: &octocrab::Octocrab,
    owner: &str,
    repo: &str,
    pull_number: u64,
) -> Result<HashMap<u64, (bool, String)>> {
    let mut after: Option<String> = None;
    let mut resolved_by_comment_id = HashMap::new();

    loop {
        let raw: serde_json::Value = client
            .graphql(&serde_json::json!({
                "query": REVIEW_THREADS_RESOLUTION_QUERY,
                "variables": {
                    "owner": owner,
                    "repo": repo,
                    "pullNumber": pull_number,
                    "after": after,
                }
            }))
            .await?;

        let review_threads = parse_review_threads_page(raw)?;

        for thread in &review_threads.nodes {
            let mut comment_ids = thread
                .comments
                .nodes
                .iter()
                .filter_map(|comment| comment.database_id)
                .collect::<Vec<_>>();

            if thread.comments.page_info.has_next_page {
                comment_ids.extend(
                    fetch_remaining_thread_comment_ids(
                        client,
                        &thread.id,
                        thread.comments.page_info.end_cursor.clone(),
                    )
                    .await?,
                );
            }

            for comment_id in comment_ids {
                resolved_by_comment_id.insert(comment_id, (thread.is_resolved, thread.id.clone()));
            }
        }

        if !review_threads.page_info.has_next_page {
            break;
        }

        after = review_threads.page_info.end_cursor;
    }

    Ok(resolved_by_comment_id)
}

async fn fetch_remaining_thread_comment_ids(
    client: &octocrab::Octocrab,
    thread_id: &str,
    mut after: Option<String>,
) -> Result<Vec<u64>> {
    let mut comment_ids = Vec::new();

    while let Some(cursor) = after {
        let response: serde_json::Value = client
            .graphql(&serde_json::json!({
                "query": REVIEW_THREAD_COMMENTS_QUERY,
                "variables": {
                    "threadId": thread_id,
                    "after": cursor,
                }
            }))
            .await?;

        if let Some(errors) = response.get("errors").and_then(|value| value.as_array())
            && !errors.is_empty()
        {
            let message = errors
                .iter()
                .filter_map(|value| value.get("message").and_then(|message| message.as_str()))
                .collect::<Vec<_>>()
                .join("; ");
            return Err(PullRequestCommentsError::GraphQlResponseError(message));
        }

        let comments = response
            .get("data")
            .and_then(|value| value.get("node"))
            .and_then(|value| value.get("comments"))
            .ok_or_else(|| {
                PullRequestCommentsError::GraphQlResponseError(
                    "missing review thread comments data in GraphQL response".to_owned(),
                )
            })?;

        if let Some(nodes) = comments.get("nodes").and_then(|value| value.as_array()) {
            comment_ids.extend(
                nodes
                    .iter()
                    .filter_map(|node| node.get("databaseId").and_then(|id| id.as_u64())),
            );
        }

        let page_info = comments.get("pageInfo").ok_or_else(|| {
            PullRequestCommentsError::GraphQlResponseError(
                "missing review thread comments page info in GraphQL response".to_owned(),
            )
        })?;
        let has_next_page = page_info
            .get("hasNextPage")
            .and_then(|value| value.as_bool())
            .unwrap_or(false);
        if has_next_page {
            after = page_info
                .get("endCursor")
                .and_then(|value| value.as_str())
                .map(str::to_owned);
        } else {
            break;
        }
    }

    Ok(comment_ids)
}

async fn pull_request_file_paths(
    client: &octocrab::Octocrab,
    owner: &str,
    repo: &str,
    pull_number: u64,
) -> Result<HashSet<String>> {
    let first_page = client.pulls(owner, repo).list_files(pull_number).await?;
    let files = client.all_pages(first_page).await?;

    Ok(files.into_iter().map(|file| file.filename).collect())
}

#[cfg(test)]
mod tests {
    use super::{ReviewComment, build_review_threads, parse_review_threads_page};
    use serde_json::json;

    fn review_comment(id: u64, in_reply_to_id: Option<u64>) -> ReviewComment {
        let mut payload = json!({
            "url": format!("https://example.invalid/comments/{id}"),
            "id": id,
            "node_id": format!("PRRC_{id}"),
            "diff_hunk": "@@ -1,1 +1,1 @@",
            "path": "src/lib.rs",
            "commit_id": "deadbeef",
            "original_commit_id": "deadbeef",
            "body": format!("comment {id}"),
            "created_at": "2026-02-01T00:00:00Z",
            "updated_at": "2026-02-01T00:00:00Z",
            "html_url": format!("https://example.invalid/comments/{id}"),
            "_links": {},
            "line": 1
        });

        if let Some(reply_to) = in_reply_to_id {
            payload["in_reply_to_id"] = json!(reply_to);
        }

        serde_json::from_value(payload).expect("valid pull review comment fixture")
    }

    #[test]
    fn builds_reply_under_parent() {
        let comments = vec![review_comment(1, None), review_comment(2, Some(1))];
        let threads = build_review_threads(comments);

        assert_eq!(threads.len(), 1);
        assert_eq!(threads[0].comment.id.into_inner(), 1);
        assert_eq!(threads[0].replies.len(), 1);
        assert_eq!(threads[0].replies[0].comment.id.into_inner(), 2);
    }

    #[test]
    fn orphan_reply_becomes_root() {
        let comments = vec![review_comment(5, Some(999))];
        let threads = build_review_threads(comments);

        assert_eq!(threads.len(), 1);
        assert_eq!(threads[0].comment.id.into_inner(), 5);
    }

    #[test]
    fn parses_healthy_review_threads_page() {
        let raw = json!({
            "data": {
                "repository": {
                    "pullRequest": {
                        "reviewThreads": {
                            "pageInfo": {"hasNextPage": false, "endCursor": null},
                            "nodes": [{
                                "id": "PRRT_abc",
                                "isResolved": true,
                                "comments": {
                                    "pageInfo": {"hasNextPage": false, "endCursor": null},
                                    "nodes": [{"databaseId": 42}, {"databaseId": null}]
                                }
                            }]
                        }
                    }
                }
            }
        });

        let page = parse_review_threads_page(raw).expect("healthy page parses");
        assert_eq!(page.nodes.len(), 1);
        assert_eq!(page.nodes[0].id, "PRRT_abc");
        assert!(page.nodes[0].is_resolved);
        assert_eq!(page.nodes[0].comments.nodes[0].database_id, Some(42));
        assert_eq!(page.nodes[0].comments.nodes[1].database_id, None);
    }

    #[test]
    fn reports_graphql_errors_before_missing_data() {
        let raw = json!({
            "data": null,
            "errors": [{"message": "API rate limit exceeded"}]
        });

        let error = parse_review_threads_page(raw).expect_err("errors are surfaced");
        assert!(error.to_string().contains("API rate limit exceeded"));
    }

    #[test]
    fn pinpoints_null_data() {
        let raw = json!({"data": null});
        let error = parse_review_threads_page(raw).expect_err("null data is an error");
        let message = error.to_string();
        assert!(message.contains("`data` is null"), "got: {message}");
        assert!(message.contains("raw response"), "got: {message}");
    }

    #[test]
    fn pinpoints_null_repository() {
        let raw = json!({"data": {"repository": null}});
        let error = parse_review_threads_page(raw).expect_err("null repository is an error");
        assert!(
            error.to_string().contains("`data.repository` is null"),
            "got: {error}"
        );
    }

    #[test]
    fn pinpoints_null_pull_request() {
        let raw = json!({"data": {"repository": {"pullRequest": null}}});
        let error = parse_review_threads_page(raw).expect_err("null pullRequest is an error");
        assert!(
            error
                .to_string()
                .contains("`data.repository.pullRequest` is null"),
            "got: {error}"
        );
    }

    #[test]
    fn reports_shape_mismatch_with_body() {
        // reviewThreads present but with the wrong shape: typed parse fails.
        let raw = json!({
            "data": {"repository": {"pullRequest": {"reviewThreads": {"nodes": "oops"}}}}
        });
        let error = parse_review_threads_page(raw).expect_err("shape mismatch is an error");
        let message = error.to_string();
        assert!(
            message.contains("unexpected review thread response shape"),
            "got: {message}"
        );
    }

    #[test]
    fn truncates_large_response_bodies() {
        let large = json!({"data": {"repository": null, "padding": "x".repeat(5000)}});
        let error = parse_review_threads_page(large).expect_err("null repository is an error");
        assert!(error.to_string().len() < 1000, "error should stay bounded");
    }
}
