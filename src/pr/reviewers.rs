use anyhow::Result;
use serde::Serialize;
use serde_json::json;
use std::collections::HashSet;

use crate::api::pulls::{self, Contributor, fetch_pr, is_bot};
use crate::cli::ReviewersArgs;
use crate::context::Ctx;
use crate::output;

#[derive(Debug, PartialEq, Serialize)]
struct Candidate {
    login: String,
    contributions: u64,
    /// "requested", "reviewed", or empty.
    status: String,
}

pub fn run(ctx: &Ctx, args: ReviewersArgs) -> Result<()> {
    let pr = ctx.pr(args.pr.as_deref())?;
    let client = &ctx.client;

    if !args.add.is_empty() {
        // gh's --add-reviewer fails with empty-string errors in some repos, the endpoint does not.
        let requested = pulls::request_reviewers(client, &pr, &args.add)?;
        if ctx.json {
            return output::print_json(&json!({ "pr": pr, "requested": requested }));
        }
        for login in &args.add {
            println!("requested review from {login} on {pr}");
        }
        return Ok(());
    }

    let info = fetch_pr(client, &pr)?;
    let viewer = client.viewer()?.to_string();
    // A few extra so filtering out bots and the author still leaves `limit`.
    let contributors = pulls::contributors(client, &pr.repo, (args.limit + 10).min(100))?;
    let requested: HashSet<String> = pulls::requested_reviewers(client, &pr)?
        .into_iter()
        .collect();
    let reviewed: HashSet<String> = pulls::reviews(client, &pr)?
        .into_iter()
        .filter_map(|r| r.user.and_then(|u| u.login))
        .collect();
    let excluded = [viewer.as_str(), info.author.as_str()];
    let candidates = rank(&contributors, &excluded, &requested, &reviewed, args.limit);

    if ctx.json {
        return output::print_json(&candidates);
    }
    if candidates.is_empty() {
        println!("No contributors to suggest for {pr}");
        return Ok(());
    }
    let width = candidates.iter().map(|c| c.login.len()).max().unwrap_or(0);
    for c in &candidates {
        let line = format!("{:<width$}  {:>6}  {}", c.login, c.contributions, c.status);
        println!("{}", line.trim_end());
    }
    Ok(())
}

/// Top human contributors in API order, minus `excluded`, tagged with their review status.
fn rank(
    contributors: &[Contributor],
    excluded: &[&str],
    requested: &HashSet<String>,
    reviewed: &HashSet<String>,
    limit: usize,
) -> Vec<Candidate> {
    contributors
        .iter()
        .filter_map(|c| {
            let login = c.login.as_deref()?;
            let typename = (c.kind.as_deref() == Some("Bot")).then_some("Bot");
            if is_bot(typename, login) || excluded.contains(&login) {
                return None;
            }
            let status = if requested.contains(login) {
                "requested"
            } else if reviewed.contains(login) {
                "reviewed"
            } else {
                ""
            };
            Some(Candidate {
                login: login.to_string(),
                contributions: c.contributions,
                status: status.to_string(),
            })
        })
        .take(limit)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::Client;
    use mockito::{Matcher, Server};

    fn ctx(server: &Server, json: bool) -> Ctx {
        Ctx {
            client: Client::for_tests(&server.url()),
            repo_flag: Some("o/r".into()),
            json,
            yes: true,
        }
    }

    fn args(add: &[&str], limit: usize) -> ReviewersArgs {
        ReviewersArgs {
            pr: Some("7".into()),
            add: add.iter().map(|s| s.to_string()).collect(),
            limit,
        }
    }

    #[test]
    fn add_requests_reviews_without_ranking() {
        let mut server = Server::new();
        let post = server
            .mock("POST", "/repos/o/r/pulls/7/requested_reviewers")
            .match_body(Matcher::Json(json!({"reviewers": ["bob", "carol"]})))
            .with_body(r#"{"requested_reviewers": [{"login": "bob"}, {"login": "carol"}]}"#)
            .expect(2)
            .create();
        let contributors = server
            .mock("GET", "/repos/o/r/contributors")
            .match_query(Matcher::Any)
            .expect(0)
            .create();
        run(&ctx(&server, true), args(&["bob", "carol"], 10)).unwrap();
        run(&ctx(&server, false), args(&["bob", "carol"], 10)).unwrap();
        post.assert();
        contributors.assert();
    }

    fn mock_suggest_inputs(server: &mut Server, contributors: &str) {
        server
            .mock("POST", "/graphql")
            .match_body(Matcher::Regex("isCrossRepository".into()))
            .with_body(
                r#"{"data": {"repository": {"pullRequest": {"number": 7,
                    "createdAt": "2026-01-01T00:00:00Z", "author": {"login": "author"}}}}}"#,
            )
            .create();
        server
            .mock("POST", "/graphql")
            .match_body(Matcher::Regex(r"query \{ viewer".into()))
            .with_body(r#"{"data": {"viewer": {"login": "me"}}}"#)
            .create();
        server
            .mock("GET", "/repos/o/r/contributors")
            // Ten extra, so dropping bots and the author still leaves the limit.
            .match_query(Matcher::UrlEncoded("per_page".into(), "12".into()))
            .with_body(contributors)
            .create();
        server
            .mock("GET", "/repos/o/r/pulls/7/requested_reviewers")
            .with_body(r#"{"users": [{"login": "bob"}]}"#)
            .create();
        server
            .mock("GET", "/repos/o/r/pulls/7/reviews")
            .match_query(Matcher::Any)
            .with_body(r#"[{"id": 1, "user": {"login": "alice"}}, {"id": 2, "user": null}]"#)
            .create();
    }

    #[test]
    fn suggest_ranks_contributors() {
        let mut server = Server::new();
        mock_suggest_inputs(
            &mut server,
            r#"[{"login": "me", "type": "User", "contributions": 9},
                {"login": "renovate[bot]", "type": "Bot", "contributions": 8},
                {"login": "alice", "type": "User", "contributions": 7},
                {"login": "bob", "type": "User", "contributions": 6},
                {"login": "carol", "type": "User", "contributions": 5}]"#,
        );
        run(&ctx(&server, true), args(&[], 2)).unwrap();
        run(&ctx(&server, false), args(&[], 2)).unwrap();
    }

    #[test]
    fn suggest_on_an_empty_repository() {
        let mut server = Server::new();
        mock_suggest_inputs(&mut server, "");
        run(&ctx(&server, false), args(&[], 2)).unwrap();
    }

    #[test]
    fn rank_skips_bots_author_and_viewer() {
        let contributors: Vec<Contributor> = [
            json!({"login": "me", "type": "User", "contributions": 90}),
            json!({"login": "dependabot[bot]", "type": "Bot", "contributions": 80}),
            json!({"login": "alice", "type": "User", "contributions": 70}),
            json!({"login": "author", "type": "User", "contributions": 60}),
            json!({"login": "bob", "type": "User", "contributions": 50}),
            json!({"login": "carol", "type": "User", "contributions": 40}),
        ]
        .into_iter()
        .map(|v| serde_json::from_value(v).unwrap())
        .collect();
        let requested = HashSet::from(["bob".to_string()]);
        let reviewed = HashSet::from(["alice".to_string(), "bob".to_string()]);
        let ranked = rank(&contributors, &["me", "author"], &requested, &reviewed, 2);
        let summary: Vec<(&str, &str)> = ranked
            .iter()
            .map(|c| (c.login.as_str(), c.status.as_str()))
            .collect();
        assert_eq!(summary, vec![("alice", "reviewed"), ("bob", "requested")]);
    }
}
