use anyhow::Result;
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::HashSet;

use crate::cli::ReviewersArgs;
use crate::context::Ctx;
use crate::output;
use crate::pr::model::{fetch_pr, is_bot};

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
        let resp = client.post(
            &format!("{}/requested_reviewers", pr.api()),
            &json!({ "reviewers": args.add }),
        )?;
        let requested: Vec<String> = resp["requested_reviewers"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|u| u["login"].as_str().map(str::to_string))
            .collect();
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
    let contributors = client.get(&format!(
        "{}/contributors?per_page={}",
        pr.repo.api(),
        (args.limit + 10).min(100)
    ))?;
    let requested = client.get(&format!("{}/requested_reviewers", pr.api()))?;
    let reviews = client.get_all(&format!("{}/reviews", pr.api()))?;

    let requested: HashSet<String> = logins(&requested["users"]);
    let reviewed: HashSet<String> = reviews
        .iter()
        .filter_map(|r| r["user"]["login"].as_str().map(str::to_string))
        .collect();
    let excluded = [viewer.as_str(), info.author.as_str()];
    let candidates = rank(
        contributors
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default(),
        &excluded,
        &requested,
        &reviewed,
        args.limit,
    );

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

fn logins(users: &Value) -> HashSet<String> {
    users
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|u| u["login"].as_str().map(str::to_string))
        .collect()
}

/// Top human contributors in API order, minus `excluded`, tagged with their review status.
fn rank(
    contributors: &[Value],
    excluded: &[&str],
    requested: &HashSet<String>,
    reviewed: &HashSet<String>,
    limit: usize,
) -> Vec<Candidate> {
    contributors
        .iter()
        .filter_map(|c| {
            let login = c["login"].as_str()?;
            let typename = (c["type"].as_str() == Some("Bot")).then_some("Bot");
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
                contributions: c["contributions"].as_u64().unwrap_or(0),
                status: status.to_string(),
            })
        })
        .take(limit)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rank_skips_bots_author_and_viewer() {
        let contributors = vec![
            json!({"login": "me", "type": "User", "contributions": 90}),
            json!({"login": "dependabot[bot]", "type": "Bot", "contributions": 80}),
            json!({"login": "alice", "type": "User", "contributions": 70}),
            json!({"login": "author", "type": "User", "contributions": 60}),
            json!({"login": "bob", "type": "User", "contributions": 50}),
            json!({"login": "carol", "type": "User", "contributions": 40}),
        ];
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
