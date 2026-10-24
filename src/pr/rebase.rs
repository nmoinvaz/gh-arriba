use anyhow::{Context, Result, bail};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::api::pulls::{self, PrInfo, fetch_pr};
use crate::api::{PrRef, Repo};
use crate::cli::RebaseArgs;
use crate::context::{Ctx, git, git_in};
use crate::output;

#[derive(Debug, Serialize)]
struct Outcome {
    pr: String,
    base: String,
    head: String,
    /// "local", "remote", or "check".
    method: &'static str,
    /// "rebased", "up-to-date", "clean", or "conflicts".
    result: &'static str,
    conflicts: Vec<String>,
    new_head: Option<String>,
    commented: bool,
}

pub fn run(ctx: &Ctx, args: RebaseArgs) -> Result<()> {
    let pr = ctx.pr(args.pr.as_deref())?;
    let info = fetch_pr(&ctx.client, &pr)?;
    if info.state != "OPEN" {
        bail!("{pr} is {}", info.state.to_lowercase());
    }

    let mut outcome = if args.remote {
        remote(ctx, &pr, &info)?
    } else {
        local(&pr, &info, args.check)?
    };

    if outcome.result == "rebased" && !args.no_comment {
        pulls::add_issue_comment(&ctx.client, &pr, "Rebased.")?;
        outcome.commented = true;
    }

    if ctx.json {
        output::print_json(&outcome)?;
    } else {
        print_outcome(&outcome);
    }
    if outcome.result == "conflicts" {
        bail!(
            "Rebase onto {} has conflicts, resolve them locally",
            outcome.base
        );
    }
    Ok(())
}

fn print_outcome(o: &Outcome) {
    match o.result {
        "up-to-date" => println!("{} is already up to date with {}", o.pr, o.base),
        "clean" => println!("{} rebases cleanly onto {}", o.pr, o.base),
        "conflicts" => {
            println!("{} conflicts with {} in:", o.pr, o.base);
            for f in &o.conflicts {
                println!("  {f}");
            }
        }
        _ => {
            let short = o
                .new_head
                .as_deref()
                .map(|h| &h[..h.len().min(12)])
                .unwrap_or("");
            let how = if o.method == "remote" {
                "server-side, commits are unsigned"
            } else {
                "locally"
            };
            println!(
                "Rebased {} onto {} {how}, head is now {short}",
                o.pr, o.base
            );
            if o.commented {
                println!("Commented \"Rebased.\" on {}", o.pr);
            }
        }
    }
}

fn outcome(pr: &PrRef, info: &PrInfo, method: &'static str, result: &'static str) -> Outcome {
    Outcome {
        pr: pr.to_string(),
        base: info.base_ref.clone(),
        head: info.head_ref.clone(),
        method,
        result,
        conflicts: Vec::new(),
        new_head: None,
        commented: false,
    }
}

/// Rebase server-side, which leaves the commits unsigned.
fn remote(ctx: &Ctx, pr: &PrRef, info: &PrInfo) -> Result<Outcome> {
    let behind = pulls::behind_by(&ctx.client, &pr.repo, &info.base_ref, &info.head_oid)?;
    if behind == Some(0) {
        return Ok(outcome(pr, info, "remote", "up-to-date"));
    }
    let mut o = outcome(pr, info, "remote", "rebased");
    o.new_head = pulls::rebase_branch(&ctx.client, &info.node_id, &info.head_oid)?;
    Ok(o)
}

