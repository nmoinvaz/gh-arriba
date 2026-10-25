use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::api::pulls::{self, NewCommit, PrFile, PrInfo, fetch_pr};
use crate::api::{PrRef, parse_pr_ref};
use crate::cli::StripArgs;
use crate::context::Ctx;
use crate::output;

/// How the strip commit undoes the PR's change to one path.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(tag = "action", content = "path", rename_all = "snake_case")]
enum Change {
    /// The PR added the file, so it goes.
    Delete(String),
    /// The file exists on the base, so it goes back to the base version.
    Restore(String),
}

impl Change {
    fn path(&self) -> &str {
        match self {
            Change::Delete(p) | Change::Restore(p) => p,
        }
    }
}

#[derive(Serialize)]
struct Report {
    pr: String,
    branch: String,
    paths: Vec<String>,
    changes: Vec<Change>,
    dry_run: bool,
    /// The strip commit, absent on a dry run or when nothing matched.
    #[serde(skip_serializing_if = "Option::is_none")]
    commit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    commit_url: Option<String>,
    /// With --merge: "merged", or "auto-merge" when GitHub waits on checks first.
    #[serde(skip_serializing_if = "Option::is_none")]
    merged: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    method: Option<&'static str>,
}

pub fn run(ctx: &Ctx, args: StripArgs) -> Result<()> {
    let (pr_arg, paths) = split_args(&args.args);
    let pr = ctx.pr(pr_arg)?;
    let info = fetch_pr(&ctx.client, &pr)?;
    if info.state != "OPEN" {
        bail!("{pr} is {}", info.state.to_lowercase());
    }
    let paths = if paths.is_empty() {
        configured_paths(ctx, &pr, &info.base_ref)?
    } else {
        normalize(paths)
    };
    if paths.is_empty() {
        bail!(
            "Pass the paths to strip, as in `gh arriba pr strip 42 .plans/`, or list them under \
             strip: in {CONFIG}"
        );
    }

    let changes = plan(&pulls::pr_files(&ctx.client, &pr)?, &paths);
    let mut report = Report {
        pr: pr.to_string(),
        branch: info.head_ref.clone(),
        paths: paths.clone(),
        changes,
        dry_run: args.dry_run,
        commit: None,
        commit_url: None,
        merged: None,
        method: args.merge.then(|| args.method.name()),
    };

    if !report.changes.is_empty() && !args.dry_run {
        let message = args
            .message
            .clone()
            .unwrap_or_else(|| format!("Strip {} before merging", paths.join(", ")));
        let commit = strip(ctx, &pr, &info, &mut report.changes, &message)?;
        report.commit = Some(commit.oid);
        report.commit_url = Some(commit.url);
    }

    if args.merge && !args.dry_run {
        let method = args.method.name();
        if method != "squash" && !report.changes.is_empty() {
            eprintln!(
                "warning: a {method} merge keeps the stripped files in the commits that added them, \
                 squash leaves them out"
            );
        }
        let head = report.commit.as_deref().unwrap_or(&info.head_oid);
        report.merged = Some(merge(ctx, &pr, &info.node_id, method, head)?);
    }

    if ctx.json {
        return output::print_json(&report);
    }
    print!("{}", render(&report));
    Ok(())
}

/// Merge the PR at `head`. A head that just got the strip commit has checks to run again, so when
/// GitHub refuses with 405 the PR gets auto-merge instead and merges once they pass.
fn merge(ctx: &Ctx, pr: &PrRef, node_id: &str, method: &str, head: &str) -> Result<&'static str> {
    match pulls::merge(&ctx.client, pr, method, Some(head)) {
        Ok(()) => Ok("merged"),
        Err(e) if e.to_string().contains("GitHub API 405") => {
            pulls::enable_auto_merge(&ctx.client, node_id, method)
                .with_context(|| format!("{e:#}, and auto-merge could not be enabled"))?;
            Ok("auto-merge")
        }
        Err(e) => Err(e),
    }
}

/// Repo settings for gh arriba, read from the base branch.
const CONFIG: &str = ".github/arriba.yml";

#[derive(Default, Deserialize)]
struct Config {
    /// Paths `pr strip` takes out when none are given.
    #[serde(default)]
    strip: Vec<String>,
}

