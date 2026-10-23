use anyhow::{Result, anyhow, bail};
use serde::Serialize;
use serde_json::json;

use super::list::print_group;
use super::{Category, DepPr, LockStep, Overview, REBASE_COMMENT, Scope, table};
use crate::cli::{DependabotMergeArgs, MergeMethod};
use crate::context::{Ctx, Repo};
use crate::output::print_json;
use crate::prompt;

/// How one PR gets merged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Action {
    /// Approve and merge now. With `fallback`, a PR that conflicts switches to auto-merge and asks
    /// Dependabot to rebase, which is what later PRs in a lock-step group usually need.
    Merge { fallback: bool },
    /// Approve and enable auto-merge. With `rebase`, also ask Dependabot to rebase.
    Auto { rebase: bool },
}

/// A lock-step group or a lone PR, the unit a person decides on.
enum Unit<'a> {
    Group(&'a LockStep),
    Single(&'a DepPr),
}

#[derive(Serialize)]
struct Outcome {
    number: u64,
    package: String,
    versions: String,
    action: String,
    ok: bool,
}

pub fn run(ctx: &Ctx, args: DependabotMergeArgs) -> Result<()> {
    let interactive = args.prs.is_empty() && !args.all_ready;
    if interactive && !prompt::interactive() {
        bail!("Pass PR numbers or --all-ready, or run in a terminal to decide PR by PR.");
    }

    let repo = ctx.repo()?;
    let overview = Overview::load(&ctx.client, &Scope::Repo(repo.clone()))?;
    let units = plan_units(
        &overview,
        &repo.to_string(),
        &args.prs,
        args.all_ready,
        args.auto,
    )?;
    let merger = Merger {
        ctx,
        repo: &repo,
        comment: args.comment,
        method: args.method,
    };

    let mut outcomes = Vec::new();
    if units.is_empty() {
        if !ctx.json {
            println!("Nothing to merge in {repo}.");
        }
    } else if interactive {
        run_interactive(&merger, &overview, &units, args.auto, &mut outcomes)?;
    } else {
        let steps: Vec<(&DepPr, Action)> = units
            .iter()
            .flat_map(|u| unit_steps(&overview, u, args.auto))
            .collect();
        if !ctx.json {
            println!("Plan for {repo}");
            let rows: Vec<Vec<String>> = steps
                .iter()
                .map(|(p, a)| {
                    vec![
                        format!("#{}", p.number),
                        p.update.name(),
                        p.update.versions(),
                        describe(p, *a, merger.comment.is_some(), merger.method),
                    ]
                })
                .collect();
            println!("{}", table(&rows, "  "));
        }
        let all_auto = steps.iter().all(|(_, a)| matches!(a, Action::Auto { .. }));
        let verb = if all_auto {
            "Approve and auto-merge"
        } else {
            "Merge"
        };
        let question = format!("{verb} {} PR(s) in {repo}?", steps.len());
        if !prompt::confirm(&question, ctx.yes)? {
            println!("Cancelled.");
            return Ok(());
        }
        for (pr, action) in steps {
            outcomes.push(merger.run(pr, action));
        }
    }

    let acted: Vec<u64> = outcomes.iter().map(|o| o.number).collect();
    let blocked: Vec<&DepPr> = overview
        .prs
        .iter()
        .filter(|p| p.category == Category::Blocked && !acted.contains(&p.number))
        .collect();
    let failed = outcomes.iter().filter(|o| !o.ok).count();

    if ctx.json {
        let blocked: Vec<_> = blocked
            .iter()
            .map(|p| json!({ "number": p.number, "package": p.update.name(), "reason": p.reason }))
            .collect();
        print_json(&json!({ "repo": repo.to_string(), "outcomes": outcomes, "blocked": blocked }))?;
    } else {
        print_summary(&repo, &outcomes, &blocked);
    }
    if failed > 0 {
        bail!("{failed} PR(s) were not merged");
    }
    Ok(())
}

// -------------------------------------------------------------------------
//  planning
// -------------------------------------------------------------------------

/// Pick what to merge. Explicit numbers come first, in the order given. Otherwise take lock-step
/// groups and then lone PRs. `--all-ready` skips work that needs a rebase unless `--auto` lets it
/// wait for one, and the interactive flow offers both.
fn plan_units<'a>(
    o: &'a Overview,
    repo: &str,
    numbers: &[u64],
    all_ready: bool,
    auto: bool,
) -> Result<Vec<Unit<'a>>> {
    if !numbers.is_empty() {
        let mut units = Vec::new();
        for n in numbers {
            let pr = o
                .get(repo, *n)
                .ok_or_else(|| anyhow!("#{n} is not an open Dependabot PR in {repo}"))?;
            if !units
                .iter()
                .any(|u| matches!(u, Unit::Single(p) if p.number == *n))
            {
                units.push(Unit::Single(pr));
            }
        }
        return Ok(units);
    }

    let wanted = |c: Category| {
        c == Category::Ready || (c == Category::NeedsRebase && (auto || !all_ready))
    };
    let mut units: Vec<Unit> = o
        .groups
        .iter()
        .filter(|g| wanted(g.status))
        .map(Unit::Group)
        .collect();
    units.extend(
        o.prs
            .iter()
            .filter(|p| !o.is_grouped(p) && wanted(p.category))
            .map(Unit::Single),
    );
    Ok(units)
}

