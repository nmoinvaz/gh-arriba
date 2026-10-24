use anyhow::{Result, bail};
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;
use std::collections::HashSet;

use crate::cli::RunnersArgs;
use crate::context::{Ctx, Repo};
use crate::dependabot::{Scope, table};
use crate::github::Client;
use crate::output;

// -------------------------------------------------------------------------
//  runners
// -------------------------------------------------------------------------

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
        name: v["name"].as_str().unwrap_or_default().to_string(),
        status: v["status"].as_str().unwrap_or_default().to_string(),
        busy: v["busy"].as_bool().unwrap_or(false),
        os: v["os"].as_str().unwrap_or_default().to_string(),
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

// -------------------------------------------------------------------------
//  jobs
// -------------------------------------------------------------------------

/// A workflow job that is running or waiting for a runner.
#[derive(Clone, Debug, PartialEq, Serialize)]
struct Job {
    repo: String,
    workflow: String,
    name: String,
    run_id: u64,
    job_id: u64,
    /// "in_progress" or "queued".
    status: String,
    /// Runner the job was assigned to, absent while queued.
    runner: Option<String>,
    /// Labels the job asked for in `runs-on`.
    labels: Vec<String>,
    created_at: Option<DateTime<Utc>>,
    started_at: Option<DateTime<Utc>>,
    url: String,
}

fn parse_job(v: &Value, repo: &Repo, workflow: &str) -> Job {
    let time = |key: &str| {
        v[key]
            .as_str()
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|t| t.with_timezone(&Utc))
    };
    Job {
        repo: repo.to_string(),
        workflow: v["workflow_name"].as_str().unwrap_or(workflow).to_string(),
        name: v["name"].as_str().unwrap_or_default().to_string(),
        run_id: v["run_id"].as_u64().unwrap_or(0),
        job_id: v["id"].as_u64().unwrap_or(0),
        status: v["status"].as_str().unwrap_or_default().to_string(),
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
        created_at: time("created_at"),
        started_at: time("started_at"),
        url: v["html_url"].as_str().unwrap_or_default().to_string(),
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

/// Requests in flight at once when scanning an organization.
const PARALLEL_REPOS: usize = 8;

/// Run `f` over every repo a few at a time. A repo that fails is skipped with a warning, so one
/// repo without Actions doesn't hide the rest.
fn each_repo<T: Send>(repos: &[Repo], f: impl Fn(&Repo) -> Result<Vec<T>> + Sync) -> Vec<T> {
    let mut out = Vec::new();
    for batch in repos.chunks(PARALLEL_REPOS) {
        let results: Vec<(&Repo, Result<Vec<T>>)> = std::thread::scope(|s| {
            let handles: Vec<_> = batch
                .iter()
                .map(|repo| {
                    let f = &f;
                    s.spawn(move || (repo, f(repo)))
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        for (repo, result) in results {
            match result {
                Ok(items) => out.extend(items),
                Err(e) => eprintln!("warning: skipped {repo}: {e:#}"),
            }
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

// -------------------------------------------------------------------------
//  run
// -------------------------------------------------------------------------

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
    let scope = Scope::new(ctx, args.org)?;
    let mut runners: Vec<Runner> = match client.get_all_in(&runners_path(&scope), "runners") {
        Ok(list) => list.iter().map(parse_runner).collect(),
        Err(e) if is_forbidden(&e) => bail!("{}\n{e:#}", access_hint(&scope)),
        Err(e) => return Err(e),
    };
    runners.sort_by_key(|r| r.name.to_lowercase());

    // Scanning an organization costs a few requests per repo, so only do it when there is
    // something to find.
    let jobs = match &scope {
        Scope::Repo(repo) => Some(active_jobs(client, repo)?),
        Scope::Org(org) if args.queued || runners.iter().any(|r| r.busy) => {
            let repos = org_repos(client, org)?;
            Some(each_repo(&repos, |repo| active_jobs(client, repo)))
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
    if !queued.is_empty() {
        out.push_str(&format!("\nQueued ({}):\n", queued.len()));
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
        out.push_str(&table(&rows, "  "));
        out.push('\n');
    }
    out
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
