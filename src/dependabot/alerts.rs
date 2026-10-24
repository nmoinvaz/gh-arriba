use anyhow::{Result, anyhow, bail};
use clap::ValueEnum;
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::HashSet;

use super::{Scope, fetch_light, parse_title};
use crate::api::Client;
use crate::api::json::str_of;
use crate::cli::{DependabotAlertsArgs, DependabotDismissArgs};
use crate::context::Ctx;
use crate::output::print_json;
use crate::output::table;
use crate::prompt;

/// GitHub's limit on a dismissal comment.
const MAX_COMMENT: usize = 280;

#[derive(Clone, Debug, Serialize)]
pub struct Alert {
    pub repo: String,
    pub number: u64,
    pub state: String,
    pub severity: String,
    pub package: String,
    pub ecosystem: String,
    pub cve: Option<String>,
    pub ghsa: String,
    pub summary: String,
    pub manifest: String,
    pub patched: Option<String>,
    pub url: String,
    pub fix_pr: Option<FixPr>,
}

#[derive(Clone, Debug, Serialize)]
pub struct FixPr {
    pub number: u64,
    pub title: String,
    pub url: String,
}

impl Alert {
    /// CVE when the advisory has one, the GHSA ID otherwise.
    fn advisory(&self) -> &str {
        self.cve.as_deref().unwrap_or(&self.ghsa)
    }
}

pub fn run_alerts(ctx: &Ctx, args: DependabotAlertsArgs) -> Result<()> {
    let scope = Scope::new(ctx, args.org)?;
    let mut alerts = fetch_alerts(&ctx.client, &scope)?;

    // Matching fix PRs is a convenience, so a failure leaves the alerts without them.
    if !alerts.is_empty() {
        match fetch_candidates(&ctx.client, &scope) {
            Ok(candidates) => {
                for alert in &mut alerts {
                    alert.fix_pr = find_fix(&candidates, &alert.repo, &alert.package);
                }
            }
            Err(e) => eprintln!("warning: couldn't match alerts to Dependabot PRs: {e}"),
        }
    }
    alerts.sort_by(|a, b| {
        (severity_rank(&a.severity), &a.repo, a.number).cmp(&(
            severity_rank(&b.severity),
            &b.repo,
            b.number,
        ))
    });

    if ctx.json {
        return print_json(&alerts);
    }
    if alerts.is_empty() {
        println!("No open Dependabot alerts in {scope}.");
        return Ok(());
    }
    println!("{scope}: {} open Dependabot alerts", alerts.len());
    for a in &alerts {
        let label = if scope.is_org() {
            format!("{}#{}", a.repo, a.number)
        } else {
            format!("#{}", a.number)
        };
        println!(
            "\n{label}  {}  {} ({})  {}",
            a.severity,
            a.package,
            a.ecosystem,
            a.advisory()
        );
        println!("  {}", a.summary);
        let mut details = vec![a.manifest.clone()];
        if let Some(v) = &a.patched {
            details.push(format!("patched in {v}"));
        }
        match &a.fix_pr {
            Some(pr) => details.push(format!("fix PR #{}", pr.number)),
            None => details.push("no fix PR".to_string()),
        }
        println!("  {}", details.join("  "));
    }
    Ok(())
}

/// Open alerts as `(OWNER/REPO, lowercase package)`, for matching PRs that fix them.
pub fn alerted_packages(client: &Client, scope: &Scope) -> Result<HashSet<(String, String)>> {
    Ok(fetch_alerts(client, scope)?
        .into_iter()
        .map(|a| (a.repo, a.package.to_lowercase()))
        .collect())
}

pub fn fetch_alerts(client: &Client, scope: &Scope) -> Result<Vec<Alert>> {
    let path = match scope {
        Scope::Repo(r) => format!("{}/dependabot/alerts?state=open", r.api()),
        Scope::Org(o) => format!("orgs/{o}/dependabot/alerts?state=open"),
    };
    let items = client
        .get_all(&path)
        .map_err(|e| access_error(e, &scope.to_string()))?;
    Ok(items
        .iter()
        .map(|a| parse_alert(a, scope_repo(scope)))
        .collect())
}

fn scope_repo(scope: &Scope) -> Option<String> {
    match scope {
        Scope::Repo(r) => Some(r.to_string()),
        Scope::Org(_) => None,
    }
}

