use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::{Value, json};

use crate::api::Client;
use crate::context::PrRef;
use crate::json::{login_of, str_of, time_of};

/// Pull request metadata most commands need.
#[derive(Clone, Debug, Serialize)]
pub struct PrInfo {
    pub node_id: String,
    pub number: u64,
    pub title: String,
    pub url: String,
    pub state: String,
    pub draft: bool,
    pub body: String,
    pub author: String,
    pub created_at: DateTime<Utc>,
    pub base_ref: String,
    pub head_ref: String,
    pub head_oid: String,
    /// OWNER/REPO the head branch lives in, which differs from the base for forks.
    pub head_repo: Option<String>,
    pub cross_repository: bool,
}

const PR_QUERY: &str = r#"
query($owner: String!, $repo: String!, $number: Int!) {
  repository(owner: $owner, name: $repo) {
    pullRequest(number: $number) {
      id number title url state isDraft body createdAt
      author { login }
      baseRefName headRefName headRefOid
      isCrossRepository
      headRepository { nameWithOwner }
    }
  }
}"#;

pub fn fetch_pr(client: &Client, pr: &PrRef) -> Result<PrInfo> {
    let data = client.graphql(PR_QUERY, vars(pr))?;
    let p = &data["repository"]["pullRequest"];
    if p.is_null() {
        anyhow::bail!("Pull request {pr} not found");
    }
    Ok(PrInfo {
        node_id: str_of(p, "id"),
        number: p["number"].as_u64().unwrap_or(pr.number),
        title: str_of(p, "title"),
        url: str_of(p, "url"),
        state: str_of(p, "state"),
        draft: p["isDraft"].as_bool().unwrap_or(false),
        body: str_of(p, "body"),
        author: login_of(&p["author"]),
        created_at: time_of(p, "createdAt")?,
        base_ref: str_of(p, "baseRefName"),
        head_ref: str_of(p, "headRefName"),
        head_oid: str_of(p, "headRefOid"),
        head_repo: p["headRepository"]["nameWithOwner"]
            .as_str()
            .map(str::to_string),
        cross_repository: p["isCrossRepository"].as_bool().unwrap_or(false),
    })
}

#[derive(Clone, Debug, Serialize)]
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

#[derive(Clone, Debug, Serialize)]
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

/// Fetch every review thread on a PR, resolved and outdated included.
pub fn fetch_threads(client: &Client, pr: &PrRef) -> Result<PrThreads> {
    let mut threads = Vec::new();
    let mut cursor = Value::Null;
    let mut header = None;
    loop {
        let mut variables = vars(pr);
        variables["cursor"] = cursor;
        let data = client.graphql(THREADS_QUERY, variables)?;
        let p = &data["repository"]["pullRequest"];
        if p.is_null() {
            anyhow::bail!("Pull request {pr} not found");
        }
        if header.is_none() {
            header = Some((
                str_of(p, "id"),
                login_of(&p["author"]),
                str_of(p, "headRefOid"),
            ));
        }
        let page = &p["reviewThreads"];
        for node in page["nodes"].as_array().into_iter().flatten() {
            threads.push(parse_thread(node)?);
        }
        if page["pageInfo"]["hasNextPage"].as_bool() != Some(true) {
            break;
        }
        cursor = page["pageInfo"]["endCursor"].clone();
    }
    let (pr_node_id, author, head_oid) = header.context("Empty response for review threads")?;
    Ok(PrThreads {
        pr_node_id,
        author,
        head_oid,
        threads,
    })
}

fn parse_thread(node: &Value) -> Result<Thread> {
    let comments = node["comments"]["nodes"]
        .as_array()
        .into_iter()
        .flatten()
        .map(parse_comment)
        .collect::<Result<Vec<_>>>()?;
    Ok(Thread {
        id: str_of(node, "id"),
        path: str_of(node, "path"),
        line: node["line"].as_u64(),
        start_line: node["startLine"].as_u64(),
        original_line: node["originalLine"].as_u64(),
        original_start_line: node["originalStartLine"].as_u64(),
        side: str_of(node, "diffSide"),
        resolved: node["isResolved"].as_bool().unwrap_or(false),
        outdated: node["isOutdated"].as_bool().unwrap_or(false),
        comments,
    })
}

fn parse_comment(node: &Value) -> Result<Comment> {
    let author = &node["author"];
    Ok(Comment {
        id: node["databaseId"].as_u64().unwrap_or(0),
        node_id: str_of(node, "id"),
        author: login_of(author),
        bot: is_bot(author["__typename"].as_str(), &login_of(author)),
        body: str_of(node, "body"),
        created_at: time_of(node, "createdAt")?,
        url: str_of(node, "url"),
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

pub fn vars(pr: &PrRef) -> Value {
    json!({
        "owner": pr.repo.owner,
        "repo": pr.repo.name,
        "number": pr.number,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let t = parse_thread(&node).unwrap();
        assert_eq!(t.original_line, Some(7));
        assert!(t.matches("PRRT_x"));
        assert!(t.matches("123"));
        assert!(!t.matches("124"));
        assert!(t.starter().unwrap().bot);
    }
}
