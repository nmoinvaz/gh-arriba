use anyhow::{Result, anyhow, bail};
use clap::ValueEnum;
use serde::Serialize;
use serde_json::json;
use std::collections::HashSet;

use super::{Scope, fetch_light, parse_title};
use crate::api::{Client, dependabot};
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
    let items = match scope {
        Scope::Repo(r) => dependabot::repo_alerts(client, r),
        Scope::Org(o) => dependabot::org_alerts(client, o),
    }
    .map_err(|e| access_error(e, &scope.to_string()))?;
    Ok(items
        .into_iter()
        .map(|a| Alert::new(a, scope_repo(scope)))
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

impl Alert {
    /// Org listings carry the repository, repo listings rely on `repo`.
    fn new(a: dependabot::Alert, repo: Option<String>) -> Self {
        let advisory = a.advisory();
        let package = a.dependency.package.clone().unwrap_or_default();
        Alert {
            repo: repo.unwrap_or_else(|| a.repo().to_string()),
            number: a.number,
            severity: a.severity().to_string(),
            patched: a.patched().map(str::to_string),
            state: a.state,
            package: package.name.unwrap_or_default(),
            ecosystem: package.ecosystem.unwrap_or_default(),
            cve: advisory.cve_id,
            ghsa: advisory.ghsa_id.unwrap_or_default(),
            summary: advisory.summary.unwrap_or_default(),
            manifest: a.dependency.manifest_path.unwrap_or_default(),
            url: a.html_url,
            fix_pr: None,
        }
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
        .into_iter()
        .map(|n| {
            let update = parse_title(&n.title);
            Candidate {
                repo: n.repository.name_with_owner,
                package: (!update.group).then_some(update.package),
                pr: FixPr {
                    number: n.number,
                    title: n.title,
                    url: n.url,
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
        let value = dependabot::get_alert(&ctx.client, &repo, *number).map_err(|e| {
            if e.to_string().contains("No alert found") {
                anyhow!("Alert #{number} not found in {repo}")
            } else {
                access_error(e, &repo.to_string())
            }
        })?;
        let alert = Alert::new(value, Some(repo.to_string()));
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

    let mut dismissed = Vec::new();
    let mut failed = 0usize;
    for a in &targets {
        let comment = args.comment.as_deref();
        match dependabot::dismiss_alert(&ctx.client, &repo, a.number, &reason, comment) {
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
    use crate::api::Repo;
    use crate::api::dependabot::alert_fixture;
    use crate::cli::DismissReason;
    use crate::dependabot::tests::{ctx, mock_prs, ready};
    use mockito::{Matcher, Mock, Server, ServerGuard};
    use serde_json::Value;

    /// Alert `number` on `package`, without the repository a repo listing leaves out.
    fn repo_alert(number: u64, package: &str, severity: &str) -> Value {
        let mut v = alert_fixture();
        v["number"] = json!(number);
        v["dependency"]["package"]["name"] = json!(package);
        v["security_advisory"]["severity"] = json!(severity);
        v.as_object_mut().unwrap().remove("repository");
        v
    }

    fn mock_repo_alerts(server: &mut ServerGuard, alerts: Value) -> Mock {
        server
            .mock("GET", "/repos/o/r/dependabot/alerts")
            .match_query(Matcher::UrlEncoded("state".into(), "open".into()))
            .with_body(alerts.to_string())
            .create()
    }

    #[test]
    fn fetch_fills_in_the_repo_and_lowercases_packages() {
        let mut server = Server::new();
        mock_repo_alerts(&mut server, json!([repo_alert(1, "Lodash", "high")]));
        let mut other = alert_fixture();
        other["repository"]["full_name"] = json!("o/b");
        server
            .mock("GET", "/orgs/acme/dependabot/alerts")
            .match_query(Matcher::Any)
            .with_body(json!([alert_fixture(), other]).to_string())
            .create();
        let client = Client::for_tests(&server.url());
        let repo = Scope::Repo(Repo::parse("o/r").unwrap());
        let alerts = fetch_alerts(&client, &repo).unwrap();
        assert_eq!(alerts[0].repo, "o/r");
        assert_eq!(alerts[0].ecosystem, "npm");
        assert_eq!(alerts[0].manifest, "package-lock.json");
        let packages = alerted_packages(&client, &Scope::Org("acme".into())).unwrap();
        assert_eq!(
            packages,
            HashSet::from([
                ("o/r".to_string(), "lodash".to_string()),
                ("o/b".to_string(), "lodash".to_string()),
            ])
        );
    }

    #[test]
    fn alerts_listed_with_fix_prs() {
        for json in [false, true] {
            let mut server = Server::new();
            let alerts = mock_repo_alerts(
                &mut server,
                json!([
                    repo_alert(1, "minimist", "low"),
                    repo_alert(2, "Lodash", "critical")
                ]),
            );
            // Only the light listing is needed to match fix PRs.
            let (light, details) = mock_prs(
                &mut server,
                false,
                &[ready(5, "Bump lodash from 4.17.20 to 4.17.21")],
            );
            let args = DependabotAlertsArgs { org: None };
            run_alerts(&ctx(&server, json), args).unwrap();
            alerts.assert();
            light.assert();
            details.expect(0).assert();
        }
    }

    #[test]
    fn org_alerts_match_prs_through_search() {
        let mut server = Server::new();
        server
            .mock("GET", "/orgs/acme/dependabot/alerts")
            .match_query(Matcher::Any)
            .with_body(json!([alert_fixture()]).to_string())
            .create();
        let (light, _) = mock_prs(&mut server, true, &[]);
        let args = DependabotAlertsArgs {
            org: Some("acme".into()),
        };
        run_alerts(&ctx(&server, false), args).unwrap();
        light.assert();
    }

    #[test]
    fn no_alerts_skips_pr_matching() {
        for json in [false, true] {
            let mut server = Server::new();
            mock_repo_alerts(&mut server, json!([]));
            let graphql = server.mock("POST", "/graphql").expect(0).create();
            run_alerts(&ctx(&server, json), DependabotAlertsArgs { org: None }).unwrap();
            graphql.assert();
        }
    }

    #[test]
    fn pr_matching_failure_still_lists_alerts() {
        let mut server = Server::new();
        mock_repo_alerts(&mut server, json!([repo_alert(1, "lodash", "high")]));
        server
            .mock("POST", "/graphql")
            .with_status(502)
            .with_body(r#"{"message": "Bad Gateway"}"#)
            .create();
        run_alerts(&ctx(&server, true), DependabotAlertsArgs { org: None }).unwrap();
    }

    #[test]
    fn disabled_alerts_explained() {
        let mut server = Server::new();
        server
            .mock("GET", "/repos/o/r/dependabot/alerts")
            .match_query(Matcher::Any)
            .with_status(403)
            .with_body(r#"{"message": "Dependabot alerts are disabled for this repository."}"#)
            .create();
        let err = run_alerts(&ctx(&server, false), DependabotAlertsArgs { org: None })
            .unwrap_err()
            .to_string();
        assert!(err.starts_with("Dependabot alerts are disabled for o/r, or the token"));
    }

    fn dismiss_args(alerts: &[u64], comment: Option<&str>) -> DependabotDismissArgs {
        DependabotDismissArgs {
            alerts: alerts.to_vec(),
            reason: DismissReason::TolerableRisk,
            comment: comment.map(str::to_string),
        }
    }

    fn mock_alert(server: &mut ServerGuard, number: u64, state: &str) -> Mock {
        let mut v = repo_alert(number, "lodash", "high");
        v["state"] = json!(state);
        server
            .mock(
                "GET",
                format!("/repos/o/r/dependabot/alerts/{number}").as_str(),
            )
            .with_body(v.to_string())
            .create()
    }

    fn mock_dismiss(server: &mut ServerGuard, number: u64, body: Value, status: usize) -> Mock {
        server
            .mock(
                "PATCH",
                format!("/repos/o/r/dependabot/alerts/{number}").as_str(),
            )
            .match_body(Matcher::Json(body))
            .with_status(status)
            .with_body(r#"{"message": "Validation Failed"}"#)
            .create()
    }

    #[test]
    fn dismiss_open_alerts_and_skip_closed_ones() {
        for json in [false, true] {
            let mut server = Server::new();
            mock_alert(&mut server, 7, "open");
            mock_alert(&mut server, 8, "fixed");
            let seven = mock_dismiss(
                &mut server,
                7,
                json!({ "state": "dismissed", "dismissed_reason": "tolerable_risk",
                        "dismissed_comment": "dev only" }),
                200,
            );
            let eight = server
                .mock("PATCH", "/repos/o/r/dependabot/alerts/8")
                .expect(0)
                .create();
            run_dismiss(&ctx(&server, json), dismiss_args(&[7, 8], Some("dev only"))).unwrap();
            seven.assert();
            eight.assert();
        }
    }

    #[test]
    fn dismiss_with_nothing_open_sends_nothing() {
        let mut server = Server::new();
        mock_alert(&mut server, 8, "dismissed");
        let patch = server.mock("PATCH", Matcher::Any).expect(0).create();
        run_dismiss(&ctx(&server, false), dismiss_args(&[8], None)).unwrap();
        patch.assert();
    }

    #[test]
    fn dismiss_failures_are_counted() {
        let mut server = Server::new();
        mock_alert(&mut server, 7, "open");
        mock_alert(&mut server, 9, "open");
        let body = json!({ "state": "dismissed", "dismissed_reason": "tolerable_risk" });
        mock_dismiss(&mut server, 7, body.clone(), 422);
        mock_dismiss(&mut server, 9, body, 200);
        let err = run_dismiss(&ctx(&server, true), dismiss_args(&[7, 9], None)).unwrap_err();
        assert_eq!(err.to_string(), "1 alert(s) could not be dismissed");
    }

    #[test]
    fn dismiss_explains_missing_and_hidden_alerts() {
        let mut server = Server::new();
        server
            .mock("GET", "/repos/o/r/dependabot/alerts/5")
            .with_status(404)
            .with_body(r#"{"message": "No alert found for alert number 5"}"#)
            .create();
        server
            .mock("GET", "/repos/o/r/dependabot/alerts/6")
            .with_status(403)
            .with_body(r#"{"message": "Resource not accessible by integration"}"#)
            .create();
        let c = ctx(&server, false);
        let err = run_dismiss(&c, dismiss_args(&[5], None)).unwrap_err();
        assert_eq!(err.to_string(), "Alert #5 not found in o/r");
        let err = run_dismiss(&c, dismiss_args(&[6], None)).unwrap_err();
        assert!(
            err.to_string()
                .starts_with("Dependabot alerts are disabled for o/r")
        );
    }

    #[test]
    fn dismiss_rejects_long_comments_up_front() {
        let mut server = Server::new();
        let any = server.mock("GET", Matcher::Any).expect(0).create();
        let long = "x".repeat(MAX_COMMENT + 1);
        let err = run_dismiss(&ctx(&server, false), dismiss_args(&[7], Some(&long))).unwrap_err();
        assert_eq!(
            err.to_string(),
            "Dismissal comments are limited to 280 characters."
        );
        any.assert();
    }

    fn parse_alert(v: &Value, repo: Option<String>) -> Alert {
        Alert::new(serde_json::from_value(v.clone()).unwrap(), repo)
    }

    #[test]
    fn parses_repo_and_org_alerts() {
        let a = parse_alert(&alert_fixture(), None);
        assert_eq!(a.repo, "o/r");
        assert_eq!(a.package, "Lodash");
        assert_eq!(a.advisory(), "CVE-2021-23337");
        assert_eq!(a.patched.as_deref(), Some("4.17.21"));
        assert_eq!(a.severity, "high");

        let mut v = alert_fixture();
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