/// Explain 403 and 404, which mean alerts are off or the token can't see them.
fn access_error(e: anyhow::Error, scope: &str) -> anyhow::Error {
    let msg = e.to_string();
    if msg.contains("GitHub API 403") || msg.contains("GitHub API 404") {
        anyhow!("Dependabot alerts are disabled for {scope}, or the token can't read them. {msg}")
    } else {
        e
    }
}

/// Parse a REST alert. Org listings carry the repository, repo listings rely on `repo`.
fn parse_alert(a: &Value, repo: Option<String>) -> Alert {
    let advisory = &a["security_advisory"];
    let package = &a["dependency"]["package"];
    Alert {
        repo: repo.unwrap_or_else(|| str_of(&a["repository"], "full_name")),
        number: a["number"].as_u64().unwrap_or(0),
        state: str_of(a, "state"),
        severity: advisory["severity"]
            .as_str()
            .or_else(|| a["security_vulnerability"]["severity"].as_str())
            .unwrap_or("unknown")
            .to_string(),
        package: str_of(package, "name"),
        ecosystem: str_of(package, "ecosystem"),
        cve: advisory["cve_id"].as_str().map(str::to_string),
        ghsa: str_of(advisory, "ghsa_id"),
        summary: str_of(advisory, "summary"),
        manifest: str_of(&a["dependency"], "manifest_path"),
        patched: a["security_vulnerability"]["first_patched_version"]["identifier"]
            .as_str()
            .map(str::to_string),
        url: str_of(a, "html_url"),
        fix_pr: None,
    }
}

/// An open Dependabot PR reduced to what fix matching needs.
struct Candidate {
    repo: String,
    /// Updated package. None for grouped updates, which name no single package.
    package: Option<String>,
    pr: FixPr,
}

/// Titles of open Dependabot PRs, without the costly check and mergeability fields.
fn fetch_candidates(client: &Client, scope: &Scope) -> Result<Vec<Candidate>> {
    let nodes = fetch_light(client, scope)?;
    Ok(nodes
        .iter()
        .map(|n| {
            let title = str_of(n, "title");
            let update = parse_title(&title);
            Candidate {
                repo: str_of(&n["repository"], "nameWithOwner"),
                package: (!update.group).then_some(update.package),
                pr: FixPr {
                    number: n["number"].as_u64().unwrap_or(0),
                    title,
                    url: str_of(n, "url"),
                },
            }
        })
        .collect())
}

/// The open Dependabot PR that bumps `package` in `repo`, matched case-insensitively.
fn find_fix(candidates: &[Candidate], repo: &str, package: &str) -> Option<FixPr> {
    candidates
        .iter()
        .find(|c| {
            c.repo == repo
                && c.package
                    .as_deref()
                    .is_some_and(|p| p.eq_ignore_ascii_case(package))
        })
        .map(|c| c.pr.clone())
}

fn severity_rank(severity: &str) -> u8 {
    match severity {
        "critical" => 0,
        "high" => 1,
        "medium" | "moderate" => 2,
        "low" => 3,
        _ => 4,
    }
}