/// The PRs in a unit with the action each gets.
fn unit_steps<'a>(o: &'a Overview, unit: &Unit<'a>, auto: bool) -> Vec<(&'a DepPr, Action)> {
    match unit {
        Unit::Single(p) => vec![(*p, single_action(p, auto))],
        Unit::Group(g) => {
            let members = o.members(g);
            if auto || g.status == Category::NeedsRebase {
                return members
                    .into_iter()
                    .map(|p| {
                        let rebase = p.category == Category::NeedsRebase;
                        (p, Action::Auto { rebase })
                    })
                    .collect();
            }
            // The first PR merges now. The rest touch the same lockfile, so each one that already
            // conflicts waits for a rebase, and each that doesn't yet tries a direct merge.
            members
                .into_iter()
                .enumerate()
                .map(|(i, p)| {
                    let action = if i == 0 {
                        Action::Merge { fallback: false }
                    } else if p.category == Category::NeedsRebase {
                        Action::Auto { rebase: true }
                    } else {
                        Action::Merge { fallback: true }
                    };
                    (p, action)
                })
                .collect()
        }
    }
}

fn single_action(p: &DepPr, auto: bool) -> Action {
    let rebase = p.category == Category::NeedsRebase;
    if auto || rebase {
        Action::Auto { rebase }
    } else {
        Action::Merge { fallback: false }
    }
}

/// What will happen to a PR, as `approve, merge (rebase)`.
fn describe(p: &DepPr, action: Action, comment: bool, method: MergeMethod) -> String {
    let mut parts = Vec::new();
    if comment {
        parts.push("comment".to_string());
    }
    if p.review_decision.as_deref() != Some("APPROVED") {
        parts.push("approve".to_string());
    }
    let method = method_name(method);
    parts.push(match action {
        Action::Merge { fallback: false } => format!("merge ({method})"),
        Action::Merge { fallback: true } => {
            format!("merge ({method}), or auto-merge if it conflicts")
        }
        Action::Auto { rebase: true } => "auto-merge, request rebase".to_string(),
        Action::Auto { rebase: false } => "auto-merge".to_string(),
    });
    if let Some(reason) = &p.reason {
        parts.push(format!("despite {reason}"));
    }
    parts.join(", ")
}

// -------------------------------------------------------------------------
//  interactive
// -------------------------------------------------------------------------

fn run_interactive(
    merger: &Merger,
    o: &Overview,
    units: &[Unit],
    auto: bool,
    outcomes: &mut Vec<Outcome>,
) -> Result<()> {
    for (i, unit) in units.iter().enumerate() {
        println!();
        show_unit(o, unit);
        let steps = unit_steps(o, unit, auto);
        let all = matches!(unit, Unit::Group(_));
        let go = match (steps[0].1, all) {
            (Action::Merge { .. }, true) => "Merge all",
            (Action::Merge { .. }, false) => "Merge",
            (Action::Auto { rebase: true }, true) => "Rebase & auto-merge all",
            (Action::Auto { rebase: true }, false) => "Rebase & auto-merge",
            (Action::Auto { rebase: false }, true) => "Approve & auto-merge all",
            (Action::Auto { rebase: false }, false) => "Approve & auto-merge",
        };
        let skip = if all { "Skip all" } else { "Skip" };
        let options = [go.to_string(), skip.to_string(), "Stop".to_string()];
        match prompt::select("Action", &options, 0)? {
            0 => {
                for (pr, action) in steps {
                    outcomes.push(merger.run(pr, action));
                }
            }
            1 => outcomes.extend(steps.iter().map(|(p, _)| skipped(p))),
            _ => {
                for rest in &units[i..] {
                    outcomes.extend(unit_steps(o, rest, auto).iter().map(|(p, _)| skipped(p)));
                }
                break;
            }
        }
    }
    Ok(())
}

