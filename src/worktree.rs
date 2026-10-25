use anyhow::{Context, Result};
use serde::Serialize;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::api::pulls::{self, BranchPr};
use crate::cli::WorktreeCommand;
use crate::context::{Ctx, git, git_in};
use crate::output;

pub fn run(ctx: &Ctx, cmd: WorktreeCommand) -> Result<()> {
    match cmd {
        WorktreeCommand::List => list(ctx),
    }
}

/// One worktree from `git worktree list --porcelain`.
#[derive(Clone, Debug, PartialEq, Serialize)]
struct Worktree {
    path: PathBuf,
    head: String,
    /// Short branch name, absent for a detached HEAD.
    branch: Option<String>,
    /// The worktree the repository was cloned into, which is never removed.
    main: bool,
    bare: bool,
    locked: bool,
    /// Git reports the directory as missing, so `git worktree prune` would drop it.
    prunable: bool,
}

/// Parse `git worktree list --porcelain`, whose first entry is the main worktree.
fn parse_list(text: &str) -> Vec<Worktree> {
    let mut out: Vec<Worktree> = Vec::new();
    for line in text.lines() {
        let (key, value) = line.split_once(' ').unwrap_or((line, ""));
        if key == "worktree" {
            out.push(Worktree {
                path: PathBuf::from(value),
                head: String::new(),
                branch: None,
                main: out.is_empty(),
                bare: false,
                locked: false,
                prunable: false,
            });
            continue;
        }
        let Some(wt) = out.last_mut() else {
            continue;
        };
        match key {
            "HEAD" => wt.head = value.to_string(),
            "branch" => {
                wt.branch = Some(
                    value
                        .strip_prefix("refs/heads/")
                        .unwrap_or(value)
                        .to_string(),
                )
            }
            "bare" => wt.bare = true,
            "locked" => wt.locked = true,
            "prunable" => wt.prunable = true,
            _ => {}
        }
    }
    out
}

/// What a worktree holds that isn't on the remote yet.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
struct Local {
    /// Uncommitted changes and untracked files.
    changes: usize,
    /// Commits ahead of and behind the branch's upstream, when it has one.
    ahead: u64,
    behind: u64,
    /// The upstream branch was deleted on the remote, as it usually is after a merge.
    upstream_gone: bool,
}

/// Parse `%(upstream:track)`, as in `[ahead 2, behind 1]` or `[gone]`.
fn parse_track(track: &str, local: &mut Local) {
    let inner = track.trim().trim_start_matches('[').trim_end_matches(']');
    for part in inner.split(", ") {
        match part.split_once(' ') {
            Some(("ahead", n)) => local.ahead = n.parse().unwrap_or(0),
            Some(("behind", n)) => local.behind = n.parse().unwrap_or(0),
            _ if part == "gone" => local.upstream_gone = true,
            _ => {}
        }
    }
}

fn local_state(root: &Path, wt: &Worktree) -> Local {
    let mut local = Local::default();
    if wt.prunable || wt.bare {
        return local;
    }
    local.changes = git_in(&wt.path, &["status", "--porcelain"])
        .map(|s| s.lines().count())
        .unwrap_or(0);
    if let Some(branch) = &wt.branch {
        let track = git_in(
            root,
            &[
                "for-each-ref",
                "--format=%(upstream:track)",
                &format!("refs/heads/{branch}"),
            ],
        )
        .unwrap_or_default();
        parse_track(&track, &mut local);
    }
    local
}

/// A worktree with its local state and the PR its branch is the head of.
#[derive(Debug, Serialize)]
struct Row {
    #[serde(flatten)]
    worktree: Worktree,
    local: Local,
    pr: Option<BranchPr>,
}

/// The PR to show for a branch: the viewer's newest, else the newest, since branch names match across
/// forks.
fn pick_pr(prs: &[BranchPr], viewer: &str) -> Option<BranchPr> {
    prs.iter()
        .find(|p| p.author.eq_ignore_ascii_case(viewer))
        .or_else(|| prs.first())
        .cloned()
}

