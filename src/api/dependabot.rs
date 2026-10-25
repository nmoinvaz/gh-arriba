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

/// Submit an approving review.
pub fn approve(client: &Client, repo: &Repo, number: u64) -> Result<()> {
    let path = format!("{}/pulls/{number}/reviews", repo.api());
    client.post(&path, &json!({ "event": "APPROVE" }))?;
    Ok(())
}

/// Delete a branch by name.
pub fn delete_branch(client: &Client, repo: &Repo, branch: &str) -> Result<()> {
    client.delete(&format!("{}/git/refs/heads/{branch}", repo.api()))?;
    Ok(())
}

/// A Dependabot security alert from REST. Org listings carry the repository, repo listings don't.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Alert {
    pub number: u64,
    pub state: String,
    pub html_url: String,
    pub dependency: Dependency,
    pub security_advisory: Option<Advisory>,
    pub security_vulnerability: Option<Vulnerability>,
    pub repository: Option<AlertRepo>,
}

impl Alert {
    /// The advisory's severity, falling back to the vulnerability's, then `unknown`.
    pub fn severity(&self) -> &str {
        self.security_advisory
            .as_ref()
            .and_then(|a| a.severity.as_deref())
            .or_else(|| {
                self.security_vulnerability
                    .as_ref()
                    .and_then(|v| v.severity.as_deref())
            })
            .unwrap_or("unknown")
    }

    /// First version that fixes the vulnerability, when one exists.
    pub fn patched(&self) -> Option<&str> {
        self.security_vulnerability
            .as_ref()
            .and_then(|v| v.first_patched_version.as_ref())
            .and_then(|p| p.identifier.as_deref())
    }

    pub fn advisory(&self) -> Advisory {
        self.security_advisory.clone().unwrap_or_default()
    }