fn show_unit(o: &Overview, unit: &Unit) {
    match unit {
        Unit::Group(g) => {
            print_group(o, g, false);
            let members = o.members(g);
            let files: u64 = members.iter().map(|p| p.changed_files).sum();
            let adds: u64 = members.iter().map(|p| p.additions).sum();
            let dels: u64 = members.iter().map(|p| p.deletions).sum();
            println!("   {}", g.change.risk());
            println!("   {files} files changed in total (+{adds} -{dels})");
        }
        Unit::Single(p) => {
            println!("📦 {}", p.update.name());
            if p.update.from.is_some() {
                println!("   {}  {}", p.update.versions(), p.update.change.risk());
            } else {
                println!("   {}", p.title);
            }
            println!(
                "   PR #{} opened {}  {}",
                p.number,
                p.created_at.format("%Y-%m-%d"),
                p.url
            );
            println!("   {}  {}", p.size(), p.category.label());
        }
    }
}

fn skipped(p: &DepPr) -> Outcome {
    Outcome {
        number: p.number,
        package: p.update.name(),
        versions: p.update.versions(),
        action: "skipped".to_string(),
        ok: true,
    }
}

fn print_summary(repo: &Repo, outcomes: &[Outcome], blocked: &[&DepPr]) {
    if !outcomes.is_empty() {
        println!("\nDependabot merge summary for {repo}");
        let mut rows = vec![vec![
            "PR".to_string(),
            "Package".to_string(),
            "Version change".to_string(),
            "Action".to_string(),
        ]];
        rows.extend(outcomes.iter().map(|o| {
            vec![
                format!("#{}", o.number),
                o.package.clone(),
                o.versions.clone(),
                o.action.clone(),
            ]
        }));
        println!("{}", table(&rows, "  "));
    }
    if !blocked.is_empty() {
        println!("\nNot ready (blocked)");
        let rows: Vec<Vec<String>> = blocked
            .iter()
            .map(|p| {
                vec![
                    format!("#{}", p.number),
                    p.update.name(),
                    p.update.versions(),
                    p.reason.clone().unwrap_or_default(),
                ]
            })
            .collect();
        println!("{}", table(&rows, "  "));
    }
}

// -------------------------------------------------------------------------
//  execution
// -------------------------------------------------------------------------

const ENABLE_AUTO_MERGE: &str = r#"
mutation($id: ID!, $method: PullRequestMergeMethod!) {
  enablePullRequestAutoMerge(input: {pullRequestId: $id, mergeMethod: $method}) {
    clientMutationId
  }
}"#;

struct Merger<'a> {
    ctx: &'a Ctx,
    repo: &'a Repo,
    comment: Option<String>,
    method: MergeMethod,
}

