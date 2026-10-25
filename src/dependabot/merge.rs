use anyhow::{Result, anyhow, bail};
use serde::Serialize;
use serde_json::json;

use super::list::print_group;
use super::{Category, DepPr, LockStep, Overview, REBASE_COMMENT, Scope};
use crate::api::{PrRef, Repo, dependabot, pulls};
use crate::cli::{DependabotMergeArgs, MergeMethod};
use crate::context::Ctx;
use crate::output::print_json;
use crate::output::table;
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
        let immediate = steps.iter().any(|(p, a)| merges_now(p, *a));
        let verb = if immediate {
            "Merge"
        } else {
            "Approve and auto-merge"
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

    let wanted =
        |c: Category| c == Category::Ready || (c == Category::NeedsRebase && (auto || !all_ready));
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
    let method = method.name();
    parts.push(match action {
        Action::Merge { fallback: false } => format!("merge ({method})"),
        Action::Merge { fallback: true } => {
            format!("merge ({method}), or auto-merge if it conflicts")
        }
        Action::Auto { rebase: true } => "auto-merge, request rebase".to_string(),
        Action::Auto { rebase: false } if immediately_mergeable(&p.merge_state) => {
            format!("merge ({method}), it can't wait for auto-merge")
        }
        Action::Auto { rebase: false } => "auto-merge".to_string(),
    });
    if let Some(reason) = &p.reason {
        parts.push(format!("despite {reason}"));
    }
    parts.join(", ")
}

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
                Ok(()) => Ok(format!("merged ({})", self.method.name())),
                Err(e) if fallback && is_conflict(&e) => self.auto(pr, true),
                Err(e) => Err(e),
            },
            Action::Auto { rebase } => self.auto(pr, rebase),
        }
    }

    fn auto(&self, pr: &DepPr, rebase: bool) -> Result<String> {
        // GitHub refuses auto-merge on a PR that can merge right now, so merge it directly, as
        // `gh pr merge --auto` does. A PR waiting on a rebase never qualifies, and its cached state
        // may predate the conflict that sent it here.
        if !rebase && immediately_mergeable(&pr.merge_state) {
            self.merge(pr)?;
            return Ok(format!("merged ({})", self.method.name()));
        }
        if let Err(e) = self.enable_auto_merge(pr) {
            // The PR became mergeable after the approval above.
            if e.to_string().contains("clean status") {
                self.merge(pr)?;
                return Ok(format!("merged ({})", self.method.name()));
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
        let pr_ref = PrRef {
            repo: self.repo.clone(),
            number: pr.number,
        };
        pulls::add_issue_comment(&self.ctx.client, &pr_ref, body)
    }

    fn approve(&self, pr: &DepPr) -> Result<()> {
        // An existing approval already satisfies review requirements.
        if pr.review_decision.as_deref() == Some("APPROVED") {
            return Ok(());
        }
        dependabot::approve(&self.ctx.client, self.repo, pr.number)
    }

    fn merge(&self, pr: &DepPr) -> Result<()> {
        let client = &self.ctx.client;
        let pr_ref = PrRef {
            repo: self.repo.clone(),
            number: pr.number,
        };
        pulls::merge(client, &pr_ref, self.method.name(), None)?;

        // Repos that delete merged branches themselves answer 422 or 404 here.
        if let Err(e) = dependabot::delete_branch(client, self.repo, &pr.head_ref) {
            let msg = e.to_string();
            if !msg.contains("GitHub API 422") && !msg.contains("GitHub API 404") {
                eprintln!("  warning: could not delete {}: {msg}", pr.head_ref);
            }
        }
        Ok(())
    }

    fn enable_auto_merge(&self, pr: &DepPr) -> Result<()> {
        pulls::enable_auto_merge(&self.ctx.client, &pr.node_id, self.method.name())
    }
}

/// Whether a step merges right away rather than waiting on auto-merge.
fn merges_now(p: &DepPr, action: Action) -> bool {
    match action {
        Action::Merge { .. } => true,
        Action::Auto { rebase } => !rebase && immediately_mergeable(&p.merge_state),
    }
}

/// Merge states GitHub merges without waiting, mirroring `isImmediatelyMergeable` in gh.
fn immediately_mergeable(merge_state: &str) -> bool {
    matches!(merge_state, "CLEAN" | "HAS_HOOKS" | "UNSTABLE")
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
    use super::super::tests::{ctx, mock_prs, node, pr};
    use super::*;
    use mockito::{Matcher, Mock, Server, ServerGuard};
    use serde_json::Value;

    fn args(prs: &[u64], all_ready: bool, auto: bool) -> DependabotMergeArgs {
        DependabotMergeArgs {
            prs: prs.to_vec(),
            all_ready,
            auto,
            comment: None,
            method: MergeMethod::Rebase,
        }
    }

    fn ok(server: &mut ServerGuard, method: &str, path: &str, body: Value) -> Mock {
        server
            .mock(method, path)
            .match_body(Matcher::PartialJson(body))
            .with_body("{}")
            .create()
    }

    fn approve(server: &mut ServerGuard, n: u64) -> Mock {
        let path = format!("/repos/o/r/pulls/{n}/reviews");
        ok(server, "POST", &path, json!({ "event": "APPROVE" }))
    }

    fn merge(server: &mut ServerGuard, n: u64, method: &str, status: usize) -> Mock {
        server
            .mock("PUT", format!("/repos/o/r/pulls/{n}/merge").as_str())
            .match_body(Matcher::Json(json!({ "merge_method": method })))
            .with_status(status)
            .with_body(r#"{"message": "Pull Request is not mergeable"}"#)
            .create()
    }

    fn delete(server: &mut ServerGuard, n: u64, status: usize) -> Mock {
        let path = format!("/repos/o/r/git/refs/heads/dependabot/cargo/pr-{n}");
        server
            .mock("DELETE", path.as_str())
            .with_status(status)
            .with_body(r#"{"message": "Reference does not exist"}"#)
            .create()
    }

    /// The auto-merge mutation for PR `n`, answering with `errors` when given.
    fn auto(server: &mut ServerGuard, n: u64, error: Option<&str>) -> Mock {
        let body = match error {
            Some(e) => json!({ "data": null, "errors": [{ "message": e }] }),
            None => json!({ "data": { "enablePullRequestAutoMerge": null } }),
        };
        server
            .mock("POST", "/graphql")
            .match_body(Matcher::AllOf(vec![
                Matcher::Regex("enablePullRequestAutoMerge".into()),
                Matcher::PartialJson(
                    json!({ "variables": { "id": format!("PR_{n}"), "method": "REBASE" } }),
                ),
            ]))
            .with_body(body.to_string())
            .create()
    }

    fn comment(server: &mut ServerGuard, n: u64, body: &str) -> Mock {
        let path = format!("/repos/o/r/issues/{n}/comments");
        ok(server, "POST", &path, json!({ "body": body }))
    }

    #[test]
    fn merges_a_chosen_pr_with_comment_and_method() {
        for json in [false, true] {
            let mut server = Server::new();
            mock_prs(
                &mut server,
                false,
                &[
                    node(
                        "o/r",
                        3,
                        "Bump a from 1.0.0 to 1.0.1",
                        "MERGEABLE",
                        "CLEAN",
                        None,
                    ),
                    node(
                        "o/r",
                        4,
                        "Bump b from 1.0.0 to 1.0.1",
                        "MERGEABLE",
                        "CLEAN",
                        None,
                    ),
                ],
            );
            let mocks = [
                comment(&mut server, 3, "Thanks!"),
                approve(&mut server, 3),
                merge(&mut server, 3, "squash", 200),
                // The repository deleted the branch itself.
                delete(&mut server, 3, 422),
            ];
            let mut a = args(&[3], false, false);
            a.comment = Some("Thanks!".into());
            a.method = MergeMethod::Squash;
            run(&ctx(&server, json), a).unwrap();
            for m in mocks {
                m.assert();
            }
        }
    }

    #[test]
    fn all_ready_merges_a_lockstep_group_in_order() {
        for json in [false, true] {
            let mut server = Server::new();
            let mut approved = node(
                "o/r",
                3,
                "Bump serde_derive from 1.0.1 to 1.0.2",
                "CONFLICTING",
                "DIRTY",
                Some("SUCCESS"),
            );
            approved["reviewDecision"] = json!("APPROVED");
            mock_prs(
                &mut server,
                false,
                &[
                    node(
                        "o/r",
                        1,
                        "Bump serde from 1.0.1 to 1.0.2",
                        "MERGEABLE",
                        "CLEAN",
                        None,
                    ),
                    node(
                        "o/r",
                        2,
                        "Bump serde_json from 1.0.1 to 1.0.2",
                        "MERGEABLE",
                        "CLEAN",
                        None,
                    ),
                    approved,
                    // Left alone: it needs a rebase and --auto isn't set.
                    node(
                        "o/r",
                        4,
                        "Bump tokio from 1.0.0 to 1.1.0",
                        "CONFLICTING",
                        "DIRTY",
                        None,
                    ),
                    // Left alone and reported: its checks fail.
                    node(
                        "o/r",
                        5,
                        "Bump pino from 9.0.0 to 10.0.0",
                        "MERGEABLE",
                        "UNSTABLE",
                        Some("FAILURE"),
                    ),
                ],
            );
            let mocks = [
                // The first PR merges. A failed branch delete only warns.
                approve(&mut server, 1),
                merge(&mut server, 1, "rebase", 200),
                delete(&mut server, 1, 500),
                // The second conflicts after the first lands, so it waits for a rebase.
                approve(&mut server, 2),
                merge(&mut server, 2, "rebase", 405),
                auto(&mut server, 2, None),
                comment(&mut server, 2, "@dependabot rebase"),
                // The third already conflicted and was approved, so it only waits.
                auto(&mut server, 3, None),
                comment(&mut server, 3, "@dependabot rebase"),
            ];
            run(&ctx(&server, json), args(&[], true, false)).unwrap();
            for m in mocks {
                m.assert();
            }
        }
    }

    #[test]
    fn auto_merge_falls_back_to_merging_and_reports_failures() {
        for json in [false, true] {
            let mut server = Server::new();
            mock_prs(
                &mut server,
                false,
                &[
                    // Waiting on a required review, but auto-merge is off for the repo.
                    node(
                        "o/r",
                        1,
                        "Bump a from 1.0.0 to 1.0.1",
                        "MERGEABLE",
                        "BLOCKED",
                        None,
                    ),
                    // Clean PRs can't take auto-merge, so they merge now.
                    node(
                        "o/r",
                        2,
                        "Bump b from 1.0.0 to 1.0.1",
                        "MERGEABLE",
                        "CLEAN",
                        None,
                    ),
                    // Became clean after approval, which GitHub reports as an error.
                    node(
                        "o/r",
                        3,
                        "Bump c from 1.0.0 to 1.0.1",
                        "MERGEABLE",
                        "BLOCKED",
                        None,
                    ),
                ],
            );
            let mocks = [
                approve(&mut server, 1),
                auto(
                    &mut server,
                    1,
                    Some("Pull request Auto merge is not allowed for this repository"),
                ),
                approve(&mut server, 2),
                merge(&mut server, 2, "rebase", 200),
                delete(&mut server, 2, 404),
                approve(&mut server, 3),
                auto(&mut server, 3, Some("Pull request is in clean status")),
                merge(&mut server, 3, "rebase", 200),
                delete(&mut server, 3, 204),
            ];
            let err = run(&ctx(&server, json), args(&[1, 2, 3], false, true)).unwrap_err();
            assert_eq!(err.to_string(), "1 PR(s) were not merged");
            for m in mocks {
                m.assert();
            }
        }
    }

    #[test]
    fn all_ready_with_auto_queues_rebase_work() {
        let mut server = Server::new();
        mock_prs(
            &mut server,
            false,
            &[node(
                "o/r",
                4,
                "Bump tokio from 1.0.0 to 1.1.0",
                "CONFLICTING",
                "DIRTY",
                None,
            )],
        );
        let mocks = [
            approve(&mut server, 4),
            auto(&mut server, 4, None),
            comment(&mut server, 4, "@dependabot rebase"),
        ];
        run(&ctx(&server, false), args(&[], true, true)).unwrap();
        for m in mocks {
            m.assert();
        }
    }

    #[test]
    fn approval_failure_stops_that_pr() {
        let mut server = Server::new();
        mock_prs(
            &mut server,
            false,
            &[node(
                "o/r",
                1,
                "Bump a from 1.0.0 to 1.0.1",
                "MERGEABLE",
                "CLEAN",
                None,
            )],
        );
        let approve = server
            .mock("POST", "/repos/o/r/pulls/1/reviews")
            .with_status(422)
            .with_body(r#"{"message": "Can not approve your own pull request"}"#)
            .create();
        let merge = merge(&mut server, 1, "rebase", 200).expect(0);
        let err = run(&ctx(&server, true), args(&[1], false, false)).unwrap_err();
        assert_eq!(err.to_string(), "1 PR(s) were not merged");
        approve.assert();
        merge.assert();
    }

    #[test]
    fn nothing_ready_does_nothing() {
        for json in [false, true] {
            let mut server = Server::new();
            mock_prs(
                &mut server,
                false,
                &[node(
                    "o/r",
                    5,
                    "Bump pino from 9.0.0 to 10.0.0",
                    "MERGEABLE",
                    "UNSTABLE",
                    Some("PENDING"),
                )],
            );
            run(&ctx(&server, json), args(&[], true, false)).unwrap();
        }
    }

    #[test]
    fn unknown_numbers_are_rejected_before_acting() {
        let mut server = Server::new();
        mock_prs(&mut server, false, &[]);
        let err = run(&ctx(&server, true), args(&[7], false, false)).unwrap_err();
        assert_eq!(err.to_string(), "#7 is not an open Dependabot PR in o/r");
    }

    fn overview() -> Overview {
        let mut needs = pr(
            "o/r",
            13,
            "Bump @types/x from 1.0.0 to 1.0.1",
            Category::NeedsRebase,
        );
        needs.review_decision = Some("APPROVED".into());
        let mut blocked = pr(
            "o/r",
            20,
            "Bump pino from 9.14.0 to 10.4.0",
            Category::Blocked,
        );
        blocked.reason = Some("failing checks".into());
        Overview::new(vec![
            pr("o/r", 10, "Bump serde from 1.0.1 to 1.0.2", Category::Ready),
            pr(
                "o/r",
                11,
                "Bump serde_json from 1.0.1 to 1.0.2",
                Category::Ready,
            ),
            pr(
                "o/r",
                12,
                "Bump serde_derive from 1.0.1 to 1.0.2",
                Category::NeedsRebase,
            ),
            needs,
            pr("o/r", 14, "Bump tokio from 1.0.0 to 1.1.0", Category::Ready),
            pr(
                "o/r",
                15,
                "Bump eslint-a from 2.0.0 to 3.0.0",
                Category::NeedsRebase,
            ),
            pr(
                "o/r",
                16,
                "Bump eslint-b from 2.0.0 to 3.0.0",
                Category::Ready,
            ),
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
        assert_eq!(
            numbers(&units),
            vec!["group serde", "group eslint", "#13", "#14"]
        );
    }

    #[test]
    fn interactive_offers_ready_and_rebase_work() {
        let o = overview();
        let units = plan_units(&o, "o/r", &[], false, false).unwrap();
        assert_eq!(
            numbers(&units),
            vec!["group serde", "group eslint", "#13", "#14"]
        );
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
            describe(
                ready,
                Action::Merge { fallback: false },
                false,
                MergeMethod::Rebase
            ),
            "approve, merge (rebase)"
        );
        let approved = o.get("o/r", 13).unwrap();
        assert_eq!(
            describe(
                approved,
                Action::Auto { rebase: true },
                true,
                MergeMethod::Squash
            ),
            "comment, auto-merge, request rebase"
        );
        let blocked = o.get("o/r", 20).unwrap();
        assert_eq!(
            describe(
                blocked,
                Action::Merge { fallback: false },
                false,
                MergeMethod::Merge
            ),
            "approve, merge (merge), despite failing checks"
        );
    }

    #[test]
    fn auto_merge_on_clean_pr_merges_now() {
        let o = overview();
        let clean = o.get("o/r", 14).unwrap();
        assert_eq!(
            describe(
                clean,
                Action::Auto { rebase: false },
                false,
                MergeMethod::Rebase
            ),
            "approve, merge (rebase), it can't wait for auto-merge"
        );
        let mut blocked_by_review = clean.clone();
        blocked_by_review.merge_state = "BLOCKED".into();
        assert_eq!(
            describe(
                &blocked_by_review,
                Action::Auto { rebase: false },
                false,
                MergeMethod::Rebase
            ),
            "approve, auto-merge"
        );
        assert!(merges_now(clean, Action::Auto { rebase: false }));
        assert!(!merges_now(
            &blocked_by_review,
            Action::Auto { rebase: false }
        ));
        assert!(!merges_now(clean, Action::Auto { rebase: true }));
        assert!(immediately_mergeable("UNSTABLE"));
        assert!(!immediately_mergeable("BEHIND"));
    }

    #[test]
    fn conflict_and_settings_errors() {
        assert!(is_conflict(&anyhow!(
            "GitHub API 405 on PUT x: Pull Request is not mergeable"
        )));
        assert!(!is_conflict(&anyhow!("GitHub API 403 on PUT x: forbidden")));
        let repo = Repo::parse("o/r").unwrap();
        assert_eq!(
            explain(
                &anyhow!(
                    "GraphQL error: Pull request Auto merge is not allowed for this repository"
                ),
                &repo
            ),
            "auto-merge is disabled in o/r settings"
        );
    }
}
