use anyhow::{Context, Result};
use serde::Serialize;
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

use crate::cli::{LogsArgs, RunCommand};
use crate::context::{Ctx, Repo};
use crate::output;

pub fn run(ctx: &Ctx, cmd: RunCommand) -> Result<()> {
    match cmd {
        RunCommand::Logs(args) => logs(ctx, args),
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
                    if !ctx.json {
                        let label = conclusion.as_deref().unwrap_or("running");
                        println!("{label:<10} {}", path.display());
                    }
                    downloaded.push(Downloaded {
                        job_id: id,
                        name,
                        conclusion,
                        path,
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
    fn failed_filter() {
        assert!(is_failed(Some("failure")));
        assert!(is_failed(Some("timed_out")));
        assert!(!is_failed(Some("success")));
        assert!(!is_failed(None));
    }
}
