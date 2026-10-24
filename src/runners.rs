use anyhow::{Result, bail};
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};
use std::fmt;

use crate::api::{Client, Repo};
use crate::cli::RunnersArgs;
use crate::context::Ctx;
use crate::dependabot::Scope;
use crate::json::{opt_time_of, str_of};
use crate::output::table;
use crate::{output, parallel};

/// A self-hosted runner as the runners API reports it.
#[derive(Debug, PartialEq, Serialize)]
struct Runner {
    id: u64,
    name: String,
    /// "online" or "offline".
    status: String,
    busy: bool,
    os: String,
    labels: Vec<String>,
    /// The job a busy runner is running, when it was found.
    #[serde(skip_serializing_if = "Option::is_none")]
    job: Option<Job>,
}

impl Runner {
    /// Status column: busy, idle, or offline.
    fn state(&self) -> &str {
        if self.status != "online" {
            "offline"
        } else if self.busy {
            "busy"
        } else {
            "idle"
        }
    }
}

fn parse_runner(v: &Value) -> Runner {
    Runner {
        id: v["id"].as_u64().unwrap_or(0),
        name: str_of(v, "name"),
        status: str_of(v, "status"),
        busy: v["busy"].as_bool().unwrap_or(false),
        os: str_of(v, "os"),
        labels: v["labels"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|l| l["name"].as_str().map(str::to_string))
            .collect(),
        job: None,
    }
}

/// Runners API path for the scope.
fn runners_path(scope: &Scope) -> String {
    match scope {
        Scope::Repo(r) => format!("{}/actions/runners", r.api()),
        Scope::Org(o) => format!("orgs/{o}/actions/runners"),
    }
}

/// Whether an API error means the token lacks the access the runners API needs.
fn is_forbidden(e: &anyhow::Error) -> bool {
    let text = format!("{e:#}");
    text.contains("GitHub API 403") || text.contains("GitHub API 404")
}

/// What to do when the runners API refuses the token.
fn access_hint(scope: &Scope) -> String {
    match scope {
        Scope::Org(o) => format!(
            "Listing runners for {o} needs an org admin and the admin:org scope. Run `gh auth refresh -s admin:org`."
        ),
        Scope::Repo(r) => format!("Listing runners for {r} needs admin access to the repo."),
    }
}

/// A workflow job, usually one running or waiting for a runner.
#[derive(Clone, Debug, PartialEq, Serialize)]
struct Job {
    repo: String,
    workflow: String,
    name: String,
    run_id: u64,
    job_id: u64,
    /// "in_progress", "queued", or "completed".
    status: String,
    /// Runner the job was assigned to, absent while queued.
    runner: Option<String>,
    /// Labels the job asked for in `runs-on`.
    labels: Vec<String>,
    created_at: Option<DateTime<Utc>>,
    started_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    completed_at: Option<DateTime<Utc>>,
    url: String,
}

