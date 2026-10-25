//! Merging a pull request now, or once its requirements pass.

use anyhow::Result;
use serde_json::json;

use crate::api::{Client, PrRef};

/// Merge a PR now with a REST merge method: `merge`, `squash`, or `rebase`. With `sha`, GitHub
/// refuses if the head has moved on from that commit.
pub fn merge(client: &Client, pr: &PrRef, method: &str, sha: Option<&str>) -> Result<()> {
    let mut body = json!({ "merge_method": method });
    if let Some(sha) = sha {
        body["sha"] = json!(sha);
    }
    client.put(&format!("{}/merge", pr.api()), &body)?;
    Ok(())
}

const ENABLE_AUTO_MERGE: &str = r#"
mutation($id: ID!, $method: PullRequestMergeMethod!) {
  enablePullRequestAutoMerge(input: {pullRequestId: $id, mergeMethod: $method}) {
    clientMutationId
  }
}"#;

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
    use crate::api::Repo;
    use mockito::{Matcher, Server};

    fn pr() -> PrRef {
        PrRef {
            repo: Repo::parse("o/r").unwrap(),
            number: 3,
        }
    }

    #[test]
    fn merge_sends_method_and_optional_sha() {
        let mut server = Server::new();
        let plain = server
            .mock("PUT", "/repos/o/r/pulls/3/merge")
            .match_body(Matcher::Json(json!({ "merge_method": "squash" })))
            .with_body(r#"{"merged": true}"#)
            .create();
        let pinned = server
            .mock("PUT", "/repos/o/r/pulls/3/merge")
            .match_body(Matcher::Json(
                json!({ "merge_method": "rebase", "sha": "abc" }),
            ))
            .with_body(r#"{"merged": true}"#)
            .create();
        let client = Client::for_tests(&server.url());
        merge(&client, &pr(), "squash", None).unwrap();
        merge(&client, &pr(), "rebase", Some("abc")).unwrap();
        plain.assert();
        pinned.assert();
    }

    #[test]
    fn merge_refusal_surfaces_status() {
        let mut server = Server::new();
        server
            .mock("PUT", "/repos/o/r/pulls/3/merge")
            .with_status(405)
            .with_body(r#"{"message": "Pull Request is not mergeable"}"#)
            .create();
        let client = Client::for_tests(&server.url());
        let err = merge(&client, &pr(), "rebase", None).unwrap_err();
        assert_eq!(
            err.to_string(),
            "GitHub API 405 on PUT repos/o/r/pulls/3/merge: Pull Request is not mergeable"
        );
    }

    #[test]
    fn auto_merge_sends_the_graphql_method() {
        let mut server = Server::new();
        let mock = server
            .mock("POST", "/graphql")
            .match_body(Matcher::AllOf(vec![
                Matcher::Regex("enablePullRequestAutoMerge".into()),
                Matcher::PartialJson(json!({ "variables": { "id": "PR_3", "method": "REBASE" } })),
            ]))
            .with_body(r#"{"data": {"enablePullRequestAutoMerge": {"clientMutationId": null}}}"#)
            .create();
        let client = Client::for_tests(&server.url());
        enable_auto_merge(&client, "PR_3", "rebase").unwrap();
        mock.assert();
    }
}
