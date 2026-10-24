//! Labels on a PR and its repository, and the people asked to review it.

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::json;

use super::{User, get_all, nullable};
use crate::api::{Client, PrRef, Repo, encode_segment};

/// A label as the REST labels endpoints return it.
#[derive(Clone, Debug, Deserialize)]
pub struct RepoLabel {
    pub name: Option<String>,
    #[serde(default, deserialize_with = "nullable")]
    pub description: String,
}

/// Every label defined in the repository.
pub fn repo_labels(client: &Client, repo: &Repo) -> Result<Vec<RepoLabel>> {
    get_all(client, &format!("{}/labels", repo.api()))
}

/// The labels on a PR.
pub fn pr_labels(client: &Client, pr: &PrRef) -> Result<Vec<RepoLabel>> {
    get_all(client, &format!("{}/labels", pr.issue_api()))
}

/// Add existing labels to a PR.
pub fn add_labels(client: &Client, pr: &PrRef, names: &[&str]) -> Result<()> {
    client.post(
        &format!("{}/labels", pr.issue_api()),
        &json!({ "labels": names }),
    )?;
    Ok(())
}

/// Remove one label from a PR.
pub fn remove_label(client: &Client, pr: &PrRef, name: &str) -> Result<()> {
    client.delete(&format!(
        "{}/labels/{}",
        pr.issue_api(),
        encode_segment(name)
    ))?;
    Ok(())
}

/// A repository contributor. Anonymous contributors have no login.
#[derive(Clone, Debug, Deserialize)]
pub struct Contributor {
    pub login: Option<String>,
    /// User or Bot.
    #[serde(rename = "type")]
    pub kind: Option<String>,
    #[serde(default, deserialize_with = "nullable")]
    pub contributions: u64,
}

/// The repository's top `count` contributors, most commits first.
pub fn contributors(client: &Client, repo: &Repo, count: usize) -> Result<Vec<Contributor>> {
    let v = client.get(&format!("{}/contributors?per_page={count}", repo.api()))?;
    // An empty repository answers with no body at all.
    let list: Option<Vec<Contributor>> =
        serde_json::from_value(v).context("Failed to parse contributors")?;
    Ok(list.unwrap_or_default())
}

#[derive(Deserialize)]
struct Requested {
    #[serde(default, deserialize_with = "nullable")]
    users: Vec<User>,
}

/// Logins of the users whose review is requested on the PR.
pub fn requested_reviewers(client: &Client, pr: &PrRef) -> Result<Vec<String>> {
    let v = client.get(&format!("{}/requested_reviewers", pr.api()))?;
    let requested: Requested =
        serde_json::from_value(v).context("Failed to parse requested reviewers")?;
    Ok(logins(requested.users))
}

#[derive(Deserialize)]
struct RequestedOnPr {
    #[serde(default, deserialize_with = "nullable")]
    requested_reviewers: Vec<User>,
}

/// Request reviews from `logins`, returning everyone now requested.
pub fn request_reviewers(client: &Client, pr: &PrRef, reviewers: &[String]) -> Result<Vec<String>> {
    let v = client.post(
        &format!("{}/requested_reviewers", pr.api()),
        &json!({ "reviewers": reviewers }),
    )?;
    let pr: RequestedOnPr = serde_json::from_value(v).context("Failed to parse pull request")?;
    Ok(logins(pr.requested_reviewers))
}

fn logins(users: Vec<User>) -> Vec<String> {
    users.into_iter().filter_map(|u| u.login).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contributors_tolerate_anonymous_and_empty() {
        let list: Option<Vec<Contributor>> = serde_json::from_value(json!([
            {"login": "alice", "type": "User", "contributions": 7},
            {"type": "Anonymous", "contributions": 2}
        ]))
        .unwrap();
        let list = list.unwrap();
        assert_eq!(list[0].kind.as_deref(), Some("User"));
        assert_eq!(list[1].login, None);
        let empty: Option<Vec<Contributor>> = serde_json::from_value(json!(null)).unwrap();
        assert!(empty.is_none());
    }

    #[test]
    fn requested_users_skip_teams() {
        let r: Requested = serde_json::from_value(json!({
            "users": [{"login": "bob"}], "teams": [{"slug": "core"}]
        }))
        .unwrap();
        assert_eq!(logins(r.users), vec!["bob"]);
    }
}
