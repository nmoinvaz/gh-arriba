//! Single comments by ID: inline review comments, conversation comments, and review bodies.

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::json;

use super::{User, nullable};
use crate::api::{Client, Repo};

/// A review comment, issue comment, or review, which share most of their fields. Fields one
/// kind lacks stay None.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct RestComment {
    #[serde(default, deserialize_with = "nullable")]
    pub id: u64,
    pub user: Option<User>,
    /// Comments carry `created_at`, reviews `submitted_at`. Both are kept as GitHub sent them.
    pub created_at: Option<String>,
    pub submitted_at: Option<String>,
    #[serde(default, deserialize_with = "nullable")]
    pub html_url: String,
    #[serde(default, deserialize_with = "nullable")]
    pub body: String,
    pub pull_request_url: Option<String>,
    pub issue_url: Option<String>,
    /// Review state, such as APPROVED.
    pub state: Option<String>,
    pub path: Option<String>,
    /// Line on the PR head, null once the comment no longer maps onto it.
    pub line: Option<u64>,
    pub start_line: Option<u64>,
    pub original_line: Option<u64>,
    pub original_start_line: Option<u64>,
    pub side: Option<String>,
    pub in_reply_to_id: Option<u64>,
    pub diff_hunk: Option<String>,
}

fn review_comment_path(repo: &Repo, id: u64) -> String {
    format!("{}/pulls/comments/{id}", repo.api())
}

fn issue_comment_path(repo: &Repo, id: u64) -> String {
    format!("{}/issues/comments/{id}", repo.api())
}

fn review_path(repo: &Repo, number: u64, id: u64) -> String {
    format!("{}/pulls/{number}/reviews/{id}", repo.api())
}

fn parse(value: serde_json::Value) -> Result<RestComment> {
    serde_json::from_value(value).context("Failed to parse comment")
}

/// An inline review comment.
pub fn review_comment(client: &Client, repo: &Repo, id: u64) -> Result<RestComment> {
    parse(client.get(&review_comment_path(repo, id))?)
}

/// An inline review comment, or None when `repo` has none with that ID.
pub fn find_review_comment(client: &Client, repo: &Repo, id: u64) -> Result<Option<RestComment>> {
    client
        .get_opt(&review_comment_path(repo, id))?
        .map(parse)
        .transpose()
}

/// A comment on a PR's or issue's conversation tab.
pub fn issue_comment(client: &Client, repo: &Repo, id: u64) -> Result<RestComment> {
    parse(client.get(&issue_comment_path(repo, id))?)
}

/// A conversation comment, or None when `repo` has none with that ID.
pub fn find_issue_comment(client: &Client, repo: &Repo, id: u64) -> Result<Option<RestComment>> {
    client
        .get_opt(&issue_comment_path(repo, id))?
        .map(parse)
        .transpose()
}

/// A submitted review on PR `number`.
pub fn review(client: &Client, repo: &Repo, number: u64, id: u64) -> Result<RestComment> {
    parse(client.get(&review_path(repo, number, id))?)
}

pub fn edit_review_comment(
    client: &Client,
    repo: &Repo,
    id: u64,
    body: &str,
) -> Result<RestComment> {
    parse(client.patch(&review_comment_path(repo, id), &json!({ "body": body }))?)
}

pub fn edit_issue_comment(
    client: &Client,
    repo: &Repo,
    id: u64,
    body: &str,
) -> Result<RestComment> {
    parse(client.patch(&issue_comment_path(repo, id), &json!({ "body": body }))?)
}

