//! Pass rates, flaky re-runs, and run times of each job over a branch's recent runs.

use anyhow::Result;
use serde::Serialize;
use std::collections::BTreeMap;

use crate::api;
use crate::api::actions::{self, Job, WorkflowRun};
use crate::cli::StatsArgs;
use crate::context::Ctx;
use crate::{output, parallel, workflow};

pub fn run(ctx: &Ctx, args: &StatsArgs) -> Result<()> {
    let repo = ctx.repo()?;
    let branch = match &args.branch {
        Some(b) => b.clone(),
        None => api::default_branch(&ctx.client, &repo)?,
    };
    let workflow_id = match &args.workflow {
        Some(query) => {
            let workflows = workflow::list(&ctx.client, &repo)?;
            Some(workflow::find(&workflows, query)?.id)
        }
        None => None,
    };
    let runs = actions::completed_runs(
        &ctx.client,
        &repo,
        workflow_id,
        &branch,
        args.limit as usize,
    )?;
    let jobs = parallel::map(&runs, |r| {
        actions::all_attempt_jobs(&ctx.client, &repo, r.id)
    });
    let mut measured: Vec<(WorkflowRun, Vec<Job>)> = Vec::new();
    for (run, jobs) in runs.into_iter().zip(jobs) {
        match jobs {
            Ok(jobs) => measured.push((run, jobs)),
            // A run whose jobs GitHub fails to list should not cost the rest.
            Err(e) => eprintln!("warning: skipped run {}: {e:#}", run.id),
        }
    }
    let report = Report {
        repo: repo.to_string(),
        branch,
        runs: measured.len(),
        jobs: tally(&measured),
    };
    if ctx.json {
        return output::print_json(&report);
    }
    print!("{}", render(&report));
    Ok(())
}

#[derive(Serialize)]
struct Report {
    repo: String,
    branch: String,
    /// Completed runs measured.
    runs: usize,
    jobs: Vec<JobStats>,
}

/// One job's record across the measured runs, judged by each run's last attempt.
#[derive(Debug, Default, PartialEq, Serialize)]
struct JobStats {
    workflow: String,
    job: String,
    passed: u32,
    failed: u32,
    /// Share of judged runs whose last attempt passed.
    pass_rate: Option<f64>,
    /// Runs where the job failed and then passed on a re-run of the same commit.
    flaky: u32,
    /// Seconds a passing last attempt took, at the median and the 90th percentile.
    p50_secs: Option<i64>,
    p90_secs: Option<i64>,
}

/// Whether a conclusion passed, failed, or says nothing about the job. Unlike `is_failed`,
/// cancelled runs count for neither, since a newer push cancelling a run is not a failure.
fn verdict(conclusion: Option<&str>) -> Option<bool> {
    match conclusion? {
        "success" => Some(true),
        "failure" | "timed_out" | "startup_failure" => Some(false),
        _ => None,
    }
}

/// Every job's record, by workflow and then job name.
fn tally(runs: &[(WorkflowRun, Vec<Job>)]) -> Vec<JobStats> {
    let mut stats: BTreeMap<(String, String), (JobStats, Vec<i64>)> = BTreeMap::new();
    for (run, jobs) in runs {
        let mut attempts: BTreeMap<&str, Vec<&Job>> = BTreeMap::new();
        for job in jobs {
            attempts.entry(&job.name).or_default().push(job);
        }
        for (name, mut tries) in attempts {
            // Servers that leave out run_attempt still finish the attempts in order.
            tries.sort_by_key(|j| (j.run_attempt, j.completed_at));
            let last = tries[tries.len() - 1];
            let Some(passed) = verdict(last.conclusion.as_deref()) else {
                continue;
            };
            let (entry, times) = stats
                .entry((run.name.clone(), name.to_string()))
                .or_insert_with(|| {
                    let job = JobStats {
                        workflow: run.name.clone(),
                        job: name.to_string(),
                        ..JobStats::default()
                    };
                    (job, Vec::new())
                });
            if !passed {
                entry.failed += 1;
                continue;
            }
            entry.passed += 1;
            let failed_before = tries[..tries.len() - 1]
                .iter()
                .any(|t| verdict(t.conclusion.as_deref()) == Some(false));
            if failed_before {
                entry.flaky += 1;
            }
            if let (Some(start), Some(end)) = (last.started_at, last.completed_at) {
                times.push((end - start).num_seconds());
            }
        }
    }
    stats
        .into_values()
        .map(|(mut job, mut times)| {
            let judged = job.passed + job.failed;
            job.pass_rate = (judged > 0).then(|| f64::from(job.passed) / f64::from(judged));
            times.sort_unstable();
            job.p50_secs = median(&times);
            job.p90_secs = percentile(&times, 0.9);
            job
        })
        .collect()
}

/// The middle of sorted values, or the mean of the two middle ones.
fn median(sorted: &[i64]) -> Option<i64> {
    let n = sorted.len();
    match n {
        0 => None,
        _ if n % 2 == 1 => Some(sorted[n / 2]),
        _ => Some((sorted[n / 2 - 1] + sorted[n / 2]) / 2),
    }
}

/// The nearest-rank percentile of sorted values.
fn percentile(sorted: &[i64], q: f64) -> Option<i64> {
    let rank = (q * sorted.len() as f64).ceil() as usize;
    sorted.get(rank.max(1) - 1).copied()
}

