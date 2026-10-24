use anyhow::{Result, bail};
use serde::Serialize;

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
}

pub fn run(ctx: &Ctx, args: StripArgs) -> Result<()> {
    let (pr_arg, paths) = split_args(&args.args);
    let pr = ctx.pr(pr_arg)?;
    let paths = normalize(paths);
    if paths.is_empty() {
        bail!("Pass the paths to strip, as in `gh arriba pr strip 42 .plans/`");
    }
    let info = fetch_pr(&ctx.client, &pr)?;
    if info.state != "OPEN" {
        bail!("{pr} is {}", info.state.to_lowercase());
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

    if ctx.json {
        return output::print_json(&report);
    }
    print!("{}", render(&report));
    Ok(())
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
    fn refuses_closed_prs_and_missing_paths() {
        let mut server = Server::new();
        mock_pr(&mut server, "MERGED");
        let err = run(&ctx(&server, true), args(&[".plans"], false)).unwrap_err();
        assert_eq!(err.to_string(), "o/r#7 is merged");
        let err = run(&ctx(&server, true), args(&["./"], false)).unwrap_err();
        assert!(err.to_string().starts_with("Pass the paths to strip"));
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
}
