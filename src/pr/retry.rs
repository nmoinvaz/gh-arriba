use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};

use crate::cli::RetryArgs;
use crate::context::{Ctx, PrRef, Repo, git, parse_pr_ref};
use crate::github::Client;
use crate::output;
use crate::pr::model::vars;

/// One check run on the PR's head commit.
#[derive(Clone, Debug)]
pub(super) struct CheckRun {
    pub(super) name: String,
    pub(super) workflow: String,
    pub(super) event: String,
    /// Actions workflow run, absent for checks from other apps.
    pub(super) run_id: Option<u64>,
    pub(super) status: String,
    pub(super) conclusion: Option<String>,
    pub(super) started_at: Option<DateTime<Utc>>,
    pub(super) completed_at: Option<DateTime<Utc>>,
    /// Page with the check's details, such as the Actions job.
    pub(super) url: Option<String>,
}

/// A commit status from an external service, which cannot be rerun.
#[derive(Clone, Debug)]
struct StatusContext {
    context: String,
    state: String,
}

struct Checks {
    title: String,
    runs: Vec<CheckRun>,
    statuses: Vec<StatusContext>,
}

const CHECKS_QUERY: &str = r#"
query($owner: String!, $repo: String!, $number: Int!, $cursor: String) {
  repository(owner: $owner, name: $repo) {
    pullRequest(number: $number) {
      title
      commits(last: 1) {
        nodes {
          commit {
            statusCheckRollup {
              contexts(first: 100, after: $cursor) {
                pageInfo { hasNextPage endCursor }
                nodes {
                  __typename
                  ... on CheckRun {
                    name status conclusion startedAt
                    checkSuite { workflowRun { databaseId event workflow { name } } }
                  }
                  ... on StatusContext { context state }
                }
              }
            }
          }
        }
      }
    }
  }
}"#;

fn fetch_checks(client: &Client, pr: &PrRef) -> Result<Checks> {
    let mut checks = Checks {
        title: String::new(),
        runs: Vec::new(),
        statuses: Vec::new(),
    };
    let mut cursor = Value::Null;
    loop {
        let mut variables = vars(pr);
        variables["cursor"] = cursor;
        let data = client.graphql(CHECKS_QUERY, variables)?;
        let p = &data["repository"]["pullRequest"];
        if p.is_null() {
            anyhow::bail!("Pull request {pr} not found");
        }
        checks.title = p["title"].as_str().unwrap_or_default().to_string();
        let contexts = &p["commits"]["nodes"][0]["commit"]["statusCheckRollup"]["contexts"];
        // A commit with no checks at all has a null rollup.
        if contexts.is_null() {
            break;
        }
        for node in contexts["nodes"].as_array().into_iter().flatten() {
            match node["__typename"].as_str() {
                Some("CheckRun") => checks.runs.push(parse_check_run(node)),
                Some("StatusContext") => checks.statuses.push(StatusContext {
                    context: node["context"].as_str().unwrap_or_default().to_string(),
                    state: node["state"].as_str().unwrap_or_default().to_string(),
                }),
                _ => {}
            }
        }
        if contexts["pageInfo"]["hasNextPage"].as_bool() != Some(true) {
            break;
        }
        cursor = contexts["pageInfo"]["endCursor"].clone();
    }
    Ok(checks)
}

pub(super) fn parse_check_run(node: &Value) -> CheckRun {
    let run = &node["checkSuite"]["workflowRun"];
    CheckRun {
        name: node["name"].as_str().unwrap_or_default().to_string(),
        workflow: run["workflow"]["name"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
        event: run["event"].as_str().unwrap_or_default().to_string(),
        run_id: run["databaseId"].as_u64(),
        status: node["status"].as_str().unwrap_or_default().to_string(),
        conclusion: node["conclusion"].as_str().map(str::to_string),
        started_at: node["startedAt"]
            .as_str()
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|t| t.with_timezone(&Utc)),
        completed_at: node["completedAt"]
            .as_str()
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|t| t.with_timezone(&Utc)),
        url: node["detailsUrl"].as_str().map(str::to_string),
    }
}