fn parse_job(v: &Value, repo: &Repo, workflow: &str) -> Job {
    Job {
        repo: repo.to_string(),
        workflow: v["workflow_name"].as_str().unwrap_or(workflow).to_string(),
        name: str_of(v, "name"),
        run_id: v["run_id"].as_u64().unwrap_or(0),
        job_id: v["id"].as_u64().unwrap_or(0),
        status: str_of(v, "status"),
        runner: v["runner_name"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        labels: v["labels"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|l| l.as_str().map(str::to_string))
            .collect(),
        created_at: opt_time_of(v, "created_at"),
        started_at: opt_time_of(v, "started_at"),
        completed_at: opt_time_of(v, "completed_at"),
        url: str_of(v, "html_url"),
    }
}

/// Running and queued jobs in a repo. Queued jobs often sit inside in-progress runs, as with a
/// matrix that outgrows its runners, so both kinds of run are scanned.
fn active_jobs(client: &Client, repo: &Repo) -> Result<Vec<Job>> {
    let mut seen = HashSet::new();
    let mut jobs = Vec::new();
    for status in ["in_progress", "queued"] {
        let runs = client.get_all_in(
            &format!("{}/actions/runs?status={status}", repo.api()),
            "workflow_runs",
        )?;
        for run in runs {
            let Some(id) = run["id"].as_u64() else {
                continue;
            };
            if !seen.insert(id) {
                continue;
            }
            let workflow = run["name"].as_str().unwrap_or_default();
            let path = format!("{}/actions/runs/{id}/jobs?filter=latest", repo.api());
            for j in client.get_all_in(&path, "jobs")? {
                let job = parse_job(&j, repo, workflow);
                if matches!(job.status.as_str(), "in_progress" | "queued") {
                    jobs.push(job);
                }
            }
        }
    }
    Ok(jobs)
}

/// Repositories of an organization that can still run workflows.
fn org_repos(client: &Client, org: &str) -> Result<Vec<Repo>> {
    let repos = client.get_all(&format!("orgs/{org}/repos?type=all"))?;
    Ok(repos
        .iter()
        .filter(|r| r["archived"].as_bool() != Some(true))
        .filter_map(|r| Repo::parse(r["full_name"].as_str()?).ok())
        .collect())
}

/// Run `f` over every item a few at a time. An item that fails is skipped with a warning, so one
/// repo without Actions doesn't hide the rest.
fn in_parallel<I: Sync + fmt::Display, T: Send>(
    items: &[I],
    f: impl Fn(&I) -> Result<Vec<T>> + Sync,
) -> Vec<T> {
    let mut out = Vec::new();
    for (item, result) in items.iter().zip(parallel::map(items, f)) {
        match result {
            Ok(found) => out.extend(found),
            Err(e) => eprintln!("warning: skipped {item}: {e:#}"),
        }
    }
    out
}

/// Attach each running job to its runner and return the jobs still waiting for one, oldest first.
fn assign(runners: &mut [Runner], jobs: Vec<Job>) -> Vec<Job> {
    let mut queued = Vec::new();
    for job in jobs {
        if job.status == "queued" {
            queued.push(job);
            continue;
        }
        if let Some(r) = runners
            .iter_mut()
            .find(|r| job.runner.as_deref() == Some(r.name.as_str()))
        {
            r.job = Some(job);
        }
    }
    queued.sort_by_key(|j| j.created_at);
    queued
}

/// A workflow run whose jobs show which runners picked them up.
struct RunRef {
    repo: Repo,
    id: u64,
    workflow: String,
}

impl fmt::Display for RunRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} run {}", self.repo, self.id)
    }
}

/// A self-hosted runner seen in recent jobs, for tokens the runners API refuses.
#[derive(Debug, PartialEq, Serialize)]
struct SeenRunner {
    name: String,
    /// Labels its jobs asked for, which may be fewer than the runner has.
    labels: Vec<String>,
    /// Whether one of its jobs is in progress.
    running: bool,
    last_seen: Option<DateTime<Utc>>,
    last_job: Job,
}

/// The most recent `runs` runs of a repo.
fn recent_runs(client: &Client, repo: &Repo, runs: usize) -> Result<Vec<RunRef>> {
    let page = client.get(&format!(
        "{}/actions/runs?per_page={}",
        repo.api(),
        runs.clamp(1, 100)
    ))?;
    Ok(page["workflow_runs"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|r| {
            Some(RunRef {
                repo: repo.clone(),
                id: r["id"].as_u64()?,
                workflow: str_of(r, "name"),
            })
        })
        .collect())
}

/// The `limit` most recently pushed repositories of an organization.
fn recent_org_repos(client: &Client, org: &str, limit: usize) -> Result<Vec<Repo>> {
    let page = client.get(&format!(
        "orgs/{org}/repos?type=all&sort=pushed&direction=desc&per_page=100"
    ))?;
    Ok(page
        .as_array()
        .into_iter()
        .flatten()
        .filter(|r| r["archived"].as_bool() != Some(true))
        .filter_map(|r| Repo::parse(r["full_name"].as_str()?).ok())
        .take(limit)
        .collect())
}

/// Every job of the recent runs in `repos`.
fn recent_jobs(client: &Client, repos: &[Repo], runs: usize) -> Vec<Job> {
    let refs = in_parallel(repos, |repo| recent_runs(client, repo, runs));
    in_parallel(&refs, |run| {
        let path = format!(
            "{}/actions/runs/{}/jobs?filter=latest",
            run.repo.api(),
            run.id
        );
        Ok(client
            .get_all_in(&path, "jobs")?
            .iter()
            .map(|j| parse_job(j, &run.repo, &run.workflow))
            .collect())
    })
}

/// GitHub-hosted runners are named `GitHub Actions N` and aren't worth listing.
fn is_hosted(runner: &str) -> bool {
    runner.starts_with("GitHub Actions ")
}

