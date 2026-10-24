use anyhow::{Result, bail};
use serde_json::json;

use super::{DepPr, REBASE_COMMENT, Scope, fetch_prs};
use crate::api::{PrRef, pulls};
use crate::cli::DependabotRebaseArgs;
use crate::context::Ctx;
use crate::output::print_json;
use crate::prompt;

pub fn run(ctx: &Ctx, args: DependabotRebaseArgs) -> Result<()> {
    let repo = ctx.repo()?;
    let prs = fetch_prs(&ctx.client, &Scope::Repo(repo.clone()), false)?;

    let targets: Vec<&DepPr> = if args.all {
        prs.iter().collect()
    } else if !args.prs.is_empty() {
        select_numbers(&prs, &args.prs)?
    } else if prompt::interactive() {
        if prs.is_empty() {
            println!("No open Dependabot PRs in {repo}.");
            return Ok(());
        }
        let items: Vec<String> = prs
            .iter()
            .map(|p| format!("#{} {}", p.number, p.title))
            .collect();
        prompt::multi_select("Which Dependabot PRs should rebase?", &items)?
            .into_iter()
            .map(|i| &prs[i])
            .collect()
    } else {
        bail!("Pass PR numbers or --all.");
    };

    if targets.is_empty() {
        println!("No Dependabot PRs to rebase in {repo}.");
        return Ok(());
    }
    if args.all {
        let question = format!(
            "Ask Dependabot to rebase {} PR(s) in {repo}?",
            targets.len()
        );
        if !prompt::confirm(&question, ctx.yes)? {
            println!("Cancelled.");
            return Ok(());
        }
    }

    let mut requested = Vec::new();
    for pr in &targets {
        let pr_ref = PrRef {
            repo: repo.clone(),
            number: pr.number,
        };
        pulls::add_issue_comment(&ctx.client, &pr_ref, REBASE_COMMENT)?;
        if !ctx.json {
            println!("  requested rebase on #{} {}", pr.number, pr.title);
        }
        requested.push(pr.number);
    }

    if ctx.json {
        return print_json(&json!({ "repo": repo.to_string(), "requested": requested }));
    }
    println!(
        "Asked Dependabot to rebase {} PR(s). It works through them asynchronously.",
        requested.len()
    );
    Ok(())
}

/// Look up PR numbers among Dependabot's open PRs, failing on any that aren't.
fn select_numbers<'a>(prs: &'a [DepPr], numbers: &[u64]) -> Result<Vec<&'a DepPr>> {
    numbers
        .iter()
        .map(|n| {
            prs.iter()
                .find(|p| p.number == *n)
                .ok_or_else(|| anyhow::anyhow!("#{n} is not an open Dependabot PR"))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::super::Category;
    use super::super::tests::{ctx, mock_prs, pr, ready};
    use super::*;
    use mockito::{Matcher, Mock, Server, ServerGuard};

    fn args(prs: &[u64], all: bool) -> DependabotRebaseArgs {
        DependabotRebaseArgs {
            prs: prs.to_vec(),
            all,
        }
    }

    fn rebase_comment(server: &mut ServerGuard, n: u64, hits: usize) -> Mock {
        server
            .mock("POST", format!("/repos/o/r/issues/{n}/comments").as_str())
            .match_body(Matcher::Json(json!({ "body": "@dependabot rebase" })))
            .with_status(201)
            .with_body("{}")
            .expect(hits)
            .create()
    }

    #[test]
    fn all_comments_on_every_pr() {
        for json in [false, true] {
            let mut server = Server::new();
            mock_prs(
                &mut server,
                false,
                &[
                    ready(1, "Bump a from 1.0.0 to 1.0.1"),
                    ready(2, "Bump b from 1.0.0 to 1.0.1"),
                ],
            );
            let one = rebase_comment(&mut server, 1, 1);
            let two = rebase_comment(&mut server, 2, 1);
            run(&ctx(&server, json), args(&[], true)).unwrap();
            one.assert();
            two.assert();
        }
    }

    #[test]
    fn numbers_comment_on_those_alone() {
        let mut server = Server::new();
        mock_prs(
            &mut server,
            false,
            &[
                ready(1, "Bump a from 1.0.0 to 1.0.1"),
                ready(2, "Bump b from 1.0.0 to 1.0.1"),
            ],
        );
        let one = rebase_comment(&mut server, 1, 0);
        let two = rebase_comment(&mut server, 2, 1);
        run(&ctx(&server, false), args(&[2], false)).unwrap();
        one.assert();
        two.assert();

        let err = run(&ctx(&server, false), args(&[9], false)).unwrap_err();
        assert_eq!(err.to_string(), "#9 is not an open Dependabot PR");
    }

    #[test]
    fn all_with_nothing_open_does_nothing() {
        let mut server = Server::new();
        mock_prs(&mut server, false, &[]);
        run(&ctx(&server, true), args(&[], true)).unwrap();
    }

    #[test]
    fn comment_failure_stops_the_run() {
        let mut server = Server::new();
        mock_prs(
            &mut server,
            false,
            &[ready(1, "Bump a from 1.0.0 to 1.0.1")],
        );
        server
            .mock("POST", "/repos/o/r/issues/1/comments")
            .with_status(403)
            .with_body(r#"{"message": "Resource not accessible"}"#)
            .create();
        let err = run(&ctx(&server, true), args(&[], true)).unwrap_err();
        assert_eq!(
            err.to_string(),
            "GitHub API 403 on POST repos/o/r/issues/1/comments: Resource not accessible"
        );
    }

    #[test]
    fn numbers_must_be_dependabot_prs() {
        let prs = vec![pr("o/r", 3, "Bump a from 1 to 2", Category::Ready)];
        assert_eq!(select_numbers(&prs, &[3]).unwrap()[0].number, 3);
        let err = select_numbers(&prs, &[3, 9]).unwrap_err();
        assert_eq!(err.to_string(), "#9 is not an open Dependabot PR");
    }
}
