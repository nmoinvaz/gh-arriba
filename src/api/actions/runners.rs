use anyhow::Result;
use serde::{Deserialize, Serialize};

use super::{null_default, parse, parse_all};
use crate::api::{Client, Repo};

/// A self-hosted runner as the runners API reports it.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Runner {
    pub id: u64,
    #[serde(default, deserialize_with = "null_default")]
    pub name: String,
    /// "online" or "offline".
    #[serde(default, deserialize_with = "null_default")]
    pub status: String,
    #[serde(default, deserialize_with = "null_default")]
    pub busy: bool,
    #[serde(default, deserialize_with = "null_default")]
    pub os: String,
    #[serde(default, deserialize_with = "null_default")]
    pub labels: Vec<RunnerLabel>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RunnerLabel {
    pub name: String,
}

/// Self-hosted runners registered to a repository.
pub fn repo_runners(client: &Client, repo: &Repo) -> Result<Vec<Runner>> {
    runners(client, &format!("{}/actions/runners", repo.api()))
}

/// Self-hosted runners registered to an organization.
pub fn org_runners(client: &Client, org: &str) -> Result<Vec<Runner>> {
    runners(client, &format!("orgs/{org}/actions/runners"))
}

fn runners(client: &Client, path: &str) -> Result<Vec<Runner>> {
    parse_all(client.get_all_in(path, "runners")?, "runner")
}

/// A repository of an organization, as the org repos listing reports it.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct OrgRepo {
    #[serde(default, deserialize_with = "null_default")]
    pub full_name: String,
    #[serde(default, deserialize_with = "null_default")]
    pub archived: bool,
}

/// Every repository of an organization.
pub fn org_repos(client: &Client, org: &str) -> Result<Vec<OrgRepo>> {
    parse_all(
        client.get_all(&format!("orgs/{org}/repos?type=all"))?,
        "repository",
    )
}

/// The hundred most recently pushed repositories of an organization.
pub fn recently_pushed_org_repos(client: &Client, org: &str) -> Result<Vec<OrgRepo>> {
    parse(
        client.get(&format!(
            "orgs/{org}/repos?type=all&sort=pushed&direction=desc&per_page=100"
        ))?,
        "repositories",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_runner_labels() {
        let r: Runner = serde_json::from_value(json!({
            "id": 1, "name": "build-1", "os": "Linux", "status": "online", "busy": true,
            "labels": [
                { "id": 1, "name": "self-hosted", "type": "read-only" },
                { "id": 2, "name": "linux", "type": "custom" }
            ]
        }))
        .unwrap();
        let labels: Vec<&str> = r.labels.iter().map(|l| l.name.as_str()).collect();
        assert_eq!(labels, vec!["self-hosted", "linux"]);
        assert!(r.busy);
    }

    #[test]
    fn org_repo_archived_defaults_to_false() {
        let repos: Vec<OrgRepo> = serde_json::from_value(json!([
            { "full_name": "o/a", "archived": true },
            { "full_name": "o/b", "archived": null },
            { "full_name": "o/c" }
        ]))
        .unwrap();
        let archived: Vec<bool> = repos.iter().map(|r| r.archived).collect();
        assert_eq!(archived, vec![true, false, false]);
    }
}
