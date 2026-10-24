//! GitHub Actions: workflows and their dispatch, workflow runs, jobs and their logs, artifacts,
//! caches, and self-hosted runners.

mod artifacts;
mod caches;
mod runners;
mod runs;
mod workflows;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer};
use serde_json::Value;

use super::{Client, Repo};

pub use artifacts::{Artifact, artifacts, delete_artifact, download_artifact, run_artifacts};
pub use caches::{Cache, cache_usage, caches, delete_cache, pull_state};
pub use runners::{
    OrgRepo, Runner, org_repos, org_runners, recently_pushed_org_repos, repo_runners,
};
pub use runs::{
    Job, WorkflowRun, attempt_jobs, branch_runs, commit_runs, job, job_log, job_opt, recent_runs,
    run_jobs, runs_with_status, successful_branch_runs, workflow_run, workflow_run_opt,
};
pub use workflows::{
    Workflow, dispatch_workflow, file_name, file_stem, workflow_dispatch_runs, workflow_file,
    workflows,
};

/// Default branch of the repository, which dispatches and branch lookups fall back to.
pub fn default_branch(client: &Client, repo: &Repo) -> Result<String> {
    #[derive(Deserialize)]
    struct Repository {
        default_branch: Option<String>,
    }
    let r: Repository = parse(client.get(&repo.api())?, "repository")?;
    r.default_branch.context("Repository has no default branch")
}

/// Deserialize one response body into its model.
fn parse<T: DeserializeOwned>(value: Value, what: &str) -> Result<T> {
    serde_json::from_value(value).with_context(|| format!("Failed to parse {what}"))
}

/// Deserialize every item of a paginated listing.
fn parse_all<T: DeserializeOwned>(values: Vec<Value>, what: &str) -> Result<Vec<T>> {
    values.into_iter().map(|v| parse(v, what)).collect()
}

/// Read a field GitHub may send as null or leave out as its default, as an empty string.
fn null_default<'de, D, T>(d: D) -> std::result::Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

/// Read an RFC 3339 timestamp that may be null, missing, or malformed, as None in each case.
fn opt_time<'de, D>(d: D) -> std::result::Result<Option<DateTime<Utc>>, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(Option::<String>::deserialize(d)?
        .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
        .map(|t| t.with_timezone(&Utc)))
}
