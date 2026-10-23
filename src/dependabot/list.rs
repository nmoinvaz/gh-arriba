use anyhow::Result;
use serde::Serialize;
use std::collections::BTreeSet;

use super::{Category, DepPr, LockStep, Overview, Scope, alerts, table};
use crate::cli::DependabotListArgs;
use crate::context::Ctx;
use crate::output::print_json;

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
    use super::super::tests::pr;
    use super::*;

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