impl Merger<'_> {
    /// Carry out one PR's action and report it. Failures are recorded, never raised, so one bad
    /// PR doesn't stop the rest.
    fn run(&self, pr: &DepPr, action: Action) -> Outcome {
        let (action, ok) = match self.try_run(pr, action) {
            Ok(done) => (done, true),
            Err(e) => (format!("failed: {}", explain(&e, self.repo)), false),
        };
        if !self.ctx.json {
            println!("  #{} {}: {action}", pr.number, pr.update.name());
        }
        Outcome {
            number: pr.number,
            package: pr.update.name(),
            versions: pr.update.versions(),
            action,
            ok,
        }
    }

    fn try_run(&self, pr: &DepPr, action: Action) -> Result<String> {
        if let Some(c) = &self.comment {
            self.comment(pr, c)?;
        }
        self.approve(pr)?;
        match action {
            Action::Merge { fallback } => match self.merge(pr) {
                Ok(()) => Ok(format!("merged ({})", method_name(self.method))),
                Err(e) if fallback && is_conflict(&e) => self.auto(pr, true),
                Err(e) => Err(e),
            },
            Action::Auto { rebase } => self.auto(pr, rebase),
        }
    }

    fn auto(&self, pr: &DepPr, rebase: bool) -> Result<String> {
        if let Err(e) = self.enable_auto_merge(pr) {
            // GitHub refuses auto-merge on a PR that can merge right now, so merge it directly.
            if e.to_string().contains("clean status") {
                self.merge(pr)?;
                return Ok(format!("merged ({})", method_name(self.method)));
            }
            return Err(e);
        }
        if rebase {
            self.comment(pr, REBASE_COMMENT)?;
            Ok("auto-merge enabled, rebase requested".to_string())
        } else {
            Ok("auto-merge enabled".to_string())
        }
    }

    fn comment(&self, pr: &DepPr, body: &str) -> Result<()> {
        let path = format!("{}/issues/{}/comments", self.repo.api(), pr.number);
        self.ctx.client.post(&path, &json!({ "body": body }))?;
        Ok(())
    }

    fn approve(&self, pr: &DepPr) -> Result<()> {
        // An existing approval already satisfies review requirements.
        if pr.review_decision.as_deref() == Some("APPROVED") {
            return Ok(());
        }
        let path = format!("{}/pulls/{}/reviews", self.repo.api(), pr.number);
        self.ctx.client.post(&path, &json!({ "event": "APPROVE" }))?;
        Ok(())
    }

    fn merge(&self, pr: &DepPr) -> Result<()> {
        let path = format!("{}/pulls/{}/merge", self.repo.api(), pr.number);
        let body = json!({ "merge_method": method_name(self.method) });
        self.ctx.client.put(&path, &body)?;

        // Repos that delete merged branches themselves answer 422 or 404 here.
        let branch = format!("{}/git/refs/heads/{}", self.repo.api(), pr.head_ref);
        if let Err(e) = self.ctx.client.delete(&branch) {
            let msg = e.to_string();
            if !msg.contains("GitHub API 422") && !msg.contains("GitHub API 404") {
                eprintln!("  warning: could not delete {}: {msg}", pr.head_ref);
            }
        }
        Ok(())
    }

    fn enable_auto_merge(&self, pr: &DepPr) -> Result<()> {
        let method = method_name(self.method).to_uppercase();
        self.ctx.client.graphql(
            ENABLE_AUTO_MERGE,
            json!({ "id": pr.node_id, "method": method }),
        )?;
        Ok(())
    }
}

/// REST merge method name, which uppercased is also the GraphQL enum value.
fn method_name(method: MergeMethod) -> &'static str {
    match method {
        MergeMethod::Merge => "merge",
        MergeMethod::Squash => "squash",
        MergeMethod::Rebase => "rebase",
    }
}

/// Merge refusals that a rebase can fix: 405 not mergeable, 409 head changed.
fn is_conflict(e: &anyhow::Error) -> bool {
    let msg = e.to_string();
    msg.contains("GitHub API 405") || msg.contains("GitHub API 409")
}