/// Replace a review's body. Reviews take PUT where comments take PATCH.
pub fn edit_review(
    client: &Client,
    repo: &Repo,
    number: u64,
    id: u64,
    body: &str,
) -> Result<RestComment> {
    parse(client.put(&review_path(repo, number, id), &json!({ "body": body }))?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mockito::{Matcher, Server};

    #[test]
    fn get_comments_by_kind() {
        let mut server = Server::new();
        server
            .mock("GET", "/repos/o/r/pulls/comments/1")
            .with_body(r#"{"id": 1, "body": "inline", "path": "a.c"}"#)
            .create();
        server
            .mock("GET", "/repos/o/r/issues/comments/2")
            .with_body(r#"{"id": 2, "body": "conversation"}"#)
            .create();
        server
            .mock("GET", "/repos/o/r/pulls/42/reviews/3")
            .with_body(r#"{"id": 3, "body": "review", "state": "APPROVED"}"#)
            .create();
        let client = Client::for_tests(&server.url());
        let repo = Repo::parse("o/r").unwrap();
        assert_eq!(review_comment(&client, &repo, 1).unwrap().body, "inline");
        assert_eq!(
            issue_comment(&client, &repo, 2).unwrap().body,
            "conversation"
        );
        let r = review(&client, &repo, 42, 3).unwrap();
        assert_eq!(r.state.as_deref(), Some("APPROVED"));
    }

    #[test]
    fn find_maps_missing_to_none() {
        let mut server = Server::new();
        server
            .mock("GET", "/repos/o/r/pulls/comments/1")
            .with_status(404)
            .with_body(r#"{"message": "Not Found"}"#)
            .create();
        server
            .mock("GET", "/repos/o/r/issues/comments/1")
            .with_body(r#"{"id": 1, "body": "found"}"#)
            .create();
        server
            .mock("GET", "/repos/o/r/issues/comments/2")
            .with_status(404)
            .create();
        let client = Client::for_tests(&server.url());
        let repo = Repo::parse("o/r").unwrap();
        assert!(find_review_comment(&client, &repo, 1).unwrap().is_none());
        assert_eq!(
            find_issue_comment(&client, &repo, 1).unwrap().unwrap().body,
            "found"
        );
        assert!(find_issue_comment(&client, &repo, 2).unwrap().is_none());
    }

    #[test]
    fn edits_patch_comments_and_put_reviews() {
        let mut server = Server::new();
        let inline = server
            .mock("PATCH", "/repos/o/r/pulls/comments/1")
            .match_body(Matcher::Json(json!({"body": "new"})))
            .with_body(r#"{"id": 1, "body": "new"}"#)
            .create();
        let issue = server
            .mock("PATCH", "/repos/o/r/issues/comments/2")
            .match_body(Matcher::Json(json!({"body": "new"})))
            .with_body(r#"{"id": 2, "body": "new"}"#)
            .create();
        let rev = server
            .mock("PUT", "/repos/o/r/pulls/42/reviews/3")
            .match_body(Matcher::Json(json!({"body": "new"})))
            .with_body(r#"{"id": 3, "body": "new"}"#)
            .create();
        let client = Client::for_tests(&server.url());
        let repo = Repo::parse("o/r").unwrap();
        assert_eq!(edit_review_comment(&client, &repo, 1, "new").unwrap().id, 1);
        assert_eq!(edit_issue_comment(&client, &repo, 2, "new").unwrap().id, 2);
        assert_eq!(edit_review(&client, &repo, 42, 3, "new").unwrap().id, 3);
        inline.assert();
        issue.assert();
        rev.assert();
    }

    #[test]
    fn paths_by_kind() {
        let repo = Repo::parse("zlib-ng/zlib-ng").unwrap();
        assert_eq!(
            review_comment_path(&repo, 7),
            "repos/zlib-ng/zlib-ng/pulls/comments/7"
        );
        assert_eq!(
            issue_comment_path(&repo, 8),
            "repos/zlib-ng/zlib-ng/issues/comments/8"
        );
        assert_eq!(
            review_path(&repo, 42, 9),
            "repos/zlib-ng/zlib-ng/pulls/42/reviews/9"
        );
    }

    #[test]
    fn review_comment_fields() {
        let c = parse(json!({
            "id": 7, "user": { "login": "alice" }, "created_at": "2026-01-02T03:04:05Z",
            "html_url": "h", "body": null, "path": "a.c", "line": null,
            "original_line": 5, "in_reply_to_id": 6,
            "pull_request_url": "https://api.github.com/repos/o/r/pulls/42"
        }))
        .unwrap();
        assert_eq!(c.body, "");
        assert_eq!((c.line, c.original_line), (None, Some(5)));
        assert_eq!(c.created_at.as_deref(), Some("2026-01-02T03:04:05Z"));
    }
}
