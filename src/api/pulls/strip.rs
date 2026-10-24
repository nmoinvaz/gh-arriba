//! Commits made on a PR branch without a checkout, and the file contents they restore.

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::json;

use super::query;
use crate::api::{Client, Repo, encode_path, encode_segment};

#[derive(Deserialize)]
struct Contents {
    #[serde(default)]
    content: String,
    #[serde(default)]
    encoding: String,
}

/// Base64 contents of `path` at `git_ref`, or None when the file doesn't exist there.
pub fn file_base64(
    client: &Client,
    repo: &Repo,
    path: &str,
    git_ref: &str,
) -> Result<Option<String>> {
    let url = format!(
        "{}/contents/{}?ref={}",
        repo.api(),
        encode_path(path),
        encode_segment(git_ref)
    );
    let Some(v) = client.get_opt(&url)? else {
        return Ok(None);
    };
    if v.is_array() {
        bail!("{path} is a directory at {git_ref}");
    }
    let c: Contents = serde_json::from_value(v).context("Failed to parse file contents")?;
    // Files over 1 MB come back without content.
    if c.encoding != "base64" {
        bail!("{path} is too large to restore through the API");
    }
    Ok(Some(c.content.split_whitespace().collect()))
}

const CREATE_COMMIT: &str = r#"
mutation($input: CreateCommitOnBranchInput!) {
  createCommitOnBranch(input: $input) { commit { oid url } }
}"#;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateData {
    create_commit_on_branch: Option<CreatePayload>,
}

#[derive(Deserialize)]
struct CreatePayload {
    commit: Option<NewCommit>,
}

/// A commit just created on a branch.
#[derive(Clone, Debug, Deserialize)]
pub struct NewCommit {
    pub oid: String,
    pub url: String,
}

/// Commit to `branch` of `repo`, provided its head is still `expected_head`. `additions` are
/// paths with base64 contents. GitHub signs the commit, so it shows as verified.
pub fn commit_on_branch(
    client: &Client,
    repo: &str,
    branch: &str,
    expected_head: &str,
    headline: &str,
    additions: &[(String, String)],
    deletions: &[String],
) -> Result<NewCommit> {
    let additions: Vec<_> = additions
        .iter()
        .map(|(path, contents)| json!({ "path": path, "contents": contents }))
        .collect();
    let deletions: Vec<_> = deletions
        .iter()
        .map(|path| json!({ "path": path }))
        .collect();
    let input = json!({
        "branch": { "repositoryNameWithOwner": repo, "branchName": branch },
        "expectedHeadOid": expected_head,
        "message": { "headline": headline },
        "fileChanges": { "additions": additions, "deletions": deletions },
    });
    let data: CreateData = query(client, CREATE_COMMIT, json!({ "input": input }))?;
    data.create_commit_on_branch
        .and_then(|p| p.commit)
        .context("GitHub did not return the new commit")
}

#[cfg(test)]
mod tests {
    use super::*;
    use mockito::{Matcher, Server};

    fn repo() -> Repo {
        Repo::parse("o/r").unwrap()
    }

    #[test]
    fn file_base64_joins_lines_and_maps_missing_files() {
        let mut server = Server::new();
        server
            .mock("GET", "/repos/o/r/contents/docs/a%20b.md")
            .match_query(Matcher::UrlEncoded("ref".into(), "main".into()))
            .with_body(r#"{"type": "file", "encoding": "base64", "content": "aGVs\nbG8=\n"}"#)
            .create();
        server
            .mock("GET", "/repos/o/r/contents/gone.md")
            .match_query(Matcher::Any)
            .with_status(404)
            .with_body(r#"{"message": "Not Found"}"#)
            .create();
        server
            .mock("GET", "/repos/o/r/contents/big.bin")
            .match_query(Matcher::Any)
            .with_body(r#"{"type": "file", "encoding": "none", "content": ""}"#)
            .create();
        server
            .mock("GET", "/repos/o/r/contents/dir")
            .match_query(Matcher::Any)
            .with_body("[]")
            .create();
        let client = Client::for_tests(&server.url());
        assert_eq!(
            file_base64(&client, &repo(), "docs/a b.md", "main").unwrap(),
            Some("aGVsbG8=".to_string())
        );
        assert_eq!(
            file_base64(&client, &repo(), "gone.md", "main").unwrap(),
            None
        );
        let err = file_base64(&client, &repo(), "big.bin", "main").unwrap_err();
        assert!(err.to_string().contains("too large"));
        let err = file_base64(&client, &repo(), "dir", "main").unwrap_err();
        assert!(err.to_string().contains("directory"));
    }

    #[test]
    fn commit_on_branch_sends_file_changes() {
        let mut server = Server::new();
        let mock = server
            .mock("POST", "/graphql")
            .match_body(Matcher::PartialJson(json!({"variables": {"input": {
                "branch": { "repositoryNameWithOwner": "fork/r", "branchName": "feature" },
                "expectedHeadOid": "abc",
                "message": { "headline": "Strip .plans" },
                "fileChanges": {
                    "additions": [{ "path": "README.md", "contents": "aGk=" }],
                    "deletions": [{ "path": ".plans/a.md" }]
                }
            }}})))
            .with_body(
                r#"{"data": {"createCommitOnBranch": {"commit": {"oid": "def", "url": "u"}}}}"#,
            )
            .create();
        let client = Client::for_tests(&server.url());
        let commit = commit_on_branch(
            &client,
            "fork/r",
            "feature",
            "abc",
            "Strip .plans",
            &[("README.md".to_string(), "aGk=".to_string())],
            &[".plans/a.md".to_string()],
        )
        .unwrap();
        assert_eq!(commit.oid, "def");
        mock.assert();
    }
}
