pub mod watch;

use anyhow::{Context, Result};
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::cli::{DispatchArgs, LogsArgs, RunCommand, WatchArgs};
use crate::context::{Ctx, Repo, git};
use crate::log::{self, Excerpt};
use crate::{output, workflow};

pub fn run(ctx: &Ctx, cmd: RunCommand) -> Result<()> {
    match cmd {
        RunCommand::Logs(args) => logs(ctx, args),
        RunCommand::Watch(args) => watch_runs(ctx, args),
        RunCommand::Dispatch(args) => dispatch(ctx, args),
    }
}

/// What a RUN_OR_JOB argument names.
#[derive(Debug, PartialEq, Eq)]
enum Target {
    Run {
        id: u64,
        attempt: Option<u64>,
    },
    Job(u64),
    /// A bare number, which may be either.
    Unknown(u64),
}

/// Parse a number or an Actions URL. A URL also yields its repository.
fn parse_target(s: &str) -> Result<(Option<Repo>, Target)> {
    let s = s.trim();
    if let Ok(id) = s.parse() {
        return Ok((None, Target::Unknown(id)));
    }
    let path = s
        .split_once("://")
        .and_then(|(_, rest)| rest.split_once('/'))
        .map(|(_, path)| path)
        .with_context(|| format!("Expected a run ID, job ID, or Actions URL, got {s:?}"))?;
    let path = path.split(['#', '?']).next().unwrap_or_default();
    let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
    let bad = || anyhow::anyhow!("Expected an Actions run or job URL, got {s:?}");
    let [owner, name, "actions", "runs", run, rest @ ..] = parts.as_slice() else {
        return Err(bad());
    };
    let repo = Repo {
        owner: owner.to_string(),
        name: name.to_string(),
    };
    let run: u64 = run.parse().map_err(|_| bad())?;
    let target = match rest {
        ["job", job, ..] => Target::Job(job.parse().map_err(|_| bad())?),
        ["attempts", attempt, ..] => Target::Run {
            id: run,
            attempt: Some(attempt.parse().map_err(|_| bad())?),
        },
        _ => Target::Run {
            id: run,
            attempt: None,
        },
    };
    Ok((Some(repo), target))
}

#[derive(Serialize)]
struct Downloaded {
    job_id: u64,
    name: String,
    conclusion: Option<String>,
    path: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    excerpt: Option<Excerpt>,
}

/// Conclusions `--failed` keeps.
fn is_failed(conclusion: Option<&str>) -> bool {
    matches!(conclusion, Some("failure" | "cancelled" | "timed_out"))
}

/// Whether a job name contains any of `patterns`, ignoring case. No patterns keeps every job.
pub(crate) fn name_matches(name: &str, patterns: &[String]) -> bool {
    let name = name.to_lowercase();
    patterns.is_empty() || patterns.iter().any(|p| name.contains(&p.to_lowercase()))
}

