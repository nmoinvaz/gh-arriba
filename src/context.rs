use anyhow::{Context, Result, bail};
use std::path::Path;
use std::process::Command;

use crate::api::{Client, PrRef, Repo, parse_pr_ref};

/// State shared by every command: the API client and the global flags.
pub struct Ctx {
    pub client: Client,
    pub repo_flag: Option<String>,
    pub json: bool,
    pub yes: bool,
}

impl Ctx {
    /// Repository from -R, or the one gh resolves for the working directory.
    pub fn repo(&self) -> Result<Repo> {
        match &self.repo_flag {
            Some(r) => Repo::parse(r),
            None => current_repo(),
        }
    }

    /// `repo` when given, as when a URL named it, otherwise the one [`Ctx::repo`] resolves.
    pub fn repo_or(&self, repo: Option<Repo>) -> Result<Repo> {
        match repo {
            Some(r) => Ok(r),
            None => self.repo(),
        }
    }

    /// Pull request from a number, URL, owner/repo#N, or branch, defaulting to the current branch.
    pub fn pr(&self, arg: Option<&str>) -> Result<PrRef> {
        resolve_pr(self.repo_flag.as_deref(), arg)
    }
}

/// Resolve a PR argument against the -R flag, falling back to gh for branches.
pub fn resolve_pr(repo_flag: Option<&str>, arg: Option<&str>) -> Result<PrRef> {
    if let Some(a) = arg {
        if let Some(pr) = parse_pr_ref(a) {
            return Ok(pr);
        }
        if let Ok(number) = a.trim_start_matches('#').parse::<u64>() {
            let repo = match repo_flag {
                Some(r) => Repo::parse(r)?,
                None => current_repo()?,
            };
            return Ok(PrRef { repo, number });
        }
    } else if repo_flag.is_some() {
        bail!("Pass a PR number or URL when using --repo");
    }

    // A branch name or no argument at all. gh knows how to map either to a PR.
    let mut args = vec!["pr", "view"];
    if let Some(a) = arg {
        args.push(a);
    }
    if let Some(r) = repo_flag {
        args.extend(["--repo", r]);
    }
    args.extend(["--json", "url", "--jq", ".url"]);
    let url = gh(&args).context("No pull request found")?;
    parse_pr_ref(&url).with_context(|| format!("Unexpected PR URL from gh: {url}"))
}

/// Repository gh resolves for the working directory, honoring `gh repo set-default`.
pub fn current_repo() -> Result<Repo> {
    let name = gh(&[
        "repo",
        "view",
        "--json",
        "nameWithOwner",
        "--jq",
        ".nameWithOwner",
    ])
    .context("Not in a GitHub repository. Pass --repo OWNER/REPO.")?;
    Repo::parse(&name)
}

/// Run gh and return trimmed stdout.
pub fn gh(args: &[&str]) -> Result<String> {
    run("gh", args, None)
}

/// Run git in the working directory and return trimmed stdout.
pub fn git(args: &[&str]) -> Result<String> {
    run("git", args, None)
}

/// Run git in `dir` and return trimmed stdout.
pub fn git_in(dir: &Path, args: &[&str]) -> Result<String> {
    run("git", args, Some(dir))
}

fn run(program: &str, args: &[&str], dir: Option<&Path>) -> Result<String> {
    let mut cmd = Command::new(program);
    cmd.args(args);
    if let Some(d) = dir {
        cmd.current_dir(d);
    }
    let output = cmd
        .output()
        .with_context(|| format!("Failed to run {program}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("{program} {} failed: {}", args.join(" "), stderr.trim());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}
