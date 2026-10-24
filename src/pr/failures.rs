use anyhow::{Context, Result};
use serde::Serialize;
use std::fs;
use std::path::PathBuf;

use crate::api::actions::{self, CheckRun, Job};
use crate::api::{Repo, is_failed};
use crate::cli::FailuresArgs;
use crate::context::Ctx;
use crate::log::{self, Excerpt};
use crate::pr::model::fetch_pr;
use crate::run::{download, name_matches, short_error};
use crate::{output, parallel};

/// A failed check run from the check-runs API.
#[derive(Debug, PartialEq)]
struct FailedCheck {
    /// Check run ID, which is the job ID for GitHub Actions.
    id: u64,
    name: String,
    conclusion: String,
    url: String,
    actions: bool,
}

/// Failed checks and the number still running.
fn classify(check_runs: &[CheckRun], patterns: &[String]) -> (Vec<FailedCheck>, usize) {
    let mut failed = Vec::new();
    let mut pending = 0;
    for c in check_runs {
        if !name_matches(&c.name, patterns) {
            continue;
        }
        if c.status != "completed" {
            pending += 1;
            continue;
        }
        let conclusion = c.conclusion.as_deref();
        if !is_failed(conclusion) {
            continue;
        }
        failed.push(FailedCheck {
            id: c.id,
            name: c.name.clone(),
            conclusion: conclusion.unwrap_or_default().to_string(),
            url: c.url().to_string(),
            actions: c.is_actions(),
        });
    }
    (failed, pending)
}

#[derive(Debug, Serialize)]
struct FailedJob {
    job_id: u64,
    run_id: u64,
    workflow: String,
    name: String,
    conclusion: String,
    url: String,
    failed_steps: Vec<String>,
    log_path: Option<PathBuf>,
    /// Whether the log was saved by an earlier call instead of downloaded now.
    cached: bool,
    excerpt: Option<Excerpt>,
    /// Why the log is missing, such as a job cancelled before it started.
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Debug, Serialize)]
struct Skipped {
    name: String,
    conclusion: String,
    url: String,
}

#[derive(Serialize)]
struct Report {
    pr: String,
    title: String,
    head_sha: String,
    pending: usize,
    jobs: Vec<FailedJob>,
    /// The `--grep` pattern the excerpts were cut with, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    grep: Option<String>,
    /// Failed checks from other apps and commit statuses, which have no Actions log.
    skipped: Vec<Skipped>,
}

/// Steps of a job that did not succeed.
fn failed_steps(job: &Job) -> Vec<String> {
    job.steps
        .iter()
        .filter(|s| is_failed(s.conclusion.as_deref()))
        .filter_map(|s| s.name.clone())
        .collect()
}

/// Fetch a failed Actions job, save its log under `base/<run_id>`, and cut its error excerpt.
fn inspect(
    ctx: &Ctx,
    repo: &Repo,
    check: &FailedCheck,
    base: &std::path::Path,
    args: &FailuresArgs,
) -> FailedJob {
    let mut job = FailedJob {
        job_id: check.id,
        run_id: 0,
        workflow: String::new(),
        name: check.name.clone(),
        conclusion: check.conclusion.clone(),
        url: check.url.clone(),
        failed_steps: Vec::new(),
        log_path: None,
        cached: false,
        excerpt: None,
        error: None,
    };
    let details = match actions::job(&ctx.client, repo, check.id) {
        Ok(d) => d,
        Err(e) => {
            job.error = Some(short_error(&e));
            return job;
        }
    };
    job.run_id = details.run_id;
    job.failed_steps = failed_steps(&details);
    job.workflow = details.workflow_name.unwrap_or_default();

    let dir = base.join(job.run_id.to_string());
    let saved = fs::create_dir_all(&dir)
        .with_context(|| format!("Failed to create {}", dir.display()))
        // A failed job has finished, so its saved log is complete.
        .and_then(|_| download(ctx, repo, check.id, &dir, &check.name, !args.refresh))
        .and_then(|(path, cached)| {
            let text = fs::read_to_string(&path)
                .with_context(|| format!("Failed to read {}", path.display()))?;
            Ok((path, cached, text))
        });
    match saved {
        Ok((path, cached, text)) => {
            job.excerpt = Some(log::select(
                &text,
                args.grep.as_ref(),
                args.context,
                args.max,
            ));
            job.log_path = Some(path);
            job.cached = cached;
        }
        Err(e) => job.error = Some(short_error(&e)),
    }
    job
}