fn render(report: &Report) -> String {
    let mut out = format!(
        "{} runs on {} in {}\n",
        report.runs, report.branch, report.repo
    );
    let mut by_workflow: BTreeMap<&str, Vec<&JobStats>> = BTreeMap::new();
    for job in &report.jobs {
        by_workflow.entry(&job.workflow).or_default().push(job);
    }
    for (workflow, jobs) in by_workflow {
        let mut rows = vec![
            ["JOB", "RUNS", "PASSED", "FLAKY", "P50", "P90"]
                .map(String::from)
                .to_vec(),
        ];
        for job in jobs {
            let time = |secs: Option<i64>| secs.map_or("-".into(), output::duration);
            rows.push(vec![
                job.job.clone(),
                (job.passed + job.failed).to_string(),
                job.pass_rate
                    .map_or("-".into(), |r| format!("{:.0}%", r * 100.0)),
                job.flaky.to_string(),
                time(job.p50_secs),
                time(job.p90_secs),
            ]);
        }
        out.push_str(&format!("\n{workflow}\n{}\n", output::table(&rows, "  ")));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn run(id: u64, workflow: &str) -> WorkflowRun {
        serde_json::from_value(json!({"id": id, "name": workflow})).unwrap()
    }

    /// A job attempt that started at 0 and took `secs`.
    fn job(name: &str, attempt: u64, conclusion: &str, secs: i64) -> Job {
        let end = chrono::DateTime::from_timestamp(secs, 0).unwrap();
        serde_json::from_value(json!({
            "id": attempt, "name": name, "run_attempt": attempt, "conclusion": conclusion,
            "started_at": "1970-01-01T00:00:00Z", "completed_at": end.to_rfc3339(),
        }))
        .unwrap()
    }

    #[test]
    fn judges_each_run_by_its_last_attempt() {
        let runs = vec![
            (
                run(1, "CI"),
                vec![
                    job("build", 1, "success", 60),
                    job("test", 1, "failure", 30),
                ],
            ),
            // test failed, then passed on a re-run: flaky, and counted as passed.
            (
                run(2, "CI"),
                vec![
                    job("build", 1, "success", 120),
                    job("test", 1, "failure", 30),
                    job("test", 2, "success", 90),
                ],
            ),
            // Cancelled and skipped jobs say nothing about either job.
            (
                run(3, "CI"),
                vec![
                    job("build", 1, "cancelled", 5),
                    job("test", 1, "skipped", 0),
                ],
            ),
            (run(4, "Lint"), vec![job("fmt", 1, "success", 10)]),
        ];
        let stats = tally(&runs);
        let get = |w: &str, j: &str| {
            stats
                .iter()
                .find(|s| s.workflow == w && s.job == j)
                .unwrap()
        };

        let build = get("CI", "build");
        assert_eq!((build.passed, build.failed, build.flaky), (2, 0, 0));
        assert_eq!((build.p50_secs, build.p90_secs), (Some(90), Some(120)));

        let test = get("CI", "test");
        assert_eq!((test.passed, test.failed, test.flaky), (1, 1, 1));
        assert_eq!(test.pass_rate, Some(0.5));
        assert_eq!(test.p50_secs, Some(90));

        assert_eq!(get("Lint", "fmt").passed, 1);
        assert_eq!(stats.len(), 3);
    }

    #[test]
    fn attempts_without_a_number_go_by_when_they_finished() {
        // Listed newest first, as if the server left out run_attempt.
        let mut retried = job("test", 0, "success", 90);
        let mut first = job("test", 0, "failure", 30);
        retried.completed_at = Some(chrono::DateTime::from_timestamp(200, 0).unwrap());
        first.completed_at = Some(chrono::DateTime::from_timestamp(100, 0).unwrap());
        let stats = tally(&[(run(1, "CI"), vec![retried, first])]);
        assert_eq!(
            (stats[0].passed, stats[0].failed, stats[0].flaky),
            (1, 0, 1)
        );
    }

    #[test]
    fn json_carries_the_pass_rate() {
        let stats = tally(&[(run(1, "CI"), vec![job("build", 1, "success", 60)])]);
        let json = serde_json::to_value(&stats[0]).unwrap();
        assert_eq!(json["pass_rate"], 1.0);
    }

    #[test]
    fn medians_average_and_percentiles_take_the_nearest_rank() {
        let secs: Vec<i64> = (1..=10).collect();
        assert_eq!(percentile(&secs, 0.5), Some(5));
        assert_eq!(percentile(&secs, 0.9), Some(9));
        assert_eq!(percentile(&[42], 0.9), Some(42));
        assert_eq!(percentile(&[], 0.5), None);

        assert_eq!(median(&[60, 120]), Some(90));
        assert_eq!(median(&[1, 5, 9]), Some(5));
        assert_eq!(median(&[]), None);
    }

    #[test]
    fn renders_a_table_per_workflow() {
        let report = Report {
            repo: "o/r".into(),
            branch: "main".into(),
            runs: 2,
            jobs: vec![JobStats {
                workflow: "CI".into(),
                job: "build".into(),
                passed: 3,
                failed: 1,
                pass_rate: Some(0.75),
                flaky: 1,
                p50_secs: Some(250),
                p90_secs: None,
            }],
        };
        assert_eq!(
            render(&report),
            "2 runs on main in o/r\n\nCI\n  JOB    RUNS  PASSED  FLAKY  P50    P90\n  build  4     75%     1      4m10s  -\n"
        );
    }
}