/// When a job last showed its runner was alive.
fn seen_at(job: &Job) -> Option<DateTime<Utc>> {
    job.completed_at.or(job.started_at)
}

/// Group jobs by the self-hosted runner that ran them, keeping each runner's latest job.
fn infer(jobs: &[Job]) -> Vec<SeenRunner> {
    let mut by_name: BTreeMap<String, SeenRunner> = BTreeMap::new();
    for job in jobs {
        let Some(name) = job.runner.as_deref().filter(|n| !is_hosted(n)) else {
            continue;
        };
        let seen = by_name
            .entry(name.to_string())
            .or_insert_with(|| SeenRunner {
                name: name.to_string(),
                labels: Vec::new(),
                running: false,
                last_seen: None,
                last_job: job.clone(),
            });
        for label in &job.labels {
            if !seen.labels.contains(label) {
                seen.labels.push(label.clone());
            }
        }
        // A running job is the most recent one a runner can have.
        let running = job.status == "in_progress";
        if running || (!seen.running && seen_at(job) >= seen.last_seen) {
            seen.last_job = job.clone();
            seen.last_seen = seen_at(job);
        }
        seen.running |= running;
    }
    by_name.into_values().collect()
}

#[derive(Serialize)]
struct InferredReport {
    scope: String,
    /// Always true, so scripts can tell this from the runners API listing.
    inferred: bool,
    runners: Vec<SeenRunner>,
    queued: Vec<Job>,
}

/// Runners seen in recent jobs, then the jobs still queued among them.
fn render_inferred(report: &InferredReport, runs: usize, now: DateTime<Utc>) -> String {
    let mut out = format!(
        "Inferred from the jobs of the last {runs} runs, not the runners API, so idle and offline runners look alike.\n"
    );
    if report.runners.is_empty() {
        out.push_str(&format!(
            "No self-hosted runners ran those jobs in {}.\n",
            report.scope
        ));
    } else {
        let mut rows = vec![vec![
            "NAME".to_string(),
            "LAST SEEN".to_string(),
            "LAST JOB".to_string(),
            "LABELS".to_string(),
        ]];
        for r in &report.runners {
            let seen = match (r.running, r.last_seen) {
                (true, _) => "running".to_string(),
                (false, Some(t)) => format!("{} ago", elapsed(t, now)),
                (false, None) => "unknown".to_string(),
            };
            let j = &r.last_job;
            rows.push(vec![
                r.name.clone(),
                seen,
                format!("{}  {} / {}", j.repo, j.workflow, j.name),
                r.labels.join(", "),
            ]);
        }
        out.push_str(&table(&rows, ""));
        out.push('\n');
    }
    out.push_str(&render_queue(&report.queued, now));
    out
}

#[derive(Serialize)]
struct Report {
    scope: String,
    runners: Vec<Runner>,
    /// Jobs waiting for a runner. Absent when jobs were not looked up.
    #[serde(skip_serializing_if = "Option::is_none")]
    queued: Option<Vec<Job>>,
}

pub fn run(ctx: &Ctx, args: RunnersArgs) -> Result<()> {
    let client = &ctx.client;
    let scope = Scope::new(ctx, args.org.clone())?;
    if args.from_jobs {
        return run_inferred(ctx, &scope, &args);
    }
    let mut runners: Vec<Runner> = match client.get_all_in(&runners_path(&scope), "runners") {
        Ok(list) => list.iter().map(parse_runner).collect(),
        Err(e) if is_forbidden(&e) => {
            eprintln!(
                "warning: {} Falling back to recent jobs.",
                access_hint(&scope)
            );
            return run_inferred(ctx, &scope, &args);
        }
        Err(e) => return Err(e),
    };
    runners.sort_by_key(|r| r.name.to_lowercase());

    // Scanning an organization costs a few requests per repo, so only do it when there is
    // something to find.
    let jobs = match &scope {
        Scope::Repo(repo) => Some(active_jobs(client, repo)?),
        Scope::Org(org) if args.queued || runners.iter().any(|r| r.busy) => {
            let repos = org_repos(client, org)?;
            Some(in_parallel(&repos, |repo| active_jobs(client, repo)))
        }
        Scope::Org(_) => None,
    };
    let queued = jobs.map(|jobs| assign(&mut runners, jobs));

    let report = Report {
        scope: scope.to_string(),
        runners,
        queued,
    };
    if ctx.json {
        return output::print_json(&report);
    }
    print!("{}", render(&report, &scope, Utc::now()));
    Ok(())
}

