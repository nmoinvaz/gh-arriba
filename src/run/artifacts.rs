use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use super::{Commit, find_runs, resolve_run};
use crate::cli::ArtifactsArgs;
use crate::context::{Ctx, Repo, git};
use crate::github;
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

/// Successful runs scanned for artifacts before giving up, newest first.
const SUCCESS_SCAN: usize = 10;

/// IDs of successful runs in API order (newest first), narrowed to a workflow.
fn successful_runs(runs: &[Value], workflow: Option<&str>) -> Vec<u64> {
    runs.iter()
        .filter(|r| r["conclusion"].as_str() == Some("success"))
        .filter(|r| {
            workflow.is_none_or(|w| {
                crate::workflow::matches(
                    r["name"].as_str().unwrap_or_default(),
                    r["path"].as_str().unwrap_or_default(),
                    w,
                )
            })
        })
        .filter_map(|r| r["id"].as_u64())
        .collect()
}

/// The newest successful run on a branch that uploaded artifacts, such as the latest green build
/// of the default branch. Runs without artifacts, like lint-only workflows, are skipped.
fn latest_success(ctx: &Ctx, args: &ArtifactsArgs) -> Result<((Repo, u64), Vec<Artifact>)> {
    let repo = ctx.repo()?;
    let branch = match &args.branch {
        Some(b) => b.clone(),
        None => ctx.client.get(&repo.api())?["default_branch"]
            .as_str()
            .context("Repository has no default branch")?
            .to_string(),
    };
    let path = format!(
        "{}/actions/runs?branch={}&status=success&per_page=100",
        repo.api(),
        github::encode_path(&branch)
    );
    let runs = ctx.client.get(&path)?["workflow_runs"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let for_workflow = args
        .workflow
        .as_deref()
        .map(|w| format!(" of {w}"))
        .unwrap_or_default();
    for id in successful_runs(&runs, args.workflow.as_deref())
        .into_iter()
        .take(SUCCESS_SCAN)
    {
        let artifacts = run_artifacts(ctx, &repo, id)?;
        if artifacts.iter().any(|a| wanted(args, &a.name)) {
            return Ok(((repo, id), artifacts));
        }
    }
    bail!("No successful run{for_workflow} with artifacts among the newest on {branch} in {repo}")
}

pub fn run(ctx: &Ctx, args: ArtifactsArgs) -> Result<()> {
    let mut found = Vec::new();
    if args.success {
        found.push(latest_success(ctx, &args)?);
    } else {
        for (repo, id) in select(ctx, &args)? {
            let artifacts = run_artifacts(ctx, &repo, id)?;
            found.push(((repo, id), artifacts));
        }
    }
    for (_, artifacts) in &mut found {
        artifacts.retain(|a| wanted(&args, &a.name));
    }

    if args.download {
        return download(ctx, &found, &args.dir);
    }
    if ctx.json {
        let all: Vec<&Artifact> = found.iter().flat_map(|(_, a)| a).collect();
        return output::print_json(&all);
    }
    print!("{}", render(&found, Utc::now()));
    Ok(())
}

/// Whether `-n` and `-p` keep an artifact. With neither, every artifact is kept.
fn wanted(args: &ArtifactsArgs, name: &str) -> bool {
    (args.names.is_empty() && args.patterns.is_empty())
        || args.names.iter().any(|n| n == name)
        || args.patterns.iter().any(|p| glob(p, name))
}

/// Match `name` against a glob where `*` is any run of characters and `?` is any one.
fn glob(pattern: &str, name: &str) -> bool {
    let (p, n): (Vec<char>, Vec<char>) = (pattern.chars().collect(), name.chars().collect());
    let (mut pi, mut ni) = (0, 0);
    // Where the last `*` was, and the name position it currently stands in for.
    let mut star: Option<(usize, usize)> = None;
    while ni < n.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == n[ni]) {
            pi += 1;
            ni += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some((pi, ni));
            pi += 1;
        } else if let Some((sp, sn)) = star {
            // Let the `*` swallow one more character and retry from there.
            pi = sp + 1;
            ni = sn + 1;
            star = Some((sp, sn + 1));
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|&c| c == '*')
}

#[derive(Serialize)]
struct Downloaded {
    #[serde(flatten)]
    artifact: Artifact,
    path: PathBuf,
    files: usize,
}

