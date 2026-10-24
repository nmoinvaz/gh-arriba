use anyhow::Result;
use serde::Serialize;
use std::collections::BTreeSet;

use super::{Category, DepPr, LockStep, Overview, Scope, alerts};
use crate::cli::DependabotListArgs;
use crate::context::Ctx;
use crate::output::print_json;
use crate::output::table;

pub fn run(ctx: &Ctx, args: DependabotListArgs) -> Result<()> {
    let scope = Scope::new(ctx, args.org)?;
    let mut overview = Overview::load(&ctx.client, &scope)?;

    if args.security {
        let alerted = alerts::alerted_packages(&ctx.client, &scope)?;
        let prs = overview
            .prs
            .into_iter()
            .filter(|p| alerted.contains(&(p.repo.clone(), p.update.package.to_lowercase())))
            .collect();
        overview = Overview::new(prs);
    }

    if ctx.json {
        return print_json(&ListJson::new(&scope, &overview));
    }
    print_overview(&scope, &overview);
    Ok(())
}

fn print_overview(scope: &Scope, o: &Overview) {
    let with_repo = scope.is_org();
    let repos: BTreeSet<&str> = o.prs.iter().map(|p| p.repo.as_str()).collect();
    let across = if with_repo {
        format!(" across {} repos", repos.len())
    } else {
        String::new()
    };
    println!(
        "{scope}: {} Dependabot PRs{across}, {} ready, {} need a rebase, {} blocked",
        o.prs.len(),
        o.count(Category::Ready),
        o.count(Category::NeedsRebase),
        o.count(Category::Blocked),
    );

    if !o.groups.is_empty() {
        println!("\nLock-step groups");
        for g in &o.groups {
            print_group(o, g, with_repo);
        }
    }

    let sections = [
        ("Ready", Category::Ready),
        ("Needs rebase", Category::NeedsRebase),
        ("Blocked", Category::Blocked),
    ];
    for (title, category) in sections {
        let prs = o.singles(category);
        if prs.is_empty() {
            continue;
        }
        println!("\n{title}");
        let rows: Vec<Vec<String>> = prs.iter().map(|p| row(p, with_repo)).collect();
        println!("{}", table(&rows, "  "));
    }
}

/// One PR as table cells. Blocked PRs show their reason in place of the diff size.
fn row(p: &DepPr, with_repo: bool) -> Vec<String> {
    let tail = p.reason.clone().unwrap_or_else(|| p.size());
    let badge = if p.update.from.is_some() {
        p.update.change.badge().to_string()
    } else {
        String::new()
    };
    vec![
        p.label(with_repo),
        p.update.name(),
        p.update.versions(),
        badge,
        tail,
    ]
}

pub fn print_group(o: &Overview, g: &LockStep, with_repo: bool) {
    let repo = if with_repo {
        format!("{}  ", g.repo)
    } else {
        String::new()
    };
    println!(
        "📦 {repo}{}  {} → {}  {}  ({})",
        g.prefix,
        g.from,
        g.to,
        g.change.badge(),
        g.status.label(),
    );
    let rows: Vec<Vec<String>> = o
        .members(g)
        .iter()
        .map(|p| {
            let state = p
                .reason
                .clone()
                .unwrap_or_else(|| p.category.label().to_string());
            vec![p.label(false), p.update.name(), state]
        })
        .collect();
    println!("{}", table(&rows, "   "));
}

#[derive(Serialize)]
struct ListJson<'a> {
    scope: String,
    counts: Counts,
    groups: Vec<GroupJson<'a>>,
    ready: Vec<&'a DepPr>,
    needs_rebase: Vec<&'a DepPr>,
    blocked: Vec<&'a DepPr>,
}

#[derive(Serialize)]
struct Counts {
    total: usize,
    ready: usize,
    needs_rebase: usize,
    blocked: usize,
}

#[derive(Serialize)]
struct GroupJson<'a> {
    #[serde(flatten)]
    group: &'a LockStep,
    members: Vec<&'a DepPr>,
}