pub fn run(ctx: &Ctx, args: FailuresArgs) -> Result<()> {
    let pr = ctx.pr(args.pr.as_deref())?;
    let client = &ctx.client;
    let info = fetch_pr(client, &pr)?;
    let sha = &info.head_oid;

    let check_runs = actions::check_runs(client, &pr.repo, sha)?;
    let (failed, mut pending) = classify(&check_runs, &args.jobs);

    let mut skipped: Vec<Skipped> = failed
        .iter()
        .filter(|c| !c.actions)
        .map(|c| Skipped {
            name: c.name.clone(),
            conclusion: c.conclusion.clone(),
            url: c.url.clone(),
        })
        .collect();
    // Commit statuses come from external services and never have an Actions log.
    for s in actions::commit_statuses(client, &pr.repo, sha)? {
        if !name_matches(&s.context, &args.jobs) {
            continue;
        }
        match s.state.as_str() {
            "pending" => pending += 1,
            "failure" | "error" => skipped.push(Skipped {
                name: s.context,
                conclusion: s.state,
                url: s.target_url.unwrap_or_default(),
            }),
            _ => {}
        }
    }

    let base = args.out.clone().unwrap_or_else(log::default_dir);
    let actions: Vec<&FailedCheck> = failed.iter().filter(|c| c.actions).collect();
    // Each job costs a details call and a log download, so fetch a few at a time.
    let jobs = parallel::map(&actions, |check| {
        inspect(ctx, &pr.repo, check, &base, &args)
    });

    let report = Report {
        pr: pr.to_string(),
        title: info.title,
        head_sha: sha.clone(),
        pending,
        jobs,
        grep: args.grep.as_ref().map(|re| re.as_str().to_string()),
        skipped,
    };
    if ctx.json {
        return output::print_json(&report);
    }
    print!("{}", render(&report));
    Ok(())
}

