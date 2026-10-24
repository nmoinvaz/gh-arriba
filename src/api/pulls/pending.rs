//! The viewer's pending review, the mutations that build and submit it, and the PR's changed
//! files that inline comments attach to.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{Actor, RepoData, get_all, login_or_ghost, nullable, query, vars};
use crate::api::{Client, PrRef};

/// A file the PR changes.
#[derive(Clone, Debug, Deserialize)]
pub struct PrFile {
    #[serde(default, deserialize_with = "nullable")]
    pub filename: String,
    /// Unified diff hunks. Absent for binary files and very large diffs.
    pub patch: Option<String>,
}

/// Every file the PR changes.
pub fn pr_files(client: &Client, pr: &PrRef) -> Result<Vec<PrFile>> {
    get_all(client, &format!("{}/files", pr.api()))
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(from = "PendingCommentNode")]
pub struct PendingComment {
    pub id: u64,
    pub path: String,
    pub start_line: Option<u64>,
    pub line: Option<u64>,
    pub body: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PendingCommentNode {
    #[serde(default, deserialize_with = "nullable")]
    database_id: u64,
    #[serde(default, deserialize_with = "nullable")]
    path: String,
    start_line: Option<u64>,
    line: Option<u64>,
    original_start_line: Option<u64>,
    original_line: Option<u64>,
    #[serde(default, deserialize_with = "nullable")]
    body: String,
}

impl From<PendingCommentNode> for PendingComment {
    fn from(c: PendingCommentNode) -> Self {
        PendingComment {
            id: c.database_id,
            path: c.path,
            start_line: c.start_line.or(c.original_start_line),
            line: c.line.or(c.original_line),
            body: c.body,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(from = "PendingNode")]
pub struct PendingReview {
    pub id: String,
    pub url: String,
    pub body: String,
    pub comment_count: u64,
    pub comments: Vec<PendingComment>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PendingNode {
    #[serde(default, deserialize_with = "nullable")]
    id: String,
    #[serde(default, deserialize_with = "nullable")]
    url: String,
    #[serde(default, deserialize_with = "nullable")]
    body: String,
    #[serde(default, deserialize_with = "nullable")]
    viewer_did_author: bool,
    comments: Option<PendingComments>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PendingComments {
    #[serde(default, deserialize_with = "nullable")]
    total_count: u64,
    #[serde(default, deserialize_with = "nullable")]
    nodes: Vec<PendingComment>,
}

impl From<PendingNode> for PendingReview {
    fn from(r: PendingNode) -> Self {
        let (comment_count, comments) = r
            .comments
            .map(|c| (c.total_count, c.nodes))
            .unwrap_or_default();
        PendingReview {
            id: r.id,
            url: r.url,
            body: r.body,
            comment_count,
            comments,
        }
    }
}

/// The PR's node ID and author, plus the viewer's pending review if there is one.
pub struct ReviewState {
    pub pr_id: String,
    pub author: String,
    pub pending: Option<PendingReview>,
}

const PENDING_QUERY: &str = r#"
query($owner: String!, $repo: String!, $number: Int!) {
  repository(owner: $owner, name: $repo) {
    pullRequest(number: $number) {
      id
      author { login }
      reviews(first: 10, states: PENDING) {
        nodes {
          id url body viewerDidAuthor
          comments(first: 100) {
            totalCount
            nodes { databaseId path startLine line originalStartLine originalLine body }
          }
        }
      }
    }
  }
}"#;

#[derive(Deserialize)]
struct PendingPr {
    #[serde(default, deserialize_with = "nullable")]
    id: String,
    author: Option<Actor>,
    reviews: Option<PendingReviews>,
}

#[derive(Deserialize)]
struct PendingReviews {
    #[serde(default, deserialize_with = "nullable")]
    nodes: Vec<PendingNode>,
}

/// Pending reviews are visible only to their author, so at most one comes back for the viewer.
pub fn fetch_pending(client: &Client, pr: &PrRef) -> Result<ReviewState> {
    let data: RepoData<Option<PendingPr>> = query(client, PENDING_QUERY, vars(pr))?;
    let p = data
        .repository
        .pull_request
        .with_context(|| format!("Pull request {pr} not found"))?;
    let pending = p
        .reviews
        .into_iter()
        .flat_map(|r| r.nodes)
        .find(|r| r.viewer_did_author)
        .map(PendingReview::from);
    Ok(ReviewState {
        pr_id: p.id,
        author: login_or_ghost(p.author.as_ref()),
        pending,
    })
}

const ADD_REVIEW: &str = r#"
mutation($pr: ID!) {
  addPullRequestReview(input: { pullRequestId: $pr }) { pullRequestReview { id url } }
}"#;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AddReviewData {
    add_pull_request_review: Option<ReviewPayload>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReviewPayload {
    pull_request_review: Option<SubmittedReview>,
}

/// Start a pending review on the PR with node ID `pr_id`, returning the review's node ID.
pub fn add_pending_review(client: &Client, pr_id: &str) -> Result<String> {
    let data: AddReviewData = query(client, ADD_REVIEW, json!({ "pr": pr_id }))?;
    let id = data
        .add_pull_request_review
        .and_then(|p| p.pull_request_review)
        .and_then(|r| r.id)
        .unwrap_or_default();
    if id.is_empty() {
        bail!("GitHub did not return the new pending review");
    }
    Ok(id)
}

const ADD_THREAD: &str = r#"
mutation($input: AddPullRequestReviewThreadInput!) {
  addPullRequestReviewThread(input: $input) { thread { id } }
}"#;

/// An inline comment to add to a pending review.
pub struct NewThread<'a> {
    pub review_id: &'a str,
    pub path: &'a str,
    pub start_line: u64,
    pub line: u64,
    /// LEFT or RIGHT.
    pub side: &'a str,
    pub body: &'a str,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AddThreadData {
    add_pull_request_review_thread: Option<AddThreadPayload>,
}

#[derive(Deserialize)]
struct AddThreadPayload {
    thread: Option<NodeId>,
}

#[derive(Deserialize)]
struct NodeId {
    id: Option<String>,
}

/// Add an inline comment thread to a pending review, returning the thread's node ID.
pub fn add_review_thread(client: &Client, t: &NewThread) -> Result<Option<String>> {
    let mut input = json!({
        "pullRequestReviewId": t.review_id,
        "path": t.path,
        "line": t.line,
        "side": t.side,
        "body": t.body,
    });
    if t.start_line != t.line {
        input["startLine"] = json!(t.start_line);
        input["startSide"] = json!(t.side);
    }
    let data: AddThreadData = query(client, ADD_THREAD, json!({ "input": input }))?;
    Ok(data
        .add_pull_request_review_thread
        .and_then(|p| p.thread)
        .and_then(|t| t.id))
}

/// A review just submitted. Fields sort by name, as the raw response printed.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct SubmittedReview {
    pub id: Option<String>,
    pub state: Option<String>,
    pub url: Option<String>,
}

const SUBMIT_REVIEW: &str = r#"
mutation($id: ID!, $event: PullRequestReviewEvent!, $body: String) {
  submitPullRequestReview(input: { pullRequestReviewId: $id, event: $event, body: $body }) {
    pullRequestReview { id url state }
  }
}"#;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SubmitData {
    submit_pull_request_review: Option<ReviewPayload>,
}

/// Submit pending review `id` as `event`, such as APPROVE.
pub fn submit_review(
    client: &Client,
    id: &str,
    event: &str,
    body: Option<&str>,
) -> Result<Option<SubmittedReview>> {
    let vars = json!({ "id": id, "event": event, "body": body });
    let data: SubmitData = query(client, SUBMIT_REVIEW, vars)?;
    Ok(data
        .submit_pull_request_review
        .and_then(|p| p.pull_request_review))
}

const CREATE_REVIEW: &str = r#"
mutation($pr: ID!, $event: PullRequestReviewEvent!, $body: String) {
  addPullRequestReview(input: { pullRequestId: $pr, event: $event, body: $body }) {
    pullRequestReview { id url state }
  }
}"#;

/// Create and submit a review in one step on the PR with node ID `pr_id`.
pub fn create_review(
    client: &Client,
    pr_id: &str,
    event: &str,
    body: Option<&str>,
) -> Result<Option<SubmittedReview>> {
    let vars = json!({ "pr": pr_id, "event": event, "body": body });
    let data: AddReviewData = query(client, CREATE_REVIEW, vars)?;
    Ok(data
        .add_pull_request_review
        .and_then(|p| p.pull_request_review))
}

const DELETE_REVIEW: &str = r#"
mutation($id: ID!) {
  deletePullRequestReview(input: { pullRequestReviewId: $id }) { pullRequestReview { id } }
}"#;

/// Delete a pending review and its comments.
pub fn delete_review(client: &Client, id: &str) -> Result<()> {
    client.graphql(DELETE_REVIEW, json!({ "id": id }))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_review_found_among_others() {
        let p: PendingPr = serde_json::from_value(json!({
            "id": "PR_1", "author": { "login": "alice" },
            "reviews": { "nodes": [
                { "id": "PRR_other", "viewerDidAuthor": false },
                {
                    "id": "PRR_1", "url": "u", "body": null, "viewerDidAuthor": true,
                    "comments": { "totalCount": 3, "nodes": [{
                        "databaseId": 5, "path": "a.c", "startLine": null, "line": null,
                        "originalStartLine": 2, "originalLine": 4, "body": "x"
                    }]}
                }
            ]}
        }))
        .unwrap();
        let pending = p
            .reviews
            .into_iter()
            .flat_map(|r| r.nodes)
            .find(|r| r.viewer_did_author)
            .map(PendingReview::from)
            .unwrap();
        assert_eq!(pending.id, "PRR_1");
        assert_eq!(pending.comment_count, 3);
        let c = &pending.comments[0];
        assert_eq!((c.start_line, c.line), (Some(2), Some(4)));
        assert_eq!(
            serde_json::to_string(&pending).unwrap(),
            r#"{"id":"PRR_1","url":"u","body":"","comment_count":3,"comments":[{"id":5,"path":"a.c","start_line":2,"line":4,"body":"x"}]}"#
        );
    }

    #[test]
    fn submitted_review_prints_like_the_raw_response() {
        let raw = json!({ "id": "PRR_1", "url": "u", "state": "APPROVED" });
        let typed: SubmittedReview = serde_json::from_value(raw.clone()).unwrap();
        assert_eq!(
            serde_json::to_string_pretty(&typed).unwrap(),
            serde_json::to_string_pretty(&raw).unwrap()
        );
    }

    #[test]
    fn files_without_patch() {
        let f: PrFile =
            serde_json::from_value(json!({ "filename": "a.png", "status": "added" })).unwrap();
        assert_eq!((f.filename.as_str(), f.patch), ("a.png", None));
    }
}
