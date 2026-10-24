//! Pull requests: their metadata, checks, review threads, comments, reviews, labels, and the
//! calls that change them.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Value, json};

use crate::api::{Client, PrRef};

/// Pull request metadata most commands need.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(from = "PrNode")]
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

/// The pull request as `PR_QUERY` returns it.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PrNode {
    #[serde(default, deserialize_with = "nullable")]
    id: String,
    number: u64,
    #[serde(default, deserialize_with = "nullable")]
    title: String,
    #[serde(default, deserialize_with = "nullable")]
    url: String,
    #[serde(default, deserialize_with = "nullable")]
    state: String,
    #[serde(default, deserialize_with = "nullable")]
    is_draft: bool,
    #[serde(default, deserialize_with = "nullable")]
    body: String,
    author: Option<Actor>,
    created_at: DateTime<Utc>,
    #[serde(default, deserialize_with = "nullable")]
    base_ref_name: String,
    #[serde(default, deserialize_with = "nullable")]
    head_ref_name: String,
    #[serde(default, deserialize_with = "nullable")]
    head_ref_oid: String,
    #[serde(default, deserialize_with = "nullable")]
    is_cross_repository: bool,
    head_repository: Option<NameWithOwner>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct NameWithOwner {
    name_with_owner: Option<String>,
}

impl From<PrNode> for PrInfo {
    fn from(p: PrNode) -> Self {
        PrInfo {
            node_id: p.id,
            number: p.number,
            title: p.title,
            url: p.url,
            state: p.state,
            draft: p.is_draft,
            body: p.body,
            author: login_or_ghost(p.author.as_ref()),
            created_at: p.created_at,
            base_ref: p.base_ref_name,
            head_ref: p.head_ref_name,
            head_oid: p.head_ref_oid,
            head_repo: p.head_repository.and_then(|r| r.name_with_owner),
            cross_repository: p.is_cross_repository,
        }
    }
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
    let data: RepoData<Option<PrInfo>> = query(client, PR_QUERY, vars(pr))?;
    data.repository
        .pull_request
        .with_context(|| format!("Pull request {pr} not found"))
}

/// A GraphQL user, bot, or other actor. Null for deleted accounts.
#[derive(Clone, Debug, Deserialize)]
pub struct Actor {
    pub login: Option<String>,
}

/// Login of an actor, or "ghost" for deleted accounts.
pub fn login_or_ghost(actor: Option<&Actor>) -> String {
    actor
        .and_then(|a| a.login.clone())
        .unwrap_or_else(|| "ghost".to_string())
}

/// `data.repository` of a query on one repository.
#[derive(Deserialize)]
struct RepoData<T> {
    repository: PullRequestData<T>,
}

/// `repository.pullRequest`, null when the PR does not exist.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PullRequestData<T> {
    pull_request: T,
}

/// Run a GraphQL query and deserialize its `data`.
fn query<T: DeserializeOwned>(client: &Client, query: &str, variables: Value) -> Result<T> {
    let data = client.graphql(query, variables)?;
    serde_json::from_value(data).context("Failed to parse GraphQL response")
}

/// Variables naming a pull request, as every PR query takes them.
pub fn vars(pr: &PrRef) -> Value {
    json!({
        "owner": pr.repo.owner,
        "repo": pr.repo.name,
        "number": pr.number,
    })
}

/// Treat null like an absent field, as reading `v["field"]` with a fallback did.
fn nullable<'de, D, T>(d: D) -> std::result::Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pr_info_from_graphql() {
        let node = json!({
            "id": "PR_1", "number": 7, "title": "Fix", "url": "u", "state": "OPEN",
            "isDraft": true, "body": "b", "createdAt": "2026-01-02T03:04:05Z",
            "author": null, "baseRefName": "main", "headRefName": "fix",
            "headRefOid": "abc", "isCrossRepository": true,
            "headRepository": { "nameWithOwner": "fork/r" }
        });
        let info: PrInfo = serde_json::from_value(node).unwrap();
        assert_eq!(info.node_id, "PR_1");
        assert_eq!(info.author, "ghost");
        assert!(info.draft && info.cross_repository);
        assert_eq!(info.head_repo.as_deref(), Some("fork/r"));
        let out = serde_json::to_value(&info).unwrap();
        assert_eq!(out["base_ref"], "main");
        assert_eq!(out["created_at"], "2026-01-02T03:04:05Z");
    }

    #[test]
    fn missing_pr_is_null() {
        let data: RepoData<Option<PrInfo>> =
            serde_json::from_value(json!({ "repository": { "pullRequest": null } })).unwrap();
        assert!(data.repository.pull_request.is_none());
    }
}
