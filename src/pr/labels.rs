use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde_json::json;
use std::collections::HashSet;

use crate::api::pulls::{self, RepoLabel, fetch_pr};
use crate::api::{Client, PrRef, Repo};
use crate::cli::LabelsArgs;
use crate::context::Ctx;
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

    let all = pulls::repo_labels(client, &repo)?;
    let applied = match &pr {
        Some(pr) => pr_labels(client, pr)?,
        None => HashSet::new(),
    };
    let labels = parse_labels(&all, &applied);

    if !args.add.is_empty() || !args.remove.is_empty() {
        let pr = pr.context("No pull request found. Pass a PR number or URL.")?;
        return edit(ctx, &pr, &labels, &args.add, &args.remove);
    }
    if args.suggest {
        let pr = pr.context("No pull request found. Pass a PR number or URL.")?;
        return suggest(ctx, &pr, &labels);
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
    Ok(pulls::pr_labels(client, pr)?
        .into_iter()
        .filter_map(|l| l.name)
        .collect())
}

/// Repo labels in name order, marked where `applied` holds them.
fn parse_labels(all: &[RepoLabel], applied: &HashSet<String>) -> Vec<Label> {
    let mut labels: Vec<Label> = all
        .iter()
        .filter_map(|l| {
            let name = l.name.as_deref()?;
            Some(Label {
                name: name.to_string(),
                description: l.description.clone(),
                applied: applied.contains(name),
            })
        })
        .collect();
    labels.sort_by_key(|l| l.name.to_lowercase());
    labels
}

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
        pulls::add_labels(client, pr, &new)?;
        added.extend(new.iter().map(|n| n.to_string()));
    }

    let mut removed = Vec::new();
    for l in to_remove.iter().filter(|l| l.applied) {
        pulls::remove_label(client, pr, &l.name)?;
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

#[derive(Debug, PartialEq, Serialize)]
struct Suggestion {
    name: String,
    score: usize,
    /// Label words also found in the PR.
    matched: Vec<String>,
}

/// Labels suggested at most.
const SUGGESTIONS: usize = 5;

fn suggest(ctx: &Ctx, pr: &PrRef, labels: &[Label]) -> Result<()> {
    let client = &ctx.client;
    let info = fetch_pr(client, pr)?;
    let files = pulls::pr_files(client, pr)?;
    let mut text = vec![info.title, info.body, info.head_ref];
    text.extend(files.into_iter().map(|f| f.filename));
    let words: HashSet<String> = text.iter().flat_map(|t| words(t)).collect();
    let ranked = rank(labels, &words);

    if ctx.json {
        return output::print_json(&ranked);
    }
    if ranked.is_empty() {
        println!("No labels share words with {pr}");
        return Ok(());
    }
    let width = ranked.iter().map(|s| s.name.len()).max().unwrap_or(0);
    for s in &ranked {
        println!(
            "{:<width$}  {:>2}  {}",
            s.name,
            s.score,
            s.matched.join(", ")
        );
    }
    Ok(())
}

/// Labels not yet applied, scored by the PR words they share: two points for a word of the name,
/// one for a word only in the description. Ties go by name.
fn rank(labels: &[Label], pr_words: &HashSet<String>) -> Vec<Suggestion> {
    let mut ranked: Vec<Suggestion> = labels
        .iter()
        .filter(|l| !l.applied)
        .filter_map(|l| {
            let name: HashSet<String> = words(&l.name).into_iter().collect();
            let mut all: Vec<String> = name.iter().cloned().chain(words(&l.description)).collect();
            all.sort();
            all.dedup();
            let matched: Vec<String> = all
                .into_iter()
                .filter(|w| pr_words.iter().any(|p| same_word(w, p)))
                .collect();
            if matched.is_empty() {
                return None;
            }
            let score = matched
                .iter()
                .map(|w| if name.contains(w) { 2 } else { 1 })
                .sum();
            Some(Suggestion {
                name: l.name.clone(),
                score,
                matched,
            })
        })
        .collect();
    ranked.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    ranked.truncate(SUGGESTIONS);
    ranked
}

/// Equal, or one a prefix of the other once both have four letters, as `test` and `tests`.
fn same_word(a: &str, b: &str) -> bool {
    a == b || (a.len().min(b.len()) >= 4 && (a.starts_with(b) || b.starts_with(a)))
}

/// Lowercase words of two or more letters, minus words too common to say anything.
fn words(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .map(str::to_lowercase)
        .filter(|w| w.len() >= 2 && !w.chars().all(|c| c.is_ascii_digit()))
        .filter(|w| !STOP_WORDS.contains(&w.as_str()))
        .collect()
}

const STOP_WORDS: &[&str] = &[
    "a", "an", "and", "are", "as", "at", "be", "by", "for", "from", "has", "in", "into", "is",
    "it", "its", "of", "on", "or", "that", "the", "this", "to", "was", "with", "add", "adds",
    "change", "changes", "code", "file", "files", "fix", "fixes", "main", "new", "pr", "pull",
    "request", "src", "update", "updates", "use", "uses",
];

/// One label per line, applied ones marked with `*`.
fn render(labels: &[Label]) -> String {
    let rows: Vec<Vec<String>> = labels
        .iter()
        .map(|l| {
            let mark = if l.applied { "*" } else { " " };
            vec![format!("{mark} {}", l.name), l.description.clone()]
        })
        .collect();
    let mut out = output::table(&rows, "");
    if !out.is_empty() {
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use mockito::{Matcher, Mock, Server};

    fn ctx(server: &Server, json: bool) -> Ctx {
        Ctx {
            client: Client::for_tests(&server.url()),
            repo_flag: Some("o/r".into()),
            json,
            yes: true,
        }
    }

    fn args(pr: Option<&str>, add: &[&str], remove: &[&str], suggest: bool) -> LabelsArgs {
        let owned = |v: &[&str]| v.iter().map(|s| s.to_string()).collect();
        LabelsArgs {
            pr: pr.map(str::to_string),
            add: owned(add),
            remove: owned(remove),
            suggest,
        }
    }

    fn mock_repo_labels(server: &mut Server) -> Mock {
        server
            .mock("GET", "/repos/o/r/labels")
            .match_query(Matcher::Any)
            .with_body(
                json!([
                    {"name": "Bug", "description": "Something broken"},
                    {"name": "ui/ux", "description": null},
                    {"name": "testing", "description": "Tests and the test suite"},
                    {"name": "cleanup", "description": ""}
                ])
                .to_string(),
            )
            .create()
    }

    fn mock_pr_labels(server: &mut Server) -> Mock {
        server
            .mock("GET", "/repos/o/r/issues/7/labels")
            .match_query(Matcher::Any)
            .with_body(r#"[{"name": "cleanup"}]"#)
            .create()
    }

    #[test]
    fn list_without_a_pr_marks_nothing_applied() {
        let mut server = Server::new();
        let labels = mock_repo_labels(&mut server).expect(2);
        let applied = mock_pr_labels(&mut server).expect(0);
        // With -R and no PR the repo's labels are still listed.
        run(&ctx(&server, true), args(None, &[], &[], false)).unwrap();
        run(&ctx(&server, false), args(None, &[], &[], false)).unwrap();
        labels.assert();
        applied.assert();
        let err = run(&ctx(&server, true), args(None, &["bug"], &[], false))
            .unwrap_err()
            .to_string();
        assert_eq!(err, "No pull request found. Pass a PR number or URL.");
    }

    #[test]
    fn list_for_a_pr_reads_its_labels() {
        let mut server = Server::new();
        mock_repo_labels(&mut server);
        let applied = mock_pr_labels(&mut server).expect(1);
        run(&ctx(&server, false), args(Some("o/r#7"), &[], &[], false)).unwrap();
        applied.assert();
    }

    #[test]
    fn list_of_a_repo_without_labels() {
        let mut server = Server::new();
        server
            .mock("GET", "/repos/o/r/labels")
            .match_query(Matcher::Any)
            .with_body("[]")
            .create();
        run(&ctx(&server, false), args(None, &[], &[], false)).unwrap();
    }

    #[test]
    fn edit_adds_new_and_removes_applied_labels_only() {
        let mut server = Server::new();
        mock_repo_labels(&mut server);
        // Read once for the listing and once more for the result.
        let applied = mock_pr_labels(&mut server).expect(2);
        let add = server
            .mock("POST", "/repos/o/r/issues/7/labels")
            .match_body(Matcher::Json(json!({"labels": ["Bug", "ui/ux"]})))
            .with_body("[]")
            .create();
        let remove = server
            .mock("DELETE", "/repos/o/r/issues/7/labels/cleanup")
            .with_body("[]")
            .create();
        let not_applied = server
            .mock("DELETE", "/repos/o/r/issues/7/labels/testing")
            .expect(0)
            .create();
        // "cleanup" is already applied, so adding it again sends nothing for it.
        let a = args(
            Some("7"),
            &["bug", "ui", "cleanup"],
            &["cleanup", "testing"],
            false,
        );
        run(&ctx(&server, true), a).unwrap();
        add.assert();
        remove.assert();
        not_applied.assert();
        applied.assert();
    }

    #[test]
    fn edit_with_a_typo_changes_nothing() {
        let mut server = Server::new();
        mock_repo_labels(&mut server);
        mock_pr_labels(&mut server);
        let add = server
            .mock("POST", "/repos/o/r/issues/7/labels")
            .expect(0)
            .create();
        let a = args(Some("7"), &["bug", "clenaup"], &[], false);
        let err = run(&ctx(&server, false), a).unwrap_err().to_string();
        assert_eq!(err, "No label \"clenaup\". Closest: cleanup");
        add.assert();
    }

    #[test]
    fn edit_in_human_output() {
        let mut server = Server::new();
        mock_repo_labels(&mut server);
        server
            .mock("GET", "/repos/o/r/issues/7/labels")
            .match_query(Matcher::Any)
            .with_body("[]")
            .create();
        server
            .mock("POST", "/repos/o/r/issues/7/labels")
            .with_body("[]")
            .create();
        run(
            &ctx(&server, false),
            args(Some("7"), &["testing"], &[], false),
        )
        .unwrap();
    }

    #[test]
    fn suggest_uses_title_body_branch_and_files() {
        let mut server = Server::new();
        mock_repo_labels(&mut server);
        mock_pr_labels(&mut server);
        let pr = server
            .mock("POST", "/graphql")
            .match_body(Matcher::Regex("isCrossRepository".into()))
            .with_body(
                r#"{"data": {"repository": {"pullRequest": {
                    "number": 7, "title": "Run tests on arm64", "body": "Broken ui layout.",
                    "headRefName": "ci/cleanup", "createdAt": "2026-01-01T00:00:00Z"}}}}"#,
            )
            .expect(2)
            .create();
        let files = server
            .mock("GET", "/repos/o/r/pulls/7/files")
            .match_query(Matcher::Any)
            .with_body(r#"[{"filename": "test/test_inflate.c"}]"#)
            .expect(2)
            .create();
        run(&ctx(&server, true), args(Some("7"), &[], &[], true)).unwrap();
        run(&ctx(&server, false), args(Some("7"), &[], &[], true)).unwrap();
        pr.assert();
        files.assert();
    }
    use serde_json::Value;

    fn parse_repo_labels(values: Vec<Value>) -> Vec<RepoLabel> {
        values
            .into_iter()
            .map(|v| serde_json::from_value(v).unwrap())
            .collect()
    }

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
    fn rank_prefers_name_words_and_skips_applied() {
        let mut labels = vec![
            Label {
                name: "documentation".to_string(),
                description: "Improvements or additions to docs".to_string(),
                applied: false,
            },
            Label {
                name: "continuous integration".to_string(),
                description: "GitHub workflows".to_string(),
                applied: false,
            },
            Label {
                name: "testing".to_string(),
                description: "Tests and the test suite".to_string(),
                applied: false,
            },
            label("cleanup"),
        ];
        labels[3].applied = true;
        let text = [
            "ci: run tests on arm64 runners",
            "Adds a cleanup job.",
            "ci/arm64-tests",
            ".github/workflows/build.yml",
            "test/test_inflate.c",
        ];
        let pr_words: HashSet<String> = text.iter().flat_map(|t| words(t)).collect();
        let ranked: Vec<(String, usize, Vec<String>)> = rank(&labels, &pr_words)
            .into_iter()
            .map(|s| (s.name, s.score, s.matched))
            .collect();
        assert_eq!(
            ranked,
            vec![
                (
                    "testing".to_string(),
                    4,
                    vec![
                        "test".to_string(),
                        "testing".to_string(),
                        "tests".to_string()
                    ]
                ),
                (
                    "continuous integration".to_string(),
                    2,
                    vec!["github".to_string(), "workflows".to_string()]
                ),
            ]
        );
    }

    #[test]
    fn words_drop_noise() {
        assert_eq!(
            words("Fix the src/pr/labels.rs file for PR 42"),
            vec!["labels", "rs"]
        );
        assert!(same_word("test", "tests"));
        assert!(!same_word("ci", "cint"));
    }

    #[test]
    fn edit_distance() {
        assert_eq!(distance("kitten", "sitting"), 3);
        assert_eq!(distance("", "abc"), 3);
        assert_eq!(distance("same", "same"), 0);
    }

    #[test]
    fn labels_sorted_and_marked() {
        let all = vec![
            json!({"name": "ui/ux", "description": "User interface"}),
            json!({"name": "Bug", "description": null}),
            json!({"name": "cleanup", "description": "Code cleanup"}),
        ];
        let applied = HashSet::from(["cleanup".to_string()]);
        let labels = parse_labels(&parse_repo_labels(all), &applied);
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