/// List runners from the jobs of recent runs, for tokens without admin access.
fn run_inferred(ctx: &Ctx, scope: &Scope, args: &RunnersArgs) -> Result<()> {
    let client = &ctx.client;
    let repos = match scope {
        Scope::Repo(r) => vec![r.clone()],
        Scope::Org(o) => recent_org_repos(client, o, args.repos)?,
    };
    if repos.is_empty() {
        bail!("No repositories to scan in {scope}");
    }
    let jobs = recent_jobs(client, &repos, args.runs);
    let mut queued: Vec<Job> = jobs
        .iter()
        .filter(|j| j.status == "queued")
        .cloned()
        .collect();
    queued.sort_by_key(|j| j.created_at);
    let report = InferredReport {
        scope: scope.to_string(),
        inferred: true,
        runners: infer(&jobs),
        queued,
    };
    if ctx.json {
        return output::print_json(&report);
    }
    print!("{}", render_inferred(&report, args.runs, Utc::now()));
    Ok(())
}

/// Runner table and count summary, then busy runners' jobs and the queue.
fn render(report: &Report, scope: &Scope, now: DateTime<Utc>) -> String {
    let mut out = String::new();
    if report.runners.is_empty() {
        let hint = match scope {
            Scope::Repo(_) => " Runners registered to the organization show with --org ORG.",
            Scope::Org(_) => "",
        };
        out.push_str(&format!(
            "No self-hosted runners in {}.{hint}\n",
            report.scope
        ));
    } else {
        let mut rows = vec![vec![
            "NAME".to_string(),
            "STATUS".to_string(),
            "OS".to_string(),
            "LABELS".to_string(),
        ]];
        for r in &report.runners {
            rows.push(vec![
                r.name.clone(),
                r.state().to_string(),
                r.os.clone(),
                r.labels.join(", "),
            ]);
        }
        out.push_str(&format!(
            "{}\n{}\n",
            table(&rows, ""),
            summary(&report.runners)
        ));
    }

    // Jobs are only known when they were looked up.
    let Some(queued) = &report.queued else {
        return out;
    };
    let busy: Vec<&Runner> = report
        .runners
        .iter()
        .filter(|r| r.status == "online" && r.busy)
        .collect();
    if !busy.is_empty() {
        out.push_str("\nRunning:\n");
        let rows: Vec<Vec<String>> = busy
            .iter()
            .map(|r| {
                let detail = match &r.job {
                    Some(j) => {
                        let since = j
                            .started_at
                            .map(|t| format!("  for {}", elapsed(t, now)))
                            .unwrap_or_default();
                        format!("{}  {} / {}{since}", j.repo, j.workflow, j.name)
                    }
                    None => "job not found".to_string(),
                };
                vec![r.name.clone(), detail]
            })
            .collect();
        out.push_str(&table(&rows, "  "));
        out.push('\n');
    }
    out.push_str(&render_queue(queued, now));
    out
}

/// Jobs waiting for a runner, with how long they've waited and the labels they asked for.
fn render_queue(queued: &[Job], now: DateTime<Utc>) -> String {
    if queued.is_empty() {
        return String::new();
    }
    let rows: Vec<Vec<String>> = queued
        .iter()
        .map(|j| {
            let waited = j
                .created_at
                .map(|t| format!("waiting {}", elapsed(t, now)))
                .unwrap_or_else(|| "waiting".to_string());
            vec![
                j.repo.clone(),
                format!("{} / {}", j.workflow, j.name),
                format!("{waited} for {}", j.labels.join(", ")),
            ]
        })
        .collect();
    format!("\nQueued ({}):\n{}\n", queued.len(), table(&rows, "  "))
}

/// Counts line, as `12 runners: 9 online (4 busy), 3 offline`.
fn summary(runners: &[Runner]) -> String {
    let online = runners.iter().filter(|r| r.status == "online").count();
    let busy = runners
        .iter()
        .filter(|r| r.status == "online" && r.busy)
        .count();
    let offline = runners.len() - online;
    let noun = if runners.len() == 1 {
        "runner"
    } else {
        "runners"
    };
    format!(
        "{} {noun}: {online} online ({busy} busy), {offline} offline",
        runners.len()
    )
}