/// Rebase with the local git config, so commits keep the user's signature.
fn local(pr: &PrRef, info: &PrInfo, check: bool) -> Result<Outcome> {
    let root = PathBuf::from(
        git(&["rev-parse", "--show-toplevel"])
            .with_context(|| format!("Run inside a clone of {}", pr.repo))?,
    );
    let remote = find_remote(&git_in(&root, &["remote", "-v"])?, &pr.repo).with_context(|| {
        format!(
            "No git remote points at {}. Run inside a clone of it.",
            pr.repo
        )
    })?;
    if info.cross_repository && !check {
        let fork = info.head_repo.as_deref().unwrap_or("a fork");
        bail!("{pr} comes from {fork}. Rebase it with --remote, or from a clone of the fork.");
    }

    // The pull ref serves the head for forks too, which --check relies on.
    let base_ref = format!("refs/remotes/{remote}/{}", info.base_ref);
    git_in(
        &root,
        &[
            "fetch",
            "--quiet",
            &remote,
            &format!("+refs/heads/{}:{base_ref}", info.base_ref),
            &format!("+refs/pull/{}/head", pr.number),
        ],
    )?;
    if !git_ok(
        &root,
        &["cat-file", "-e", &format!("{}^{{commit}}", info.head_oid)],
    ) {
        bail!(
            "The PR head {} is not in the fetched refs. The PR may have changed, run again.",
            info.head_oid
        );
    }
    let base_oid = git_in(&root, &["rev-parse", &base_ref])?;
    if git_ok(
        &root,
        &["merge-base", "--is-ancestor", &base_oid, &info.head_oid],
    ) {
        return Ok(outcome(
            pr,
            info,
            if check { "check" } else { "local" },
            "up-to-date",
        ));
    }

    let current = git_in(&root, &["branch", "--show-current"]).unwrap_or_default();
    if !check && current == info.head_ref {
        return in_place(pr, info, &root, &remote, &base_oid);
    }
    in_worktree(pr, info, &root, &remote, &base_oid, check)
}

/// Rebase the checked-out head branch itself.
fn in_place(
    pr: &PrRef,
    info: &PrInfo,
    root: &Path,
    remote: &str,
    base_oid: &str,
) -> Result<Outcome> {
    let dirty = git_in(root, &["status", "--porcelain", "--untracked-files=no"])?;
    if !dirty.is_empty() {
        bail!(
            "Commit or stash your changes before rebasing {}",
            info.head_ref
        );
    }
    // Pushing a branch that lacks the PR head would drop commits others pushed.
    if !git_ok(
        root,
        &["merge-base", "--is-ancestor", &info.head_oid, "HEAD"],
    ) {
        bail!(
            "Your {} branch is missing commits that are on {pr}. Pull them first.",
            info.head_ref
        );
    }
    let extra = git_in(
        root,
        &["rev-list", "--count", &format!("{}..HEAD", info.head_oid)],
    )?;
    if extra != "0" {
        eprintln!("note: {extra} local commit(s) not on the PR will be pushed too");
    }

    let conflicts = rebase_at(root, base_oid)?;
    if !conflicts.is_empty() {
        let mut o = outcome(pr, info, "local", "conflicts");
        o.conflicts = conflicts;
        return Ok(o);
    }
    let mut o = outcome(pr, info, "local", "rebased");
    o.new_head = Some(push_from(root, remote, info)?);
    Ok(o)
}

/// Removes the temporary worktree however the rebase ends.
struct TempWorktree<'a> {
    root: &'a Path,
    dir: PathBuf,
}

impl Drop for TempWorktree<'_> {
    fn drop(&mut self) {
        let dir = self.dir.to_string_lossy();
        let _ = git_in(self.root, &["worktree", "remove", "--force", &dir]);
    }
}

/// Rebase a detached copy of the PR head so the user's checkout is left alone.
fn in_worktree(
    pr: &PrRef,
    info: &PrInfo,
    root: &Path,
    remote: &str,
    base_oid: &str,
    check: bool,
) -> Result<Outcome> {
    let dir = std::env::temp_dir().join(format!(
        "gh-arriba-rebase-{}-{}",
        info.number,
        std::process::id()
    ));
    git_in(
        root,
        &[
            "worktree",
            "add",
            "--detach",
            "--quiet",
            &dir.to_string_lossy(),
            &info.head_oid,
        ],
    )?;
    let worktree = TempWorktree { root, dir };

    let method = if check { "check" } else { "local" };
    let conflicts = rebase_at(&worktree.dir, base_oid)?;
    if !conflicts.is_empty() {
        let mut o = outcome(pr, info, method, "conflicts");
        o.conflicts = conflicts;
        return Ok(o);
    }
    if check {
        return Ok(outcome(pr, info, method, "clean"));
    }
    let mut o = outcome(pr, info, method, "rebased");
    o.new_head = Some(push_from(&worktree.dir, remote, info)?);
    Ok(o)
}

