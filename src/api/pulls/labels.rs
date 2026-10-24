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
    use mockito::{Matcher, Server};

    fn pr() -> PrRef {
        PrRef {
            repo: Repo::parse("o/r").unwrap(),
            number: 7,
        }
    }

    #[test]
    fn labels_of_repo_and_pr() {
        let mut server = Server::new();
        server
            .mock("GET", "/repos/o/r/labels")
            .match_query(Matcher::Any)
            .with_body(
                r#"[{"name": "bug", "description": null}, {"name": "ci", "description": "CI"}]"#,
            )
            .create();
        server
            .mock("GET", "/repos/o/r/issues/7/labels")
            .match_query(Matcher::Any)
            .with_body(r#"[{"name": "ci"}]"#)
            .create();
        let client = Client::for_tests(&server.url());
        let all = repo_labels(&client, &pr().repo).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].description, "");
        assert_eq!(all[1].description, "CI");
        let on_pr = pr_labels(&client, &pr()).unwrap();
        assert_eq!(on_pr[0].name.as_deref(), Some("ci"));
    }

    #[test]
    fn add_and_remove_labels() {
        let mut server = Server::new();
        let add = server
            .mock("POST", "/repos/o/r/issues/7/labels")
            .match_body(Matcher::Json(json!({"labels": ["bug", "ci"]})))
            .with_body("[]")
            .create();
        let remove = server
            .mock("DELETE", "/repos/o/r/issues/7/labels/type%3A%20bug")
            .with_body("[]")
            .create();
        let client = Client::for_tests(&server.url());
        add_labels(&client, &pr(), &["bug", "ci"]).unwrap();
        remove_label(&client, &pr(), "type: bug").unwrap();
        add.assert();
        remove.assert();
    }

    #[test]
    fn contributors_request_count_and_tolerate_empty_body() {
        let mut server = Server::new();
        server
            .mock("GET", "/repos/o/r/contributors")
            .match_query(Matcher::UrlEncoded("per_page".into(), "5".into()))
            .with_body(r#"[{"login": "alice", "type": "User", "contributions": 3}]"#)
            .create();
        server
            .mock("GET", "/repos/o/empty/contributors")
            .match_query(Matcher::Any)
            .with_status(204)
            .create();
        let client = Client::for_tests(&server.url());
        let list = contributors(&client, &pr().repo, 5).unwrap();
        assert_eq!(list[0].contributions, 3);
        let empty = contributors(&client, &Repo::parse("o/empty").unwrap(), 5).unwrap();
        assert!(empty.is_empty());
    }

    #[test]
    fn reviewers_requested_and_requesting() {
        let mut server = Server::new();
        server
            .mock("GET", "/repos/o/r/pulls/7/requested_reviewers")
            .with_body(r#"{"users": [{"login": "bob"}, {"login": null}], "teams": []}"#)
            .create();
        let post = server
            .mock("POST", "/repos/o/r/pulls/7/requested_reviewers")
            .match_body(Matcher::Json(json!({"reviewers": ["carol"]})))
            .with_body(
                r#"{"number": 7, "requested_reviewers": [{"login": "bob"}, {"login": "carol"}]}"#,
            )
            .create();
        let client = Client::for_tests(&server.url());
        assert_eq!(requested_reviewers(&client, &pr()).unwrap(), vec!["bob"]);
        assert_eq!(
            request_reviewers(&client, &pr(), &["carol".to_string()]).unwrap(),
            vec!["bob", "carol"]
        );
        post.assert();
    }

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