fn explain(e: &anyhow::Error, repo: &Repo) -> String {
    let msg = e.to_string();
    if msg.to_lowercase().contains("auto merge is not allowed") {
        format!("auto-merge is disabled in {repo} settings")
    } else {
        msg
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::pr;
    use super::*;

    fn overview() -> Overview {
        let mut needs = pr("o/r", 13, "Bump @types/x from 1.0.0 to 1.0.1", Category::NeedsRebase);
        needs.review_decision = Some("APPROVED".into());
        let mut blocked = pr("o/r", 20, "Bump pino from 9.14.0 to 10.4.0", Category::Blocked);
        blocked.reason = Some("failing checks".into());
        Overview::new(vec![
            pr("o/r", 10, "Bump serde from 1.0.1 to 1.0.2", Category::Ready),
            pr("o/r", 11, "Bump serde_json from 1.0.1 to 1.0.2", Category::Ready),
            pr("o/r", 12, "Bump serde_derive from 1.0.1 to 1.0.2", Category::NeedsRebase),
            needs,
            pr("o/r", 14, "Bump tokio from 1.0.0 to 1.1.0", Category::Ready),
            pr("o/r", 15, "Bump eslint-a from 2.0.0 to 3.0.0", Category::NeedsRebase),
            pr("o/r", 16, "Bump eslint-b from 2.0.0 to 3.0.0", Category::Ready),
            blocked,
        ])
    }

    fn numbers(units: &[Unit]) -> Vec<String> {
        units
            .iter()
            .map(|u| match u {
                Unit::Group(g) => format!("group {}", g.prefix),
                Unit::Single(p) => format!("#{}", p.number),
            })
            .collect()
    }

    #[test]
    fn all_ready_takes_ready_groups_and_singles() {
        let o = overview();
        let units = plan_units(&o, "o/r", &[], true, false).unwrap();
        assert_eq!(numbers(&units), vec!["group serde", "#14"]);
    }

    #[test]
    fn all_ready_with_auto_adds_rebase_work() {
        let o = overview();
        let units = plan_units(&o, "o/r", &[], true, true).unwrap();
        assert_eq!(numbers(&units), vec!["group eslint", "group serde", "#13", "#14"]);
    }

    #[test]
    fn interactive_offers_ready_and_rebase_work() {
        let o = overview();
        let units = plan_units(&o, "o/r", &[], false, false).unwrap();
        assert_eq!(numbers(&units), vec!["group eslint", "group serde", "#13", "#14"]);
    }

    #[test]
    fn explicit_numbers_keep_order_and_reject_unknown() {
        let o = overview();
        let units = plan_units(&o, "o/r", &[14, 10, 14], false, false).unwrap();
        assert_eq!(numbers(&units), vec!["#14", "#10"]);
        let err = plan_units(&o, "o/r", &[99], false, false).err().unwrap();
        assert_eq!(err.to_string(), "#99 is not an open Dependabot PR in o/r");
    }

    #[test]
    fn ready_group_merges_first_then_falls_back() {
        let o = overview();
        let g = o.groups.iter().find(|g| g.prefix == "serde").unwrap();
        let steps: Vec<(u64, Action)> = unit_steps(&o, &Unit::Group(g), false)
            .into_iter()
            .map(|(p, a)| (p.number, a))
            .collect();
        assert_eq!(
            steps,
            vec![
                (10, Action::Merge { fallback: false }),
                (11, Action::Merge { fallback: true }),
                (12, Action::Auto { rebase: true }),
            ]
        );
    }

    #[test]
    fn rebase_group_waits_for_auto_merge() {
        let o = overview();
        let g = o.groups.iter().find(|g| g.prefix == "eslint").unwrap();
        assert_eq!(g.status, Category::NeedsRebase);
        let steps: Vec<(u64, Action)> = unit_steps(&o, &Unit::Group(g), false)
            .into_iter()
            .map(|(p, a)| (p.number, a))
            .collect();
        assert_eq!(
            steps,
            vec![
                (15, Action::Auto { rebase: true }),
                (16, Action::Auto { rebase: false }),
            ]
        );
    }

    #[test]
    fn singles_merge_or_wait() {
        let o = overview();
        assert_eq!(
            single_action(o.get("o/r", 14).unwrap(), false),
            Action::Merge { fallback: false }
        );
        assert_eq!(
            single_action(o.get("o/r", 13).unwrap(), false),
            Action::Auto { rebase: true }
        );
        assert_eq!(
            single_action(o.get("o/r", 14).unwrap(), true),
            Action::Auto { rebase: false }
        );
    }

    #[test]
    fn descriptions() {
        let o = overview();
        let ready = o.get("o/r", 14).unwrap();
        assert_eq!(
            describe(ready, Action::Merge { fallback: false }, false, MergeMethod::Rebase),
            "approve, merge (rebase)"
        );
        let approved = o.get("o/r", 13).unwrap();
        assert_eq!(
            describe(approved, Action::Auto { rebase: true }, true, MergeMethod::Squash),
            "comment, auto-merge, request rebase"
        );
        let blocked = o.get("o/r", 20).unwrap();
        assert_eq!(
            describe(blocked, Action::Merge { fallback: false }, false, MergeMethod::Merge),
            "approve, merge (merge), despite failing checks"
        );
    }

    #[test]
    fn conflict_and_settings_errors() {
        assert!(is_conflict(&anyhow!("GitHub API 405 on PUT x: Pull Request is not mergeable")));
        assert!(!is_conflict(&anyhow!("GitHub API 403 on PUT x: forbidden")));
        let repo = Repo::parse("o/r").unwrap();
        assert_eq!(
            explain(
                &anyhow!("GraphQL error: Pull request Auto merge is not allowed for this repository"),
                &repo
            ),
            "auto-merge is disabled in o/r settings"
        );
    }
}
