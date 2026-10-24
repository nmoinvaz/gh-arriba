use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::Path;

use super::{null_default, opt_time, parse_all};
use crate::api::{Client, Repo};

/// An artifact uploaded by a workflow run.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Artifact {
    pub id: u64,
    #[serde(default, deserialize_with = "null_default")]
    pub name: String,
    #[serde(default, deserialize_with = "null_default")]
    pub size_in_bytes: u64,
    #[serde(default, deserialize_with = "opt_time")]
    pub created_at: Option<DateTime<Utc>>,
    #[serde(default, deserialize_with = "opt_time")]
    pub expires_at: Option<DateTime<Utc>>,
    /// Expired artifacts are still listed but can no longer be downloaded.
    #[serde(default, deserialize_with = "null_default")]
    pub expired: bool,
    #[serde(default)]
    pub workflow_run: Option<ArtifactRun>,
}

/// The run an artifact came from.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ArtifactRun {
    #[serde(default, deserialize_with = "null_default")]
    pub id: u64,
}

impl Artifact {
    /// ID of the run that uploaded it, 0 when the response leaves it out.
    pub fn run_id(&self) -> u64 {
        self.workflow_run.as_ref().map_or(0, |r| r.id)
    }
}

/// Artifacts of one run, in the order the API lists them.
pub fn run_artifacts(client: &Client, repo: &Repo, run_id: u64) -> Result<Vec<Artifact>> {
    parse_all(
        client.get_all_in(
            &format!("{}/actions/runs/{run_id}/artifacts", repo.api()),
            "artifacts",
        )?,
        "artifact",
    )
}

/// Every artifact in the repository, expired ones included.
pub fn artifacts(client: &Client, repo: &Repo) -> Result<Vec<Artifact>> {
    parse_all(
        client.get_all_in(&format!("{}/actions/artifacts", repo.api()), "artifacts")?,
        "artifact",
    )
}

/// Download an artifact's zip into `dest`. Returns the bytes written.
pub fn download_artifact(client: &Client, repo: &Repo, id: u64, dest: &Path) -> Result<u64> {
    client.download(&format!("{}/actions/artifacts/{id}/zip", repo.api()), dest)
}

pub fn delete_artifact(client: &Client, repo: &Repo, id: u64) -> Result<()> {
    client.delete(&format!("{}/actions/artifacts/{id}", repo.api()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use mockito::{Matcher, Server};
    use serde_json::json;

    #[test]
    fn parses_artifact_fields() {
        let a: Artifact = serde_json::from_value(json!({
            "id": 5, "name": "linux-build", "size_in_bytes": 1536, "expired": false,
            "created_at": "2026-10-20T00:00:00Z", "expires_at": "2027-01-18T00:00:00Z",
            "workflow_run": { "id": 99, "head_sha": "abc" }
        }))
        .unwrap();
        assert_eq!(
            (a.id, a.name.as_str(), a.size_in_bytes, a.run_id()),
            (5, "linux-build", 1536, 99)
        );
        assert_eq!(a.created_at, Some("2026-10-20T00:00:00Z".parse().unwrap()));
        assert!(!a.expired);

        let bare: Artifact =
            serde_json::from_value(json!({"id": 6, "workflow_run": null})).unwrap();
        assert_eq!(bare.run_id(), 0);
    }

    #[test]
    fn listings_download_and_delete() {
        let mut server = Server::new();
        let run = server
            .mock("GET", "/repos/o/r/actions/runs/9/artifacts")
            .match_query(Matcher::UrlEncoded("per_page".into(), "100".into()))
            .with_body(r#"{"total_count": 1, "artifacts": [{"id": 1, "name": "logs"}]}"#)
            .create();
        let all = server
            .mock("GET", "/repos/o/r/actions/artifacts")
            .match_query(Matcher::UrlEncoded("per_page".into(), "100".into()))
            .with_body(r#"{"artifacts": [{"id": 1, "expired": true}, {"id": 2}]}"#)
            .create();
        let zip = server
            .mock("GET", "/repos/o/r/actions/artifacts/1/zip")
            .with_body("PK-bytes")
            .create();
        let delete = server
            .mock("DELETE", "/repos/o/r/actions/artifacts/2")
            .with_status(204)
            .create();
        let client = Client::for_tests(&server.url());
        let repo = Repo::parse("o/r").unwrap();
        assert_eq!(run_artifacts(&client, &repo, 9).unwrap()[0].name, "logs");
        let listed = artifacts(&client, &repo).unwrap();
        assert_eq!(
            listed.iter().map(|a| a.expired).collect::<Vec<_>>(),
            vec![true, false]
        );
        let dest =
            std::env::temp_dir().join(format!("arriba-api-artifact-{}.zip", std::process::id()));
        assert_eq!(download_artifact(&client, &repo, 1, &dest).unwrap(), 8);
        assert_eq!(std::fs::read(&dest).unwrap(), b"PK-bytes");
        std::fs::remove_file(&dest).unwrap();
        delete_artifact(&client, &repo, 2).unwrap();
        for m in [run, all, zip, delete] {
            m.assert();
        }
    }
}