/// Rebase HEAD in `dir` onto `onto`. Returns the conflicting files, after aborting, if any.
fn rebase_at(dir: &Path, onto: &str) -> Result<Vec<String>> {
    let output = Command::new("git")
        .current_dir(dir)
        .args(["rebase", "--quiet", onto])
        .output()
        .context("Failed to run git rebase")?;
    if output.status.success() {
        return Ok(Vec::new());
    }
    let conflicts: Vec<String> = git_in(dir, &["diff", "--name-only", "--diff-filter=U"])
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect();
    let _ = git_in(dir, &["rebase", "--abort"]);
    if conflicts.is_empty() {
        // Not a conflict, such as a failed commit signature.
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git rebase failed: {}", stderr.trim());
    }
    Ok(conflicts)
}

/// Force-push HEAD to the PR branch, leased on the head we rebased so newer pushes are kept.
fn push_from(dir: &Path, remote: &str, info: &PrInfo) -> Result<String> {
    let new_head = git_in(dir, &["rev-parse", "HEAD"])?;
    git_in(
        dir,
        &[
            "push",
            "--quiet",
            &format!(
                "--force-with-lease=refs/heads/{}:{}",
                info.head_ref, info.head_oid
            ),
            remote,
            &format!("HEAD:refs/heads/{}", info.head_ref),
        ],
    )
    .context("Push rejected. Someone may have pushed to the PR since it was fetched")?;
    Ok(new_head)
}

/// Name of the remote whose fetch URL points at `repo`, preferring origin.
fn find_remote(remotes: &str, repo: &Repo) -> Option<String> {
    let matching: Vec<&str> = remotes
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let (name, url, kind) = (parts.next()?, parts.next()?, parts.next()?);
            let parsed = Repo::parse(url).ok()?;
            // GitHub owner and repo names are case-insensitive.
            let same = parsed.owner.eq_ignore_ascii_case(&repo.owner)
                && parsed.name.eq_ignore_ascii_case(&repo.name);
            (kind == "(fetch)" && same).then_some(name)
        })
        .collect();
    matching
        .iter()
        .find(|n| **n == "origin")
        .or(matching.first())
        .map(|n| n.to_string())
}

/// Run git in `dir` and report only whether it succeeded.
fn git_ok(dir: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .is_ok_and(|o| o.status.success())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_matched_by_fetch_url() {
        let repo = Repo::parse("zlib-ng/zlib-ng").unwrap();
        let remotes = "\
fork\tgit@github.com:nmoinvaz/zlib-ng.git (fetch)
fork\tgit@github.com:nmoinvaz/zlib-ng.git (push)
upstream\thttps://github.com/Zlib-NG/zlib-ng (fetch)
upstream\thttps://github.com/Zlib-NG/zlib-ng (push)";
        assert_eq!(find_remote(remotes, &repo).as_deref(), Some("upstream"));
        assert_eq!(find_remote("", &repo), None);
    }

    #[test]
    fn origin_preferred_when_several_match() {
        let repo = Repo::parse("a/b").unwrap();
        let remotes = "\
mirror\thttps://github.com/a/b (fetch)
origin\tgit@github.com:a/b.git (fetch)";
        assert_eq!(find_remote(remotes, &repo).as_deref(), Some("origin"));
    }

    #[test]
    fn push_url_alone_does_not_match() {
        let repo = Repo::parse("a/b").unwrap();
        let remotes = "\
origin\thttps://github.com/x/y (fetch)
origin\thttps://github.com/a/b (push)";
        assert_eq!(find_remote(remotes, &repo), None);
    }
}
