//! Dependabot pull requests: cheap listings by repository or search, and full details fetched in
//! aliased batches.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;

use super::{Client, Repo};

/// A GraphQL actor. Deleted accounts come back as null.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Actor {
    #[serde(default)]
    pub login: String,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct RepoName {
    /// OWNER/REPO.
    #[serde(rename = "nameWithOwner", default)]
    pub name_with_owner: String,
}

/// An open PR with just the fields that are cheap to list. Search returns an empty object for
/// results that are not pull requests, so every field tolerates absence.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct PrSummary {
    pub number: u64,
    pub title: String,
    pub url: String,
    pub author: Option<Actor>,
    pub repository: RepoName,
}

impl PrSummary {
    pub fn author_login(&self) -> Option<&str> {
        self.author.as_ref().map(|a| a.login.as_str())
    }
}

/// An open PR with the checks, mergeability, and diff size used to decide how it merges.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PrDetails {
    /// GraphQL node ID.
    #[serde(default)]
    pub id: String,
    pub number: u64,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub url: String,
    pub created_at: DateTime<Utc>,
    #[serde(default)]
    pub head_ref_name: String,
    #[serde(default)]
    pub mergeable: String,
    #[serde(default)]
    pub merge_state_status: String,
    pub review_decision: Option<String>,
    #[serde(default)]
    pub additions: u64,
    #[serde(default)]
    pub deletions: u64,
    #[serde(default)]
    pub changed_files: u64,
    pub author: Option<Actor>,
    #[serde(default)]
    pub repository: RepoName,
    #[serde(default)]
    pub commits: Commits,
}

impl PrDetails {
    /// Rollup state of the head commit's checks. None when the commit has no checks.
    pub fn checks(&self) -> Option<&str> {
        self.commits
            .nodes
            .first()
            .and_then(|n| n.commit.status_check_rollup.as_ref())
            .and_then(|r| r.state.as_deref())
    }

