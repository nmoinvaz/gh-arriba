use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;

use super::{Commit, find_runs, resolve_run};
use crate::cli::ArtifactsArgs;
use crate::context::{Ctx, Repo, git};
use crate::output;
use crate::pr::model::fetch_pr;

/// An artifact uploaded by a workflow run.
#[derive(Clone, Debug, Serialize)]
pub struct Artifact {
    pub id: u64,
    pub name: String,
    pub size: u64,
    pub created_at: Option<DateTime<Utc>>,
    pub expires_at: Option<DateTime<Utc>>,
    /// Expired artifacts are still listed by the API but can no longer be downloaded.
    pub expired: bool,
    pub run_id: u64,
    pub repo: String,
}

fn parse_artifact(repo: &Repo, v: &Value) -> Artifact {
    let time = |key: &str| {
        v[key]
            .as_str()
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|t| t.with_timezone(&Utc))
    };
    Artifact {
        id: v["id"].as_u64().unwrap_or(0),
        name: v["name"].as_str().unwrap_or_default().to_string(),
        size: v["size_in_bytes"].as_u64().unwrap_or(0),
        created_at: time("created_at"),
        expires_at: time("expires_at"),
        expired: v["expired"].as_bool().unwrap_or(false),
        run_id: v["workflow_run"]["id"].as_u64().unwrap_or(0),
        repo: repo.to_string(),
    }
}

/// Artifacts of one run, in the order the API lists them.
fn run_artifacts(ctx: &Ctx, repo: &Repo, run_id: u64) -> Result<Vec<Artifact>> {
    let path = format!("{}/actions/runs/{run_id}/artifacts", repo.api());
    Ok(ctx
        .client
        .get_all_in(&path, "artifacts")?
        .iter()
        .map(|v| Artifact {
            // The run is known here even when the response leaves out workflow_run.
            run_id,
            ..parse_artifact(repo, v)
        })
        .collect())
}

/// The runs the arguments select: the RUN arguments, or the runs for a commit, branch, or PR.
fn select(ctx: &Ctx, args: &ArtifactsArgs) -> Result<Vec<(Repo, u64)>> {
    if !args.runs.is_empty() {
        return args.runs.iter().map(|r| resolve_run(ctx, r)).collect();
    }
    let (repo, commit) = match (&args.pr, &args.sha, &args.branch) {
        (Some(pr), _, _) => {
            let pr = ctx.pr(Some(pr))?;
            let head = fetch_pr(&ctx.client, &pr)?.head_oid;
            (pr.repo, Commit::Sha(head))
        }
        (None, Some(sha), _) => (ctx.repo()?, Commit::Sha(sha.clone())),
        (None, None, Some(branch)) => (ctx.repo()?, Commit::Branch(branch.clone())),
        (None, None, None) => (ctx.repo()?, Commit::Sha(git(&["rev-parse", "HEAD"])?)),
    };
    // Artifacts only exist once a run has finished, so there is nothing to wait for.
    Ok(
        find_runs(ctx, &repo, &commit, args.workflow.as_deref(), None)?
            .into_iter()
            .map(|id| (repo.clone(), id))
            .collect(),
    )
}

pub fn run(ctx: &Ctx, args: ArtifactsArgs) -> Result<()> {
    let runs = select(ctx, &args)?;
    let mut found = Vec::new();
    for (repo, id) in runs {
        let artifacts = run_artifacts(ctx, &repo, id)?;
        found.push(((repo, id), artifacts));
    }

    if ctx.json {
        let all: Vec<&Artifact> = found.iter().flat_map(|(_, a)| a).collect();
        return output::print_json(&all);
    }
    print!("{}", render(&found, Utc::now()));
    Ok(())
}

/// Artifacts grouped under each run, one row per artifact.
fn render(found: &[((Repo, u64), Vec<Artifact>)], now: DateTime<Utc>) -> String {
    let mut out = String::new();
    for ((repo, id), artifacts) in found {
        out.push_str(&format!("{repo} run {id}\n"));
        if artifacts.is_empty() {
            out.push_str("  no artifacts\n");
            continue;
        }
        let rows: Vec<Vec<String>> = artifacts
            .iter()
            .map(|a| {
                vec![
                    a.name.clone(),
                    size(a.size),
                    a.created_at
                        .map(|t| format!("{} ago", age(now - t)))
                        .unwrap_or_default(),
                    expiry(a, now),
                    a.id.to_string(),
                ]
            })
            .collect();
        out.push_str(&output::table(&rows, "  "));
        out.push('\n');
    }
    out
}

/// "expired", or how long until the artifact expires.
fn expiry(a: &Artifact, now: DateTime<Utc>) -> String {
    match a.expires_at {
        _ if a.expired => "expired".to_string(),
        Some(t) if t <= now => "expired".to_string(),
        Some(t) => format!("expires in {}", age(t - now)),
        None => String::new(),
    }
}

/// Byte count with a binary unit, as in `512 B` or `1.5 MB`.
fn size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

/// A span in its largest whole unit, as in `45s`, `12m`, `5h`, or `3d`.
fn age(span: chrono::TimeDelta) -> String {
    match span.num_seconds().max(0) {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86400 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn repo() -> Repo {
        Repo::parse("o/r").unwrap()
    }

    fn at(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    #[test]
    fn parses_artifact_fields() {
        let a = parse_artifact(
            &repo(),
            &json!({
                "id": 5, "name": "linux-build", "size_in_bytes": 1536, "expired": false,
                "created_at": "2026-10-20T00:00:00Z", "expires_at": "2027-01-18T00:00:00Z",
                "workflow_run": { "id": 99 }
            }),
        );
        assert_eq!(
            (a.id, a.name.as_str(), a.size, a.run_id),
            (5, "linux-build", 1536, 99)
        );
        assert_eq!(a.created_at, Some(at("2026-10-20T00:00:00Z")));
        assert!(!a.expired);
    }

    #[test]
    fn sizes_use_binary_units() {
        assert_eq!(size(0), "0 B");
        assert_eq!(size(1023), "1023 B");
        assert_eq!(size(1536), "1.5 KB");
        assert_eq!(size(5 * 1024 * 1024), "5.0 MB");
        assert_eq!(size(3 * 1024 * 1024 * 1024), "3.0 GB");
    }

    #[test]
    fn renders_runs_with_size_age_and_expiry() {
        let now = at("2026-10-24T00:00:00Z");
        let artifact = |id, name: &str, expires: &str, expired| Artifact {
            id,
            name: name.to_string(),
            size: 2048,
            created_at: Some(at("2026-10-21T00:00:00Z")),
            expires_at: Some(at(expires)),
            expired,
            run_id: 7,
            repo: "o/r".to_string(),
        };
        let found = vec![
            (
                (repo(), 7),
                vec![
                    artifact(1, "linux", "2026-11-20T00:00:00Z", false),
                    artifact(2, "logs", "2026-10-23T00:00:00Z", true),
                ],
            ),
            ((repo(), 8), Vec::new()),
        ];
        assert_eq!(
            render(&found, now),
            "o/r run 7\n  \
             linux  2.0 KB  3d ago  expires in 27d  1\n  \
             logs   2.0 KB  3d ago  expired         2\n\
             o/r run 8\n  \
             no artifacts\n"
        );
    }
}
