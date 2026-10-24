//! The PR head branch: how far it trails its base, rebasing it server-side, and reading files
//! at its head commit.

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::json;

use super::query;
use crate::api::{Client, Repo, encode_path};

#[derive(Deserialize)]
struct Comparison {
    behind_by: Option<u64>,
}

/// How many commits `head` is missing from `base`.
pub fn behind_by(client: &Client, repo: &Repo, base: &str, head: &str) -> Result<Option<u64>> {
    let v = client.get(&format!("{}/compare/{base}...{head}", repo.api()))?;
    let c: Comparison = serde_json::from_value(v).context("Failed to parse comparison")?;
    Ok(c.behind_by)
}

const UPDATE_BRANCH: &str = r#"
mutation($id: ID!, $oid: GitObjectID!) {
  updatePullRequestBranch(input: {pullRequestId: $id, expectedHeadOid: $oid, updateMethod: REBASE}) {
    pullRequest { headRefOid }
  }
}"#;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UpdateData {
    update_pull_request_branch: Option<UpdatePayload>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UpdatePayload {
    pull_request: Option<HeadOid>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct HeadOid {
    head_ref_oid: Option<String>,
}

/// Rebase the PR with node ID `pr_id` onto its base server-side, provided its head is still
/// `head_oid`. Returns the new head. The REST update-branch endpoint only merges, so this uses
/// GraphQL.
pub fn rebase_branch(client: &Client, pr_id: &str, head_oid: &str) -> Result<Option<String>> {
    let data: UpdateData = query(
        client,
        UPDATE_BRANCH,
        json!({ "id": pr_id, "oid": head_oid }),
    )?;
    Ok(data
        .update_pull_request_branch
        .and_then(|p| p.pull_request)
        .and_then(|p| p.head_ref_oid))
}

/// Contents of `path` at commit `oid`.
pub fn file_at(client: &Client, repo: &Repo, path: &str, oid: &str) -> Result<String> {
    client.get_raw(&format!(
        "{}/contents/{}?ref={oid}",
        repo.api(),
        encode_path(path)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use mockito::{Matcher, Server};

    #[test]
    fn behind_by_compares_base_to_head() {
        let mut server = Server::new();
        server
            .mock("GET", "/repos/o/r/compare/main...fork:fix")
            .with_body(r#"{"behind_by": 4, "ahead_by": 1}"#)
            .create();
        let client = Client::for_tests(&server.url());
        let repo = Repo::parse("o/r").unwrap();
        assert_eq!(
            behind_by(&client, &repo, "main", "fork:fix").unwrap(),
            Some(4)
        );
    }

    #[test]
    fn rebase_branch_sends_id_and_expected_head() {
        let mut server = Server::new();
        let mock = server
            .mock("POST", "/graphql")
            .match_body(Matcher::AllOf(vec![
                Matcher::Regex("updateMethod: REBASE".into()),
                Matcher::PartialJson(json!({"variables": {"id": "PR_1", "oid": "old"}})),
            ]))
            .with_body(
                r#"{"data": {"updatePullRequestBranch": {"pullRequest": {"headRefOid": "new"}}}}"#,
            )
            .create();
        let client = Client::for_tests(&server.url());
        assert_eq!(
            rebase_branch(&client, "PR_1", "old").unwrap().as_deref(),
            Some("new")
        );
        mock.assert();
    }

    #[test]
    fn file_at_encodes_the_path_and_ref() {
        let mut server = Server::new();
        server
            .mock("GET", "/repos/o/r/contents/dir/a%20b.c")
            .match_query(Matcher::UrlEncoded("ref".into(), "abc".into()))
            .match_header("accept", "application/vnd.github.raw+json")
            .with_body("int x;\n")
            .create();
        let client = Client::for_tests(&server.url());
        let repo = Repo::parse("o/r").unwrap();
        assert_eq!(
            file_at(&client, &repo, "dir/a b.c", "abc").unwrap(),
            "int x;\n"
        );
    }

    #[test]
    fn update_payload_new_head() {
        let data: UpdateData = serde_json::from_value(json!({
            "updatePullRequestBranch": { "pullRequest": { "headRefOid": "abc" } }
        }))
        .unwrap();
        let head = data
            .update_pull_request_branch
            .and_then(|p| p.pull_request)
            .and_then(|p| p.head_ref_oid);
        assert_eq!(head.as_deref(), Some("abc"));
    }
}
