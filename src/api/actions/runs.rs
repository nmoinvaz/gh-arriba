use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::{null_default, opt_time, parse, parse_all};
use crate::api::{Client, Repo, encode_path};

/// A user as the Actions API embeds it.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Actor {
    pub login: String,
}

/// A workflow run.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct WorkflowRun {
    pub id: u64,
    /// The workflow's display name.
    #[serde(default, deserialize_with = "null_default")]
    pub name: String,
    /// Path of the workflow file.
    #[serde(default, deserialize_with = "null_default")]
    pub path: String,
    #[serde(default, deserialize_with = "null_default")]
    pub head_sha: String,
    #[serde(default, deserialize_with = "null_default")]
    pub display_title: String,
    #[serde(default, deserialize_with = "null_default")]
    pub html_url: String,
    #[serde(default, deserialize_with = "null_default")]
    pub status: String,
    #[serde(default)]
    pub conclusion: Option<String>,
    #[serde(default)]
    pub actor: Option<Actor>,
    /// Who started this attempt, which differs from `actor` on a re-run.
    #[serde(default)]
    pub triggering_actor: Option<Actor>,
}

impl WorkflowRun {
    /// Login of whoever started the run, preferring the one who triggered this attempt.
    pub fn started_by(&self) -> Option<&str> {
        self.triggering_actor
            .as_ref()
            .or(self.actor.as_ref())
            .map(|a| a.login.as_str())
    }
}

/// A job of a workflow run.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Job {
    pub id: u64,
    #[serde(default)]
    pub run_id: u64,
    #[serde(default, deserialize_with = "null_default")]
    pub name: String,
    /// "queued", "in_progress", or "completed".
    #[serde(default, deserialize_with = "null_default")]
    pub status: String,
    #[serde(default)]
    pub conclusion: Option<String>,
    /// Labels the job asked for in `runs-on`.
    #[serde(default, deserialize_with = "null_default")]
    pub labels: Vec<String>,
    #[serde(default, deserialize_with = "null_default")]
    pub html_url: String,
    /// Name of the run's workflow, which older servers leave out.
    #[serde(default)]
    pub workflow_name: Option<String>,
    /// Runner the job was assigned to, null or empty while queued.
    #[serde(default)]
    pub runner_name: Option<String>,
    #[serde(default, deserialize_with = "opt_time")]
    pub created_at: Option<DateTime<Utc>>,
    #[serde(default, deserialize_with = "opt_time")]
    pub started_at: Option<DateTime<Utc>>,
    #[serde(default, deserialize_with = "opt_time")]
    pub completed_at: Option<DateTime<Utc>>,
    #[serde(default, deserialize_with = "null_default")]
    pub steps: Vec<Step>,
}

/// A step of a job.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Step {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub conclusion: Option<String>,
}

/// One page of the runs listing.
#[derive(Deserialize)]
pub(super) struct RunsPage {
    #[serde(default)]
    pub(super) workflow_runs: Vec<WorkflowRun>,
}

/// A workflow run, or None when the repository has no run with that ID.
pub fn workflow_run_opt(client: &Client, repo: &Repo, id: u64) -> Result<Option<WorkflowRun>> {
    client
        .get_opt(&format!("{}/actions/runs/{id}", repo.api()))?
        .map(|v| parse(v, "workflow run"))
        .transpose()
}

pub fn workflow_run(client: &Client, repo: &Repo, id: u64) -> Result<WorkflowRun> {
    parse(
        client.get(&format!("{}/actions/runs/{id}", repo.api()))?,
        "workflow run",
    )
}

/// Every run for a commit, newest first.
pub fn commit_runs(client: &Client, repo: &Repo, sha: &str) -> Result<Vec<WorkflowRun>> {
    parse_all(
        client.get_all_in(
            &format!("{}/actions/runs?head_sha={sha}", repo.api()),
            "workflow_runs",
        )?,
        "workflow run",
    )
}

/// The newest hundred runs on a branch, newest first.
pub fn branch_runs(client: &Client, repo: &Repo, branch: &str) -> Result<Vec<WorkflowRun>> {
    let page: RunsPage = parse(
        client.get(&format!(
            "{}/actions/runs?branch={}&per_page=100",
            repo.api(),
            encode_path(branch)
        ))?,
        "workflow runs",
    )?;
    Ok(page.workflow_runs)
}