    pub fn repo(&self) -> &str {
        &self.repository.name_with_owner
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Commits {
    #[serde(default)]
    pub nodes: Vec<CommitNode>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct CommitNode {
    #[serde(default)]
    pub commit: Commit,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Commit {
    #[serde(rename = "statusCheckRollup")]
    pub status_check_rollup: Option<Rollup>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Rollup {
    pub state: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PageInfo {
    #[serde(default)]
    has_next_page: bool,
    end_cursor: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Connection<T> {
    #[serde(default)]
    page_info: PageInfo,
    #[serde(default)]
    nodes: Vec<T>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RepoPrs {
    pull_requests: Connection<PrSummary>,
}

#[derive(Debug, Deserialize)]
struct RepoPrsData {
    repository: Option<RepoPrs>,
}

#[derive(Debug, Deserialize)]
struct SearchData {
    search: Option<Connection<PrSummary>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AliasedRepo {
    pull_request: Option<PrDetails>,
}

macro_rules! dep_fragment {
    () => {
        r#"
fragment Dep on PullRequest {
  id number title url createdAt headRefName
  mergeable mergeStateStatus reviewDecision
  additions deletions changedFiles
  author { login }
  repository { nameWithOwner }
  commits(last: 1) { nodes { commit { statusCheckRollup { state } } } }
}
"#
    };
}

const LIGHT_REPO_QUERY: &str = r#"
query($owner: String!, $repo: String!, $cursor: String) {
  repository(owner: $owner, name: $repo) {
    pullRequests(states: OPEN, first: 100, after: $cursor) {
      pageInfo { hasNextPage endCursor }
      nodes { number title url author { login } repository { nameWithOwner } }
    }
  }
}"#;

const LIGHT_SEARCH_QUERY: &str = r#"
query($q: String!, $cursor: String) {
  search(query: $q, type: ISSUE, first: 100, after: $cursor) {
    pageInfo { hasNextPage endCursor }
    nodes {
      ... on PullRequest { number title url author { login } repository { nameWithOwner } }
    }
  }
}"#;

/// PRs per detail request. Each costs GitHub about 0.3s to compute, and requests that run past
/// 10s fail with a 502.
const DETAIL_BATCH: usize = 20;

fn from_data<T: DeserializeOwned>(data: Value) -> Result<T> {
    serde_json::from_value(data).context("Failed to parse GraphQL response")
}

/// Every open PR in a repository with just its number, title, URL, author, and repository. These
/// fields are cheap, so pages hold 100 PRs. None when the repository is not found.
pub fn open_prs(client: &Client, repo: &Repo) -> Result<Option<Vec<PrSummary>>> {
    let mut nodes = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let variables = json!({ "owner": repo.owner, "repo": repo.name, "cursor": cursor });
        let data: RepoPrsData = from_data(client.graphql(LIGHT_REPO_QUERY, variables)?)?;
        let Some(repository) = data.repository else {
            return Ok(None);
        };
        let page = repository.pull_requests;
        nodes.extend(page.nodes);
        if !page.page_info.has_next_page {
            break;
        }
        cursor = page.page_info.end_cursor;
    }
    Ok(Some(nodes))
}

/// Every open PR Dependabot authored in an organization's unarchived repositories, with the same
/// cheap fields as `open_prs`. None when the search returns nothing at all.
pub fn search_dependabot_prs(client: &Client, org: &str) -> Result<Option<Vec<PrSummary>>> {
    let q = format!("is:pr is:open author:app/dependabot org:{org} archived:false");
    let mut nodes = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let variables = json!({ "q": q, "cursor": cursor });
        let data: SearchData = from_data(client.graphql(LIGHT_SEARCH_QUERY, variables)?)?;
        let Some(page) = data.search else {
            return Ok(None);
        };
        nodes.extend(page.nodes);
        if !page.page_info.has_next_page {
            break;
        }
        cursor = page.page_info.end_cursor;
    }
    Ok(Some(nodes))
}

/// Full details for specific PRs, given as `(OWNER/REPO, number)`, in batched, aliased queries,
/// keeping their order. PRs that no longer exist are left out.
pub fn pr_details(client: &Client, prs: &[(String, u64)]) -> Result<Vec<PrDetails>> {
    let mut fresh = Vec::new();
    for batch in prs.chunks(DETAIL_BATCH) {
        let query = details_query(batch)?;
        let mut data: HashMap<String, Option<AliasedRepo>> =
            from_data(client.graphql(&query, json!({}))?)?;
        for i in 0..batch.len() {
            if let Some(pr) = data
                .remove(&format!("p{i}"))
                .flatten()
                .and_then(|r| r.pull_request)
            {
                fresh.push(pr);
            }
        }
    }
    Ok(fresh)
}

/// One query that fetches each PR in `batch` under the alias `p{index}`.
fn details_query(batch: &[(String, u64)]) -> Result<String> {
    let mut query = String::from(dep_fragment!());
    query.push_str("query {\n");
    for (i, (repo, number)) in batch.iter().enumerate() {
        let (owner, name) = repo
            .split_once('/')
            .with_context(|| format!("Bad repository name {repo}"))?;
        query.push_str(&format!(
            "  p{i}: repository(owner: {owner:?}, name: {name:?}) {{ pullRequest(number: {number}) {{ ...Dep }} }}\n"
        ));
    }
    query.push('}');
    Ok(query)
}

const ENABLE_AUTO_MERGE: &str = r#"
mutation($id: ID!, $method: PullRequestMergeMethod!) {
  enablePullRequestAutoMerge(input: {pullRequestId: $id, mergeMethod: $method}) {
    clientMutationId
  }
}"#;

/// Comment on a PR, as `@dependabot rebase`.
pub fn comment(client: &Client, repo: &Repo, number: u64, body: &str) -> Result<()> {
    let path = format!("{}/issues/{number}/comments", repo.api());
    client.post(&path, &json!({ "body": body }))?;
    Ok(())
}

/// Submit an approving review.
pub fn approve(client: &Client, repo: &Repo, number: u64) -> Result<()> {
    let path = format!("{}/pulls/{number}/reviews", repo.api());
    client.post(&path, &json!({ "event": "APPROVE" }))?;
    Ok(())
}

/// Merge a PR now with a REST merge method: `merge`, `squash`, or `rebase`.
pub fn merge(client: &Client, repo: &Repo, number: u64, method: &str) -> Result<()> {
    let path = format!("{}/pulls/{number}/merge", repo.api());
    client.put(&path, &json!({ "merge_method": method }))?;
    Ok(())
}

/// Delete a branch by name.
pub fn delete_branch(client: &Client, repo: &Repo, branch: &str) -> Result<()> {
    client.delete(&format!("{}/git/refs/heads/{branch}", repo.api()))?;
    Ok(())
}

/// Turn on auto-merge for a PR by node ID. `method` is the REST merge method name, whose
/// uppercase form is the GraphQL enum value.
pub fn enable_auto_merge(client: &Client, node_id: &str, method: &str) -> Result<()> {
    client.graphql(
        ENABLE_AUTO_MERGE,
        json!({ "id": node_id, "method": method.to_uppercase() }),
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn details_query_aliases_each_pr() {
        let q = details_query(&[("o/r".into(), 3), ("x/y".into(), 9)]).unwrap();
        assert!(q.contains("fragment Dep on PullRequest"));
        assert!(q.contains(
            "  p0: repository(owner: \"o\", name: \"r\") { pullRequest(number: 3) { ...Dep } }\n"
        ));
        assert!(q.contains("  p1: repository(owner: \"x\", name: \"y\")"));
        assert!(details_query(&[("bad".into(), 1)]).is_err());
    }

    #[test]
    fn summaries_tolerate_non_pr_search_results() {
        let data: SearchData = serde_json::from_value(json!({
            "search": {
                "pageInfo": { "hasNextPage": true, "endCursor": "abc" },
                "nodes": [
                    {},
                    {
                        "number": 4, "title": "Bump a from 1 to 2", "url": "u",
                        "author": { "login": "dependabot" },
                        "repository": { "nameWithOwner": "o/r" }
                    },
                    { "number": 5, "title": "t", "url": "u", "author": null,
                      "repository": { "nameWithOwner": "o/r" } }
                ]
            }
        }))
        .unwrap();
        let page = data.search.unwrap();
        assert!(page.page_info.has_next_page);
        assert_eq!(page.page_info.end_cursor.as_deref(), Some("abc"));
        assert_eq!(page.nodes[0].author_login(), None);
        assert_eq!(page.nodes[1].author_login(), Some("dependabot"));
        assert_eq!(page.nodes[1].repository.name_with_owner, "o/r");
        assert_eq!(page.nodes[2].author_login(), None);
    }

    #[test]
    fn details_read_checks_and_skip_missing_aliases() {
        let data: HashMap<String, Option<AliasedRepo>> = serde_json::from_value(json!({
            "p0": { "pullRequest": {
                "id": "PR_1", "number": 1, "title": "Bump a from 1 to 2", "url": "u",
                "createdAt": "2026-01-02T03:04:05Z", "headRefName": "dependabot/a",
                "mergeable": "MERGEABLE", "mergeStateStatus": "CLEAN", "reviewDecision": null,
                "additions": 3, "deletions": 1, "changedFiles": 2,
                "author": { "login": "dependabot" },
                "repository": { "nameWithOwner": "o/r" },
                "commits": { "nodes": [ { "commit": { "statusCheckRollup": { "state": "SUCCESS" } } } ] }
            } },
            "p1": { "pullRequest": null },
            "p2": null
        }))
        .unwrap();
        let pr = data["p0"].as_ref().unwrap().pull_request.as_ref().unwrap();
        assert_eq!(pr.checks(), Some("SUCCESS"));
        assert_eq!(pr.repo(), "o/r");
        assert_eq!(pr.changed_files, 2);
        assert_eq!(pr.review_decision, None);
        assert!(data["p1"].as_ref().unwrap().pull_request.is_none());
        assert!(data["p2"].is_none());

        let no_checks: PrDetails = serde_json::from_value(json!({
            "number": 2, "createdAt": "2026-01-02T03:04:05Z",
            "commits": { "nodes": [ { "commit": { "statusCheckRollup": null } } ] }
        }))
        .unwrap();
        assert_eq!(no_checks.checks(), None);
    }
}
