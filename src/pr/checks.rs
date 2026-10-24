use anyhow::{Result, bail};
use serde::Serialize;
use serde_json::Value;
use std::collections::HashSet;

use crate::cli::ChecksArgs;
use crate::context::{Ctx, PrRef};
use crate::github::Client;
use crate::output;
use crate::pr::model::{str_of, vars};
use crate::pr::retry::{CheckRun, is_failed, latest, parse_check_run};

// -------------------------------------------------------------------------
//  model
// -------------------------------------------------------------------------

/// Outcome of one check, folding check-run conclusions and commit-status states together.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
enum State {
    Pass,
    Fail,
    Pending,
    Skipped,
}

impl State {
    fn label(self) -> &'static str {
        match self {
            State::Pass => "pass",
            State::Fail => "fail",
            State::Pending => "pending",
            State::Skipped => "skipped",
        }
    }

    /// State of an Actions or app check run.
    fn of_run(run: &CheckRun) -> Self {
        if run.status != "COMPLETED" {
            return State::Pending;
        }
        match run.conclusion.as_deref() {
            Some("SUCCESS") => State::Pass,
            // Waiting on a maintainer to approve a fork's workflow blocks the PR like a failure.
            Some("ACTION_REQUIRED") => State::Fail,
            c if is_failed(c) => State::Fail,
            _ => State::Skipped,
        }
    }

    /// State of a commit status from an external service.
    fn of_status(state: &str) -> Self {
        match state {
            "SUCCESS" => State::Pass,
            "FAILURE" | "ERROR" => State::Fail,
            _ => State::Pending,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
struct Check {
    /// `Workflow / job` for Actions, the check or status name otherwise.
    name: String,
    state: State,
    url: Option<String>,
}

#[derive(Debug, Default, PartialEq, Serialize)]
struct Counts {
    pass: usize,
    fail: usize,
    pending: usize,
    skipped: usize,
}

#[derive(Debug, Serialize)]
struct PrStatus {
    repo: String,
    number: u64,
    title: String,
    url: String,
    /// open, draft, closed, or merged.
    state: String,
    /// Merge state, such as clean, blocked, behind, conflicting, or unknown.
    mergeable: String,
    review: Option<String>,
    head_oid: String,
    commits: u64,
    counts: Counts,
    checks: Vec<Check>,
}

const STATUS_QUERY: &str = r#"
query($owner: String!, $repo: String!, $number: Int!, $cursor: String) {
  repository(owner: $owner, name: $repo) {
    pullRequest(number: $number) {
      title url state isDraft mergeable mergeStateStatus reviewDecision headRefOid
      commits { totalCount }
      last: commits(last: 1) {
        nodes {
          commit {
            statusCheckRollup {
              contexts(first: 100, after: $cursor) {
                pageInfo { hasNextPage endCursor }
                nodes {
                  __typename
                  ... on CheckRun {
                    name status conclusion startedAt detailsUrl
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

fn fetch_status(client: &Client, pr: &PrRef) -> Result<PrStatus> {
    let mut header = None;
    let mut runs = Vec::new();
    let mut statuses = Vec::new();
    let mut cursor = Value::Null;
    loop {
        let mut variables = vars(pr);
        variables["cursor"] = cursor;
        let data = client.graphql(STATUS_QUERY, variables)?;
        let p = &data["repository"]["pullRequest"];
        if p.is_null() {
            bail!("Pull request {pr} not found");
        }
        if header.is_none() {
            header = Some(p.clone());
        }
        let contexts = &p["last"]["nodes"][0]["commit"]["statusCheckRollup"]["contexts"];
        // A commit with no checks at all has a null rollup.
        if contexts.is_null() {
            break;
        }
        for node in contexts["nodes"].as_array().into_iter().flatten() {
            match node["__typename"].as_str() {
                Some("CheckRun") => runs.push(parse_check_run(node)),
                Some("StatusContext") => statuses.push(node.clone()),
                _ => {}
            }
        }
        if contexts["pageInfo"]["hasNextPage"].as_bool() != Some(true) {
            break;
        }
        cursor = contexts["pageInfo"]["endCursor"].clone();
    }
    let p = header.unwrap_or_default();
    Ok(build(pr, &p, runs, &statuses))
}

/// Fold the PR fields and its head's checks into one status.
fn build(pr: &PrRef, p: &Value, runs: Vec<CheckRun>, statuses: &[Value]) -> PrStatus {
    let mut checks: Vec<Check> = latest(runs)
        .iter()
        .map(|r| Check {
            name: if r.workflow.is_empty() {
                r.name.clone()
            } else {
                format!("{} / {}", r.workflow, r.name)
            },
            state: State::of_run(r),
            url: r.url.clone(),
        })
        .collect();
    checks.extend(statuses.iter().map(|s| Check {
        name: str_of(s, "context"),
        state: State::of_status(s["state"].as_str().unwrap_or_default()),
        url: s["targetUrl"].as_str().map(str::to_string),
    }));

    let mut counts = Counts::default();
    for c in &checks {
        match c.state {
            State::Pass => counts.pass += 1,
            State::Fail => counts.fail += 1,
            State::Pending => counts.pending += 1,
            State::Skipped => counts.skipped += 1,
        }
    }

    let state = match p["state"].as_str() {
        Some("OPEN") if p["isDraft"].as_bool() == Some(true) => "draft".to_string(),
        Some(s) => s.to_lowercase(),
        None => "unknown".to_string(),
    };
    PrStatus {
        repo: pr.repo.to_string(),
        number: pr.number,
        title: str_of(p, "title"),
        url: str_of(p, "url"),
        state,
        mergeable: merge_state(p),
        review: p["reviewDecision"].as_str().map(|r| r.to_lowercase()),
        head_oid: str_of(p, "headRefOid"),
        commits: p["commits"]["totalCount"].as_u64().unwrap_or(0),
        counts,
        checks,
    }
}

/// Merge state in plain words. GitHub calls a conflicting PR "dirty".
fn merge_state(p: &Value) -> String {
    if p["mergeable"].as_str() == Some("CONFLICTING") {
        return "conflicting".to_string();
    }
    match p["mergeStateStatus"].as_str() {
        Some("DIRTY") => "conflicting".to_string(),
        Some(s) => s.to_lowercase(),
        None => "unknown".to_string(),
    }
}

// -------------------------------------------------------------------------
//  render
// -------------------------------------------------------------------------

/// One summary line, then the failing and pending checks indented below it.
fn render(status: &PrStatus, full_ref: bool) -> String {
    let id = if full_ref {
        format!("{}#{}", status.repo, status.number)
    } else {
        format!("#{}", status.number)
    };
    let head: String = status.head_oid.chars().take(8).collect();
    let plural = if status.commits == 1 { "" } else { "s" };
    let mut line = format!(
        "{id} {}  {}  mergeable={}  head={head} ({} commit{plural})",
        status.title, status.state, status.mergeable, status.commits
    );
    if let Some(review) = &status.review {
        line.push_str(&format!("  review={review}"));
    }
    line.push_str(&format!("  checks: {}", counts_text(&status.counts)));

    let mut out = line + "\n";
    for state in [State::Fail, State::Pending] {
        for c in status.checks.iter().filter(|c| c.state == state) {
            let url = c.url.as_deref().unwrap_or_default();
            let row = format!("  {:<8} {}  {url}", state.label(), c.name);
            out.push_str(row.trim_end());
            out.push('\n');
        }
    }
    out
}

fn counts_text(c: &Counts) -> String {
    if *c == Counts::default() {
        return "none".to_string();
    }
    let mut parts = vec![
        format!("{} pass", c.pass),
        format!("{} fail", c.fail),
        format!("{} pending", c.pending),
    ];
    if c.skipped > 0 {
        parts.push(format!("{} skipped", c.skipped));
    }
    parts.join(", ")
}

// -------------------------------------------------------------------------
//  run
// -------------------------------------------------------------------------

pub fn run(ctx: &Ctx, args: ChecksArgs) -> Result<()> {
    let prs = resolve_prs(ctx, &args.prs)?;
    let statuses = prs
        .iter()
        .map(|pr| fetch_status(&ctx.client, pr))
        .collect::<Result<Vec<_>>>()?;
    report(ctx, &statuses)
}

/// Each PR argument, or the current branch's PR when there are none.
fn resolve_prs(ctx: &Ctx, args: &[String]) -> Result<Vec<PrRef>> {
    if args.is_empty() {
        return Ok(vec![ctx.pr(None)?]);
    }
    args.iter().map(|a| ctx.pr(Some(a))).collect()
}

/// Print the statuses, failing when any check failed so scripts can branch on the exit code.
fn report(ctx: &Ctx, statuses: &[PrStatus]) -> Result<()> {
    if ctx.json {
        output::print_json(statuses)?;
    } else {
        let repos: HashSet<&str> = statuses.iter().map(|s| s.repo.as_str()).collect();
        for s in statuses {
            print!("{}", render(s, repos.len() > 1));
        }
    }
    let failed: usize = statuses.iter().map(|s| s.counts.fail).sum();
    if failed > 0 {
        let plural = if failed == 1 { "" } else { "s" };
        bail!("{failed} check{plural} failed");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::Repo;
    use serde_json::json;

    fn pr() -> PrRef {
        PrRef {
            repo: Repo::parse("zlib-ng/zlib-ng").unwrap(),
            number: 42,
        }
    }

    fn run_node(name: &str, workflow: &str, status: &str, conclusion: Option<&str>) -> Value {
        json!({
            "__typename": "CheckRun",
            "name": name, "status": status, "conclusion": conclusion,
            "startedAt": "2026-01-01T00:00:00Z",
            "detailsUrl": format!("https://github.com/zlib-ng/zlib-ng/actions/runs/1/job/{name}"),
            "checkSuite": { "workflowRun": {
                "databaseId": 1, "event": "pull_request", "workflow": { "name": workflow }
            }}
        })
    }

    fn header() -> Value {
        json!({
            "title": "Fix thing", "url": "https://github.com/zlib-ng/zlib-ng/pull/42",
            "state": "OPEN", "isDraft": false, "mergeable": "MERGEABLE",
            "mergeStateStatus": "BLOCKED", "reviewDecision": "REVIEW_REQUIRED",
            "headRefOid": "1a2b3c4d5e6f", "commits": { "totalCount": 3 }
        })
    }

    #[test]
    fn states_fold_runs_and_statuses() {
        let runs = [
            run_node("Ubuntu", "Build", "COMPLETED", Some("SUCCESS")),
            run_node("macOS", "Build", "COMPLETED", Some("FAILURE")),
            run_node("Docs", "Docs", "IN_PROGRESS", None),
            run_node("Fuzz", "Fuzz", "COMPLETED", Some("SKIPPED")),
            run_node("Lint", "Lint", "COMPLETED", Some("NEUTRAL")),
            run_node("Fork", "CI", "COMPLETED", Some("ACTION_REQUIRED")),
        ]
        .iter()
        .map(parse_check_run)
        .collect();
        let statuses = [
            json!({"context": "ci/jenkins", "state": "ERROR", "targetUrl": "https://ci"}),
            json!({"context": "codecov", "state": "PENDING", "targetUrl": null}),
            json!({"context": "license", "state": "SUCCESS", "targetUrl": null}),
        ];
        let status = build(&pr(), &header(), runs, &statuses);
        assert_eq!(
            status.counts,
            Counts {
                pass: 2,
                fail: 3,
                pending: 2,
                skipped: 2
            }
        );
        assert_eq!(status.state, "open");
        assert_eq!(status.mergeable, "blocked");
        assert_eq!(status.review.as_deref(), Some("review_required"));
    }

    #[test]
    fn render_lists_failing_then_pending() {
        let runs = [
            run_node("Ubuntu", "Build", "COMPLETED", Some("SUCCESS")),
            run_node("macOS", "Build", "COMPLETED", Some("FAILURE")),
            run_node("Docs", "Docs", "QUEUED", None),
        ]
        .iter()
        .map(parse_check_run)
        .collect();
        let status = build(&pr(), &header(), runs, &[]);
        assert_eq!(
            render(&status, false),
            "#42 Fix thing  open  mergeable=blocked  head=1a2b3c4d (3 commits)  review=review_required  checks: 1 pass, 1 fail, 1 pending\n  \
             fail     Build / macOS  https://github.com/zlib-ng/zlib-ng/actions/runs/1/job/macOS\n  \
             pending  Docs / Docs  https://github.com/zlib-ng/zlib-ng/actions/runs/1/job/Docs\n"
        );
    }

    #[test]
    fn draft_conflicting_without_checks() {
        let mut p = header();
        p["isDraft"] = json!(true);
        p["mergeable"] = json!("CONFLICTING");
        p["mergeStateStatus"] = json!("DIRTY");
        p["reviewDecision"] = Value::Null;
        p["commits"]["totalCount"] = json!(1);
        let status = build(&pr(), &p, Vec::new(), &[]);
        assert_eq!(
            render(&status, true),
            "zlib-ng/zlib-ng#42 Fix thing  draft  mergeable=conflicting  head=1a2b3c4d (1 commit)  checks: none\n"
        );
    }

    #[test]
    fn rerun_supersedes_failed_attempt() {
        let mut failed = run_node("Ubuntu", "Build", "COMPLETED", Some("FAILURE"));
        failed["startedAt"] = json!("2026-01-01T00:00:00Z");
        let mut passed = run_node("Ubuntu", "Build", "COMPLETED", Some("SUCCESS"));
        passed["startedAt"] = json!("2026-01-01T01:00:00Z");
        let runs = [failed, passed].iter().map(parse_check_run).collect();
        let status = build(&pr(), &header(), runs, &[]);
        assert_eq!(status.counts.fail, 0);
        assert_eq!(status.counts.pass, 1);
    }
}
