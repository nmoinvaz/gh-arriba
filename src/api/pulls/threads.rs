//! Review threads and their comments, submitted reviews, and the replies and resolutions that
//! change them.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{Actor, Connection, RepoData, User, get_all, login_or_ghost, nullable, query, vars};
use crate::api::{Client, PrRef};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(from = "CommentNode")]
pub struct Comment {
    /// REST database ID, the one reply endpoints take.
    pub id: u64,
    pub node_id: String,
    pub author: String,
    pub bot: bool,
    pub body: String,
    pub created_at: DateTime<Utc>,
    pub url: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CommentNode {
    #[serde(default, deserialize_with = "nullable")]
    id: String,
    #[serde(default, deserialize_with = "nullable")]
    database_id: u64,
    #[serde(default, deserialize_with = "nullable")]
    body: String,
    created_at: DateTime<Utc>,
    #[serde(default, deserialize_with = "nullable")]
    url: String,
    author: Option<Actor>,
}

impl From<CommentNode> for Comment {
    fn from(c: CommentNode) -> Self {
        let author = login_or_ghost(c.author.as_ref());
        let typename = c.author.as_ref().and_then(|a| a.typename.as_deref());
        Comment {
            id: c.database_id,
            node_id: c.id,
            bot: is_bot(typename, &author),
            author,
            body: c.body,
            created_at: c.created_at,
            url: c.url,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(from = "ThreadNode")]
pub struct Thread {
    /// GraphQL node ID (PRRT_...), the one resolve takes.
    pub id: String,
    pub path: String,
    /// Line on the PR head. Null once the thread is outdated.
    pub line: Option<u64>,
    pub start_line: Option<u64>,
    /// Line on the commit the comment was made against. Set even when outdated.
    pub original_line: Option<u64>,
    pub original_start_line: Option<u64>,
    /// LEFT or RIGHT side of the diff.
    pub side: String,
    pub resolved: bool,
    pub outdated: bool,
    pub comments: Vec<Comment>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ThreadNode {
    #[serde(default, deserialize_with = "nullable")]
    id: String,
    #[serde(default, deserialize_with = "nullable")]
    path: String,
    line: Option<u64>,
    start_line: Option<u64>,
    original_line: Option<u64>,
    original_start_line: Option<u64>,
    #[serde(default, deserialize_with = "nullable")]
    diff_side: String,
    #[serde(default, deserialize_with = "nullable")]
    is_resolved: bool,
    #[serde(default, deserialize_with = "nullable")]
    is_outdated: bool,
    comments: Option<Nodes<Comment>>,
}

/// A connection read without paging.
#[derive(Deserialize)]
#[serde(bound(deserialize = "T: Deserialize<'de>"))]
struct Nodes<T> {
    #[serde(default, deserialize_with = "nullable")]
    nodes: Vec<T>,
}

impl From<ThreadNode> for Thread {
    fn from(t: ThreadNode) -> Self {
        Thread {
            id: t.id,
            path: t.path,
            line: t.line,
            start_line: t.start_line,
            original_line: t.original_line,
            original_start_line: t.original_start_line,
            side: t.diff_side,
            resolved: t.is_resolved,
            outdated: t.is_outdated,
            comments: t.comments.map(|c| c.nodes).unwrap_or_default(),
        }
    }
}

impl Thread {
    /// The comment that opened the thread.
    pub fn starter(&self) -> Option<&Comment> {
        self.comments.first()
    }

    /// Whether `id` names this thread, by node ID or by any comment's database ID.
    pub fn matches(&self, id: &str) -> bool {
        self.id == id || self.comments.iter().any(|c| c.id.to_string() == id)
    }
}

/// A PR's review threads plus the fields needed to act on them.
#[derive(Clone, Debug, Serialize)]
pub struct PrThreads {
    pub pr_node_id: String,
    pub author: String,
    pub head_oid: String,
    pub threads: Vec<Thread>,
}

const THREADS_QUERY: &str = r#"
query($owner: String!, $repo: String!, $number: Int!, $cursor: String) {
  repository(owner: $owner, name: $repo) {
    pullRequest(number: $number) {
      id
      author { login }
      headRefOid
      reviewThreads(first: 100, after: $cursor) {
        pageInfo { hasNextPage endCursor }
        nodes {
          id isResolved isOutdated path diffSide
          line startLine originalLine originalStartLine
          comments(first: 100) {
            nodes {
              id databaseId body createdAt url
              author { __typename login }
            }
          }
        }
      }
    }
  }
}"#;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ThreadsPage {
    #[serde(default, deserialize_with = "nullable")]
    id: String,
    author: Option<Actor>,
    #[serde(default, deserialize_with = "nullable")]
    head_ref_oid: String,
    review_threads: Connection<Thread>,
}

/// Fetch every review thread on a PR, resolved and outdated included.
pub fn fetch_threads(client: &Client, pr: &PrRef) -> Result<PrThreads> {
    let mut threads = Vec::new();
    let mut cursor = Value::Null;
    let mut header = None;
    loop {
        let mut variables = vars(pr);
        variables["cursor"] = cursor;
        let data: RepoData<Option<ThreadsPage>> = query(client, THREADS_QUERY, variables)?;
        let page = data
            .repository
            .pull_request
            .with_context(|| format!("Pull request {pr} not found"))?;
        if header.is_none() {
            header = Some((
                page.id,
                login_or_ghost(page.author.as_ref()),
                page.head_ref_oid,
            ));
        }
        threads.extend(page.review_threads.nodes);
        match page.review_threads.page_info.next_cursor() {
            Some(c) => cursor = Value::String(c),
            None => break,
        }
    }
    let (pr_node_id, author, head_oid) = header.context("Empty response for review threads")?;
    Ok(PrThreads {
        pr_node_id,
        author,
        head_oid,
        threads,
    })
}

/// Bots by GraphQL type, by REST `[bot]` suffix, or by known reviewer login.
pub fn is_bot(typename: Option<&str>, login: &str) -> bool {
    typename == Some("Bot")
        || login.ends_with("[bot]")
        || matches!(
            login,
            "copilot-pull-request-reviewer" | "coderabbitai" | "Copilot"
        )
}

const LAST_REVIEW_QUERY: &str = r#"
query($owner: String!, $repo: String!, $number: Int!, $author: String!) {
  repository(owner: $owner, name: $repo) {
    pullRequest(number: $number) {
      reviews(last: 1, author: $author,
              states: [APPROVED, CHANGES_REQUESTED, COMMENTED, DISMISSED]) {
        nodes { submittedAt }
      }
    }
  }
}"#;

#[derive(Deserialize)]
struct LastReviewPr {
    reviews: Option<Nodes<LastReview>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LastReview {
    submitted_at: Option<DateTime<Utc>>,
}

/// When `login` last submitted a review on the PR, if ever.
pub fn last_review_time(client: &Client, pr: &PrRef, login: &str) -> Result<Option<DateTime<Utc>>> {
    let mut variables = vars(pr);
    variables["author"] = json!(login);
    let data: RepoData<Option<LastReviewPr>> = query(client, LAST_REVIEW_QUERY, variables)?;
    Ok(data
        .repository
        .pull_request
        .and_then(|p| p.reviews)
        .and_then(|r| r.nodes.into_iter().next())
        .and_then(|r| r.submitted_at))
}

/// A submitted or pending review, as the REST reviews list returns it.
#[derive(Clone, Debug, Deserialize)]
pub struct Review {
    #[serde(default, deserialize_with = "nullable")]
    pub id: u64,
    pub user: Option<User>,
    #[serde(default, deserialize_with = "nullable")]
    pub body: String,
    /// APPROVED, CHANGES_REQUESTED, COMMENTED, DISMISSED, or PENDING.
    #[serde(default, deserialize_with = "nullable")]
    pub state: String,
    #[serde(default, deserialize_with = "nullable")]
    pub html_url: String,
    /// Null while the review is pending.
    pub submitted_at: Option<DateTime<Utc>>,
}

/// Every review on the PR, pending ones of the viewer included.
pub fn reviews(client: &Client, pr: &PrRef) -> Result<Vec<Review>> {
    get_all(client, &format!("{}/reviews", pr.api()))
}

/// A comment on the PR's conversation tab, as the REST issue comments list returns it.
#[derive(Clone, Debug, Deserialize)]
pub struct IssueComment {
    pub user: Option<User>,
    pub created_at: DateTime<Utc>,
    #[serde(default, deserialize_with = "nullable")]
    pub html_url: String,
    #[serde(default, deserialize_with = "nullable")]
    pub body: String,
}

/// Every comment on the PR's conversation tab.
pub fn issue_comments(client: &Client, pr: &PrRef) -> Result<Vec<IssueComment>> {
    get_all(client, &format!("{}/comments", pr.issue_api()))
}

/// Comment on the PR's conversation tab.
pub fn add_issue_comment(client: &Client, pr: &PrRef, body: &str) -> Result<()> {
    client.post(
        &format!("{}/comments", pr.issue_api()),
        &json!({ "body": body }),
    )?;
    Ok(())
}

/// A comment just posted.
#[derive(Clone, Debug, Default, Serialize)]
pub struct PostedComment {
    pub id: u64,
    pub url: String,
}

#[derive(Deserialize)]
struct RestPosted {
    #[serde(default, deserialize_with = "nullable")]
    id: u64,
    #[serde(default, deserialize_with = "nullable")]
    html_url: String,
}

/// Reply to the thread that review comment `top_id` opened. Replies to replies fail.
pub fn reply_to_comment(
    client: &Client,
    pr: &PrRef,
    top_id: u64,
    body: &str,
) -> Result<PostedComment> {
    let path = format!("{}/comments/{top_id}/replies", pr.api());
    let resp: RestPosted = serde_json::from_value(client.post(&path, &json!({ "body": body }))?)
        .context("Failed to parse the new reply")?;
    Ok(PostedComment {
        id: resp.id,
        url: resp.html_url,
    })
}

const REPLY_MUTATION: &str = r#"
mutation($id: ID!, $body: String!) {
  addPullRequestReviewThreadReply(input: {pullRequestReviewThreadId: $id, body: $body}) {
    comment { databaseId url }
  }
}"#;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReplyData {
    add_pull_request_review_thread_reply: Option<ReplyPayload>,
}

#[derive(Deserialize)]
struct ReplyPayload {
    comment: Option<GraphqlPosted>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphqlPosted {
    #[serde(default, deserialize_with = "nullable")]
    database_id: u64,
    #[serde(default, deserialize_with = "nullable")]
    url: String,
}

/// Reply to a review thread by its PRRT_ node ID.
pub fn reply_to_thread(client: &Client, thread_id: &str, body: &str) -> Result<PostedComment> {
    let data: ReplyData = query(
        client,
        REPLY_MUTATION,
        json!({ "id": thread_id, "body": body }),
    )?;
    Ok(data
        .add_pull_request_review_thread_reply
        .and_then(|p| p.comment)
        .map(|c| PostedComment {
            id: c.database_id,
            url: c.url,
        })
        .unwrap_or_default())
}

const RESOLVE_MUTATION: &str = r#"
mutation($id: ID!) {
  resolveReviewThread(input: {threadId: $id}) {
    thread { id isResolved path line originalLine }
  }
}"#;

const UNRESOLVE_MUTATION: &str = r#"
mutation($id: ID!) {
  unresolveReviewThread(input: {threadId: $id}) {
    thread { id isResolved path line originalLine }
  }
}"#;

/// A thread as the resolve and unresolve mutations return it.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolvedThread {
    pub id: Option<String>,
    pub is_resolved: Option<bool>,
    #[serde(default, deserialize_with = "nullable")]
    pub path: String,
    pub line: Option<u64>,
    pub original_line: Option<u64>,
}

#[derive(Deserialize)]
struct ResolveData {
    #[serde(rename = "resolveReviewThread", alias = "unresolveReviewThread")]
    payload: Option<ResolvePayload>,
}

#[derive(Deserialize)]
struct ResolvePayload {
    thread: Option<ResolvedThread>,
}

/// Resolve or unresolve a review thread by its PRRT_ node ID.
pub fn set_thread_resolved(
    client: &Client,
    thread_id: &str,
    resolve: bool,
) -> Result<Option<ResolvedThread>> {
    let mutation = if resolve {
        RESOLVE_MUTATION
    } else {
        UNRESOLVE_MUTATION
    };
    let data: ResolveData = query(client, mutation, json!({ "id": thread_id }))?;
    Ok(data.payload.and_then(|p| p.thread))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::Repo;
    use mockito::{Matcher, Server};

    fn pr() -> PrRef {
        PrRef {
            repo: Repo::parse("o/r").unwrap(),
            number: 7,
        }
    }

    fn thread_node(id: &str, db: u64) -> Value {
        json!({
            "id": id, "isResolved": false, "isOutdated": false, "path": "a.c",
            "diffSide": "RIGHT", "line": 1,
            "comments": { "nodes": [{
                "id": format!("C_{db}"), "databaseId": db, "body": "b",
                "createdAt": "2026-01-02T03:04:05Z", "url": "u",
                "author": { "__typename": "User", "login": "alice" }
            }]}
        })
    }

    #[test]
    fn fetch_threads_pages_and_keeps_the_first_header() {
        let mut server = Server::new();
        server
            .mock("POST", "/graphql")
            .match_body(Matcher::PartialJson(json!({"variables": {
                "owner": "o", "repo": "r", "number": 7, "cursor": null
            }})))
            .with_body(
                json!({"data": {"repository": {"pullRequest": {
                    "id": "PR_7", "author": {"login": "bob"}, "headRefOid": "abc",
                    "reviewThreads": {
                        "pageInfo": {"hasNextPage": true, "endCursor": "c1"},
                        "nodes": [thread_node("PRRT_1", 1)]
                    }
                }}}})
                .to_string(),
            )
            .create();
        server
            .mock("POST", "/graphql")
            .match_body(Matcher::PartialJson(json!({"variables": {"cursor": "c1"}})))
            .with_body(
                json!({"data": {"repository": {"pullRequest": {
                    "id": "PR_other", "author": null, "headRefOid": "def",
                    "reviewThreads": {
                        "pageInfo": {"hasNextPage": false, "endCursor": null},
                        "nodes": [thread_node("PRRT_2", 2)]
                    }
                }}}})
                .to_string(),
            )
            .create();
        let client = Client::for_tests(&server.url());
        let t = fetch_threads(&client, &pr()).unwrap();
        assert_eq!(
            (
                t.pr_node_id.as_str(),
                t.author.as_str(),
                t.head_oid.as_str()
            ),
            ("PR_7", "bob", "abc")
        );
        let ids: Vec<&str> = t.threads.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids, vec!["PRRT_1", "PRRT_2"]);
    }

    #[test]
    fn fetch_threads_of_missing_pr() {
        let mut server = Server::new();
        server
            .mock("POST", "/graphql")
            .with_body(r#"{"data": {"repository": {"pullRequest": null}}}"#)
            .create();
        let client = Client::for_tests(&server.url());
        assert!(fetch_threads(&client, &pr()).is_err());
    }

    #[test]
    fn last_review_time_by_author() {
        let mut server = Server::new();
        server
            .mock("POST", "/graphql")
            .match_body(Matcher::PartialJson(json!({"variables": {"author": "me"}})))
            .with_body(
                r#"{"data": {"repository": {"pullRequest": {"reviews": {"nodes": [
                    {"submittedAt": "2026-03-04T05:06:07Z"}]}}}}}"#,
            )
            .create();
        server
            .mock("POST", "/graphql")
            .match_body(Matcher::PartialJson(
                json!({"variables": {"author": "new"}}),
            ))
            .with_body(r#"{"data": {"repository": {"pullRequest": {"reviews": {"nodes": []}}}}}"#)
            .create();
        let client = Client::for_tests(&server.url());
        let t = last_review_time(&client, &pr(), "me").unwrap().unwrap();
        assert_eq!(t.to_rfc3339(), "2026-03-04T05:06:07+00:00");
        assert_eq!(last_review_time(&client, &pr(), "new").unwrap(), None);
    }

    #[test]
    fn reviews_and_issue_comments_lists() {
        let mut server = Server::new();
        server
            .mock("GET", "/repos/o/r/pulls/7/reviews")
            .match_query(Matcher::Any)
            .with_body(
                r#"[{"id": 1, "user": {"login": "a"}, "state": "APPROVED",
                    "submitted_at": "2026-01-02T03:04:05Z"}]"#,
            )
            .create();
        server
            .mock("GET", "/repos/o/r/issues/7/comments")
            .match_query(Matcher::Any)
            .with_body(r#"[{"user": null, "created_at": "2026-01-02T03:04:05Z", "body": "hi"}]"#)
            .create();
        let client = Client::for_tests(&server.url());
        assert_eq!(reviews(&client, &pr()).unwrap()[0].state, "APPROVED");
        let comments = issue_comments(&client, &pr()).unwrap();
        assert_eq!(comments[0].body, "hi");
        assert_eq!(comments[0].html_url, "");
    }

    #[test]
    fn posting_comments_and_replies() {
        let mut server = Server::new();
        let issue = server
            .mock("POST", "/repos/o/r/issues/7/comments")
            .match_body(Matcher::Json(json!({"body": "hello"})))
            .with_body("{}")
            .create();
        let reply = server
            .mock("POST", "/repos/o/r/pulls/7/comments/5/replies")
            .match_body(Matcher::Json(json!({"body": "thanks"})))
            .with_body(r#"{"id": 6, "html_url": "https://h/6"}"#)
            .create();
        let thread = server
            .mock("POST", "/graphql")
            .match_body(Matcher::PartialJson(
                json!({"variables": {"id": "PRRT_1", "body": "ok"}}),
            ))
            .with_body(
                r#"{"data": {"addPullRequestReviewThreadReply": {
                    "comment": {"databaseId": 8, "url": "https://h/8"}}}}"#,
            )
            .create();
        let client = Client::for_tests(&server.url());
        add_issue_comment(&client, &pr(), "hello").unwrap();
        let posted = reply_to_comment(&client, &pr(), 5, "thanks").unwrap();
        assert_eq!((posted.id, posted.url.as_str()), (6, "https://h/6"));
        let posted = reply_to_thread(&client, "PRRT_1", "ok").unwrap();
        assert_eq!((posted.id, posted.url.as_str()), (8, "https://h/8"));
        issue.assert();
        reply.assert();
        thread.assert();
    }

    #[test]
    fn resolve_and_unresolve_pick_the_mutation() {
        let mut server = Server::new();
        let resolve = server
            .mock("POST", "/graphql")
            .match_body(Matcher::AllOf(vec![
                Matcher::Regex(" resolveReviewThread".into()),
                Matcher::PartialJson(json!({"variables": {"id": "PRRT_1"}})),
            ]))
            .with_body(
                r#"{"data": {"resolveReviewThread": {"thread": {
                    "id": "PRRT_1", "isResolved": true, "path": "a.c"}}}}"#,
            )
            .create();
        let unresolve = server
            .mock("POST", "/graphql")
            .match_body(Matcher::Regex("unresolveReviewThread".into()))
            .with_body(
                r#"{"data": {"unresolveReviewThread": {"thread": {
                    "id": "PRRT_1", "isResolved": false, "path": "a.c"}}}}"#,
            )
            .create();
        let client = Client::for_tests(&server.url());
        let t = set_thread_resolved(&client, "PRRT_1", true)
            .unwrap()
            .unwrap();
        assert_eq!(t.is_resolved, Some(true));
        let t = set_thread_resolved(&client, "PRRT_1", false)
            .unwrap()
            .unwrap();
        assert_eq!(t.is_resolved, Some(false));
        resolve.assert();
        unresolve.assert();
    }

    #[test]
    fn bot_detection() {
        assert!(is_bot(Some("Bot"), "coderabbitai"));
        assert!(is_bot(None, "dependabot[bot]"));
        assert!(is_bot(Some("User"), "copilot-pull-request-reviewer"));
        assert!(!is_bot(Some("User"), "nmoinvaz"));
    }

    #[test]
    fn thread_parses_and_matches_ids() {
        let node = json!({
            "id": "PRRT_x", "isResolved": false, "isOutdated": true, "path": "a.rs",
            "diffSide": "RIGHT", "line": null, "startLine": null,
            "originalLine": 7, "originalStartLine": null,
            "comments": { "nodes": [{
                "id": "PRRC_y", "databaseId": 123, "body": "fix",
                "createdAt": "2026-01-02T03:04:05Z", "url": "u",
                "author": { "__typename": "Bot", "login": "coderabbitai" }
            }]}
        });
        let t: Thread = serde_json::from_value(node).unwrap();
        assert_eq!(t.original_line, Some(7));
        assert!(t.matches("PRRT_x"));
        assert!(t.matches("123"));
        assert!(!t.matches("124"));
        assert!(t.starter().unwrap().bot);
    }

    #[test]
    fn thread_serializes_with_its_own_field_names() {
        let node = json!({
            "id": "PRRT_x", "isResolved": true, "isOutdated": false, "path": "a.rs",
            "diffSide": "LEFT", "line": 3, "startLine": 2,
            "originalLine": 3, "originalStartLine": 2,
            "comments": { "nodes": [{
                "id": "PRRC_y", "databaseId": 5, "body": "b",
                "createdAt": "2026-01-02T03:04:05Z", "url": "u", "author": null
            }]}
        });
        let t: Thread = serde_json::from_value(node).unwrap();
        assert_eq!(
            serde_json::to_value(&t).unwrap(),
            json!({
                "id": "PRRT_x", "path": "a.rs", "line": 3, "start_line": 2,
                "original_line": 3, "original_start_line": 2, "side": "LEFT",
                "resolved": true, "outdated": false,
                "comments": [{
                    "id": 5, "node_id": "PRRC_y", "author": "ghost", "bot": false,
                    "body": "b", "created_at": "2026-01-02T03:04:05Z", "url": "u"
                }]
            })
        );
    }

    #[test]
    fn resolve_payload_under_either_mutation() {
        for field in ["resolveReviewThread", "unresolveReviewThread"] {
            let data: ResolveData = serde_json::from_value(json!({ field: { "thread": {
                "id": "PRRT_x", "isResolved": true, "path": "a.c", "line": null,
                "originalLine": 4
            }}}))
            .unwrap();
            let t = data.payload.and_then(|p| p.thread).unwrap();
            assert_eq!((t.path.as_str(), t.original_line), ("a.c", Some(4)));
        }
    }

    #[test]
    fn pending_reviews_have_no_submission_time() {
        let r: Review = serde_json::from_value(json!({
            "id": 1, "user": null, "body": null, "state": "PENDING",
            "html_url": "h", "submitted_at": null
        }))
        .unwrap();
        assert_eq!(r.submitted_at, None);
        assert_eq!(r.body, "");
    }
}