fn render(report: &Report) -> String {
    let failed = report.jobs.len() + report.skipped.len();
    let mut out = format!("{} {}: ", report.pr, report.title);
    out.push_str(&match (failed, report.pending) {
        (0, 0) => "no failed checks".to_string(),
        (0, p) => format!("no failed checks, {p} pending"),
        (f, 0) => format!("{f} failed"),
        (f, p) => format!("{f} failed, {p} pending"),
    });
    out.push('\n');

    for job in &report.jobs {
        let title = if job.workflow.is_empty() {
            job.name.clone()
        } else {
            format!("{} / {}", job.workflow, job.name)
        };
        out.push_str(&format!("\n{title} ({})\n", job.conclusion));
        out.push_str(&format!("  {}\n", job.url));
        if !job.failed_steps.is_empty() {
            out.push_str(&format!("  failed step: {}\n", job.failed_steps.join(", ")));
        }
        if let Some(path) = &job.log_path {
            let note = if job.cached { " (saved earlier)" } else { "" };
            out.push_str(&format!("  log: {}{note}\n", path.display()));
        }
        if let Some(e) = &job.error {
            out.push_str(&format!("  {e}\n"));
        }
        if let Some(x) = &job.excerpt {
            out.push_str(&log::render(x, report.grep.as_deref(), "  "));
        }
    }

    if !report.skipped.is_empty() {
        out.push('\n');
    }
    for s in &report.skipped {
        let url = if s.url.is_empty() {
            String::new()
        } else {
            format!(" {}", s.url)
        };
        out.push_str(&format!(
            "skipped external check {} ({}){url}\n",
            s.name, s.conclusion
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::Client;
    use mockito::{Matcher, Server, ServerGuard};
    use serde_json::json;

    fn check(id: u64, name: &str, status: &str, conclusion: Option<&str>, app: &str) -> CheckRun {
        serde_json::from_value(json!({
            "id": id, "name": name, "status": status, "conclusion": conclusion,
            "html_url": format!("https://github.com/o/r/runs/{id}"),
            "details_url": format!("https://example.com/{id}"),
            "app": { "slug": app },
        }))
        .unwrap()
    }

    #[test]
    fn classify_keeps_failures_and_counts_pending() {
        let runs = vec![
            check(
                1,
                "Ubuntu GCC",
                "completed",
                Some("failure"),
                "github-actions",
            ),
            check(
                2,
                "Windows MSVC",
                "completed",
                Some("success"),
                "github-actions",
            ),
            check(3, "macOS", "in_progress", None, "github-actions"),
            check(4, "Lint", "completed", Some("timed_out"), "github-actions"),
            check(
                5,
                "CodeRabbit",
                "completed",
                Some("failure"),
                "coderabbitai",
            ),
            check(6, "Docs", "completed", Some("skipped"), "github-actions"),
        ];
        let (failed, pending) = classify(&runs, &[]);
        let ids: Vec<(u64, bool)> = failed.iter().map(|c| (c.id, c.actions)).collect();
        assert_eq!(ids, vec![(1, true), (4, true), (5, false)]);
        assert_eq!(pending, 1);

        let (failed, pending) = classify(&runs, &["gcc".to_string()]);
        assert_eq!(failed.len(), 1);
        assert_eq!(pending, 0);
    }

    #[test]
    fn failed_steps_by_conclusion() {
        let job: Job = serde_json::from_value(json!({ "id": 1, "steps": [
            { "name": "Checkout", "conclusion": "success" },
            { "name": "Build", "conclusion": "failure" },
            { "name": "Test", "conclusion": "skipped" },
        ]}))
        .unwrap();
        assert_eq!(failed_steps(&job), vec!["Build"]);
    }

    #[test]
    fn render_lists_jobs_and_skipped_checks() {
        let report = Report {
            pr: "o/r#7".to_string(),
            title: "Fix".to_string(),
            head_sha: "abc".to_string(),
            pending: 1,
            jobs: vec![FailedJob {
                job_id: 1,
                run_id: 10,
                workflow: "CI".to_string(),
                name: "Ubuntu".to_string(),
                conclusion: "failure".to_string(),
                url: "https://github.com/o/r/actions/runs/10/job/1".to_string(),
                failed_steps: vec!["Build".to_string()],
                log_path: Some(PathBuf::from("/tmp/l/10/Ubuntu-1.log")),
                cached: true,
                excerpt: Some(log::excerpt("ok\n##[error]boom", 1, 5)),
                error: None,
            }],
            skipped: vec![Skipped {
                name: "ci/jenkins".to_string(),
                conclusion: "error".to_string(),
                url: "https://ci.example.com/1".to_string(),
            }],
            grep: None,
        };
        assert_eq!(
            render(&report),
            "o/r#7 Fix: 2 failed, 1 pending\n\
             \n\
             CI / Ubuntu (failure)\n  \
             https://github.com/o/r/actions/runs/10/job/1\n  \
             failed step: Build\n  \
             log: /tmp/l/10/Ubuntu-1.log (saved earlier)\n  \
             line 1:\n    \
             ok\n    \
             ##[error]boom\n\
             \n\
             skipped external check ci/jenkins (error) https://ci.example.com/1\n"
        );
    }

    fn ctx(server: &ServerGuard, json: bool) -> Ctx {
        Ctx {
            client: Client::for_tests(&server.url()),
            repo_flag: Some("o/r".into()),
            json,
            yes: true,
        }
    }

    fn failures_args(out: &std::path::Path) -> FailuresArgs {
        FailuresArgs {
            pr: Some("o/r#5".to_string()),
            jobs: Vec::new(),
            out: Some(out.to_path_buf()),
            context: log::CONTEXT,
            max: log::MAX,
            grep: None,
            refresh: false,
        }
    }

    /// PR o/r#5 at commit abc with a failed Actions job that has a log, one whose details are
    /// gone, a failed external check, a running check, and two commit statuses.
    fn serve_pr(server: &mut ServerGuard) {
        server
            .mock("POST", "/graphql")
            .with_body(
                r#"{"data": {"repository": {"pullRequest": {
                    "id": "PR_1", "number": 5, "title": "Fix inflate", "url": "u",
                    "state": "OPEN", "createdAt": "2026-01-01T00:00:00Z", "author": null,
                    "headRefOid": "abc"}}}}"#,
            )
            .create();
        server
            .mock("GET", "/repos/o/r/commits/abc/check-runs")
            .match_query(Matcher::Any)
            .with_body(
                r#"{"check_runs": [
                    {"id": 1, "name": "Ubuntu", "status": "completed", "conclusion": "failure",
                     "html_url": "https://h/1", "app": {"slug": "github-actions"}},
                    {"id": 2, "name": "Windows", "status": "completed", "conclusion": "cancelled",
                     "html_url": "https://h/2", "app": {"slug": "github-actions"}},
                    {"id": 3, "name": "CodeRabbit", "status": "completed",
                     "conclusion": "failure", "details_url": "https://cr/3",
                     "app": {"slug": "coderabbitai"}},
                    {"id": 4, "name": "macOS", "status": "queued", "app": {"slug": "github-actions"}}
                ]}"#,
            )
            .create();
        server
            .mock("GET", "/repos/o/r/commits/abc/status")
            .with_body(
                r#"{"statuses": [
                    {"context": "ci/jenkins", "state": "error"},
                    {"context": "ci/slow", "state": "pending"},
                    {"context": "ci/ok", "state": "success"}
                ]}"#,
            )
            .create();
        server
            .mock("GET", "/repos/o/r/actions/jobs/1")
            .with_body(
                r#"{"id": 1, "run_id": 10, "workflow_name": "CI", "steps": [
                    {"name": "Build", "conclusion": "failure"}]}"#,
            )
            .create();
        server
            .mock("GET", "/repos/o/r/actions/jobs/2")
            .with_status(404)
            .create();
    }

    #[test]
    fn saves_failed_logs_and_reuses_them() {
        let mut server = Server::new();
        serve_pr(&mut server);
        let log = server
            .mock("GET", "/repos/o/r/actions/jobs/1/logs")
            .with_body("compiling\nerror: boom\n")
            .expect(2)
            .create();
        let out = std::env::temp_dir().join(format!("gh-arriba-failures-{}", std::process::id()));
        let _ = fs::remove_dir_all(&out);

        run(&ctx(&server, true), failures_args(&out)).unwrap();
        let path = out.join("10").join("Ubuntu-1.log");
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "compiling\nerror: boom\n"
        );
        // The saved log is used the next time, unless refreshed.
        let mut args = failures_args(&out);
        args.grep = Some(regex::Regex::new("boom").unwrap());
        run(&ctx(&server, false), args).unwrap();
        let mut args = failures_args(&out);
        args.refresh = true;
        run(&ctx(&server, true), args).unwrap();
        log.assert();
        fs::remove_dir_all(&out).unwrap();
    }

    #[test]
    fn inspect_reports_why_a_log_is_missing() {
        let mut server = Server::new();
        serve_pr(&mut server);
        server
            .mock("GET", "/repos/o/r/actions/jobs/1/logs")
            .with_status(404)
            .create();
        let out = std::env::temp_dir().join(format!("gh-arriba-inspect-{}", std::process::id()));
        let _ = fs::remove_dir_all(&out);
        let ctx = ctx(&server, true);
        let repo = Repo::parse("o/r").unwrap();
        let args = failures_args(&out);
        let failed = |id, name: &str| FailedCheck {
            id,
            name: name.to_string(),
            conclusion: "failure".to_string(),
            url: String::new(),
            actions: true,
        };

        let job = inspect(&ctx, &repo, &failed(1, "Ubuntu"), &out, &args);
        assert_eq!((job.run_id, job.workflow.as_str()), (10, "CI"));
        assert_eq!(job.failed_steps, vec!["Build"]);
        assert_eq!(
            job.error.as_deref(),
            Some("no log available, the job may not have started")
        );
        assert_eq!(job.log_path, None);

        let job = inspect(&ctx, &repo, &failed(2, "Windows"), &out, &args);
        assert_eq!(job.run_id, 0);
        assert!(job.error.is_some());
        let _ = fs::remove_dir_all(&out);
    }

    #[test]
    fn render_summary_counts() {
        let report = |pending, skipped: usize| Report {
            pr: "o/r#7".to_string(),
            title: "Fix".to_string(),
            head_sha: "abc".to_string(),
            pending,
            jobs: Vec::new(),
            grep: None,
            skipped: (0..skipped)
                .map(|_| Skipped {
                    name: "lint".to_string(),
                    conclusion: "failure".to_string(),
                    url: String::new(),
                })
                .collect(),
        };
        assert_eq!(render(&report(0, 0)), "o/r#7 Fix: no failed checks\n");
        assert_eq!(
            render(&report(2, 0)),
            "o/r#7 Fix: no failed checks, 2 pending\n"
        );
        assert_eq!(
            render(&report(0, 1)),
            "o/r#7 Fix: 1 failed\n\nskipped external check lint (failure)\n"
        );
    }
}
