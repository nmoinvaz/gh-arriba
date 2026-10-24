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
    use mockito::{Matcher, Server};
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

    #[test]
    fn runner_and_repo_listings() {
        let mut server = Server::new();
        server
            .mock("GET", "/repos/o/r/actions/runners")
            .match_query(Matcher::UrlEncoded("per_page".into(), "100".into()))
            .with_body(r#"{"total_count": 1, "runners": [{"id": 1, "name": "repo-1"}]}"#)
            .create();
        server
            .mock("GET", "/orgs/o/actions/runners")
            .match_query(Matcher::UrlEncoded("per_page".into(), "100".into()))
            .with_body(r#"{"runners": [{"id": 2, "name": "org-1", "labels": null}]}"#)
            .create();
        let all = server
            .mock("GET", "/orgs/o/repos")
            .match_query(Matcher::Exact("type=all&per_page=100".into()))
            .with_body(r#"[{"full_name": "o/a"}, {"full_name": "o/b", "archived": true}]"#)
            .create();
        let recent = server
            .mock("GET", "/orgs/o/repos")
            .match_query(Matcher::Exact(
                "type=all&sort=pushed&direction=desc&per_page=100".into(),
            ))
            .with_body(r#"[{"full_name": "o/b"}]"#)
            .create();
        let client = Client::for_tests(&server.url());
        let repo = Repo::parse("o/r").unwrap();
        assert_eq!(repo_runners(&client, &repo).unwrap()[0].name, "repo-1");
        let org = org_runners(&client, "o").unwrap();
        assert_eq!((org[0].name.as_str(), org[0].labels.len()), ("org-1", 0));
        let repos = org_repos(&client, "o").unwrap();
        assert_eq!(
            repos
                .iter()
                .map(|r| r.full_name.as_str())
                .collect::<Vec<_>>(),
            vec!["o/a", "o/b"]
        );
        assert_eq!(
            recently_pushed_org_repos(&client, "o").unwrap()[0].full_name,
            "o/b"
        );
        all.assert();
        recent.assert();
    }
}