/// Time since `from`, as `45s`, `12m`, or `2h 5m`.
fn elapsed(from: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let secs = (now - from).num_seconds().max(0);
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m", s / 60),
        s => format!("{}h {}m", s / 3600, (s % 3600) / 60),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn runner(name: &str, status: &str, busy: bool) -> Runner {
        parse_runner(&json!({
            "id": 1, "name": name, "os": "Linux", "status": status, "busy": busy,
            "labels": [
                { "id": 1, "name": "self-hosted", "type": "read-only" },
                { "id": 2, "name": "linux", "type": "custom" }
            ]
        }))
    }

    fn job(name: &str, status: &str, runner: Option<&str>, at: &str) -> Job {
        parse_job(
            &json!({
                "id": 7, "run_id": 3, "name": name, "status": status,
                "workflow_name": "CI", "runner_name": runner,
                "labels": ["self-hosted", "windows"],
                "created_at": at, "started_at": at,
                "html_url": "https://github.com/a/b/actions/runs/3/job/7"
            }),
            &Repo::parse("a/b").unwrap(),
            "fallback",
        )
    }

    fn at(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    #[test]
    fn parses_runner_and_state() {
        let r = runner("build-1", "online", true);
        assert_eq!(r.labels, vec!["self-hosted", "linux"]);
        assert_eq!(r.state(), "busy");
        assert_eq!(runner("b", "online", false).state(), "idle");
        // An offline runner can still report busy from its last job.
        assert_eq!(runner("c", "offline", true).state(), "offline");
    }

    #[test]
    fn summary_counts() {
        let runners = vec![
            runner("a", "online", true),
            runner("b", "online", false),
            runner("c", "offline", true),
        ];
        assert_eq!(summary(&runners), "3 runners: 2 online (1 busy), 1 offline");
        assert_eq!(
            summary(&runners[..1]),
            "1 runner: 1 online (1 busy), 0 offline"
        );
    }

    #[test]
    fn parses_job_and_drops_empty_runner() {
        let j = job("Build", "queued", Some(""), "2026-01-01T00:00:00Z");
        assert_eq!(j.runner, None);
        assert_eq!(j.workflow, "CI");
        assert_eq!(j.labels, vec!["self-hosted", "windows"]);
        assert_eq!(j.created_at, Some(at("2026-01-01T00:00:00Z")));
    }

    #[test]
    fn assign_matches_runner_names_and_sorts_queue() {
        let mut runners = vec![runner("a", "online", true), runner("b", "online", false)];
        let jobs = vec![
            job("Late", "queued", None, "2026-01-01T00:10:00Z"),
            job("Build", "in_progress", Some("a"), "2026-01-01T00:00:00Z"),
            job("Early", "queued", None, "2026-01-01T00:05:00Z"),
            job(
                "Hosted",
                "in_progress",
                Some("GitHub Actions 3"),
                "2026-01-01T00:00:00Z",
            ),
        ];
        let queued = assign(&mut runners, jobs);
        assert_eq!(runners[0].job.as_ref().unwrap().name, "Build");
        assert!(runners[1].job.is_none());
        let names: Vec<&str> = queued.iter().map(|j| j.name.as_str()).collect();
        assert_eq!(names, vec!["Early", "Late"]);
    }

    #[test]
    fn render_table_jobs_and_queue() {
        let scope = Scope::Repo(Repo::parse("a/b").unwrap());
        let mut runners = vec![
            runner("build-1", "online", true),
            runner("build-2", "online", true),
            runner("build-3", "online", false),
        ];
        let queued = assign(
            &mut runners,
            vec![
                job(
                    "Build",
                    "in_progress",
                    Some("build-1"),
                    "2026-01-01T00:00:00Z",
                ),
                job("Test", "queued", None, "2026-01-01T00:46:00Z"),
            ],
        );
        let report = Report {
            scope: "a/b".to_string(),
            runners,
            queued: Some(queued),
        };
        assert_eq!(
            render(&report, &scope, at("2026-01-01T01:05:00Z")),
            "NAME     STATUS  OS     LABELS\n\
             build-1  busy    Linux  self-hosted, linux\n\
             build-2  busy    Linux  self-hosted, linux\n\
             build-3  idle    Linux  self-hosted, linux\n\
             3 runners: 3 online (2 busy), 0 offline\n\
             \n\
             Running:\n\
             \x20 build-1  a/b  CI / Build  for 1h 5m\n\
             \x20 build-2  job not found\n\
             \n\
             Queued (1):\n\
             \x20 a/b  CI / Test  waiting 19m for self-hosted, windows\n"
        );
    }

    #[test]
    fn render_without_job_lookup_or_runners() {
        let scope = Scope::Repo(Repo::parse("a/b").unwrap());
        let report = Report {
            scope: "a/b".to_string(),
            runners: vec![runner("build-1", "online", false)],
            queued: None,
        };
        assert_eq!(
            render(&report, &scope, Utc::now()),
            "NAME     STATUS  OS     LABELS\n\
             build-1  idle    Linux  self-hosted, linux\n\
             1 runner: 1 online (0 busy), 0 offline\n"
        );
        let empty = Report {
            scope: "a/b".to_string(),
            runners: Vec::new(),
            queued: Some(Vec::new()),
        };
        assert!(render(&empty, &scope, Utc::now()).contains("--org ORG"));
    }

    fn done(name: &str, runner: &str, labels: &[&str], at: &str) -> Job {
        let mut j = job(name, "completed", Some(runner), at);
        j.labels = labels.iter().map(|l| l.to_string()).collect();
        j.completed_at = j.started_at;
        j
    }

    #[test]
    fn infer_groups_by_runner_and_prefers_running_jobs() {
        let jobs = vec![
            done("Old", "box-1", &["self-hosted"], "2026-01-01T00:00:00Z"),
            done(
                "New",
                "box-1",
                &["self-hosted", "windows"],
                "2026-01-01T01:00:00Z",
            ),
            job("Live", "in_progress", Some("box-2"), "2026-01-01T00:30:00Z"),
            done("Later", "box-2", &["self-hosted"], "2026-01-01T02:00:00Z"),
            done(
                "Hosted",
                "GitHub Actions 12",
                &["ubuntu-latest"],
                "2026-01-01T00:00:00Z",
            ),
            job("Waiting", "queued", None, "2026-01-01T00:00:00Z"),
        ];
        let seen = infer(&jobs);
        let summary: Vec<(&str, bool, &str)> = seen
            .iter()
            .map(|r| (r.name.as_str(), r.running, r.last_job.name.as_str()))
            .collect();
        assert_eq!(
            summary,
            vec![("box-1", false, "New"), ("box-2", true, "Live")]
        );
        assert_eq!(seen[0].labels, vec!["self-hosted", "windows"]);
        assert_eq!(seen[0].last_seen, Some(at("2026-01-01T01:00:00Z")));
    }

    #[test]
    fn render_inferred_marks_running_and_age() {
        let jobs = vec![
            done("Build", "box-1", &["self-hosted"], "2026-01-01T00:00:00Z"),
            job("Test", "in_progress", Some("box-2"), "2026-01-01T00:50:00Z"),
        ];
        let report = InferredReport {
            scope: "a/b".to_string(),
            inferred: true,
            runners: infer(&jobs),
            queued: Vec::new(),
        };
        assert_eq!(
            render_inferred(&report, 30, at("2026-01-01T01:00:00Z")),
            "Inferred from the jobs of the last 30 runs, not the runners API, so idle and offline runners look alike.\n\
             NAME   LAST SEEN  LAST JOB         LABELS\n\
             box-1  1h 0m ago  a/b  CI / Build  self-hosted\n\
             box-2  running    a/b  CI / Test   self-hosted, windows\n"
        );
    }

    #[test]
    fn elapsed_units() {
        let t = at("2026-01-01T00:00:00Z");
        assert_eq!(elapsed(t, at("2026-01-01T00:00:45Z")), "45s");
        assert_eq!(elapsed(t, at("2026-01-01T00:12:30Z")), "12m");
        assert_eq!(elapsed(t, at("2026-01-01T02:05:00Z")), "2h 5m");
        assert_eq!(elapsed(at("2026-01-01T00:01:00Z"), t), "0s");
    }

    #[test]
    fn forbidden_detection() {
        assert!(is_forbidden(&anyhow::anyhow!(
            "GitHub API 403 on GET orgs/x/actions/runners: Must have admin rights"
        )));
        assert!(!is_forbidden(&anyhow::anyhow!(
            "GitHub API 500 on GET x: oops"
        )));
        assert!(access_hint(&Scope::Org("x".to_string())).contains("admin:org"));
    }
}
