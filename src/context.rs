use anyhow::{Context, Result, bail};
use serde::Serialize;
use std::fmt;
use std::path::Path;
use std::process::Command;

use crate::github::Client;

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

    /// Pull request from a number, URL, owner/repo#N, or branch, defaulting to the current branch.
    pub fn pr(&self, arg: Option<&str>) -> Result<PrRef> {
        resolve_pr(self.repo_flag.as_deref(), arg)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize)]
pub struct Repo {
    pub owner: String,
    pub name: String,
}

impl Repo {
    /// Parse OWNER/REPO, HOST/OWNER/REPO, or a repository URL.
    pub fn parse(s: &str) -> Result<Self> {
        let s = s.trim().trim_end_matches('/');
        let s = s.strip_suffix(".git").unwrap_or(s);
        let rest = s
            .strip_prefix("https://")
            .or_else(|| s.strip_prefix("http://"))
            .map(|r| r.split_once('/').map(|(_, path)| path).unwrap_or(""))
            .or_else(|| s.strip_prefix("git@").and_then(|r| r.split_once(':').map(|(_, p)| p)))
            .unwrap_or(s);
        let parts: Vec<&str> = rest.split('/').filter(|p| !p.is_empty()).collect();
        let (owner, name) = match parts.as_slice() {
            [owner, name] => (*owner, *name),
            // HOST/OWNER/REPO, as gh accepts for -R.
            [host, owner, name] if host.contains('.') => (*owner, *name),
            // A URL with trailing segments, such as /pull/1.
            [owner, name, ..] if s.contains("://") => (*owner, *name),
            _ => bail!("Expected OWNER/REPO, got {s:?}"),
        };
        Ok(Self {
            owner: owner.to_string(),
            name: name.to_string(),
        })
    }

    /// REST path prefix, `repos/OWNER/REPO`.
    pub fn api(&self) -> String {
        format!("repos/{}/{}", self.owner, self.name)
    }
}

impl fmt::Display for Repo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.owner, self.name)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PrRef {
    pub repo: Repo,
    pub number: u64,
}

impl PrRef {
    /// REST path prefix, `repos/OWNER/REPO/pulls/N`.
    pub fn api(&self) -> String {
        format!("{}/pulls/{}", self.repo.api(), self.number)
    }

    /// REST path prefix for the PR's issue, `repos/OWNER/REPO/issues/N`.
    pub fn issue_api(&self) -> String {
        format!("{}/issues/{}", self.repo.api(), self.number)
    }
}

impl fmt::Display for PrRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}#{}", self.repo, self.number)
    }
}

/// Parse a PR URL or OWNER/REPO#N. Returns None for anything else.
pub fn parse_pr_ref(s: &str) -> Option<PrRef> {
    let s = s.trim();
    if s.contains("://") {
        let path = s.split_once("://")?.1.split_once('/')?.1;
        let path = path.split(['#', '?']).next()?;
        let parts: Vec<&str> = path.split('/').collect();
        let [owner, name, "pull", number, ..] = parts.as_slice() else {
            return None;
        };
        return Some(PrRef {
            repo: Repo {
                owner: owner.to_string(),
                name: name.to_string(),
            },
            number: number.parse().ok()?,
        });
    }
    let (repo, number) = s.split_once('#')?;
    if repo.is_empty() {
        return None;
    }
    Some(PrRef {
        repo: Repo::parse(repo).ok()?,
        number: number.parse().ok()?,
    })
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
    let name = gh(&["repo", "view", "--json", "nameWithOwner", "--jq", ".nameWithOwner"])
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

#[cfg(test)]
mod tests {
    use super::*;

    fn repo(owner: &str, name: &str) -> Repo {
        Repo {
            owner: owner.to_string(),
            name: name.to_string(),
        }
    }

    #[test]
    fn repo_parses_common_forms() {
        assert_eq!(Repo::parse("a/b").unwrap(), repo("a", "b"));
        assert_eq!(Repo::parse("github.com/a/b").unwrap(), repo("a", "b"));
        assert_eq!(Repo::parse("https://github.com/a/b").unwrap(), repo("a", "b"));
        assert_eq!(Repo::parse("https://github.com/a/b.git").unwrap(), repo("a", "b"));
        assert_eq!(Repo::parse("https://github.com/a/b/pull/3").unwrap(), repo("a", "b"));
        assert_eq!(Repo::parse("git@github.com:a/b.git").unwrap(), repo("a", "b"));
        assert!(Repo::parse("just-a-name").is_err());
    }

    #[test]
    fn pr_ref_parses_url_and_shorthand() {
        let expected = PrRef {
            repo: repo("zlib-ng", "zlib-ng"),
            number: 42,
        };
        assert_eq!(
            parse_pr_ref("https://github.com/zlib-ng/zlib-ng/pull/42").as_ref(),
            Some(&expected)
        );
        assert_eq!(
            parse_pr_ref("https://github.com/zlib-ng/zlib-ng/pull/42/files#r1").as_ref(),
            Some(&expected)
        );
        assert_eq!(parse_pr_ref("zlib-ng/zlib-ng#42").as_ref(), Some(&expected));
        assert_eq!(parse_pr_ref("42"), None);
        assert_eq!(parse_pr_ref("#42"), None);
        assert_eq!(parse_pr_ref("feature/branch"), None);
    }
}