/// The newest hundred successful runs on a branch, newest first.
pub fn successful_branch_runs(
    client: &Client,
    repo: &Repo,
    branch: &str,
) -> Result<Vec<WorkflowRun>> {
    let page: RunsPage = parse(
        client.get(&format!(
            "{}/actions/runs?branch={}&status=success&per_page=100",
            repo.api(),
            encode_path(branch)
        ))?,
        "workflow runs",
    )?;
    Ok(page.workflow_runs)
}

/// Every run in the repository with a status such as "in_progress" or "queued".
pub fn runs_with_status(client: &Client, repo: &Repo, status: &str) -> Result<Vec<WorkflowRun>> {
    parse_all(
        client.get_all_in(
            &format!("{}/actions/runs?status={status}", repo.api()),
            "workflow_runs",
        )?,
        "workflow run",
    )
}

/// The `count` most recent runs of a repository, at most 100.
pub fn recent_runs(client: &Client, repo: &Repo, count: usize) -> Result<Vec<WorkflowRun>> {
    let page: RunsPage = parse(
        client.get(&format!(
            "{}/actions/runs?per_page={}",
            repo.api(),
            count.clamp(1, 100)
        ))?,
        "workflow runs",
    )?;
    Ok(page.workflow_runs)
}

/// A job, or None when the repository has no job with that ID.
pub fn job_opt(client: &Client, repo: &Repo, id: u64) -> Result<Option<Job>> {
    client
        .get_opt(&format!("{}/actions/jobs/{id}", repo.api()))?
        .map(|v| parse(v, "job"))
        .transpose()
}

pub fn job(client: &Client, repo: &Repo, id: u64) -> Result<Job> {
    parse(
        client.get(&format!("{}/actions/jobs/{id}", repo.api()))?,
        "job",
    )
}

/// Jobs of a run's latest attempt.
pub fn run_jobs(client: &Client, repo: &Repo, run_id: u64) -> Result<Vec<Job>> {
    parse_all(
        client.get_all_in(
            &format!("{}/actions/runs/{run_id}/jobs?filter=latest", repo.api()),
            "jobs",
        )?,
        "job",
    )
}

/// Jobs of one attempt of a run.
pub fn attempt_jobs(client: &Client, repo: &Repo, run_id: u64, attempt: u64) -> Result<Vec<Job>> {
    parse_all(
        client.get_all_in(
            &format!(
                "{}/actions/runs/{run_id}/attempts/{attempt}/jobs",
                repo.api()
            ),
            "jobs",
        )?,
        "job",
    )
}

/// A job's log as text. The endpoint redirects to blob storage, and reqwest drops the token on
/// that hop.
pub fn job_log(client: &Client, repo: &Repo, job_id: u64) -> Result<String> {
    client.get_text(&format!("{}/actions/jobs/{job_id}/logs", repo.api()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn run_fields_and_starter() {
        let run: WorkflowRun = serde_json::from_value(json!({
            "id": 9, "name": "CI", "path": ".github/workflows/ci.yml", "head_sha": "abc",
            "display_title": null, "html_url": "https://github.com/o/r/actions/runs/9",
            "status": "completed", "conclusion": "success",
            "actor": { "login": "a" }, "triggering_actor": { "login": "b" }
        }))
        .unwrap();
        assert_eq!((run.id, run.name.as_str()), (9, "CI"));
        assert_eq!(run.display_title, "");
        assert_eq!(run.started_by(), Some("b"));

        let first: WorkflowRun = serde_json::from_value(
            json!({"id": 1, "actor": {"login": "a"}, "triggering_actor": null}),
        )
        .unwrap();
        assert_eq!(first.started_by(), Some("a"));
        assert_eq!(first.conclusion, None);
    }

    #[test]
    fn job_fields_and_times() {
        let job: Job = serde_json::from_value(json!({
            "id": 1, "run_id": 9, "name": "Ubuntu", "status": "in_progress", "conclusion": null,
            "labels": ["self-hosted", "linux"],
            "html_url": "https://github.com/o/r/actions/runs/9/job/1",
            "created_at": "2026-01-01T00:00:00Z", "started_at": "2026-01-01T00:01:00Z",
            "completed_at": null
        }))
        .unwrap();
        assert_eq!(
            (job.id, job.run_id, job.status.as_str()),
            (1, 9, "in_progress")
        );
        assert_eq!(job.labels, vec!["self-hosted", "linux"]);
        assert!(job.started_at.is_some());
        assert_eq!(job.completed_at, None);
    }
}
