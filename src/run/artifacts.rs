use anyhow::{Context, Result, bail};
use chrono::{DateTime, TimeDelta, Utc};
use serde::Serialize;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use super::{Commit, find_runs, resolve_run, watch};
use crate::api::actions::{self, WorkflowRun};
use crate::api::{self, Repo};
use crate::cli::ArtifactsArgs;
use crate::context::{Ctx, git};
use crate::pr::model::fetch_pr;
use crate::{output, prompt};

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

impl Artifact {
    fn new(repo: &Repo, a: actions::Artifact) -> Self {
        Artifact {
            id: a.id,
            run_id: a.run_id(),
            name: a.name,
            size: a.size_in_bytes,
            created_at: a.created_at,
            expires_at: a.expires_at,
            expired: a.expired,
            repo: repo.to_string(),
        }
    }
}

/// Artifacts of one run, in the order the API lists them.
fn run_artifacts(ctx: &Ctx, repo: &Repo, run_id: u64) -> Result<Vec<Artifact>> {
    Ok(actions::run_artifacts(&ctx.client, repo, run_id)?
        .into_iter()
        .map(|a| Artifact {
            // The run is known here even when the response leaves out workflow_run.
            run_id,
            ..Artifact::new(repo, a)
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
fn successful_runs(runs: &[WorkflowRun], workflow: Option<&str>) -> Vec<u64> {
    runs.iter()
        .filter(|r| r.conclusion.as_deref() == Some("success"))
        .filter(|r| workflow.is_none_or(|w| crate::workflow::matches(&r.name, &r.path, w)))
        .map(|r| r.id)
        .collect()
}

/// The newest successful run on a branch that uploaded artifacts, such as the latest green build
/// of the default branch. Runs without artifacts, like lint-only workflows, are skipped.
fn latest_success(ctx: &Ctx, args: &ArtifactsArgs) -> Result<((Repo, u64), Vec<Artifact>)> {
    let repo = ctx.repo()?;
    let branch = match &args.branch {
        Some(b) => b.clone(),
        None => api::default_branch(&ctx.client, &repo)?,
    };
    let runs = actions::successful_branch_runs(&ctx.client, &repo, &branch)?;
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
    if args.prune {
        return prune(ctx, &args);
    }
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

/// Artifacts to delete: those older than `older_than`, past the `keep` newest of each name.
/// Expired artifacts are left out, since they no longer use storage.
fn prune_plan(
    artifacts: &[Artifact],
    older_than: TimeDelta,
    keep: usize,
    now: DateTime<Utc>,
) -> Vec<&Artifact> {
    let mut by_name: BTreeMap<(&str, &str), Vec<&Artifact>> = BTreeMap::new();
    for a in artifacts.iter().filter(|a| !a.expired) {
        by_name.entry((&a.repo, &a.name)).or_default().push(a);
    }
    let mut doomed: Vec<&Artifact> = Vec::new();
    for group in by_name.values_mut() {
        group.sort_by_key(|a| std::cmp::Reverse(a.created_at));
        doomed.extend(
            group
                .iter()
                .skip(keep)
                .filter(|a| a.created_at.is_some_and(|t| now - t > older_than)),
        );
    }
    doomed.sort_by_key(|a| a.created_at);
    doomed
}

#[derive(Serialize)]
struct Pruned<'a> {
    repo: String,
    /// Storage used by the repo's unexpired artifacts before pruning.
    total_bytes: u64,
    total_count: usize,
    freed_bytes: u64,
    dry_run: bool,
    artifacts: Vec<&'a Artifact>,
    /// IDs that failed to delete.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    failed: Vec<u64>,
}

/// Delete old artifacts across the repo, or only those of the runs the arguments select.
fn prune(ctx: &Ctx, args: &ArtifactsArgs) -> Result<()> {
    let scoped =
        !args.runs.is_empty() || args.sha.is_some() || args.branch.is_some() || args.pr.is_some();
    let selected = if scoped {
        select(ctx, args)?
    } else {
        Vec::new()
    };
    let repo = match selected.first() {
        Some((r, _)) => r.clone(),
        None => ctx.repo()?,
    };
    if selected.iter().any(|(r, _)| *r != repo) {
        bail!("--prune works on one repository at a time");
    }
    let runs: HashSet<u64> = selected.into_iter().map(|(_, id)| id).collect();
    let all: Vec<Artifact> = actions::artifacts(&ctx.client, &repo)?
        .into_iter()
        .map(|a| Artifact::new(&repo, a))
        .collect();
    let live: Vec<&Artifact> = all.iter().filter(|a| !a.expired).collect();
    let total_bytes: u64 = live.iter().map(|a| a.size).sum();

    let candidates: Vec<Artifact> = all
        .iter()
        .filter(|a| !scoped || runs.contains(&a.run_id))
        .filter(|a| wanted(args, &a.name))
        .cloned()
        .collect();
    let now = Utc::now();
    let plan = prune_plan(&candidates, args.older_than, args.keep_latest, now);
    let freed_bytes: u64 = plan.iter().map(|a| a.size).sum();
    let mut report = Pruned {
        repo: repo.to_string(),
        total_bytes,
        total_count: live.len(),
        freed_bytes,
        dry_run: args.dry_run,
        artifacts: plan,
        failed: Vec::new(),
    };

    let summary = format!(
        "{repo}: {} artifacts use {}. Pruning {} frees {}.",
        report.total_count,
        output::size(total_bytes),
        report.artifacts.len(),
        output::size(freed_bytes)
    );
    let say = |line: &str| watch::progress(ctx, line);
    if report.artifacts.is_empty() {
        say(&format!("{summary} Nothing is old enough."));
        return if ctx.json {
            output::print_json(&report)
        } else {
            Ok(())
        };
    }
    let rows: Vec<Vec<String>> = report
        .artifacts
        .iter()
        .map(|a| {
            vec![
                a.name.clone(),
                output::size(a.size),
                a.created_at
                    .map(|t| format!("{} ago", output::age(now - t)))
                    .unwrap_or_default(),
                format!("run {}", a.run_id),
                a.id.to_string(),
            ]
        })
        .collect();
    say(&output::table(&rows, "  "));
    say(&summary);

    if args.dry_run {
        return if ctx.json {
            output::print_json(&report)
        } else {
            Ok(())
        };
    }
    let question = format!("Delete {} artifacts?", report.artifacts.len());
    if !prompt::confirm(&question, ctx.yes)? {
        say("Cancelled");
        return Ok(());
    }
    for a in &report.artifacts {
        if let Err(e) = actions::delete_artifact(&ctx.client, &repo, a.id) {
            eprintln!("failed to delete {} ({}): {e:#}", a.name, a.id);
            report.failed.push(a.id);
        }
    }
    report.freed_bytes = report
        .artifacts
        .iter()
        .filter(|a| !report.failed.contains(&a.id))
        .map(|a| a.size)
        .sum();
    if ctx.json {
        return output::print_json(&report);
    }
    println!(
        "Deleted {} artifacts, freeing {}",
        report.artifacts.len() - report.failed.len(),
        output::size(report.freed_bytes)
    );
    if !report.failed.is_empty() {
        bail!("{} artifacts could not be deleted", report.failed.len());
    }
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
        let fetched = actions::download_artifact(&ctx.client, &repo, a.id, &zip)
            .and_then(|_| extract(&zip, &dest));
        let _ = fs::remove_file(&zip);
        let files = fetched.with_context(|| format!("Failed to download {}", a.name))?;
        if !ctx.json {
            println!(
                "{} ({}, {files} files) -> {}",
                a.name,
                output::size(a.size),
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
                    output::size(a.size),
                    a.created_at
                        .map(|t| format!("{} ago", output::age(now - t)))
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
        Some(t) => format!("expires in {}", output::age(t - now)),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::Client;
    use mockito::{Matcher, Server, ServerGuard};
    use serde_json::json;

    fn repo() -> Repo {
        Repo::parse("o/r").unwrap()
    }

    fn at(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    #[test]
    fn artifact_from_api_model() {
        let api: actions::Artifact = serde_json::from_value(json!({
            "id": 5, "name": "linux-build", "size_in_bytes": 1536, "expired": false,
            "created_at": "2026-10-20T00:00:00Z", "expires_at": "2027-01-18T00:00:00Z",
            "workflow_run": { "id": 99 }
        }))
        .unwrap();
        let a = Artifact::new(&repo(), api);
        assert_eq!(
            (a.id, a.name.as_str(), a.size, a.run_id, a.repo.as_str()),
            (5, "linux-build", 1536, 99, "o/r")
        );
        assert_eq!(a.created_at, Some(at("2026-10-20T00:00:00Z")));
    }

    #[test]
    fn successful_runs_keep_order_and_workflow() {
        let runs: Vec<WorkflowRun> = serde_json::from_value(json!([
            {"id": 30, "name": "Lint", "path": ".github/workflows/lint.yml", "conclusion": "success"},
            {"id": 20, "name": "Build", "path": ".github/workflows/build.yml", "conclusion": "failure"},
            {"id": 10, "name": "Build", "path": ".github/workflows/build.yml", "conclusion": "success"},
        ]))
        .unwrap();
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
    fn prune_keeps_newest_of_each_name_and_young_ones() {
        let now = at("2026-10-24T00:00:00Z");
        let artifact = |id, name: &str, created: &str, expired| Artifact {
            id,
            name: name.to_string(),
            size: 10,
            created_at: Some(at(created)),
            expires_at: None,
            expired,
            run_id: id,
            repo: "o/r".to_string(),
        };
        let all = vec![
            artifact(1, "build", "2026-10-01T00:00:00Z", false),
            artifact(2, "build", "2026-10-10T00:00:00Z", false),
            artifact(3, "build", "2026-10-02T00:00:00Z", false),
            artifact(4, "build", "2026-10-22T00:00:00Z", false),
            artifact(5, "logs", "2026-09-01T00:00:00Z", false),
            artifact(6, "logs", "2026-08-01T00:00:00Z", true),
            artifact(7, "docs", "2026-09-01T00:00:00Z", false),
        ];
        let ids = |plan: Vec<&Artifact>| plan.iter().map(|a| a.id).collect::<Vec<_>>();
        // Build keeps 4 as newest; 2, 3, and 1 are past a week. Logs keeps 5, 6 is expired.
        assert_eq!(
            ids(prune_plan(&all, TimeDelta::days(7), 1, now)),
            vec![1, 3, 2]
        );
        // Keeping two of each spares 2 as well.
        assert_eq!(
            ids(prune_plan(&all, TimeDelta::days(7), 2, now)),
            vec![1, 3]
        );
        // Keeping none still spares artifacts younger than the cutoff, like 4.
        assert_eq!(
            ids(prune_plan(&all, TimeDelta::days(7), 0, now)),
            vec![7, 5, 1, 3, 2]
        );
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

    fn ctx(server: &ServerGuard, json: bool) -> Ctx {
        Ctx {
            client: Client::for_tests(&server.url()),
            repo_flag: Some("o/r".into()),
            json,
            yes: true,
        }
    }

    fn args(runs: &[&str]) -> ArtifactsArgs {
        ArtifactsArgs {
            runs: runs.iter().map(|r| r.to_string()).collect(),
            sha: None,
            branch: None,
            pr: None,
            workflow: None,
            success: false,
            names: Vec::new(),
            patterns: Vec::new(),
            download: false,
            dir: PathBuf::from("."),
            prune: false,
            older_than: TimeDelta::days(7),
            keep_latest: 1,
            dry_run: false,
        }
    }

    fn zip_bytes(entries: &[(&str, &str)]) -> Vec<u8> {
        use std::io::Write;
        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let options = zip::write::SimpleFileOptions::default();
        for (name, body) in entries {
            writer.start_file(*name, options).unwrap();
            writer.write_all(body.as_bytes()).unwrap();
        }
        writer.finish().unwrap().into_inner()
    }

    fn serve_artifacts(server: &mut ServerGuard, run: u64, artifacts: serde_json::Value) {
        server
            .mock(
                "GET",
                format!("/repos/o/r/actions/runs/{run}/artifacts").as_str(),
            )
            .match_query(Matcher::Any)
            .with_body(json!({ "artifacts": artifacts }).to_string())
            .create();
    }

    #[test]
    fn lists_the_artifacts_of_named_runs() {
        let mut server = Server::new();
        let lookup = server
            .mock("GET", "/repos/o/r/actions/runs/9")
            .with_body(r#"{"id": 9}"#)
            .expect(2)
            .create();
        serve_artifacts(
            &mut server,
            9,
            json!([{"id": 1, "name": "linux-x64"}, {"id": 2, "name": "logs"}]),
        );
        let mut a = args(&["9"]);
        a.patterns = vec!["linux-*".to_string()];
        run(&ctx(&server, true), a).unwrap();
        run(&ctx(&server, false), args(&["9"])).unwrap();
        lookup.assert();
        assert!(wanted(&args(&[]), "anything"));
        let mut a = args(&[]);
        a.names = vec!["logs".to_string()];
        assert!(wanted(&a, "logs"));
        assert!(!wanted(&a, "logs-2"));
    }

    #[test]
    fn downloads_one_artifact_into_the_dir_and_several_beside_each_other() {
        let mut server = Server::new();
        serve_artifacts(
            &mut server,
            9,
            json!([
                {"id": 1, "name": "linux", "size_in_bytes": 3},
                {"id": 2, "name": "macos", "size_in_bytes": 3},
                {"id": 3, "name": "old", "expired": true}
            ]),
        );
        server
            .mock("GET", "/repos/o/r/actions/artifacts/1/zip")
            .with_body(zip_bytes(&[("bin/tool", "elf"), ("README", "hi")]))
            .expect(2)
            .create();
        server
            .mock("GET", "/repos/o/r/actions/artifacts/2/zip")
            .with_body(zip_bytes(&[("tool", "macho")]))
            .create();
        let expired = server
            .mock("GET", "/repos/o/r/actions/artifacts/3/zip")
            .expect(0)
            .create();
        let dir = std::env::temp_dir().join(format!("gh-arriba-dl-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let url = "https://github.com/o/r/actions/runs/9";

        let mut a = args(&[url]);
        a.download = true;
        a.dir = dir.join("all");
        run(&ctx(&server, true), a).unwrap();
        assert_eq!(
            fs::read_to_string(dir.join("all/linux/bin/tool")).unwrap(),
            "elf"
        );
        assert_eq!(
            fs::read_to_string(dir.join("all/macos/tool")).unwrap(),
            "macho"
        );
        assert!(!dir.join("all/old").exists());
        // The zips are removed once extracted.
        let left: Vec<_> = fs::read_dir(dir.join("all")).unwrap().collect();
        assert_eq!(left.len(), 2);

        let mut a = args(&[url]);
        a.download = true;
        a.dir = dir.join("one");
        a.names = vec!["linux".to_string()];
        run(&ctx(&server, false), a).unwrap();
        assert_eq!(fs::read_to_string(dir.join("one/README")).unwrap(), "hi");

        let mut a = args(&[url]);
        a.download = true;
        a.dir = dir.join("none");
        a.names = vec!["old".to_string()];
        let err = run(&ctx(&server, true), a).unwrap_err().to_string();
        assert_eq!(err, "No artifacts to download");
        expired.assert();
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn download_refuses_same_name_from_two_runs_and_bad_zips() {
        let mut server = Server::new();
        serve_artifacts(&mut server, 8, json!([{"id": 1, "name": "build"}]));
        serve_artifacts(&mut server, 9, json!([{"id": 2, "name": "build"}]));
        server
            .mock("GET", "/repos/o/r/actions/artifacts/2/zip")
            .with_body("not a zip")
            .create();
        let dir = std::env::temp_dir().join(format!("gh-arriba-dl-bad-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let mut a = args(&[
            "https://github.com/o/r/actions/runs/8",
            "https://github.com/o/r/actions/runs/9",
        ]);
        a.download = true;
        a.dir = dir.clone();
        let err = run(&ctx(&server, true), a).unwrap_err().to_string();
        assert_eq!(
            err,
            "Several runs uploaded an artifact named build (8, 9). Pass one run, or -w to pick a workflow."
        );

        let mut a = args(&["https://github.com/o/r/actions/runs/9"]);
        a.download = true;
        a.dir = dir.clone();
        let err = run(&ctx(&server, true), a).unwrap_err();
        assert_eq!(err.to_string(), "Failed to download build");
        assert!(!dir.join(".build-2.zip.part").exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn success_skips_runs_without_wanted_artifacts() {
        let mut server = Server::new();
        server
            .mock("GET", "/repos/o/r")
            .with_body(r#"{"default_branch": "main"}"#)
            .create();
        server
            .mock("GET", "/repos/o/r/actions/runs")
            .match_query(Matcher::AllOf(vec![
                Matcher::UrlEncoded("branch".into(), "main".into()),
                Matcher::UrlEncoded("status".into(), "success".into()),
            ]))
            .with_body(
                r#"{"workflow_runs": [
                    {"id": 30, "name": "Lint", "conclusion": "success"},
                    {"id": 20, "name": "Build", "conclusion": "success"}
                ]}"#,
            )
            .create();
        serve_artifacts(&mut server, 30, json!([{"id": 3, "name": "lint-report"}]));
        serve_artifacts(&mut server, 20, json!([{"id": 2, "name": "linux-x64"}]));
        let ctx = ctx(&server, true);

        let mut a = args(&[]);
        a.success = true;
        a.patterns = vec!["linux-*".to_string()];
        let ((repo, id), found) = latest_success(&ctx, &a).unwrap();
        assert_eq!(
            (repo.to_string(), id, found[0].run_id),
            ("o/r".to_string(), 20, 20)
        );
        run(&ctx, a).unwrap();

        let mut a = args(&[]);
        a.success = true;
        a.names = vec!["docs".to_string()];
        a.workflow = Some("Build".to_string());
        assert_eq!(
            latest_success(&ctx, &a).unwrap_err().to_string(),
            "No successful run of Build with artifacts among the newest on main in o/r"
        );
    }

    /// Artifacts of o/r: two old builds, a fresh build, and old logs from another run.
    fn serve_repo_artifacts(server: &mut ServerGuard) {
        let old = (Utc::now() - TimeDelta::days(30)).to_rfc3339();
        let older = (Utc::now() - TimeDelta::days(40)).to_rfc3339();
        let new = (Utc::now() - TimeDelta::hours(1)).to_rfc3339();
        server
            .mock("GET", "/repos/o/r/actions/artifacts")
            .match_query(Matcher::Any)
            .with_body(
                json!({"artifacts": [
                    {"id": 1, "name": "build", "size_in_bytes": 100, "created_at": older,
                     "workflow_run": {"id": 8}},
                    {"id": 2, "name": "build", "size_in_bytes": 200, "created_at": old,
                     "workflow_run": {"id": 9}},
                    {"id": 3, "name": "build", "size_in_bytes": 300, "created_at": new,
                     "workflow_run": {"id": 10}},
                    {"id": 4, "name": "logs", "size_in_bytes": 400, "created_at": older,
                     "workflow_run": {"id": 9}},
                    {"id": 5, "name": "logs", "size_in_bytes": 500, "created_at": old,
                     "workflow_run": {"id": 10}},
                    {"id": 6, "name": "gone", "expired": true, "created_at": older}
                ]})
                .to_string(),
            )
            .create();
    }

    fn prune_args(runs: &[&str]) -> ArtifactsArgs {
        let mut a = args(runs);
        a.prune = true;
        a
    }

    #[test]
    fn prune_dry_run_deletes_nothing() {
        let mut server = Server::new();
        serve_repo_artifacts(&mut server);
        let deletes = server
            .mock(
                "DELETE",
                Matcher::Regex("^/repos/o/r/actions/artifacts/".into()),
            )
            .expect(0)
            .create();
        let mut a = prune_args(&[]);
        a.dry_run = true;
        run(&ctx(&server, true), a).unwrap();
        let mut a = prune_args(&[]);
        a.dry_run = true;
        a.older_than = TimeDelta::days(365);
        run(&ctx(&server, false), a).unwrap();
        deletes.assert();
    }

    #[test]
    fn prune_deletes_the_plan_and_reports_failures() {
        let mut server = Server::new();
        serve_repo_artifacts(&mut server);
        // Repo-wide with the newest of each name kept: builds 1 and 2, logs 4.
        let ok: Vec<_> = [1, 2]
            .iter()
            .map(|id| {
                server
                    .mock(
                        "DELETE",
                        format!("/repos/o/r/actions/artifacts/{id}").as_str(),
                    )
                    .with_status(204)
                    .expect(1)
                    .create()
            })
            .collect();
        let failing = server
            .mock("DELETE", "/repos/o/r/actions/artifacts/4")
            .with_status(403)
            .with_body(r#"{"message": "Forbidden"}"#)
            .create();
        let err = run(&ctx(&server, false), prune_args(&[]))
            .unwrap_err()
            .to_string();
        assert_eq!(err, "1 artifacts could not be deleted");
        for m in ok {
            m.assert();
        }
        failing.assert();
    }

    #[test]
    fn prune_scoped_to_runs() {
        let mut server = Server::new();
        serve_repo_artifacts(&mut server);
        // Only run 9's artifacts are candidates, and with nothing kept both go.
        let deletes: Vec<_> = [2, 4]
            .iter()
            .map(|id| {
                server
                    .mock(
                        "DELETE",
                        format!("/repos/o/r/actions/artifacts/{id}").as_str(),
                    )
                    .with_status(204)
                    .create()
            })
            .collect();
        let mut a = prune_args(&["https://github.com/o/r/actions/runs/9"]);
        a.keep_latest = 0;
        run(&ctx(&server, true), a).unwrap();
        for m in deletes {
            m.assert();
        }

        let a = prune_args(&[
            "https://github.com/o/r/actions/runs/9",
            "https://github.com/o/other/actions/runs/9",
        ]);
        assert_eq!(
            run(&ctx(&server, true), a).unwrap_err().to_string(),
            "--prune works on one repository at a time"
        );
    }

    #[test]
    fn runs_of_a_commit_or_pr() {
        let mut server = Server::new();
        server
            .mock("GET", "/repos/o/r/actions/runs")
            .match_query(Matcher::UrlEncoded("head_sha".into(), "abc".into()))
            .with_body(r#"{"workflow_runs": [{"id": 9, "head_sha": "abc"}]}"#)
            .create();
        server
            .mock("POST", "/graphql")
            .match_body(Matcher::PartialJson(
                json!({"variables": {"owner": "o", "repo": "r", "number": 5}}),
            ))
            .with_body(
                r#"{"data": {"repository": {"pullRequest": {
                    "id": "PR_1", "number": 5, "title": "t", "url": "u", "state": "OPEN",
                    "isDraft": false, "body": "", "createdAt": "2026-01-01T00:00:00Z",
                    "author": {"login": "a"}, "baseRefName": "main", "headRefName": "f",
                    "headRefOid": "abc", "isCrossRepository": false,
                    "headRepository": {"nameWithOwner": "o/r"}}}}}"#,
            )
            .create();
        let ctx = ctx(&server, true);
        let o_r = Repo::parse("o/r").unwrap();
        let mut a = args(&[]);
        a.sha = Some("abc".to_string());
        assert_eq!(select(&ctx, &a).unwrap(), vec![(o_r.clone(), 9)]);
        let mut a = args(&[]);
        a.pr = Some("o/r#5".to_string());
        assert_eq!(select(&ctx, &a).unwrap(), vec![(o_r, 9)]);
    }
}