/// Turn a job name into a file name, keeping it readable.
fn sanitize(name: &str) -> String {
    let mut out = String::new();
    for c in name.chars() {
        if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
            out.push(c);
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let out = out.trim_matches('-');
    if out.is_empty() {
        "job".to_string()
    } else {
        out.to_string()
    }
}

fn logs(ctx: &Ctx, args: LogsArgs) -> Result<()> {
    let (url_repo, target) = parse_target(&args.target)?;
    let repo = match url_repo {
        Some(r) => r,
        None => ctx.repo()?,
    };
    let client = &ctx.client;

    // A bare number is tried as a run, then as a job.
    let target = match target {
        Target::Unknown(id) => {
            if client
                .get_opt(&format!("{}/actions/runs/{id}", repo.api()))?
                .is_some()
            {
                Target::Run { id, attempt: None }
            } else if client
                .get_opt(&format!("{}/actions/jobs/{id}", repo.api()))?
                .is_some()
            {
                Target::Job(id)
            } else {
                anyhow::bail!("No run or job {id} in {repo}");
            }
        }
        other => other,
    };

    let (run_id, jobs) = match target {
        Target::Job(id) => {
            let job = client.get(&format!("{}/actions/jobs/{id}", repo.api()))?;
            (job["run_id"].as_u64().unwrap_or(0), vec![job])
        }
        Target::Run { id, attempt } => {
            let path = match attempt {
                Some(a) => format!("{}/actions/runs/{id}/attempts/{a}/jobs", repo.api()),
                None => format!("{}/actions/runs/{id}/jobs?filter=latest", repo.api()),
            };
            let jobs: Vec<Value> = client
                .get_all_in(&path, "jobs")?
                .into_iter()
                .filter(|j| !args.failed || is_failed(j["conclusion"].as_str()))
                .collect();
            (id, jobs)
        }
        Target::Unknown(_) => unreachable!("resolved above"),
    };
    let jobs: Vec<Value> = jobs
        .into_iter()
        .filter(|j| name_matches(j["name"].as_str().unwrap_or_default(), &args.jobs))
        .collect();

    if jobs.is_empty() {
        let what = if args.failed { "failed jobs" } else { "jobs" };
        let named = if args.jobs.is_empty() {
            String::new()
        } else {
            format!(" matching {}", args.jobs.join(", "))
        };
        anyhow::bail!("No {what}{named} in run {run_id}");
    }

    let base = args
        .out
        .unwrap_or_else(|| std::env::temp_dir().join("gh-arriba-logs"));
    let dir = base.join(run_id.to_string());
    fs::create_dir_all(&dir).with_context(|| format!("Failed to create {}", dir.display()))?;

    let mut downloaded = Vec::new();
    // Each log is a redirect to blob storage, so fetch a few at a time.
    for batch in jobs.chunks(PARALLEL_DOWNLOADS) {
        let results: Vec<(&Value, Result<PathBuf>)> = std::thread::scope(|s| {
            let handles: Vec<_> = batch
                .iter()
                .map(|job| {
                    let (repo, dir) = (&repo, &dir);
                    s.spawn(move || {
                        let id = job["id"].as_u64().unwrap_or(0);
                        let name = job["name"].as_str().unwrap_or("job");
                        (job, download(ctx, repo, id, dir, name))
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });

        for (job, result) in results {
            let id = job["id"].as_u64().unwrap_or(0);
            let name = job["name"].as_str().unwrap_or("job").to_string();
            let conclusion = job["conclusion"].as_str().map(str::to_string);
            match result {
                Ok(path) => {
                    let excerpt = if args.errors {
                        let text = fs::read_to_string(&path)
                            .with_context(|| format!("Failed to read {}", path.display()))?;
                        Some(log::excerpt(&text, log::CONTEXT, log::MAX))
                    } else {
                        None
                    };
                    if !ctx.json {
                        let label = conclusion.as_deref().unwrap_or("running");
                        println!("{label:<10} {}", path.display());
                        match &excerpt {
                            Some(x) if x.blocks.is_empty() => println!("  no error lines found"),
                            Some(x) => print!("{}", log::render(x, "  ")),
                            None => {}
                        }
                    }
                    downloaded.push(Downloaded {
                        job_id: id,
                        name,
                        conclusion,
                        path,
                        excerpt,
                    });
                }
                Err(e) => eprintln!("warning: skipped {name} ({id}): {}", short_error(&e)),
            }
        }
    }

    if ctx.json {
        output::print_json(&downloaded)?;
    }
    Ok(())
}

/// Log downloads in flight at once.
const PARALLEL_DOWNLOADS: usize = 8;

/// One-line reason for a failed download. Jobs cancelled before they start have no log blob.
pub(crate) fn short_error(e: &anyhow::Error) -> String {
    let text = format!("{e:#}");
    if text.contains("404") {
        return "no log available, the job may not have started".to_string();
    }
    let line = text.lines().next().unwrap_or_default();
    match line.char_indices().nth(120) {
        Some((i, _)) => format!("{}...", &line[..i]),
        None => line.to_string(),
    }
}

pub(crate) fn download(
    ctx: &Ctx,
    repo: &Repo,
    job_id: u64,
    dir: &Path,
    name: &str,
) -> Result<PathBuf> {
    // The endpoint redirects to blob storage, and reqwest drops the token on that hop.
    let text = ctx
        .client
        .get_text(&format!("{}/actions/jobs/{job_id}/logs", repo.api()))?;
    let path = dir.join(format!("{}-{job_id}.log", sanitize(name)));
    fs::write(&path, text).with_context(|| format!("Failed to write {}", path.display()))?;
    Ok(path)
}

// -------------------------------------------------------------------------
//  watch
// -------------------------------------------------------------------------

fn watch_runs(ctx: &Ctx, args: WatchArgs) -> Result<()> {
    let wait: watch::Wait = (&args.wait).into();
    let runs = if args.runs.is_empty() {
        let repo = ctx.repo()?;
        let commit = match (&args.sha, &args.branch) {
            (Some(sha), _) => Commit::Sha(sha.clone()),
            (None, Some(branch)) => Commit::Branch(branch.clone()),
            (None, None) => Commit::Sha(git(&["rev-parse", "HEAD"])?),
        };
        find_runs(ctx, &repo, &commit, args.workflow.as_deref(), &wait)?
            .into_iter()
            .map(|id| (repo.clone(), id))
            .collect()
    } else {
        args.runs
            .iter()
            .map(|r| resolve_run(ctx, r))
            .collect::<Result<Vec<_>>>()?
    };
    let states = watch::watch(ctx, &runs, &wait)?;
    watch::report(ctx, &states)
}

/// Which commit's runs to watch.
enum Commit {
    Sha(String),
    /// The newest commit on the branch that has runs.
    Branch(String),
}

/// How long to wait for runs to show up after a push.
const RUNS_GRACE: Duration = Duration::from_secs(120);

/// Run IDs for a commit, polling for a while since runs appear some seconds after a push.
fn find_runs(
    ctx: &Ctx,
    repo: &Repo,
    commit: &Commit,
    workflow: Option<&str>,
    wait: &watch::Wait,
) -> Result<Vec<u64>> {
    let (path, what) = match commit {
        Commit::Sha(sha) => (
            format!("{}/actions/runs?head_sha={sha}", repo.api()),
            format!("commit {}", &sha[..sha.len().min(10)]),
        ),
        Commit::Branch(b) => (
            format!(
                "{}/actions/runs?branch={}&per_page=100",
                repo.api(),
                encode(b)
            ),
            format!("branch {b}"),
        ),
    };
    let for_workflow = workflow
        .map(|w| format!(" for workflow {w}"))
        .unwrap_or_default();
    let deadline = Instant::now() + RUNS_GRACE;
    let mut announced = false;
    loop {
        let runs = match commit {
            Commit::Sha(_) => ctx.client.get_all_in(&path, "workflow_runs")?,
            // Only the newest page matters, older commits are not watched.
            Commit::Branch(_) => ctx.client.get(&path)?["workflow_runs"]
                .as_array()
                .cloned()
                .unwrap_or_default(),
        };
        let ids = select_runs(&runs, workflow);
        if !ids.is_empty() {
            return Ok(ids);
        }
        if Instant::now() >= deadline {
            let seen: BTreeSet<&str> = runs.iter().filter_map(|r| r["name"].as_str()).collect();
            let hint = if seen.is_empty() {
                " Push the commit first, or pass --sha or --branch.".to_string()
            } else {
                format!(" Workflows with runs: {}", Vec::from_iter(seen).join(", "))
            };
            anyhow::bail!("No runs{for_workflow} on {what} in {repo}.{hint}");
        }
        if !announced {
            watch::progress(ctx, &format!("Waiting for runs{for_workflow} on {what}..."));
            announced = true;
        }
        std::thread::sleep(wait.interval.min(Duration::from_secs(10)));
    }
}

/// Runs of the newest commit in `runs`, which the API lists newest first, narrowed to a workflow.
fn select_runs(runs: &[Value], workflow: Option<&str>) -> Vec<u64> {
    let runs: Vec<&Value> = runs
        .iter()
        .filter(|r| {
            workflow.is_none_or(|w| {
                crate::workflow::matches(
                    r["name"].as_str().unwrap_or_default(),
                    r["path"].as_str().unwrap_or_default(),
                    w,
                )
            })
        })
        .collect();
    let Some(sha) = runs.first().and_then(|r| r["head_sha"].as_str()) else {
        return Vec::new();
    };
    let mut ids: Vec<u64> = runs
        .iter()
        .filter(|r| r["head_sha"].as_str() == Some(sha))
        .filter_map(|r| r["id"].as_u64())
        .collect();
    ids.sort_unstable();
    ids
}

/// Percent-encode a query value, keeping slashes so branch names stay readable.
fn encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for b in value.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// The repository and run a RUN argument names. A job ID or URL stands for the run it belongs to.
fn resolve_run(ctx: &Ctx, s: &str) -> Result<(Repo, u64)> {
    let (url_repo, target) = parse_target(s)?;
    let repo = match url_repo {
        Some(r) => r,
        None => ctx.repo()?,
    };
    let job_run = |id: u64| -> Result<Option<u64>> {
        Ok(ctx
            .client
            .get_opt(&format!("{}/actions/jobs/{id}", repo.api()))?
            .and_then(|job| job["run_id"].as_u64()))
    };
    let id = match target {
        Target::Run { id, .. } => id,
        Target::Job(id) => job_run(id)?.with_context(|| format!("No job {id} in {repo}"))?,
        Target::Unknown(id) => {
            if ctx
                .client
                .get_opt(&format!("{}/actions/runs/{id}", repo.api()))?
                .is_some()
            {
                id
            } else {
                job_run(id)?.with_context(|| format!("No run or job {id} in {repo}"))?
            }
        }
    };
    Ok((repo, id))
}

// -------------------------------------------------------------------------
//  dispatch
// -------------------------------------------------------------------------

#[derive(Serialize)]
struct Dispatched {
    repo: String,
    workflow: String,
    #[serde(rename = "ref")]
    git_ref: String,
    run: Option<workflow::NewRun>,
}

fn dispatch(ctx: &Ctx, args: DispatchArgs) -> Result<()> {
    let repo = ctx.repo()?;
    let client = &ctx.client;
    let workflows = workflow::list(client, &repo)?;
    let wf = workflow::find(&workflows, &args.workflow)?;
    let git_ref = match args.git_ref {
        Some(r) => r,
        None => client.get(&repo.api())?["default_branch"]
            .as_str()
            .context("Repository has no default branch")?
            .to_string(),
    };

    let inputs = workflow::inputs_at(client, &repo, wf, &git_ref)?;
    let fields = workflow::parse_fields(&args.fields, &inputs)?;
    let missing = workflow::missing(&inputs, &fields);
    if !missing.is_empty() {
        anyhow::bail!(
            "Required inputs have no value: {}. Set them with -f KEY=VALUE.",
            missing.join(", ")
        );
    }
    let sent = fields
        .into_iter()
        .map(|(k, v)| (k, Value::String(v)))
        .collect();
    let run = workflow::dispatch(client, &repo, wf, &git_ref, sent)?;

    let line = match &run {
        Some(r) => format!(
            "Dispatched {} on {git_ref}: run {} {}",
            wf.name, r.id, r.url
        ),
        None => format!(
            "Dispatched {} on {git_ref}. The run is not visible yet, see https://github.com/{repo}/actions/workflows/{}",
            wf.name,
            wf.file()
        ),
    };
    if !args.watch {
        if ctx.json {
            return output::print_json(&Dispatched {
                repo: repo.to_string(),
                workflow: wf.name.clone(),
                git_ref,
                run,
            });
        }
        println!("{line}");
        return Ok(());
    }

    // The watch summary is the JSON document, so the dispatch line goes with the progress.
    watch::progress(ctx, &line);
    let run = run.context("Can't watch, the dispatched run did not appear in time")?;
    let states = watch::watch(ctx, &[(repo, run.id)], &(&args.wait).into())?;
    watch::report(ctx, &states)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo() -> Option<Repo> {
        Some(Repo {
            owner: "zlib-ng".to_string(),
            name: "zlib-ng".to_string(),
        })
    }

    #[test]
    fn bare_number_is_ambiguous() {
        assert_eq!(parse_target("123").unwrap(), (None, Target::Unknown(123)));
    }

    #[test]
    fn run_url_variants() {
        let base = "https://github.com/zlib-ng/zlib-ng/actions/runs/39608462685";
        assert_eq!(
            parse_target(base).unwrap(),
            (
                repo(),
                Target::Run {
                    id: 39608462685,
                    attempt: None
                }
            )
        );
        assert_eq!(
            parse_target(&format!("{base}/attempts/2")).unwrap(),
            (
                repo(),
                Target::Run {
                    id: 39608462685,
                    attempt: Some(2)
                }
            )
        );
        assert_eq!(
            parse_target(&format!("{base}/job/119251542374?pr=2480")).unwrap(),
            (repo(), Target::Job(119251542374))
        );
    }

    #[test]
    fn non_actions_url_is_rejected() {
        assert!(parse_target("https://github.com/zlib-ng/zlib-ng/pull/1").is_err());
        assert!(parse_target("main").is_err());
    }

    #[test]
    fn sanitize_collapses_separators() {
        assert_eq!(
            sanitize("CMake / Ubuntu GCC PPC64"),
            "CMake-Ubuntu-GCC-PPC64"
        );
        assert_eq!(sanitize("build (x86_64, 1.2)"), "build-x86_64-1.2");
        assert_eq!(sanitize("///"), "job");
    }

    #[test]
    fn job_name_filter() {
        let patterns = vec!["windows".to_string(), "Lint".to_string()];
        assert!(name_matches("Build Windows MSVC", &patterns));
        assert!(name_matches("lint", &patterns));
        assert!(!name_matches("Ubuntu GCC", &patterns));
        assert!(name_matches("anything", &[]));
    }

    #[test]
    fn runs_of_newest_commit_and_workflow() {
        use serde_json::json;
        let runs = vec![
            json!({"id": 30, "name": "Lint", "path": ".github/workflows/lint.yml", "head_sha": "bbb"}),
            json!({"id": 20, "name": "Build", "path": ".github/workflows/build.yml", "head_sha": "bbb"}),
            json!({"id": 10, "name": "Build", "path": ".github/workflows/build.yml", "head_sha": "aaa"}),
        ];
        assert_eq!(select_runs(&runs, None), vec![20, 30]);
        assert_eq!(select_runs(&runs, Some("build.yml")), vec![20]);
        assert!(select_runs(&runs, Some("deploy")).is_empty());
        assert!(select_runs(&[], None).is_empty());
    }

    #[test]
    fn encode_keeps_slashes() {
        assert_eq!(encode("nathan/dev/fix#2"), "nathan/dev/fix%232");
    }

    #[test]
    fn failed_filter() {
        assert!(is_failed(Some("failure")));
        assert!(is_failed(Some("timed_out")));
        assert!(!is_failed(Some("success")));
        assert!(!is_failed(None));
    }
}
