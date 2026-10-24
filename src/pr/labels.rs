use anyhow::Result;
use serde::Serialize;
use serde_json::Value;
use std::collections::HashSet;

use crate::cli::LabelsArgs;
use crate::context::{Ctx, PrRef, Repo};
use crate::github::Client;
use crate::output;

#[derive(Clone, Debug, PartialEq, Serialize)]
struct Label {
    name: String,
    description: String,
    /// Whether the PR carries this label. Always false without a PR.
    applied: bool,
}

pub fn run(ctx: &Ctx, args: LabelsArgs) -> Result<()> {
    let (repo, pr) = target(ctx, args.pr.as_deref())?;
    let client = &ctx.client;

    let all = client.get_all(&format!("{}/labels", repo.api()))?;
    let applied = match &pr {
        Some(pr) => pr_labels(client, pr)?,
        None => HashSet::new(),
    };
    let labels = parse_labels(&all, &applied);

    if ctx.json {
        return output::print_json(&labels);
    }
    if labels.is_empty() {
        println!("No labels in {repo}");
        return Ok(());
    }
    print!("{}", render(&labels));
    Ok(())
}

/// The repo, plus the PR when there is one. Without an argument, a branch with no PR yet still lists
/// the repo's labels, so they can be checked before `gh pr create`.
fn target(ctx: &Ctx, arg: Option<&str>) -> Result<(Repo, Option<PrRef>)> {
    if arg.is_some() {
        let pr = ctx.pr(arg)?;
        return Ok((pr.repo.clone(), Some(pr)));
    }
    match ctx.pr(None) {
        Ok(pr) => Ok((pr.repo.clone(), Some(pr))),
        Err(_) => Ok((ctx.repo()?, None)),
    }
}

/// Names of the labels on a PR.
fn pr_labels(client: &Client, pr: &PrRef) -> Result<HashSet<String>> {
    Ok(client
        .get_all(&format!("{}/labels", pr.issue_api()))?
        .iter()
        .filter_map(|l| l["name"].as_str().map(str::to_string))
        .collect())
}

/// Repo labels in name order, marked where `applied` holds them.
fn parse_labels(all: &[Value], applied: &HashSet<String>) -> Vec<Label> {
    let mut labels: Vec<Label> = all
        .iter()
        .filter_map(|l| {
            let name = l["name"].as_str()?;
            Some(Label {
                name: name.to_string(),
                description: l["description"].as_str().unwrap_or_default().to_string(),
                applied: applied.contains(name),
            })
        })
        .collect();
    labels.sort_by_key(|l| l.name.to_lowercase());
    labels
}

/// One label per line, applied ones marked with `*`.
fn render(labels: &[Label]) -> String {
    let width = labels.iter().map(|l| l.name.len()).max().unwrap_or(0);
    let mut out = String::new();
    for l in labels {
        let mark = if l.applied { "*" } else { " " };
        let line = format!("{mark} {:<width$}  {}", l.name, l.description);
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn labels_sorted_and_marked() {
        let all = vec![
            json!({"name": "ui/ux", "description": "User interface"}),
            json!({"name": "Bug", "description": null}),
            json!({"name": "cleanup", "description": "Code cleanup"}),
        ];
        let applied = HashSet::from(["cleanup".to_string()]);
        let labels = parse_labels(&all, &applied);
        let names: Vec<(&str, bool)> = labels
            .iter()
            .map(|l| (l.name.as_str(), l.applied))
            .collect();
        assert_eq!(
            names,
            vec![("Bug", false), ("cleanup", true), ("ui/ux", false)]
        );
        assert_eq!(
            render(&labels),
            "  Bug\n* cleanup  Code cleanup\n  ui/ux    User interface\n"
        );
    }
}
