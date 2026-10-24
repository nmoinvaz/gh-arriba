use anyhow::Result;
use serde::{Deserialize, Serialize};

use super::{null_default, parse, parse_all};
use crate::api::{Client, Repo};

/// A check run from the REST check-runs API. For GitHub Actions its ID is the job ID.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CheckRun {
    pub id: u64,
    #[serde(default, deserialize_with = "null_default")]
    pub name: String,
    /// "queued", "in_progress", or "completed".
    #[serde(default, deserialize_with = "null_default")]
    pub status: String,
    #[serde(default)]
    pub conclusion: Option<String>,
    #[serde(default)]
    pub html_url: Option<String>,
    #[serde(default)]
    pub details_url: Option<String>,
    #[serde(default)]
    pub app: Option<CheckApp>,
}

/// The app that created a check run.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CheckApp {
    #[serde(default)]
    pub slug: Option<String>,
}

impl CheckRun {
    /// Whether GitHub Actions created the check, so it has a job and a log.
    pub fn is_actions(&self) -> bool {
        self.app.as_ref().and_then(|a| a.slug.as_deref()) == Some("github-actions")
    }

    /// Link to the check, preferring GitHub's page over the app's own.
    pub fn url(&self) -> &str {
        self.html_url
            .as_deref()
            .or(self.details_url.as_deref())
            .unwrap_or_default()
    }
}

/// A commit status, as external services report them.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CommitStatus {
    #[serde(default, deserialize_with = "null_default")]
    pub context: String,
    /// "pending", "success", "failure", or "error".
    #[serde(default, deserialize_with = "null_default")]
    pub state: String,
    #[serde(default)]
    pub target_url: Option<String>,
}

/// Latest check runs for a commit.
pub fn check_runs(client: &Client, repo: &Repo, sha: &str) -> Result<Vec<CheckRun>> {
    parse_all(
        client.get_all_in(
            &format!("{}/commits/{sha}/check-runs?filter=latest", repo.api()),
            "check_runs",
        )?,
        "check run",
    )
}

/// The statuses of the combined commit status, one per context.
pub fn commit_statuses(client: &Client, repo: &Repo, sha: &str) -> Result<Vec<CommitStatus>> {
    #[derive(Deserialize)]
    struct Combined {
        #[serde(default, deserialize_with = "null_default")]
        statuses: Vec<CommitStatus>,
    }
    let combined: Combined = parse(
        client.get(&format!("{}/commits/{sha}/status", repo.api()))?,
        "commit status",
    )?;
    Ok(combined.statuses)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mockito::{Matcher, Server};
    use serde_json::json;

    #[test]
    fn check_run_app_and_url() {
        let c: CheckRun = serde_json::from_value(json!({
            "id": 1, "name": "Ubuntu", "status": "completed", "conclusion": "failure",
            "html_url": null, "details_url": "https://example.com/1",
            "app": { "slug": "github-actions" }
        }))
        .unwrap();
        assert!(c.is_actions());
        assert_eq!(c.url(), "https://example.com/1");

        let bare: CheckRun = serde_json::from_value(json!({"id": 2, "app": null})).unwrap();
        assert!(!bare.is_actions());
        assert_eq!(bare.url(), "");
    }

    #[test]
    fn status_target_url_may_be_null() {
        let s: CommitStatus = serde_json::from_value(json!({
            "context": "ci/jenkins", "state": "error", "target_url": null
        }))
        .unwrap();
        assert_eq!(
            (s.context.as_str(), s.state.as_str()),
            ("ci/jenkins", "error")
        );
        assert_eq!(s.target_url, None);
    }

    #[test]
    fn checks_and_statuses_for_a_commit() {
        let mut server = Server::new();
        let checks = server
            .mock("GET", "/repos/o/r/commits/abc/check-runs")
            .match_query(Matcher::AllOf(vec![
                Matcher::UrlEncoded("filter".into(), "latest".into()),
                Matcher::UrlEncoded("per_page".into(), "100".into()),
            ]))
            .with_body(
                r#"{"total_count": 1, "check_runs": [{"id": 1, "name": "Ubuntu",
                    "app": {"slug": "github-actions"}, "html_url": "https://h/1"}]}"#,
            )
            .create();
        server
            .mock("GET", "/repos/o/r/commits/abc/status")
            .with_body(
                r#"{"state": "failure", "statuses": [{"context": "ci/x", "state": "failure"}]}"#,
            )
            .create();
        server
            .mock("GET", "/repos/o/r/commits/def/status")
            .with_body(r#"{"state": "pending", "statuses": null}"#)
            .create();
        let client = Client::for_tests(&server.url());
        let repo = Repo::parse("o/r").unwrap();
        let runs = check_runs(&client, &repo, "abc").unwrap();
        assert!(runs[0].is_actions());
        assert_eq!(runs[0].url(), "https://h/1");
        assert_eq!(
            commit_statuses(&client, &repo, "abc").unwrap()[0].context,
            "ci/x"
        );
        assert!(commit_statuses(&client, &repo, "def").unwrap().is_empty());
        checks.assert();
    }
}