/// Every worktree of the repository at `root`, with local state and PRs.
fn rows(ctx: &Ctx, root: &Path) -> Result<Vec<Row>> {
    let list = git_in(root, &["worktree", "list", "--porcelain"])?;
    let worktrees = parse_list(&list);
    let branches: Vec<String> = worktrees
        .iter()
        .filter_map(|w| w.branch.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let (prs, viewer) = if branches.is_empty() {
        Default::default()
    } else {
        let repo = ctx.repo()?;
        let prs = pulls::prs_by_branch(&ctx.client, &repo, &branches)?;
        (prs, ctx.client.viewer()?.to_string())
    };
    Ok(worktrees
        .into_iter()
        .map(|w| {
            let pr = w
                .branch
                .as_ref()
                .and_then(|b| prs.get(b))
                .and_then(|p| pick_pr(p, &viewer));
            Row {
                local: local_state(root, &w),
                worktree: w,
                pr,
            }
        })
        .collect())
}

/// The main worktree's root, from anywhere inside any worktree of the repository.
fn repo_root() -> Result<PathBuf> {
    let common = git(&["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .context("Not in a git repository")?;
    let common = PathBuf::from(common);
    // The common dir is the main worktree's .git, or the repository itself when bare.
    Ok(match common.file_name() {
        Some(name) if name == ".git" => common.parent().map(Path::to_path_buf).unwrap_or(common),
        _ => common,
    })
}

fn list(ctx: &Ctx) -> Result<()> {
    let root = repo_root()?;
    let rows = rows(ctx, &root)?;
    if ctx.json {
        return output::print_json(&rows);
    }
    print!("{}", render(&rows));
    Ok(())
}

/// A path relative to the main worktree's parent, where sibling worktrees usually live.
fn display_path(path: &Path, rows: &[Row]) -> String {
    let parent = rows
        .iter()
        .find(|r| r.worktree.main)
        .and_then(|r| r.worktree.path.parent());
    match parent.and_then(|p| path.strip_prefix(p).ok()) {
        Some(rel) => rel.display().to_string(),
        None => path.display().to_string(),
    }
}

/// Uncommitted changes, unpushed commits, and a deleted upstream, or `clean`.
fn describe_local(row: &Row) -> String {
    let w = &row.worktree;
    if w.prunable {
        return "missing".to_string();
    }
    let l = &row.local;
    let mut parts = Vec::new();
    if l.changes > 0 {
        parts.push(format!("{} changed", l.changes));
    }
    if l.ahead > 0 {
        parts.push(format!("{} unpushed", l.ahead));
    }
    if l.behind > 0 {
        parts.push(format!("{} behind", l.behind));
    }
    if l.upstream_gone {
        parts.push("upstream gone".to_string());
    }
    if w.locked {
        parts.push("locked".to_string());
    }
    if parts.is_empty() {
        "clean".to_string()
    } else {
        parts.join(", ")
    }
}

fn render(rows: &[Row]) -> String {
    let table: Vec<Vec<String>> = rows
        .iter()
        .map(|r| {
            let w = &r.worktree;
            let branch = match (&w.branch, w.bare) {
                (_, true) => "(bare)".to_string(),
                (Some(b), _) => b.clone(),
                (None, _) => format!("(detached {})", &w.head[..w.head.len().min(8)]),
            };
            let (pr, state, checks) = match &r.pr {
                Some(p) => (
                    format!("#{}", p.number),
                    p.state.to_lowercase(),
                    p.checks
                        .as_deref()
                        .map(checks_word)
                        .unwrap_or("")
                        .to_string(),
                ),
                None => (String::new(), String::new(), String::new()),
            };
            vec![
                display_path(&w.path, rows),
                branch,
                pr,
                state,
                checks,
                describe_local(r),
            ]
        })
        .collect();
    output::table(&table, "") + "\n"
}

/// The rollup state as `pass`, `fail`, or `pending`.
fn checks_word(state: &str) -> &'static str {
    match state {
        "SUCCESS" => "pass",
        "FAILURE" | "ERROR" => "fail",
        _ => "pending",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::Client;
    use mockito::{Matcher, Server};
    use serde_json::json;

    const LIST: &str = "\
worktree /src/repo
HEAD 1111111111111111111111111111111111111111
branch refs/heads/main

worktree /src/repo-pr-42
HEAD 2222222222222222222222222222222222222222
branch refs/heads/fix/thing
locked agent

worktree /tmp/scratch
HEAD 3333333333333333333333333333333333333333
detached
prunable gitdir file points to non-existent location
";

    #[test]
    fn parses_porcelain_list() {
        let w = parse_list(LIST);
        assert_eq!(w.len(), 3);
        assert!(w[0].main && !w[1].main);
        assert_eq!(w[0].branch.as_deref(), Some("main"));
        assert_eq!(w[1].branch.as_deref(), Some("fix/thing"));
        assert!(w[1].locked && !w[1].prunable);
        assert_eq!(w[2].branch, None);
        assert!(w[2].prunable);
        assert_eq!(w[2].head, "3333333333333333333333333333333333333333");
    }

    #[test]
    fn repo_root_is_the_main_worktree() {
        // Tests run from the crate root, which is a git checkout.
        assert!(repo_root().unwrap().join("Cargo.toml").exists());
    }

    #[test]
    fn parses_upstream_tracking() {
        let mut l = Local::default();
        parse_track("[ahead 2, behind 1]", &mut l);
        assert_eq!((l.ahead, l.behind, l.upstream_gone), (2, 1, false));
        let mut l = Local::default();
        parse_track("[gone]", &mut l);
        assert!(l.upstream_gone);
        let mut l = Local::default();
        parse_track("", &mut l);
        assert_eq!(l, Local::default());
    }

    fn pr(number: u64, author: &str, state: &str) -> BranchPr {
        BranchPr {
            number,
            title: String::new(),
            url: String::new(),
            state: state.into(),
            author: author.into(),
            head_oid: "2222222222222222222222222222222222222222".into(),
            checks: Some("SUCCESS".into()),
        }
    }

    #[test]
    fn prefers_the_viewers_pr() {
        let prs = vec![pr(9, "someone", "OPEN"), pr(4, "Me", "MERGED")];
        assert_eq!(pick_pr(&prs, "me").unwrap().number, 4);
        assert_eq!(pick_pr(&prs, "other").unwrap().number, 9);
        assert_eq!(pick_pr(&[], "me"), None);
    }

    #[test]
    fn renders_rows() {
        let worktrees = parse_list(LIST);
        let rows: Vec<Row> = worktrees
            .into_iter()
            .enumerate()
            .map(|(i, w)| Row {
                worktree: w,
                local: if i == 1 {
                    Local {
                        changes: 2,
                        ahead: 1,
                        behind: 0,
                        upstream_gone: true,
                    }
                } else {
                    Local::default()
                },
                pr: (i == 1).then(|| pr(42, "me", "MERGED")),
            })
            .collect();
        assert_eq!(
            render(&rows),
            "repo          main                                    clean\n\
             repo-pr-42    fix/thing            #42  merged  pass  2 changed, 1 unpushed, upstream gone, locked\n\
             /tmp/scratch  (detached 33333333)                     missing\n"
        );
    }

    /// A scratch repository with a bare remote, cleaned up on drop.
    pub(super) struct Scratch(pub PathBuf);

    impl Scratch {
        pub fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "gh-arriba-worktree-test-{tag}-{}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(dir.join("repo")).unwrap();
            let s = Scratch(dir);
            s.git(&[
                "init",
                "--quiet",
                "--bare",
                "--initial-branch=main",
                "../remote.git",
            ]);
            s.git(&["init", "--quiet", "--initial-branch=main"]);
            s.git(&["config", "user.name", "Test"]);
            s.git(&["config", "user.email", "test@example.com"]);
            s.git(&["config", "commit.gpgsign", "false"]);
            s.git(&["remote", "add", "origin", "../remote.git"]);
            s.commit("README.md", "hello\n");
            s.git(&["push", "--quiet", "-u", "origin", "main"]);
            s
        }

        pub fn root(&self) -> PathBuf {
            self.0.join("repo")
        }

        pub fn git(&self, args: &[&str]) -> String {
            git_in(&self.root(), args).unwrap()
        }

        pub fn commit(&self, file: &str, text: &str) {
            std::fs::write(self.root().join(file), text).unwrap();
            self.git(&["add", file]);
            self.git(&["commit", "--quiet", "-m", file]);
        }

        /// A worktree beside the repository on a new branch pushed to the remote.
        pub fn branch_worktree(&self, branch: &str) -> PathBuf {
            let path = self.0.join(branch.replace('/', "-"));
            self.git(&[
                "worktree",
                "add",
                "--quiet",
                "-b",
                branch,
                path.to_str().unwrap(),
            ]);
            git_in(&path, &["push", "--quiet", "-u", "origin", branch]).unwrap();
            path
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    pub(super) fn ctx(server: &Server, json: bool) -> Ctx {
        Ctx {
            client: Client::for_tests(&server.url()),
            repo_flag: Some("o/r".into()),
            json,
            yes: true,
        }
    }

    /// Answer the viewer query and the branch lookup with `prs` for branch `b0`.
    pub(super) fn mock_lookup(server: &mut Server, prs: serde_json::Value) {
        server
            .mock("POST", "/graphql")
            .match_body(Matcher::Regex("viewer".into()))
            .with_body(r#"{"data": {"viewer": {"login": "me"}}}"#)
            .create();
        server
            .mock("POST", "/graphql")
            .match_body(Matcher::Regex("headRefName".into()))
            .with_body(json!({"data": {"repository": {"b0": {"nodes": prs}}}}).to_string())
            .create();
    }

    #[test]
    fn rows_join_git_state_and_prs() {
        let s = Scratch::new("rows");
        let path = s.branch_worktree("fix/thing");
        std::fs::write(path.join("dirty.txt"), "x").unwrap();
        let head = git_in(&path, &["rev-parse", "HEAD"]).unwrap();
        let mut server = Server::new();
        // Branches are looked up sorted, so fix/thing is b0 ahead of main.
        mock_lookup(
            &mut server,
            json!([{
                "number": 42, "state": "OPEN", "headRefOid": head,
                "author": {"login": "me"}, "commits": {"nodes": []}
            }]),
        );
        let rows = rows(&ctx(&server, true), &s.root()).unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows[0].worktree.main);
        assert_eq!(rows[0].local, Local::default());
        let wt = &rows[1];
        assert_eq!(wt.worktree.branch.as_deref(), Some("fix/thing"));
        assert_eq!(wt.local.changes, 1);
        assert_eq!(wt.pr.as_ref().unwrap().number, 42);
        assert!(render(&rows).contains("#42"));
    }
}