/// Keep the latest attempt of each check, as `gh pr checks` does, so reruns supersede failures.
pub(super) fn latest(runs: Vec<CheckRun>) -> Vec<CheckRun> {
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

pub(super) fn is_failed(conclusion: Option<&str>) -> bool {
    matches!(
        conclusion,
        Some("FAILURE" | "CANCELLED" | "TIMED_OUT" | "STARTUP_FAILURE")
    )
}

#[derive(Debug, Serialize)]
struct FailedJob {
    name: String,
    conclusion: String,
}

#[derive(Debug, Serialize)]
struct RunPlan {
    run_id: u64,
    workflow: String,
    jobs: Vec<FailedJob>,
    /// "rerun", "dry-run", or the error from the rerun request.
    result: String,
}

#[derive(Debug, Serialize)]
struct PrPlan {
    repo: String,
    number: u64,
    title: String,
    runs: Vec<RunPlan>,
    /// Failed checks that can't be rerun, such as external commit statuses.
    skipped: Vec<String>,
    pending: usize,
    /// GitHub Actions status when checks are stuck pending with no failures.
    #[serde(skip_serializing_if = "Option::is_none")]
    actions_status: Option<String>,
}

/// Group failed check runs by workflow run, and collect what cannot be rerun.
fn plan(pr: &PrRef, checks: Checks) -> PrPlan {
    let runs = latest(checks.runs);
    let pending = runs.iter().filter(|r| r.status != "COMPLETED").count()
        + checks
            .statuses
            .iter()
            .filter(|s| matches!(s.state.as_str(), "PENDING" | "EXPECTED"))
            .count();

    let mut groups: BTreeMap<(String, u64), Vec<FailedJob>> = BTreeMap::new();
    let mut skipped = Vec::new();
    for run in &runs {
        if !is_failed(run.conclusion.as_deref()) {
            continue;
        }
        let conclusion = run.conclusion.as_deref().unwrap_or_default().to_lowercase();
        match run.run_id {
            Some(id) => groups
                .entry((run.workflow.clone(), id))
                .or_default()
                .push(FailedJob {
                    name: run.name.clone(),
                    conclusion,
                }),
            None => skipped.push(format!("{} ({conclusion})", run.name)),
        }
    }
    for status in &checks.statuses {
        if matches!(status.state.as_str(), "FAILURE" | "ERROR") {
            skipped.push(format!(
                "{} ({})",
                status.context,
                status.state.to_lowercase()
            ));
        }
    }

    PrPlan {
        repo: pr.repo.to_string(),
        number: pr.number,
        title: checks.title,
        runs: groups
            .into_iter()
            .map(|((workflow, run_id), jobs)| RunPlan {
                run_id,
                workflow,
                jobs,
                result: String::new(),
            })
            .collect(),
        skipped,
        pending,
        actions_status: None,
    }
}

/// Render a PR's failed runs as a tree.
fn render(plan: &PrPlan) -> String {
    let mut out = format!("PR #{} {}\n", plan.number, plan.title);
    let n = plan.runs.len();
    for (i, run) in plan.runs.iter().enumerate() {
        let last = i + 1 == n;
        let (branch, stem) = if last {
            ("└── ", "    ")
        } else {
            ("├── ", "│   ")
        };
        out.push_str(&format!("{branch}{} ({})\n", run.workflow, run.run_id));
        let m = run.jobs.len();
        for (j, job) in run.jobs.iter().enumerate() {
            let leaf = if j + 1 == m {
                "└── "
            } else {
                "├── "
            };
            out.push_str(&format!("{stem}{leaf}{} ({})\n", job.name, job.conclusion));
        }
    }
    out
}

/// What the positional argument names.
#[derive(Debug, PartialEq)]
enum Target {
    Pr(Option<String>),
    Repo(Option<Repo>),
}

/// A PR reference, or OWNER/REPO and repo URLs which mean every PR of yours in that repo. A
/// two-part name that is also a local branch is taken as the branch.
fn classify(arg: Option<&str>, mine: bool, is_branch: impl Fn(&str) -> bool) -> Result<Target> {
    let Some(a) = arg else {
        return Ok(if mine {
            Target::Repo(None)
        } else {
            Target::Pr(None)
        });
    };
    if parse_pr_ref(a).is_some() || a.trim_start_matches('#').parse::<u64>().is_ok() {
        if mine {
            anyhow::bail!("--mine takes a repository, not a PR");
        }
        return Ok(Target::Pr(Some(a.to_string())));
    }
    let looks_like_repo =
        a.contains("://") || (a.split('/').count() == 2 && !a.starts_with('/') && !is_branch(a));
    if looks_like_repo {
        return Ok(Target::Repo(Some(Repo::parse(a)?)));
    }
    if mine {
        anyhow::bail!("--mine takes a repository, not a branch");
    }
    Ok(Target::Pr(Some(a.to_string())))
}

fn local_branch_exists(name: &str) -> bool {
    git(&[
        "rev-parse",
        "--verify",
        "--quiet",
        &format!("refs/heads/{name}"),
    ])
    .is_ok()
}

/// Open PRs authored by the viewer.
fn my_prs(client: &Client, repo: &Repo) -> Result<Vec<PrRef>> {
    let login = client.viewer()?;
    let query = r#"
      query($q: String!, $cursor: String) {
        search(query: $q, type: ISSUE, first: 100, after: $cursor) {
          pageInfo { hasNextPage endCursor }
          nodes { ... on PullRequest { number } }
        }
      }"#;
    let q = format!("is:pr is:open author:{login} repo:{repo}");
    let mut prs = Vec::new();
    let mut cursor = Value::Null;
    loop {
        let data = client.graphql(query, json!({ "q": q, "cursor": cursor }))?;
        let search = &data["search"];
        for node in search["nodes"].as_array().into_iter().flatten() {
            if let Some(number) = node["number"].as_u64() {
                prs.push(PrRef {
                    repo: repo.clone(),
                    number,
                });
            }
        }
        if search["pageInfo"]["hasNextPage"].as_bool() != Some(true) {
            break;
        }
        cursor = search["pageInfo"]["endCursor"].clone();
    }
    prs.sort_by_key(|p| p.number);
    Ok(prs)
}

/// Status of the Actions component on githubstatus.com.
fn actions_status() -> Result<String> {
    let body: Value = reqwest::blocking::get("https://www.githubstatus.com/api/v2/components.json")
        .context("Failed to reach githubstatus.com")?
        .json()
        .context("Failed to parse githubstatus.com response")?;
    body["components"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|c| c["name"].as_str() == Some("Actions"))
        .and_then(|c| c["status"].as_str())
        .map(str::to_string)
        .context("Actions component missing from githubstatus.com")
}

pub fn run(ctx: &Ctx, args: RetryArgs) -> Result<()> {
    let client = &ctx.client;
    let prs = match classify(args.target.as_deref(), args.mine, local_branch_exists)? {
        Target::Pr(arg) => vec![ctx.pr(arg.as_deref())?],
        Target::Repo(repo) => {
            let repo = match repo {
                Some(r) => r,
                None => ctx.repo()?,
            };
            let prs = my_prs(client, &repo)?;
            if prs.is_empty() {
                println!("No open PRs by {} in {repo}", client.viewer()?);
                return Ok(());
            }
            prs
        }
    };

    let mut plans = Vec::new();
    let mut status_checked: Option<String> = None;
    for pr in &prs {
        let mut plan = plan(pr, fetch_checks(client, pr)?);

        if plan.runs.is_empty() && plan.pending > 0 {
            // Checks stuck pending with nothing failed often means an Actions outage.
            let status = match &status_checked {
                Some(s) => s.clone(),
                None => actions_status().unwrap_or_else(|e| format!("unknown ({e})")),
            };
            status_checked = Some(status.clone());
            plan.actions_status = Some(status);
        }

        if !ctx.json {
            print_summary(&plan);
        }

        for run in &mut plan.runs {
            run.result = if args.dry_run {
                "dry-run".to_string()
            } else {
                let path = format!(
                    "{}/actions/runs/{}/rerun-failed-jobs",
                    pr.repo.api(),
                    run.run_id
                );
                match client.post(&path, &json!({})) {
                    Ok(_) => "rerun".to_string(),
                    Err(e) => format!("{e:#}"),
                }
            };
            if !ctx.json && !args.dry_run {
                match run.result.as_str() {
                    "rerun" => println!("rerun {} ({})", run.workflow, run.run_id),
                    err => eprintln!("failed to rerun {} ({}): {err}", run.workflow, run.run_id),
                }
            }
        }
        plans.push(plan);
    }

    if ctx.json {
        return output::print_json(&plans);
    }
    Ok(())
}

fn print_summary(plan: &PrPlan) {
    if plan.runs.is_empty() {
        let state = if plan.pending > 0 {
            format!("no failures, {} pending", plan.pending)
        } else {
            "all checks passing".to_string()
        };
        println!("PR #{} {}: {state}", plan.number, plan.title);
    } else {
        print!("{}", render(plan));
    }
    for s in &plan.skipped {
        println!("  skipped external status {s}");
    }
    if let Some(status) = plan
        .actions_status
        .as_deref()
        .filter(|s| *s != "operational")
    {
        println!("  warning: GitHub Actions is {status}, runs may be stuck");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(
        name: &str,
        workflow: &str,
        run_id: Option<u64>,
        conclusion: Option<&str>,
        at: &str,
    ) -> CheckRun {
        CheckRun {
            name: name.to_string(),
            workflow: workflow.to_string(),
            event: "pull_request".to_string(),
            run_id,
            status: if conclusion.is_some() {
                "COMPLETED"
            } else {
                "IN_PROGRESS"
            }
            .to_string(),
            conclusion: conclusion.map(str::to_string),
            started_at: Some(at.parse().unwrap()),
            completed_at: None,
            url: None,
        }
    }

    fn pr() -> PrRef {
        PrRef {
            repo: Repo::parse("zlib-ng/zlib-ng").unwrap(),
            number: 7,
        }
    }

    #[test]
    fn queued_rerun_supersedes_failure() {
        let mut queued = check("Build", "CI", Some(2), None, "2026-01-01T01:00:00Z");
        queued.started_at = None;
        let failed = check(
            "Build",
            "CI",
            Some(1),
            Some("FAILURE"),
            "2026-01-01T00:00:00Z",
        );
        for runs in [vec![failed.clone(), queued.clone()], vec![queued, failed]] {
            let kept = latest(runs);
            assert_eq!(kept.len(), 1);
            assert_eq!(kept[0].run_id, Some(2));
        }
    }

    #[test]
    fn rerun_attempt_supersedes_failure() {
        let runs = vec![
            check(
                "Build",
                "CI",
                Some(1),
                Some("FAILURE"),
                "2026-01-01T00:00:00Z",
            ),
            check(
                "Build",
                "CI",
                Some(2),
                Some("SUCCESS"),
                "2026-01-01T01:00:00Z",
            ),
        ];
        let kept = latest(runs);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].conclusion.as_deref(), Some("SUCCESS"));
    }

    #[test]
    fn plan_groups_failures_by_run_and_skips_external() {
        let checks = Checks {
            title: "Fix".to_string(),
            runs: vec![
                check(
                    "Ubuntu",
                    "Build",
                    Some(10),
                    Some("FAILURE"),
                    "2026-01-01T00:00:00Z",
                ),
                check(
                    "macOS",
                    "Build",
                    Some(10),
                    Some("CANCELLED"),
                    "2026-01-01T00:00:00Z",
                ),
                check(
                    "Windows",
                    "Build",
                    Some(10),
                    Some("SUCCESS"),
                    "2026-01-01T00:00:00Z",
                ),
                check(
                    "Lint",
                    "Lint",
                    Some(11),
                    Some("TIMED_OUT"),
                    "2026-01-01T00:00:00Z",
                ),
                check("Docs", "Docs", Some(12), None, "2026-01-01T00:00:00Z"),
                check(
                    "coderabbit",
                    "",
                    None,
                    Some("FAILURE"),
                    "2026-01-01T00:00:00Z",
                ),
            ],
            statuses: vec![StatusContext {
                context: "ci/jenkins".to_string(),
                state: "ERROR".to_string(),
            }],
        };
        let plan = plan(&pr(), checks);
        let runs: Vec<(u64, usize)> = plan.runs.iter().map(|r| (r.run_id, r.jobs.len())).collect();
        assert_eq!(runs, vec![(10, 2), (11, 1)]);
        assert_eq!(plan.pending, 1);
        assert_eq!(
            plan.skipped,
            vec!["coderabbit (failure)", "ci/jenkins (error)"]
        );
        assert_eq!(
            render(&plan),
            "PR #7 Fix\n\
             ├── Build (10)\n\
             │   ├── macOS (cancelled)\n\
             │   └── Ubuntu (failure)\n\
             └── Lint (11)\n    \
             └── Lint (timed_out)\n"
        );
    }

    #[test]
    fn classify_targets() {
        let no_branch = |_: &str| false;
        assert_eq!(classify(None, false, no_branch).unwrap(), Target::Pr(None));
        assert_eq!(classify(None, true, no_branch).unwrap(), Target::Repo(None));
        assert_eq!(
            classify(Some("12"), false, no_branch).unwrap(),
            Target::Pr(Some("12".to_string()))
        );
        assert_eq!(
            classify(Some("a/b#12"), false, no_branch).unwrap(),
            Target::Pr(Some("a/b#12".to_string()))
        );
        assert_eq!(
            classify(Some("a/b"), false, no_branch).unwrap(),
            Target::Repo(Some(Repo::parse("a/b").unwrap()))
        );
        assert_eq!(
            classify(Some("https://github.com/a/b"), false, no_branch).unwrap(),
            Target::Repo(Some(Repo::parse("a/b").unwrap()))
        );
        // A two-part name that exists as a local branch is the branch.
        assert_eq!(
            classify(Some("fix/thing"), false, |_| true).unwrap(),
            Target::Pr(Some("fix/thing".to_string()))
        );
        assert_eq!(
            classify(Some("nathan/dev/feature"), false, no_branch).unwrap(),
            Target::Pr(Some("nathan/dev/feature".to_string()))
        );
        assert!(classify(Some("12"), true, no_branch).is_err());
    }
}