pub fn run_dismiss(ctx: &Ctx, args: DependabotDismissArgs) -> Result<()> {
    let repo = ctx.repo()?;
    let reason = args
        .reason
        .to_possible_value()
        .map(|v| v.get_name().to_string())
        .unwrap_or_default();
    if args
        .comment
        .as_ref()
        .is_some_and(|c| c.chars().count() > MAX_COMMENT)
    {
        bail!("Dismissal comments are limited to {MAX_COMMENT} characters.");
    }

    let mut targets = Vec::new();
    for number in &args.alerts {
        let value = ctx
            .client
            .get(&format!("{}/dependabot/alerts/{number}", repo.api()))
            .map_err(|e| {
                if e.to_string().contains("No alert found") {
                    anyhow!("Alert #{number} not found in {repo}")
                } else {
                    access_error(e, &repo.to_string())
                }
            })?;
        let alert = parse_alert(&value, Some(repo.to_string()));
        if alert.state != "open" {
            eprintln!("  skipped #{number}, already {}", alert.state);
            continue;
        }
        targets.push(alert);
    }
    if targets.is_empty() {
        println!("No open alerts to dismiss.");
        return Ok(());
    }

    if !ctx.json {
        let rows: Vec<Vec<String>> = targets
            .iter()
            .map(|a| {
                vec![
                    format!("#{}", a.number),
                    a.severity.clone(),
                    a.package.clone(),
                    a.summary.clone(),
                ]
            })
            .collect();
        println!("{}", table(&rows, "  "));
    }
    let question = format!("Dismiss {} alert(s) in {repo} as {reason}?", targets.len());
    if !prompt::confirm(&question, ctx.yes)? {
        println!("Cancelled.");
        return Ok(());
    }

    let mut body = json!({ "state": "dismissed", "dismissed_reason": reason });
    if let Some(c) = &args.comment {
        body["dismissed_comment"] = json!(c);
    }
    let mut dismissed = Vec::new();
    let mut failed = 0usize;
    for a in &targets {
        let path = format!("{}/dependabot/alerts/{}", repo.api(), a.number);
        match ctx.client.patch(&path, &body) {
            Ok(_) => {
                if !ctx.json {
                    println!("  dismissed #{} {} ({reason})", a.number, a.package);
                }
                dismissed.push(a.number);
            }
            Err(e) => {
                eprintln!("  failed #{}: {e}", a.number);
                failed += 1;
            }
        }
    }
    if ctx.json {
        print_json(&json!({ "dismissed": dismissed, "reason": reason }))?;
    }
    if failed > 0 {
        bail!("{failed} alert(s) could not be dismissed");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alert_json() -> Value {
        json!({
            "number": 7,
            "state": "open",
            "html_url": "https://github.com/o/r/security/dependabot/7",
            "dependency": {
                "package": { "ecosystem": "npm", "name": "Lodash" },
                "manifest_path": "package-lock.json"
            },
            "security_advisory": {
                "ghsa_id": "GHSA-35jh-r3h4-6jhm",
                "cve_id": "CVE-2021-23337",
                "summary": "Command Injection in lodash",
                "severity": "high"
            },
            "security_vulnerability": {
                "severity": "high",
                "first_patched_version": { "identifier": "4.17.21" }
            },
            "repository": { "full_name": "o/r" }
        })
    }

    #[test]
    fn parses_repo_and_org_alerts() {
        let a = parse_alert(&alert_json(), None);
        assert_eq!(a.repo, "o/r");
        assert_eq!(a.package, "Lodash");
        assert_eq!(a.advisory(), "CVE-2021-23337");
        assert_eq!(a.patched.as_deref(), Some("4.17.21"));
        assert_eq!(a.severity, "high");

        let mut v = alert_json();
        v["security_advisory"]["cve_id"] = Value::Null;
        let a = parse_alert(&v, Some("x/y".into()));
        assert_eq!(a.repo, "x/y");
        assert_eq!(a.advisory(), "GHSA-35jh-r3h4-6jhm");
    }

    fn candidate(repo: &str, number: u64, title: &str) -> Candidate {
        let update = parse_title(title);
        Candidate {
            repo: repo.to_string(),
            package: (!update.group).then_some(update.package),
            pr: FixPr {
                number,
                title: title.to_string(),
                url: String::new(),
            },
        }
    }

    #[test]
    fn fix_pr_matches_package_case_insensitively() {
        let candidates = vec![
            candidate("o/r", 2, "Bump the lodash group with 3 updates"),
            candidate("o/r", 3, "Bump lodash from 4.17.20 to 4.17.21"),
            candidate("o/other", 4, "Bump lodash from 4.17.20 to 4.17.21"),
        ];
        assert_eq!(
            find_fix(&candidates, "o/r", "Lodash").map(|p| p.number),
            Some(3)
        );
        assert!(find_fix(&candidates, "o/r", "underscore").is_none());
    }

    #[test]
    fn access_errors_explained() {
        let e = access_error(
            anyhow!("GitHub API 403 on GET x: Resource not accessible"),
            "o/r",
        );
        assert!(
            e.to_string()
                .starts_with("Dependabot alerts are disabled for o/r")
        );
        let e = access_error(anyhow!("GitHub API 500 on GET x: boom"), "o/r");
        assert!(e.to_string().starts_with("GitHub API 500"));
    }

    #[test]
    fn severity_order() {
        let mut s = vec!["low", "critical", "medium", "high"];
        s.sort_by_key(|s| severity_rank(s));
        assert_eq!(s, vec!["critical", "high", "medium", "low"]);
    }
}