/// Download and extract the artifacts. One artifact goes straight into `dir`, several each get a
/// directory named after the artifact, as `gh run download` does. Expired ones are skipped.
fn download(ctx: &Ctx, found: &[((Repo, u64), Vec<Artifact>)], dir: &Path) -> Result<()> {
    let mut chosen: Vec<&Artifact> = Vec::new();
    for a in found.iter().flat_map(|(_, a)| a) {
        if a.expired {
            eprintln!("skipped {} ({}): expired", a.name, a.id);
        } else {
            chosen.push(a);
        }
    }
    if chosen.is_empty() {
        bail!("No artifacts to download");
    }
    let mut runs_by_name: HashMap<&str, Vec<u64>> = HashMap::new();
    for a in &chosen {
        runs_by_name.entry(&a.name).or_default().push(a.run_id);
    }
    if let Some((name, runs)) = runs_by_name.iter().find(|(_, runs)| runs.len() > 1) {
        let runs: Vec<String> = runs.iter().map(u64::to_string).collect();
        bail!(
            "Several runs uploaded an artifact named {name} ({}). Pass one run, or -w to pick a workflow.",
            runs.join(", ")
        );
    }

    let single = chosen.len() == 1;
    let mut done = Vec::new();
    for a in chosen {
        let dest = if single {
            dir.to_path_buf()
        } else {
            dir.join(&a.name)
        };
        fs::create_dir_all(&dest)
            .with_context(|| format!("Failed to create {}", dest.display()))?;
        let repo = Repo::parse(&a.repo)?;
        // Download beside the destination so a failed extraction leaves nothing half-written.
        let zip = dir.join(format!(".{}-{}.zip.part", a.name, a.id));
        let fetched = ctx
            .client
            .download(
                &format!("{}/actions/artifacts/{}/zip", repo.api(), a.id),
                &zip,
            )
            .and_then(|_| extract(&zip, &dest));
        let _ = fs::remove_file(&zip);
        let files = fetched.with_context(|| format!("Failed to download {}", a.name))?;
        if !ctx.json {
            println!(
                "{} ({}, {files} files) -> {}",
                a.name,
                size(a.size),
                dest.display()
            );
        }
        done.push(Downloaded {
            artifact: a.clone(),
            path: dest,
            files,
        });
    }
    if ctx.json {
        output::print_json(&done)?;
    }
    Ok(())
}

/// Extract a zip into `dest`, refusing entries whose paths would land outside it. Returns the
/// number of files written.
fn extract(zip: &Path, dest: &Path) -> Result<usize> {
    let file = fs::File::open(zip).with_context(|| format!("Failed to open {}", zip.display()))?;
    let mut archive = zip::ZipArchive::new(file).context("Not a valid zip archive")?;
    let mut files = 0;
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i)?;
        let Some(rel) = entry.enclosed_name() else {
            bail!(
                "Refusing to extract {:?}, it points outside the target directory",
                entry.name()
            );
        };
        let out = dest.join(rel);
        if entry.is_dir() {
            fs::create_dir_all(&out)?;
            continue;
        }
        if let Some(parent) = out.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut target = fs::File::create(&out)
            .with_context(|| format!("Failed to create {}", out.display()))?;
        std::io::copy(&mut entry, &mut target)
            .with_context(|| format!("Failed to write {}", out.display()))?;
        files += 1;
    }
    Ok(files)
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
    fn successful_runs_keep_order_and_workflow() {
        let runs = vec![
            json!({"id": 30, "name": "Lint", "path": ".github/workflows/lint.yml", "conclusion": "success"}),
            json!({"id": 20, "name": "Build", "path": ".github/workflows/build.yml", "conclusion": "failure"}),
            json!({"id": 10, "name": "Build", "path": ".github/workflows/build.yml", "conclusion": "success"}),
        ];
        assert_eq!(successful_runs(&runs, None), vec![30, 10]);
        assert_eq!(successful_runs(&runs, Some("build")), vec![10]);
        assert!(successful_runs(&runs, Some("deploy")).is_empty());
    }

    #[test]
    fn glob_matches_stars_and_question_marks() {
        assert!(glob("*", "anything"));
        assert!(glob("*", ""));
        assert!(glob("linux-*", "linux-x64"));
        assert!(glob("*-x64", "linux-x64"));
        assert!(glob("lin?x-*-debug", "linux-arm64-debug"));
        assert!(glob("*a*b*", "xxaxxbxx"));
        assert!(glob("exact", "exact"));
        assert!(!glob("linux-*", "windows-x64"));
        assert!(!glob("lin?x", "linx"));
        assert!(!glob("*a*b", "xxbxxa"));
        assert!(!glob("", "a"));
    }

    /// A zip in a fresh temp directory holding the given entries.
    fn zip_with(test: &str, entries: &[(&str, &str)]) -> (PathBuf, PathBuf) {
        use std::io::Write;
        let dir = std::env::temp_dir().join(format!("gh-arriba-{test}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.zip");
        let mut writer = zip::ZipWriter::new(fs::File::create(&path).unwrap());
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        for (name, body) in entries {
            writer.start_file(*name, options).unwrap();
            writer.write_all(body.as_bytes()).unwrap();
        }
        writer.finish().unwrap();
        (dir, path)
    }

    #[test]
    fn extract_writes_nested_files() {
        let (dir, zip) = zip_with("extract", &[("bin/tool", "x"), ("README", "y")]);
        let out = dir.join("out");
        assert_eq!(extract(&zip, &out).unwrap(), 2);
        assert_eq!(fs::read_to_string(out.join("bin/tool")).unwrap(), "x");
        assert_eq!(fs::read_to_string(out.join("README")).unwrap(), "y");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn extract_refuses_paths_outside_the_target() {
        let (dir, zip) = zip_with("traversal", &[("../evil", "x")]);
        let out = dir.join("out");
        assert!(extract(&zip, &out).is_err());
        assert!(!dir.join("evil").exists());
        fs::remove_dir_all(&dir).unwrap();
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