impl<'a> ListJson<'a> {
    fn new(scope: &Scope, o: &'a Overview) -> Self {
        Self {
            scope: scope.to_string(),
            counts: Counts {
                total: o.prs.len(),
                ready: o.count(Category::Ready),
                needs_rebase: o.count(Category::NeedsRebase),
                blocked: o.count(Category::Blocked),
            },
            groups: o
                .groups
                .iter()
                .map(|g| GroupJson {
                    group: g,
                    members: o.members(g),
                })
                .collect(),
            ready: o.singles(Category::Ready),
            needs_rebase: o.singles(Category::NeedsRebase),
            blocked: o.singles(Category::Blocked),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{ctx, mock_prs, node, pr, ready};
    use super::*;
    use crate::api::Repo;
    use crate::api::dependabot::alert_fixture;
    use mockito::{Matcher, Server};
    use serde_json::{Value, json};

    fn args(org: Option<&str>, security: bool) -> DependabotListArgs {
        DependabotListArgs {
            org: org.map(str::to_string),
            security,
        }
    }

    /// A serde lock-step pair, a lone ready PR, one that needs a rebase, and one that's blocked.
    fn nodes(repo: &str) -> Vec<Value> {
        vec![
            node(
                repo,
                1,
                "Bump serde from 1.0.1 to 1.0.2",
                "MERGEABLE",
                "CLEAN",
                Some("SUCCESS"),
            ),
            node(
                repo,
                2,
                "Bump serde_json from 1.0.1 to 1.0.2",
                "MERGEABLE",
                "CLEAN",
                Some("SUCCESS"),
            ),
            node(
                repo,
                3,
                "Bump lodash from 4.17.20 to 4.17.21",
                "MERGEABLE",
                "CLEAN",
                Some("SUCCESS"),
            ),
            node(
                repo,
                4,
                "Bump tokio from 1.0.0 to 1.1.0",
                "CONFLICTING",
                "DIRTY",
                None,
            ),
            node(
                repo,
                5,
                "Bump pino from 9.0.0 to 10.0.0",
                "MERGEABLE",
                "UNSTABLE",
                Some("PENDING"),
            ),
        ]
    }

    #[test]
    fn json_sorts_prs_into_groups_and_sections() {
        let o = Overview::new(vec![
            pr("o/r", 1, "Bump serde from 1.0.1 to 1.0.2", Category::Ready),
            pr(
                "o/r",
                2,
                "Bump serde_json from 1.0.1 to 1.0.2",
                Category::Ready,
            ),
            pr(
                "o/r",
                3,
                "Bump lodash from 4.17.20 to 4.17.21",
                Category::Ready,
            ),
            pr(
                "o/r",
                4,
                "Bump tokio from 1.0.0 to 1.1.0",
                Category::NeedsRebase,
            ),
            pr(
                "o/r",
                5,
                "Bump pino from 9.0.0 to 10.0.0",
                Category::Blocked,
            ),
        ]);
        let scope = Scope::Repo(Repo::parse("o/r").unwrap());
        let v = serde_json::to_value(ListJson::new(&scope, &o)).unwrap();
        assert_eq!(v["scope"], "o/r");
        assert_eq!(
            v["counts"],
            json!({ "total": 5, "ready": 3, "needs_rebase": 1, "blocked": 1 })
        );
        let group = &v["groups"][0];
        assert_eq!(group["prefix"], "serde");
        assert_eq!(group["prs"], json!([1, 2]));
        assert_eq!(group["status"], "ready");
        assert_eq!(group["change"], "patch");
        assert_eq!(group["members"][1]["number"], 2);
        let numbers = |key: &str| -> Vec<u64> {
            v[key]
                .as_array()
                .unwrap()
                .iter()
                .map(|p| p["number"].as_u64().unwrap())
                .collect()
        };
        assert_eq!(numbers("ready"), vec![3]);
        assert_eq!(numbers("needs_rebase"), vec![4]);
        assert_eq!(numbers("blocked"), vec![5]);
    }

    #[test]
    fn lists_a_repo_as_text_and_json() {
        for json in [false, true] {
            let mut server = Server::new();
            let (light, details) = mock_prs(&mut server, false, &nodes("o/r"));
            run(&ctx(&server, json), args(None, false)).unwrap();
            light.assert();
            details.assert();
        }
    }

    #[test]
    fn lists_an_org_through_search() {
        let mut server = Server::new();
        let mut prs = nodes("o/a");
        prs.push(ready(9, "Bump serde from 1.0.1 to 1.0.2"));
        prs[5]["repository"]["nameWithOwner"] = json!("o/b");
        let (light, details) = mock_prs(&mut server, true, &prs);
        run(&ctx(&server, false), args(Some("acme"), false)).unwrap();
        light.assert();
        details.assert();
    }

    #[test]
    fn security_reads_alerts_and_fails_without_access() {
        for json in [false, true] {
            let mut server = Server::new();
            mock_prs(&mut server, false, &nodes("o/r"));
            let alerts = server
                .mock("GET", "/repos/o/r/dependabot/alerts")
                .match_query(Matcher::UrlEncoded("state".into(), "open".into()))
                .with_body(json!([alert_fixture()]).to_string())
                .create();
            run(&ctx(&server, json), args(None, true)).unwrap();
            alerts.assert();
        }

        let mut server = Server::new();
        mock_prs(&mut server, false, &nodes("o/r"));
        server
            .mock("GET", "/repos/o/r/dependabot/alerts")
            .match_query(Matcher::Any)
            .with_status(403)
            .with_body(r#"{"message": "Resource not accessible by integration"}"#)
            .create();
        let err = run(&ctx(&server, true), args(None, true)).unwrap_err();
        assert!(
            err.to_string()
                .starts_with("Dependabot alerts are disabled for o/r")
        );
    }

    #[test]
    fn row_shows_reason_for_blocked() {
        let mut p = pr(
            "o/r",
            8,
            "Bump pino from 9.14.0 to 10.4.0",
            Category::Blocked,
        );
        p.reason = Some("failing checks".into());
        assert_eq!(
            row(&p, false),
            vec![
                "#8",
                "pino",
                "9.14.0 → 10.4.0",
                "🔴 major",
                "failing checks"
            ]
        );
    }

    #[test]
    fn row_omits_badge_for_groups() {
        let p = pr(
            "o/r",
            5,
            "Bump the dev group with 7 updates",
            Category::Ready,
        );
        let r = row(&p, true);
        assert_eq!(r[0], "o/r#5");
        assert_eq!(r[1], "dev group (7 updates)");
        assert_eq!(r[3], "");
    }
}
