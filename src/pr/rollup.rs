use anyhow::{Result, bail};
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::collections::HashMap;

use crate::api::{Client, PrRef};
use crate::json::{opt_time_of, str_of};
use crate::pr::model::vars;

/// One check run on the PR's head commit.
#[derive(Clone, Debug)]
pub struct CheckRun {
    pub name: String,
    pub workflow: String,
    pub event: String,
    /// Actions workflow run, absent for checks from other apps.
    pub run_id: Option<u64>,
    pub status: String,
    pub conclusion: Option<String>,
    pub started_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
    /// Page with the check's details, such as the Actions job.
    pub url: Option<String>,
}

/// The checks on a PR's head commit, plus the PR fields the caller asked for.
pub struct Rollup {
    /// The pull request object from the first page, holding the requested fields.
    pub pr: Value,
    pub runs: Vec<CheckRun>,
    /// Commit statuses from external services, with `context`, `state`, and `targetUrl`.
    pub statuses: Vec<Value>,
}

/// `__FIELDS__` is replaced with the PR fields each caller needs alongside the checks.
const ROLLUP_QUERY: &str = r#"
query($owner: String!, $repo: String!, $number: Int!, $cursor: String) {
  repository(owner: $owner, name: $repo) {
    pullRequest(number: $number) {
      __FIELDS__
      last: commits(last: 1) {
        nodes {
          commit {
            statusCheckRollup {
              contexts(first: 100, after: $cursor) {
                pageInfo { hasNextPage endCursor }
                nodes {
                  __typename
                  ... on CheckRun {
                    name status conclusion startedAt completedAt detailsUrl
                    checkSuite { workflowRun { databaseId event workflow { name } } }
                  }
                  ... on StatusContext { context state targetUrl }
                }
              }
            }
          }
        }
      }
    }
  }
}"#;

/// Fetch every check run and commit status on the PR's head commit, with `fields` selected on
/// the pull request, such as `title url`.
pub fn fetch(client: &Client, pr: &PrRef, fields: &str) -> Result<Rollup> {
    let query = ROLLUP_QUERY.replace("__FIELDS__", fields);
    let mut rollup = Rollup {
        pr: Value::Null,
        runs: Vec::new(),
        statuses: Vec::new(),
    };
    let mut cursor = Value::Null;
    loop {
        let mut variables = vars(pr);
        variables["cursor"] = cursor;
        let mut data = client.graphql(&query, variables)?;
        let p = data["repository"]["pullRequest"].take();
        if p.is_null() {
            bail!("Pull request {pr} not found");
        }
        let contexts = &p["last"]["nodes"][0]["commit"]["statusCheckRollup"]["contexts"];
        for node in contexts["nodes"].as_array().into_iter().flatten() {
            match node["__typename"].as_str() {
                Some("CheckRun") => rollup.runs.push(parse_check_run(node)),
                Some("StatusContext") => rollup.statuses.push(node.clone()),
                _ => {}
            }
        }
        // A commit with no checks at all has a null rollup, which has no next page either.
        let next = (contexts["pageInfo"]["hasNextPage"].as_bool() == Some(true))
            .then(|| contexts["pageInfo"]["endCursor"].clone());
        if rollup.pr.is_null() {
            rollup.pr = p;
        }
        match next {
            Some(c) => cursor = c,
            None => break,
        }
    }
    Ok(rollup)
}

pub fn parse_check_run(node: &Value) -> CheckRun {
    let run = &node["checkSuite"]["workflowRun"];
    CheckRun {
        name: str_of(node, "name"),
        workflow: str_of(&run["workflow"], "name"),
        event: str_of(run, "event"),
        run_id: run["databaseId"].as_u64(),
        status: str_of(node, "status"),
        conclusion: node["conclusion"].as_str().map(str::to_string),
        started_at: opt_time_of(node, "startedAt"),
        completed_at: opt_time_of(node, "completedAt"),
        url: node["detailsUrl"].as_str().map(str::to_string),
    }
}

/// Keep the latest attempt of each check, as `gh pr checks` does, so reruns supersede failures.
pub fn latest(runs: Vec<CheckRun>) -> Vec<CheckRun> {
    let mut by_key: HashMap<(String, String, String), CheckRun> = HashMap::new();
    for run in runs {
        let key = (run.workflow.clone(), run.name.clone(), run.event.clone());
        match by_key.get(&key) {
            Some(existing) if recency(existing) >= recency(&run) => {}
            _ => {
                by_key.insert(key, run);
            }
        }
    }
    let mut runs: Vec<CheckRun> = by_key.into_values().collect();
    runs.sort_by_key(|r| (r.workflow.to_lowercase(), r.name.to_lowercase()));
    runs
}

/// Ordering key for attempts. A queued rerun has no start time yet but is the newest attempt.
fn recency(run: &CheckRun) -> (bool, Option<DateTime<Utc>>) {
    (run.started_at.is_none(), run.started_at)
}