/// The `strip:` paths from the base branch's config, so a repo can name its scratch directories
/// once. The base is used rather than the PR so a PR can't change what is stripped from it.
fn configured_paths(ctx: &Ctx, pr: &PrRef, base: &str) -> Result<Vec<String>> {
    let Some(text) = pulls::file_text(&ctx.client, &pr.repo, CONFIG, base)? else {
        return Ok(Vec::new());
    };
    let config: Config = serde_yaml::from_str(&text)
        .with_context(|| format!("Failed to parse {CONFIG} on {base}"))?;
    Ok(normalize(&config.strip))
}

/// Split `[PR] PATH...`. The first argument is the PR when it is a number, URL, or OWNER/REPO#N;
/// a branch would read the same as a path, so it isn't accepted here.
fn split_args(args: &[String]) -> (Option<&str>, &[String]) {
    match args.split_first() {
        Some((first, rest))
            if parse_pr_ref(first).is_some()
                || first.trim_start_matches('#').parse::<u64>().is_ok() =>
        {
            (Some(first), rest)
        }
        _ => (None, args),
    }
}

/// Strip paths without a leading `./` or trailing `/`, so `docs/` and `./docs` match the same files.
fn normalize(paths: &[String]) -> Vec<String> {
    let mut out: Vec<String> = paths
        .iter()
        .map(|p| {
            let p = p.trim();
            let p = p.strip_prefix("./").unwrap_or(p);
            p.trim_end_matches('/').to_string()
        })
        .filter(|p| !p.is_empty())
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Whether `path` is one of `paths` or inside one of them.
fn covered(path: &str, paths: &[String]) -> bool {
    paths.iter().any(|p| {
        path == p
            || path
                .strip_prefix(p.as_str())
                .is_some_and(|rest| rest.starts_with('/'))
    })
}

/// The changes that undo the PR's edits under `paths`: files it added are deleted, and files that
/// exist on the base are restored, so merging leaves the base's copy of `paths` untouched.
fn plan(files: &[PrFile], paths: &[String]) -> Vec<Change> {
    let mut changes = Vec::new();
    for f in files {
        let here = covered(&f.filename, paths);
        match f.status.as_str() {
            "added" | "copied" if here => changes.push(Change::Delete(f.filename.clone())),
            "renamed" => {
                if here {
                    changes.push(Change::Delete(f.filename.clone()));
                }
                if let Some(old) = f.previous_filename.as_deref().filter(|o| covered(o, paths)) {
                    changes.push(Change::Restore(old.to_string()));
                }
            }
            "modified" | "changed" | "removed" if here => {
                changes.push(Change::Restore(f.filename.clone()))
            }
            _ => {}
        }
    }
    changes.sort();
    changes.dedup();
    changes
}

/// Commit the changes to the PR's head branch and return the new commit. A file to restore that the
/// base no longer has is deleted instead, which `changes` is updated to say.
fn strip(
    ctx: &Ctx,
    pr: &PrRef,
    info: &PrInfo,
    changes: &mut [Change],
    message: &str,
) -> Result<NewCommit> {
    let mut additions = Vec::new();
    let mut deletions = Vec::new();
    for change in changes.iter_mut() {
        match change {
            Change::Delete(path) => deletions.push(path.clone()),
            Change::Restore(path) => {
                match pulls::file_base64(&ctx.client, &pr.repo, path, &info.base_ref)? {
                    Some(contents) => additions.push((path.clone(), contents)),
                    None => {
                        deletions.push(path.clone());
                        *change = Change::Delete(path.clone());
                    }
                }
            }
        }
    }
    let head_repo = info
        .head_repo
        .clone()
        .unwrap_or_else(|| pr.repo.to_string());
    pulls::commit_on_branch(
        &ctx.client,
        &head_repo,
        &info.head_ref,
        &info.head_oid,
        message,
        &additions,
        &deletions,
    )
}

fn render(r: &Report) -> String {
    let mut out = render_strip(r);
    match (r.merged, r.method) {
        (Some("merged"), Some(m)) => out.push_str(&format!("Merged {} with {m}\n", r.pr)),
        (Some(_), Some(m)) => out.push_str(&format!(
            "Enabled {m} auto-merge on {}, which merges once its checks pass\n",
            r.pr
        )),
        (None, Some(m)) if r.dry_run => out.push_str(&format!("Would merge {} with {m}\n", r.pr)),
        _ => {}
    }
    out
}

fn render_strip(r: &Report) -> String {
    let mut out = String::new();
    if r.changes.is_empty() {
        out.push_str(&format!(
            "{} changes nothing under {}\n",
            r.pr,
            r.paths.join(", ")
        ));
        return out;
    }
    for c in &r.changes {
        let verb = match c {
            Change::Delete(_) => "delete ",
            Change::Restore(_) => "restore",
        };
        out.push_str(&format!("{verb} {}\n", c.path()));
    }
    match &r.commit {
        Some(oid) => {
            out.push_str(&format!(
                "Stripped {} from {} in {}\n",
                files(r.changes.len()),
                r.branch,
                &oid[..oid.len().min(10)]
            ));
            if let Some(url) = &r.commit_url {
                out.push_str(&format!("{url}\n"));
            }
        }
        None => out.push_str(&format!(
            "Would strip {} from {}\n",
            files(r.changes.len()),
            r.branch
        )),
    }
    out
}

/// `1 file` or `3 files`.
fn files(n: usize) -> String {
    if n == 1 {
        "1 file".to_string()
    } else {
        format!("{n} files")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::Client;
    use crate::cli::MergeMethod;
    use mockito::{Matcher, Mock, Server};
    use serde_json::json;

    fn file(name: &str, status: &str, previous: Option<&str>) -> PrFile {
        serde_json::from_value(json!({
            "filename": name, "status": status, "previous_filename": previous
        }))
        .unwrap()
    }

    fn strings(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn pr_comes_first_when_it_looks_like_one() {
        let a = strings(&["42", ".plans"]);
        assert_eq!(split_args(&a), (Some("42"), &a[1..]));
        let a = strings(&["o/r#42", ".plans", "docs"]);
        assert_eq!(split_args(&a), (Some("o/r#42"), &a[1..]));
        let a = strings(&["https://github.com/o/r/pull/42", "x"]);
        assert_eq!(split_args(&a).0, Some("https://github.com/o/r/pull/42"));
        let a = strings(&[".plans", "docs"]);
        assert_eq!(split_args(&a), (None, &a[..]));
    }

    #[test]
    fn paths_normalize_and_match_whole_segments() {
        let paths = normalize(&strings(&["./.plans/", "docs", "docs/", " "]));
        assert_eq!(paths, strings(&[".plans", "docs"]));
        assert!(covered(".plans/a.md", &paths));
        assert!(covered("docs", &paths));
        assert!(covered("docs/x/y.md", &paths));
        assert!(!covered("docs2/y.md", &paths));
        assert!(!covered("src/docs/y.md", &paths));
    }

    #[test]
    fn plan_deletes_additions_and_restores_base_files() {
        let files = vec![
            file(".plans/new.md", "added", None),
            file(".plans/old.md", "modified", None),
            file(".plans/gone.md", "removed", None),
            file(".plans/moved.md", "renamed", Some("notes/moved.md")),
            file("src/lib.rs", "renamed", Some(".plans/lib.rs")),
            file("src/main.rs", "modified", None),
        ];
        let changes = plan(&files, &strings(&[".plans"]));
        assert_eq!(
            changes,
            vec![
                Change::Delete(".plans/moved.md".into()),
                Change::Delete(".plans/new.md".into()),
                Change::Restore(".plans/gone.md".into()),
                Change::Restore(".plans/lib.rs".into()),
                Change::Restore(".plans/old.md".into()),
            ]
        );
        assert!(plan(&files, &strings(&["other"])).is_empty());
    }

    fn ctx(server: &Server, json: bool) -> Ctx {
        Ctx {
            client: Client::for_tests(&server.url()),
            repo_flag: Some("o/r".into()),
            json,
            yes: true,
        }
    }

    fn args(paths: &[&str], dry_run: bool) -> StripArgs {
        let mut all = vec!["7".to_string()];
        all.extend(strings(paths));
        StripArgs {
            args: all,
            dry_run,
            message: None,
            merge: false,
            method: MergeMethod::Squash,
        }
    }

    fn mock_pr(server: &mut Server, state: &str) -> Mock {
        server
            .mock("POST", "/graphql")
            .match_body(Matcher::Regex("isCrossRepository".into()))
            .with_body(
                json!({"data": {"repository": {"pullRequest": {
                    "id": "PR_7", "number": 7, "state": state,
                    "createdAt": "2026-01-01T00:00:00Z", "baseRefName": "main",
                    "headRefName": "feature", "headRefOid": "abc123",
                    "isCrossRepository": true,
                    "headRepository": {"nameWithOwner": "fork/r"}
                }}}})
                .to_string(),
            )
            .create()
    }

    fn mock_files(server: &mut Server) -> Mock {
        server
            .mock("GET", "/repos/o/r/pulls/7/files")
            .match_query(Matcher::Any)
            .with_body(
                json!([
                    {"filename": ".plans/new.md", "status": "added"},
                    {"filename": ".plans/old.md", "status": "modified"},
                    {"filename": ".plans/dropped.md", "status": "modified"},
                    {"filename": "src/main.rs", "status": "modified"}
                ])
                .to_string(),
            )
            .create()
    }

    fn mock_contents(server: &mut Server) {
        server
            .mock("GET", "/repos/o/r/contents/.plans/old.md")
            .match_query(Matcher::UrlEncoded("ref".into(), "main".into()))
            .with_body(r#"{"encoding": "base64", "content": "b2xk\n"}"#)
            .create();
        // Deleted from the base since the PR branched, so stripping deletes it too.
        server
            .mock("GET", "/repos/o/r/contents/.plans/dropped.md")
            .match_query(Matcher::Any)
            .with_status(404)
            .with_body(r#"{"message": "Not Found"}"#)
            .create();
    }

    #[test]
    fn strips_into_the_head_repository() {
        let mut server = Server::new();
        mock_pr(&mut server, "OPEN");
        mock_files(&mut server);
        mock_contents(&mut server);
        let commit = server
            .mock("POST", "/graphql")
            .match_body(Matcher::PartialJson(json!({"variables": {"input": {
                "branch": {"repositoryNameWithOwner": "fork/r", "branchName": "feature"},
                "expectedHeadOid": "abc123",
                "message": {"headline": "Strip .plans before merging"},
                "fileChanges": {
                    "additions": [{"path": ".plans/old.md", "contents": "b2xk"}],
                    "deletions": [{"path": ".plans/new.md"}, {"path": ".plans/dropped.md"}]
                }
            }}})))
            .with_body(
                r#"{"data": {"createCommitOnBranch": {"commit": {"oid": "def4567890ab", "url": "u"}}}}"#,
            )
            .create();
        for json in [true, false] {
            run(&ctx(&server, json), args(&[".plans/"], false)).unwrap();
        }
        commit.expect(2).assert();
    }

    #[test]
    fn dry_run_commits_nothing() {
        let mut server = Server::new();
        mock_pr(&mut server, "OPEN");
        mock_files(&mut server);
        let contents = server
            .mock("GET", Matcher::Regex("/contents/".into()))
            .expect(0)
            .create();
        let commit = server
            .mock("POST", "/graphql")
            .match_body(Matcher::Regex("createCommitOnBranch".into()))
            .expect(0)
            .create();
        run(&ctx(&server, false), args(&[".plans"], true)).unwrap();
        run(&ctx(&server, true), args(&["nothing/here"], false)).unwrap();
        contents.assert();
        commit.assert();
    }

    #[test]
    fn refuses_closed_prs() {
        let mut server = Server::new();
        mock_pr(&mut server, "MERGED");
        let err = run(&ctx(&server, true), args(&[".plans"], false)).unwrap_err();
        assert_eq!(err.to_string(), "o/r#7 is merged");
    }

    fn mock_config(server: &mut Server, status: usize, body: &str) -> Mock {
        server
            .mock("GET", "/repos/o/r/contents/.github/arriba.yml")
            .match_query(Matcher::UrlEncoded("ref".into(), "main".into()))
            .with_status(status)
            .with_body(body)
            .create()
    }

    #[test]
    fn paths_default_to_the_base_config() {
        let mut server = Server::new();
        mock_pr(&mut server, "OPEN");
        mock_files(&mut server);
        let config = mock_config(&mut server, 200, "strip:\n  - ./.plans/\n");
        let ctx = ctx(&server, true);
        assert_eq!(
            configured_paths(&ctx, &ctx.pr(Some("7")).unwrap(), "main").unwrap(),
            strings(&[".plans"])
        );
        run(&ctx, args(&[], true)).unwrap();
        config.expect(2).assert();
    }

    #[test]
    fn missing_or_empty_config_asks_for_paths() {
        for (status, body) in [(404, ""), (200, "other: 1\n")] {
            let mut server = Server::new();
            mock_pr(&mut server, "OPEN");
            mock_config(&mut server, status, body);
            let err = run(&ctx(&server, true), args(&[], false)).unwrap_err();
            assert!(err.to_string().starts_with("Pass the paths to strip"));
        }
        let mut server = Server::new();
        mock_pr(&mut server, "OPEN");
        mock_config(&mut server, 200, "strip: [unclosed\n");
        let err = run(&ctx(&server, true), args(&[], false)).unwrap_err();
        assert!(
            err.to_string()
                .contains("Failed to parse .github/arriba.yml")
        );
    }

    #[test]
    fn renders_plan_and_result() {
        let mut report = Report {
            pr: "o/r#7".into(),
            branch: "feature".into(),
            paths: strings(&[".plans"]),
            changes: vec![
                Change::Delete(".plans/new.md".into()),
                Change::Restore(".plans/old.md".into()),
            ],
            dry_run: true,
            commit: None,
            commit_url: None,
            merged: None,
            method: None,
        };
        assert_eq!(
            render(&report),
            "delete  .plans/new.md\nrestore .plans/old.md\nWould strip 2 files from feature\n"
        );
        report.commit = Some("def4567890abcdef".into());
        assert!(render(&report).ends_with("Stripped 2 files from feature in def4567890\n"));
        report.changes.clear();
        assert_eq!(render(&report), "o/r#7 changes nothing under .plans\n");
    }

    fn mock_merge(server: &mut Server, method: &str, sha: &str, status: usize) -> Mock {
        server
            .mock("PUT", "/repos/o/r/pulls/7/merge")
            .match_body(Matcher::Json(json!({"merge_method": method, "sha": sha})))
            .with_status(status)
            .with_body(r#"{"message": "Required status check \"build\" is expected."}"#)
            .create()
    }

    fn merge_args(paths: &[&str], method: MergeMethod) -> StripArgs {
        StripArgs {
            merge: true,
            method,
            ..args(paths, false)
        }
    }

    #[test]
    fn merge_waits_on_checks_after_stripping() {
        let mut server = Server::new();
        mock_pr(&mut server, "OPEN");
        mock_files(&mut server);
        mock_contents(&mut server);
        server
            .mock("POST", "/graphql")
            .match_body(Matcher::Regex("createCommitOnBranch".into()))
            .with_body(
                r#"{"data": {"createCommitOnBranch": {"commit": {"oid": "def456", "url": "u"}}}}"#,
            )
            .create();
        // The strip commit's checks haven't run, so the merge is refused and auto-merge takes over.
        let refused = mock_merge(&mut server, "squash", "def456", 405);
        let auto = server
            .mock("POST", "/graphql")
            .match_body(Matcher::PartialJson(
                json!({"variables": {"id": "PR_7", "method": "SQUASH"}}),
            ))
            .with_body(r#"{"data": {"enablePullRequestAutoMerge": {"clientMutationId": null}}}"#)
            .create();
        run(
            &ctx(&server, true),
            merge_args(&[".plans"], MergeMethod::Squash),
        )
        .unwrap();
        refused.assert();
        auto.assert();
    }

    #[test]
    fn merge_with_nothing_to_strip_merges_the_current_head() {
        let mut server = Server::new();
        mock_pr(&mut server, "OPEN");
        mock_files(&mut server);
        let merged = mock_merge(&mut server, "rebase", "abc123", 200);
        run(
            &ctx(&server, false),
            merge_args(&["other"], MergeMethod::Rebase),
        )
        .unwrap();
        merged.assert();
    }

    #[test]
    fn merge_failures_other_than_405_stop() {
        let mut server = Server::new();
        mock_pr(&mut server, "OPEN");
        mock_files(&mut server);
        mock_merge(&mut server, "squash", "abc123", 409);
        let err = run(
            &ctx(&server, true),
            merge_args(&["other"], MergeMethod::Squash),
        )
        .unwrap_err();
        assert!(err.to_string().contains("GitHub API 409"));
    }

    #[test]
    fn renders_merge_outcomes() {
        let mut report = Report {
            pr: "o/r#7".into(),
            branch: "feature".into(),
            paths: strings(&[".plans"]),
            changes: Vec::new(),
            dry_run: false,
            commit: None,
            commit_url: None,
            merged: Some("merged"),
            method: Some("squash"),
        };
        assert!(render(&report).ends_with("Merged o/r#7 with squash\n"));
        report.merged = Some("auto-merge");
        assert!(
            render(&report).ends_with(
                "Enabled squash auto-merge on o/r#7, which merges once its checks pass\n"
            )
        );
        report.merged = None;
        report.dry_run = true;
        assert!(render(&report).ends_with("Would merge o/r#7 with squash\n"));
    }
}