    /// OWNER/REPO from an org listing, empty in a repo listing.
    pub fn repo(&self) -> &str {
        self.repository
            .as_ref()
            .map(|r| r.full_name.as_str())
            .unwrap_or_default()
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Dependency {
    pub package: Option<Package>,
    pub manifest_path: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Package {
    pub ecosystem: Option<String>,
    pub name: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Advisory {
    pub ghsa_id: Option<String>,
    pub cve_id: Option<String>,
    pub summary: Option<String>,
    pub severity: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Vulnerability {
    pub severity: Option<String>,
    pub first_patched_version: Option<PatchedVersion>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct PatchedVersion {
    pub identifier: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct AlertRepo {
    pub full_name: String,
}

fn parse_alerts(items: Vec<Value>) -> Result<Vec<Alert>> {
    items
        .into_iter()
        .map(|a| serde_json::from_value(a).context("Failed to parse Dependabot alert"))
        .collect()
}

/// Every open alert in a repository.
pub fn repo_alerts(client: &Client, repo: &Repo) -> Result<Vec<Alert>> {
    parse_alerts(client.get_all(&format!("{}/dependabot/alerts?state=open", repo.api()))?)
}

/// Every open alert in an organization's repositories.
pub fn org_alerts(client: &Client, org: &str) -> Result<Vec<Alert>> {
    parse_alerts(client.get_all(&format!("orgs/{org}/dependabot/alerts?state=open"))?)
}

/// One alert by number.
pub fn get_alert(client: &Client, repo: &Repo, number: u64) -> Result<Alert> {
    let value = client.get(&format!("{}/dependabot/alerts/{number}", repo.api()))?;
    serde_json::from_value(value).context("Failed to parse Dependabot alert")
}

/// Dismiss an alert with one of GitHub's reasons and an optional comment.
pub fn dismiss_alert(
    client: &Client,
    repo: &Repo,
    number: u64,
    reason: &str,
    comment: Option<&str>,
) -> Result<()> {
    let mut body = json!({ "state": "dismissed", "dismissed_reason": reason });
    if let Some(c) = comment {
        body["dismissed_comment"] = json!(c);
    }
    client.patch(&format!("{}/dependabot/alerts/{number}", repo.api()), &body)?;
    Ok(())
}

#[cfg(test)]
pub(crate) fn alert_fixture() -> Value {
    json!({
        "number": 7,
        "state": "open",
        "html_url": "https://github.com/o/r/security/dependabot/7",
        "dependency": {
            "package": { "ecosystem": "npm", "name": "Lodash" },
            "manifest_path": "package-lock.json"
        },
        "security_advisory": {
            "ghsa_id": "GHSA-35jh-r3h4-6jhm",
            "cve_id": "CVE-2021-23337",
            "summary": "Command Injection in lodash",
            "severity": "high"
        },
        "security_vulnerability": {
            "severity": "high",
            "first_patched_version": { "identifier": "4.17.21" }
        },
        "repository": { "full_name": "o/r" }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use mockito::{Matcher, Server};

    #[test]
    fn alerts_parse_with_nulls() {
        let a: Alert = serde_json::from_value(alert_fixture()).unwrap();
        assert_eq!(a.number, 7);
        assert_eq!(a.repo(), "o/r");
        assert_eq!(a.severity(), "high");
        assert_eq!(a.patched(), Some("4.17.21"));
        assert_eq!(a.advisory().cve_id.as_deref(), Some("CVE-2021-23337"));

        let mut v = alert_fixture();
        v["security_advisory"]["cve_id"] = Value::Null;
        v["security_advisory"]["severity"] = Value::Null;
        v["security_vulnerability"]["severity"] = json!("low");
        v["security_vulnerability"]["first_patched_version"] = Value::Null;
        v.as_object_mut().unwrap().remove("repository");
        let a: Alert = serde_json::from_value(v).unwrap();
        assert_eq!(a.advisory().cve_id, None);
        assert_eq!(a.severity(), "low");
        assert_eq!(a.patched(), None);
        assert_eq!(a.repo(), "");

        let a: Alert = serde_json::from_value(json!({ "security_vulnerability": null })).unwrap();
        assert_eq!(a.severity(), "unknown");
    }

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

    fn summary(number: u64, login: &str) -> Value {
        json!({
            "number": number, "title": format!("Bump a from 1.0.{number} to 1.0.9"),
            "url": format!("https://github.com/o/r/pull/{number}"),
            "author": { "login": login }, "repository": { "nameWithOwner": "o/r" }
        })
    }

    fn details(repo: &str, number: u64) -> Value {
        json!({ "pullRequest": {
            "id": format!("PR_{number}"), "number": number, "title": "t", "url": "u",
            "createdAt": "2026-01-02T03:04:05Z", "repository": { "nameWithOwner": repo }
        } })
    }

    #[test]
    fn open_prs_follows_cursors() {
        let mut server = Server::new();
        let first = server
            .mock("POST", "/graphql")
            .match_body(Matcher::PartialJson(json!({
                "variables": { "owner": "o", "repo": "r", "cursor": null }
            })))
            .with_body(
                json!({ "data": { "repository": { "pullRequests": {
                    "pageInfo": { "hasNextPage": true, "endCursor": "c1" },
                    "nodes": [summary(1, "dependabot"), summary(2, "someone")]
                } } } })
                .to_string(),
            )
            .create();
        let second = server
            .mock("POST", "/graphql")
            .match_body(Matcher::PartialJson(
                json!({ "variables": { "cursor": "c1" } }),
            ))
            .with_body(
                json!({ "data": { "repository": { "pullRequests": {
                    "pageInfo": { "hasNextPage": false, "endCursor": null },
                    "nodes": [summary(3, "dependabot")]
                } } } })
                .to_string(),
            )
            .create();
        let client = Client::for_tests(&server.url());
        let prs = open_prs(&client, &Repo::parse("o/r").unwrap())
            .unwrap()
            .unwrap();
        let numbers: Vec<u64> = prs.iter().map(|p| p.number).collect();
        assert_eq!(numbers, vec![1, 2, 3]);
        assert_eq!(prs[1].author_login(), Some("someone"));
        first.assert();
        second.assert();
    }

    #[test]
    fn open_prs_none_for_missing_repo() {
        let mut server = Server::new();
        server
            .mock("POST", "/graphql")
            .with_body(r#"{"data": {"repository": null}}"#)
            .create();
        let client = Client::for_tests(&server.url());
        let repo = Repo::parse("o/gone").unwrap();
        assert!(open_prs(&client, &repo).unwrap().is_none());
    }

    #[test]
    fn search_scopes_to_the_org_and_pages() {
        let mut server = Server::new();
        let q = "is:pr is:open author:app/dependabot org:acme archived:false";
        server
            .mock("POST", "/graphql")
            .match_body(Matcher::PartialJson(
                json!({ "variables": { "q": q, "cursor": null } }),
            ))
            .with_body(
                json!({ "data": { "search": {
                    "pageInfo": { "hasNextPage": true, "endCursor": "n" },
                    "nodes": [{}, summary(4, "dependabot")]
                } } })
                .to_string(),
            )
            .create();
        server
            .mock("POST", "/graphql")
            .match_body(Matcher::PartialJson(
                json!({ "variables": { "q": q, "cursor": "n" } }),
            ))
            .with_body(
                json!({ "data": { "search": {
                    "pageInfo": { "hasNextPage": false },
                    "nodes": [summary(5, "dependabot")]
                } } })
                .to_string(),
            )
            .create();
        let client = Client::for_tests(&server.url());
        let prs = search_dependabot_prs(&client, "acme").unwrap().unwrap();
        let numbers: Vec<u64> = prs.iter().map(|p| p.number).collect();
        // The empty object is a search hit that isn't a PR.
        assert_eq!(numbers, vec![0, 4, 5]);

        let mut server = Server::new();
        server
            .mock("POST", "/graphql")
            .with_body(r#"{"data": {"search": null}}"#)
            .create();
        let client = Client::for_tests(&server.url());
        assert!(search_dependabot_prs(&client, "acme").unwrap().is_none());
    }

    #[test]
    fn pr_details_batches_aliases_and_keeps_order() {
        let mut server = Server::new();
        // 21 PRs make two requests: p0..p19, then p0 alone for the 21st.
        let mut first = serde_json::Map::new();
        for i in 0..20u64 {
            // #6 was deleted and #7's repository is gone.
            let value = match i {
                5 => json!({ "pullRequest": null }),
                6 => Value::Null,
                _ => details("o/r", i + 1),
            };
            first.insert(format!("p{i}"), value);
        }
        let batch1 = server
            .mock("POST", "/graphql")
            .match_body(Matcher::Regex(r"p19: repository".into()))
            .with_body(json!({ "data": first }).to_string())
            .create();
        let batch2 = server
            .mock("POST", "/graphql")
            .match_body(Matcher::AllOf(vec![
                Matcher::Regex(r"pullRequest\(number: 21\)".into()),
                Matcher::Regex(r"fragment Dep on PullRequest".into()),
            ]))
            .with_body(json!({ "data": { "p0": details("x/y", 21) } }).to_string())
            .create();
        let client = Client::for_tests(&server.url());
        let prs: Vec<(String, u64)> = (1..=21)
            .map(|n| {
                let repo = if n == 21 { "x/y" } else { "o/r" };
                (repo.to_string(), n)
            })
            .collect();
        let got = pr_details(&client, &prs).unwrap();
        let numbers: Vec<u64> = got.iter().map(|p| p.number).collect();
        let want: Vec<u64> = (1..=21).filter(|n| *n != 6 && *n != 7).collect();
        assert_eq!(numbers, want);
        assert_eq!(got.last().unwrap().repo(), "x/y");
        assert_eq!(got[0].id, "PR_1");
        batch1.assert();
        batch2.assert();

        // Nothing to fetch sends nothing.
        assert!(pr_details(&client, &[]).unwrap().is_empty());
    }

    #[test]
    fn approve_and_delete_branch_calls() {
        let mut server = Server::new();
        let repo = Repo::parse("o/r").unwrap();
        let approve_mock = server
            .mock("POST", "/repos/o/r/pulls/3/reviews")
            .match_body(Matcher::Json(json!({ "event": "APPROVE" })))
            .with_body("{}")
            .create();
        let delete_mock = server
            .mock(
                "DELETE",
                "/repos/o/r/git/refs/heads/dependabot/cargo/serde-1.0.2",
            )
            .with_status(204)
            .create();
        let client = Client::for_tests(&server.url());
        approve(&client, &repo, 3).unwrap();
        delete_branch(&client, &repo, "dependabot/cargo/serde-1.0.2").unwrap();
        approve_mock.assert();
        delete_mock.assert();
    }

    #[test]
    fn alert_listings_and_lookup() {
        let mut server = Server::new();
        let next = format!(
            "<{}/repos/o/r/dependabot/alerts?state=open&page=2>; rel=\"next\"",
            server.url()
        );
        server
            .mock("GET", "/repos/o/r/dependabot/alerts")
            .match_query(Matcher::AllOf(vec![
                Matcher::UrlEncoded("state".into(), "open".into()),
                Matcher::UrlEncoded("per_page".into(), "100".into()),
            ]))
            .with_header("link", &next)
            .with_body(json!([alert_fixture()]).to_string())
            .create();
        let mut second = alert_fixture();
        second["number"] = json!(8);
        server
            .mock("GET", "/repos/o/r/dependabot/alerts")
            .match_query(Matcher::UrlEncoded("page".into(), "2".into()))
            .with_body(json!([second]).to_string())
            .create();
        server
            .mock("GET", "/orgs/acme/dependabot/alerts")
            .match_query(Matcher::UrlEncoded("state".into(), "open".into()))
            .with_body(json!([alert_fixture()]).to_string())
            .create();
        server
            .mock("GET", "/repos/o/r/dependabot/alerts/7")
            .with_body(alert_fixture().to_string())
            .create();
        server
            .mock("GET", "/repos/o/r/dependabot/alerts/9")
            .with_body(r#"{"number": "nine"}"#)
            .create();
        let client = Client::for_tests(&server.url());
        let repo = Repo::parse("o/r").unwrap();
        let numbers: Vec<u64> = repo_alerts(&client, &repo)
            .unwrap()
            .iter()
            .map(|a| a.number)
            .collect();
        assert_eq!(numbers, vec![7, 8]);
        assert_eq!(org_alerts(&client, "acme").unwrap()[0].repo(), "o/r");
        assert_eq!(get_alert(&client, &repo, 7).unwrap().severity(), "high");
        let err = get_alert(&client, &repo, 9).unwrap_err();
        assert_eq!(err.to_string(), "Failed to parse Dependabot alert");
    }

    #[test]
    fn dismiss_sends_reason_and_optional_comment() {
        let mut server = Server::new();
        let with_comment = server
            .mock("PATCH", "/repos/o/r/dependabot/alerts/7")
            .match_body(Matcher::Json(json!({
                "state": "dismissed", "dismissed_reason": "not_used",
                "dismissed_comment": "dev only"
            })))
            .with_body("{}")
            .create();
        let without = server
            .mock("PATCH", "/repos/o/r/dependabot/alerts/8")
            .match_body(Matcher::Json(json!({
                "state": "dismissed", "dismissed_reason": "tolerable_risk"
            })))
            .with_body("{}")
            .create();
        let client = Client::for_tests(&server.url());
        let repo = Repo::parse("o/r").unwrap();
        dismiss_alert(&client, &repo, 7, "not_used", Some("dev only")).unwrap();
        dismiss_alert(&client, &repo, 8, "tolerable_risk", None).unwrap();
        with_comment.assert();
        without.assert();
    }
}
