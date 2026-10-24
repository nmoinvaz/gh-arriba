use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::HashSet;

use crate::cli::LabelsArgs;
use crate::context::{Ctx, PrRef, Repo};
use crate::github::Client;
use crate::output;

// -------------------------------------------------------------------------
//  list
// -------------------------------------------------------------------------

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

    if !args.add.is_empty() || !args.remove.is_empty() {
        let pr = pr.context("No pull request found. Pass a PR number or URL.")?;
        return edit(ctx, &pr, &labels, &args.add, &args.remove);
    }

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

// -------------------------------------------------------------------------
//  add and remove
// -------------------------------------------------------------------------

/// Resolve every requested name first, so a typo adds or removes nothing.
fn edit(ctx: &Ctx, pr: &PrRef, labels: &[Label], add: &[String], remove: &[String]) -> Result<()> {
    let client = &ctx.client;
    let mut errors = Vec::new();
    let mut resolve_all = |names: &[String]| -> Vec<&Label> {
        names
            .iter()
            .filter_map(|n| match resolve(n, labels) {
                Ok(l) => {
                    if l.name != *n {
                        eprintln!("using label {:?} for {n:?}", l.name);
                    }
                    Some(l)
                }
                Err(e) => {
                    errors.push(e);
                    None
                }
            })
            .collect()
    };
    let to_add = resolve_all(add);
    let to_remove = resolve_all(remove);
    if !errors.is_empty() {
        bail!("{}", errors.join("\n"));
    }

    let mut added = Vec::new();
    let new: Vec<&str> = to_add
        .iter()
        .filter(|l| !l.applied)
        .map(|l| l.name.as_str())
        .collect();
    if !new.is_empty() {
        client.post(
            &format!("{}/labels", pr.issue_api()),
            &json!({ "labels": new }),
        )?;
        added.extend(new.iter().map(|n| n.to_string()));
    }

    let mut removed = Vec::new();
    for l in to_remove.iter().filter(|l| l.applied) {
        client.delete(&format!(
            "{}/labels/{}",
            pr.issue_api(),
            encode_segment(&l.name)
        ))?;
        removed.push(l.name.clone());
    }

    let mut now: Vec<String> = pr_labels(client, pr)?.into_iter().collect();
    now.sort_by_key(|n| n.to_lowercase());

    if ctx.json {
        return output::print_json(&json!({
            "pr": pr,
            "added": added,
            "removed": removed,
            "labels": now,
        }));
    }
    for n in &added {
        println!("added {n}");
    }
    for n in &removed {
        println!("removed {n}");
    }
    let list = if now.is_empty() {
        "none".to_string()
    } else {
        now.join(", ")
    };
    println!("labels on {pr}: {list}");
    Ok(())
}

/// Find the label `name` means: an exact match ignoring case, then a unique match ignoring
/// punctuation, then a unique label that has `name` as one of its words, as `ui` in `ui/ux`.
fn resolve<'a>(name: &str, labels: &'a [Label]) -> std::result::Result<&'a Label, String> {
    if let Some(l) = labels.iter().find(|l| l.name.eq_ignore_ascii_case(name)) {
        return Ok(l);
    }
    let wanted = normalize(name);
    if wanted.is_empty() {
        return Err(format!("{name:?} is not a label name"));
    }
    let loose: Vec<&Label> = labels
        .iter()
        .filter(|l| normalize(&l.name) == wanted)
        .collect();
    let segment: Vec<&Label> = labels
        .iter()
        .filter(|l| segments(&l.name).contains(&wanted))
        .collect();
    for found in [loose, segment] {
        match found.as_slice() {
            [l] => return Ok(l),
            [] => {}
            many => {
                let names: Vec<&str> = many.iter().map(|l| l.name.as_str()).collect();
                return Err(format!(
                    "{name:?} matches several labels: {}",
                    names.join(", ")
                ));
            }
        }
    }
    let close = closest(&wanted, labels);
    if close.is_empty() {
        return Err(format!("No label {name:?}"));
    }
    Err(format!("No label {name:?}. Closest: {}", close.join(", ")))
}

/// Lowercase alphanumerics only, so `UI / UX`, `ui-ux`, and `ui_ux` compare equal.
fn normalize(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// Words of a label name, split on anything that isn't alphanumeric.
fn segments(s: &str) -> Vec<String> {
    s.split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// Up to five labels nearest to a normalized name, by substring then by edit distance.
fn closest(wanted: &str, labels: &[Label]) -> Vec<String> {
    let mut scored: Vec<(usize, &str)> = labels
        .iter()
        .map(|l| {
            let n = normalize(&l.name);
            let score = if n.contains(wanted) || wanted.contains(n.as_str()) {
                0
            } else {
                distance(wanted, &n)
            };
            (score, l.name.as_str())
        })
        .filter(|(score, _)| *score <= wanted.len().max(3) / 2)
        .collect();
    scored.sort();
    scored
        .into_iter()
        .take(5)
        .map(|(_, n)| n.to_string())
        .collect()
}

/// Levenshtein distance between two strings.
fn distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut prev = row[0];
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cur = row[j + 1];
            row[j + 1] = if ca == *cb {
                prev
            } else {
                1 + prev.min(cur).min(row[j])
            };
            prev = cur;
        }
    }
    row[b.len()]
}

/// Percent-encode a label name for use as one URL path segment.
fn encode_segment(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

// -------------------------------------------------------------------------
//  render
// -------------------------------------------------------------------------

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

    fn label(name: &str) -> Label {
        Label {
            name: name.to_string(),
            description: String::new(),
            applied: false,
        }
    }

    fn repo_labels() -> Vec<Label> {
        [
            "ui/ux",
            "build system",
            "Bug",
            "cleanup",
            "continuous integration",
        ]
        .into_iter()
        .map(label)
        .collect()
    }

    #[test]
    fn resolve_exact_loose_and_segment() {
        let labels = repo_labels();
        let name = |n: &str| resolve(n, &labels).map(|l| l.name.as_str());
        assert_eq!(name("bug"), Ok("Bug"));
        assert_eq!(name("build-system"), Ok("build system"));
        assert_eq!(name("UI_UX"), Ok("ui/ux"));
        assert_eq!(name("ui"), Ok("ui/ux"));
        assert_eq!(name("integration"), Ok("continuous integration"));
    }

    #[test]
    fn resolve_rejects_ambiguous_and_unknown() {
        let mut labels = repo_labels();
        labels.push(label("ui/a11y"));
        let err = resolve("ui", &labels).unwrap_err();
        assert!(err.contains("several"), "{err}");
        assert!(err.contains("ui/ux") && err.contains("ui/a11y"), "{err}");

        let err = resolve("cleanups", &labels).unwrap_err();
        assert_eq!(err, "No label \"cleanups\". Closest: cleanup");
        assert_eq!(
            resolve("documentation", &labels).unwrap_err(),
            "No label \"documentation\""
        );
    }

    #[test]
    fn edit_distance() {
        assert_eq!(distance("kitten", "sitting"), 3);
        assert_eq!(distance("", "abc"), 3);
        assert_eq!(distance("same", "same"), 0);
    }

    #[test]
    fn segment_encoding() {
        assert_eq!(encode_segment("ui/ux"), "ui%2Fux");
        assert_eq!(encode_segment("build system"), "build%20system");
        assert_eq!(encode_segment("type: bug"), "type%3A%20bug");
        assert_eq!(encode_segment("naïve"), "na%C3%AFve");
    }

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
