use anyhow::{Context, Result, bail};
use serde::Serialize;
use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::process::Command;

use crate::api::pulls::{Thread, fetch_threads};
use crate::api::{Client, PrRef, encode_path};
use crate::cli::ApplyArgs;
use crate::context::{Ctx, git};
use crate::output;
use crate::prompt;

/// One suggestion block, anchored to an inclusive line range on the PR head.
#[derive(Clone, Debug)]
struct Suggestion {
    thread_id: String,
    comment_id: u64,
    author: String,
    path: String,
    start: u64,
    end: u64,
    replacement: Vec<String>,
}

#[derive(Debug, Serialize)]
struct Applied {
    thread_id: String,
    comment_id: u64,
    path: String,
    start_line: u64,
    line: u64,
}

#[derive(Debug, Serialize)]
struct Skipped {
    thread_id: String,
    reason: String,
}

#[derive(Serialize)]
struct Report {
    dry_run: bool,
    applied: Vec<Applied>,
    skipped: Vec<Skipped>,
}

pub fn run(ctx: &Ctx, args: ApplyArgs) -> Result<()> {
    let pr = ctx.pr(args.pr.as_deref())?;
    let data = fetch_threads(&ctx.client, &pr)?;
    let (candidates, mut skipped) = select(&data.threads, &args.threads);

    if candidates.is_empty() {
        return finish(ctx, args.dry_run, Vec::new(), skipped);
    }

    // A dry run reads the PR head from the API, so it works from any checkout. A real run edits
    // the working tree, which must be exactly the PR head for the thread line numbers to hold.
    let root = if args.dry_run {
        None
    } else {
        let root = git(&["rev-parse", "--show-toplevel"]).context("Not in a git repository")?;
        let root = Path::new(&root).to_path_buf();
        check_worktree(&root, &data.head_oid, &candidates, pr.number)?;
        Some(root)
    };

    // Load each touched file once, in thread order.
    let mut contents: BTreeMap<String, String> = BTreeMap::new();
    for s in &candidates {
        if contents.contains_key(&s.path) {
            continue;
        }
        let text = match &root {
            Some(root) => std::fs::read_to_string(root.join(&s.path))
                .with_context(|| format!("Failed to read {}", s.path))?,
            None => read_at_head(&ctx.client, &pr, &data.head_oid, &s.path)?,
        };
        contents.insert(s.path.clone(), text);
    }

    // Keep suggestions that fit their file and don't overlap an earlier one.
    let mut accepted: Vec<Suggestion> = Vec::new();
    for s in candidates {
        let lines = split_lines(&contents[&s.path]);
        if let Err(reason) = fits(&s, lines.len(), &accepted) {
            skipped.push(Skipped {
                thread_id: s.thread_id.clone(),
                reason,
            });
            continue;
        }
        accepted.push(s);
    }

    // Show each change, and on a terminal ask before taking it.
    let ask = !args.dry_run && !ctx.yes && prompt::interactive();
    let mut chosen = Vec::new();
    for s in accepted {
        let lines = split_lines(&contents[&s.path]);
        if !ctx.json || ask {
            let preview = render(&s, &lines);
            if ctx.json {
                eprintln!("{preview}")
            } else {
                println!("{preview}")
            }
        }
        if ask {
            let question = format!("Apply the suggestion from {}?", s.author);
            if !prompt::confirm(&question, false)? {
                skipped.push(Skipped {
                    thread_id: s.thread_id.clone(),
                    reason: "declined".to_string(),
                });
                continue;
            }
        }
        chosen.push(s);
    }

    if let Some(root) = &root {
        let mut by_file: HashMap<&str, Vec<&Suggestion>> = HashMap::new();
        for s in &chosen {
            by_file.entry(s.path.as_str()).or_default().push(s);
        }
        for (path, edits) in by_file {
            let updated = apply_edits(&contents[path], &edits);
            std::fs::write(root.join(path), updated)
                .with_context(|| format!("Failed to write {path}"))?;
        }
    }

    let applied = chosen
        .into_iter()
        .map(|s| Applied {
            thread_id: s.thread_id,
            comment_id: s.comment_id,
            path: s.path,
            start_line: s.start,
            line: s.end,
        })
        .collect();
    finish(ctx, args.dry_run, applied, skipped)
}

fn finish(ctx: &Ctx, dry_run: bool, applied: Vec<Applied>, skipped: Vec<Skipped>) -> Result<()> {
    if ctx.json {
        return output::print_json(&Report {
            dry_run,
            applied,
            skipped,
        });
    }
    let verb = if dry_run { "would apply" } else { "applied" };
    for a in &applied {
        println!(
            "{verb} {} (comment {}) {}",
            a.thread_id,
            a.comment_id,
            span(&a.path, a.start_line, a.line)
        );
    }
    for s in &skipped {
        println!("skipped {}: {}", s.thread_id, s.reason);
    }
    if applied.is_empty() && skipped.is_empty() {
        println!("No unresolved suggestions to apply.");
    } else if dry_run {
        println!(
            "{} to apply, {} skipped. Nothing was written.",
            applied.len(),
            skipped.len()
        );
    } else {
        println!(
            "{} applied, {} skipped. Nothing was committed.",
            applied.len(),
            skipped.len()
        );
    }
    Ok(())
}

/// Turn threads into suggestions. Without filters only unresolved, current threads with a
/// suggestion count, and others are passed over silently. Named threads always get a reason.
fn select(threads: &[Thread], filters: &[String]) -> (Vec<Suggestion>, Vec<Skipped>) {
    let mut out = Vec::new();
    let mut skipped = Vec::new();
    let named = !filters.is_empty();

    for t in threads {
        if named && !filters.iter().any(|f| t.matches(f)) {
            continue;
        }
        let mut skip = |reason: String| {
            if named {
                skipped.push(Skipped {
                    thread_id: t.id.clone(),
                    reason,
                });
            }
        };
        let Some(starter) = t.starter() else {
            skip("thread has no comments".to_string());
            continue;
        };
        let blocks = parse_suggestions(&starter.body);
        if blocks.is_empty() {
            skip("no suggestion block".to_string());
            continue;
        }
        if t.resolved {
            skip("thread is resolved".to_string());
            continue;
        }
        if t.outdated {
            skip("thread is outdated".to_string());
            continue;
        }
        // These problems are worth reporting even when the thread was not named.
        let reason = if blocks.len() > 1 {
            Some(format!(
                "{} suggestion blocks, apply it by hand",
                blocks.len()
            ))
        } else if t.side != "RIGHT" {
            Some("suggestion is on the LEFT side of the diff".to_string())
        } else if t.line.is_none() {
            Some("thread has no line on the PR head".to_string())
        } else {
            None
        };
        if let Some(reason) = reason {
            skipped.push(Skipped {
                thread_id: t.id.clone(),
                reason,
            });
            continue;
        }
        let end = t.line.unwrap_or_default();
        let start = t.start_line.unwrap_or(end).min(end);
        out.push(Suggestion {
            thread_id: t.id.clone(),
            comment_id: starter.id,
            author: starter.author.clone(),
            path: t.path.clone(),
            start,
            end,
            replacement: blocks.into_iter().next().unwrap_or_default(),
        });
    }

    for f in filters {
        if !threads.iter().any(|t| t.matches(f)) {
            skipped.push(Skipped {
                thread_id: f.clone(),
                reason: "no such thread or comment on this PR".to_string(),
            });
        }
    }
    (out, skipped)
}

/// Why `s` can't be applied to a file of `len` lines next to `accepted`, if it can't.
fn fits(s: &Suggestion, len: usize, accepted: &[Suggestion]) -> std::result::Result<(), String> {
    if s.start == 0 || s.end as usize > len {
        return Err(format!(
            "lines {}-{} are past the end of {} ({len} lines)",
            s.start, s.end, s.path
        ));
    }
    if let Some(other) = accepted
        .iter()
        .find(|o| o.path == s.path && o.start <= s.end && s.start <= o.end)
    {
        return Err(format!("overlaps {}", other.thread_id));
    }
    Ok(())
}

/// Contents of every closed ```suggestion fence in a comment body, one entry per line.
fn parse_suggestions(body: &str) -> Vec<Vec<String>> {
    let mut blocks = Vec::new();
    let mut lines = body.lines();
    while let Some(line) = lines.next() {
        let indent = line.len() - line.trim_start_matches(' ').len();
        if indent > 3 {
            continue;
        }
        let Some((fence, info)) = fence_open(&line[indent..]) else {
            continue;
        };
        // Consume the whole block so fences nested inside other blocks are not mistaken.
        let mut content = Vec::new();
        let mut closed = false;
        for l in lines.by_ref() {
            if is_fence_close(l, fence) {
                closed = true;
                break;
            }
            content.push(strip_indent(l, indent).to_string());
        }
        if closed && info == "suggestion" {
            blocks.push(content);
        }
    }
    blocks
}

/// The opening fence run, as in "```", and its trimmed info string.
fn fence_open(line: &str) -> Option<(&str, &str)> {
    let ch = line.chars().next().filter(|c| *c == '`' || *c == '~')?;
    let len = line.len() - line.trim_start_matches(ch).len();
    if len < 3 {
        return None;
    }
    let info = line[len..].trim();
    // A backtick fence's info string can't itself contain backticks.
    if ch == '`' && info.contains('`') {
        return None;
    }
    Some((&line[..len], info))
}

/// A closing fence uses the same character, at least as many times, and nothing else.
fn is_fence_close(line: &str, fence: &str) -> bool {
    let trimmed = line.trim_start_matches(' ');
    if line.len() - trimmed.len() > 3 {
        return false;
    }
    let ch = fence.chars().next().unwrap_or('`');
    let run = trimmed.len() - trimmed.trim_start_matches(ch).len();
    run >= fence.len() && trimmed[run..].trim().is_empty()
}

/// Remove up to `n` leading spaces, matching the fence's own indentation.
fn strip_indent(line: &str, n: usize) -> &str {
    let spaces = line.len() - line.trim_start_matches(' ').len();
    &line[spaces.min(n)..]
}

/// A line's text and the ending that followed it, empty for a final line without a newline.
#[derive(Clone, Debug, PartialEq)]
struct Line {
    text: String,
    eol: String,
}

fn split_lines(content: &str) -> Vec<Line> {
    content
        .split_inclusive('\n')
        .map(|piece| {
            let (text, eol) = if let Some(t) = piece.strip_suffix("\r\n") {
                (t, "\r\n")
            } else if let Some(t) = piece.strip_suffix('\n') {
                (t, "\n")
            } else {
                (piece, "")
            };
            Line {
                text: text.to_string(),
                eol: eol.to_string(),
            }
        })
        .collect()
}

/// Apply non-overlapping edits to `content`, bottom-up so earlier line numbers stay valid.
fn apply_edits(content: &str, edits: &[&Suggestion]) -> String {
    let mut lines = split_lines(content);
    let default_eol = lines
        .iter()
        .map(|l| l.eol.as_str())
        .find(|e| !e.is_empty())
        .unwrap_or("\n")
        .to_string();

    let mut ordered: Vec<&&Suggestion> = edits.iter().collect();
    ordered.sort_by_key(|s| std::cmp::Reverse(s.start));
    for s in ordered {
        let first = (s.start - 1) as usize;
        let last = s.end as usize;
        // The replaced range's own last ending survives, so a missing final newline stays missing.
        let tail_eol = lines[last - 1].eol.clone();
        let n = s.replacement.len();
        let new: Vec<Line> = s
            .replacement
            .iter()
            .enumerate()
            .map(|(i, text)| Line {
                text: text.clone(),
                eol: if i + 1 == n {
                    tail_eol.clone()
                } else {
                    default_eol.clone()
                },
            })
            .collect();
        lines.splice(first..last, new);
    }
    lines
        .iter()
        .map(|l| format!("{}{}", l.text, l.eol))
        .collect()
}

/// A removed/added preview of one suggestion against the current file.
fn render(s: &Suggestion, lines: &[Line]) -> String {
    let mut out = format!(
        "{}  {}  {}  comment {}",
        s.thread_id,
        span(&s.path, s.start, s.end),
        s.author,
        s.comment_id
    );
    for l in &lines[(s.start - 1) as usize..s.end as usize] {
        out.push_str(&format!("\n-{}", l.text));
    }
    for r in &s.replacement {
        out.push_str(&format!("\n+{r}"));
    }
    out
}

fn span(path: &str, start: u64, end: u64) -> String {
    if start == end {
        format!("{path}:{end}")
    } else {
        format!("{path}:{start}-{end}")
    }
}

/// Fail unless HEAD is the PR head and no target file has local changes.
fn check_worktree(root: &Path, head_oid: &str, edits: &[Suggestion], number: u64) -> Result<()> {
    let head = git(&["rev-parse", "HEAD"])?;
    if head != head_oid {
        bail!(
            "HEAD is {} but the PR head is {}. Check out the PR branch and pull it first, \
             for example `gh pr checkout {number}`.",
            short(&head),
            short(head_oid)
        );
    }
    let mut dirty = Vec::new();
    for s in edits {
        if dirty.contains(&s.path) {
            continue;
        }
        let status = Command::new("git")
            .current_dir(root)
            .args(["diff", "--quiet", "HEAD", "--", &s.path])
            .status()
            .context("Failed to run git diff")?;
        if !status.success() {
            dirty.push(s.path.clone());
        }
    }
    if !dirty.is_empty() {
        bail!(
            "These files have local changes, commit or stash them first: {}",
            dirty.join(", ")
        );
    }
    Ok(())
}

fn short(oid: &str) -> &str {
    &oid[..oid.len().min(10)]
}

/// File contents at the PR head commit, for dry runs away from a checkout.
fn read_at_head(client: &Client, pr: &PrRef, head_oid: &str, path: &str) -> Result<String> {
    let url = format!(
        "{}/contents/{}?ref={head_oid}",
        pr.repo.api(),
        encode_path(path)
    );
    client
        .get_raw(&url)
        .with_context(|| format!("Failed to read {path} at {}", short(head_oid)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::pulls::Comment;
    use chrono::Utc;

    // Trimmed from CodeRabbit's comment on zlib-ng/zlib-ng#2477, deflate.c:1276-1277.
    const CODERABBIT: &str = r#"<!-- coderabbit-ai-generated:v1 -->

**Advance `fill_end` until it passes `strstart`.**

<details>
<summary>🔎 Supported by static analysis</summary>

```bash
#!/bin/bash
rg -nP -C3 '\bstrstart\s*(\+|-)?=' --type=c -g '!test/**'
```

</details>

<details>
<summary>🐛 Suggested fix</summary>

```diff
-        if (s->strstart >= s->fill_end)
+        while (s->strstart >= s->fill_end)
```

</details>

<!-- suggestion_start -->

<details>
<summary>📝 Committable suggestion</summary>

> ‼️ **IMPORTANT**
> Carefully review the code before committing.

```suggestion
        /* deflate_stored() fills up to window_size without tracking fill_end, so
         * strstart can be several windows past it after a switch from level 0. */
        while (s->strstart >= s->fill_end)
            s->fill_end += s->w_size;
```

</details>

<!-- suggestion_end -->"#;

    #[test]
    fn parses_coderabbit_committable_suggestion() {
        let blocks = parse_suggestions(CODERABBIT);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].len(), 4);
        assert_eq!(blocks[0][2], "        while (s->strstart >= s->fill_end)");
    }

    #[test]
    fn parses_empty_crlf_and_long_fences() {
        assert_eq!(
            parse_suggestions("x\n```suggestion\n```\n"),
            vec![Vec::<String>::new()]
        );
        assert_eq!(
            parse_suggestions("```suggestion\r\na\r\n\r\nb\r\n```\r\n"),
            vec![vec!["a".to_string(), String::new(), "b".to_string()]]
        );
        // A longer fence lets the suggestion itself contain a ``` line.
        assert_eq!(
            parse_suggestions("````suggestion\n```\n````"),
            vec![vec!["```".to_string()]]
        );
        assert_eq!(
            parse_suggestions("~~~suggestion\nx\n~~~"),
            vec![vec!["x".to_string()]]
        );
    }

    #[test]
    fn ignores_unclosed_nested_and_other_fences() {
        assert!(parse_suggestions("```suggestion\nnever closed").is_empty());
        assert!(parse_suggestions("```diff\n-a\n+b\n```").is_empty());
        // A suggestion quoted inside a longer markdown fence is not a suggestion.
        assert!(parse_suggestions("````markdown\n```suggestion\nx\n```\n````").is_empty());
        assert!(parse_suggestions("```suggestion extra\nx\n```").is_empty());
        assert_eq!(
            parse_suggestions("```suggestion\na\n```\n```suggestion\nb\n```").len(),
            2
        );
    }

    #[test]
    fn indented_fence_strips_its_indent() {
        assert_eq!(
            parse_suggestions("  ```suggestion\n    x\n  y\n  ```"),
            vec![vec!["  x".to_string(), "y".to_string()]]
        );
    }

    fn sugg(id: &str, start: u64, end: u64, lines: &[&str]) -> Suggestion {
        Suggestion {
            thread_id: id.to_string(),
            comment_id: 1,
            author: "bot".to_string(),
            path: "f".to_string(),
            start,
            end,
            replacement: lines.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn edits_apply_bottom_up() {
        let a = sugg("A", 1, 1, &["one", "uno"]);
        let b = sugg("B", 3, 4, &["THREE"]);
        let out = apply_edits("1\n2\n3\n4\n5\n", &[&a, &b]);
        assert_eq!(out, "one\nuno\n2\nTHREE\n5\n");
    }

    #[test]
    fn edits_preserve_crlf_and_missing_final_newline() {
        let a = sugg("A", 2, 2, &["x", "y"]);
        assert_eq!(apply_edits("1\r\n2", &[&a]), "1\r\nx\r\ny");
        let del = sugg("D", 1, 1, &[]);
        assert_eq!(apply_edits("1\r\n2\r\n", &[&del]), "2\r\n");
    }

    #[test]
    fn fits_rejects_overlap_and_out_of_range() {
        let a = sugg("A", 3, 5, &[]);
        assert!(fits(&sugg("B", 6, 7, &[]), 10, std::slice::from_ref(&a)).is_ok());
        assert_eq!(
            fits(&sugg("B", 5, 6, &[]), 10, std::slice::from_ref(&a)),
            Err("overlaps A".to_string())
        );
        assert!(fits(&sugg("C", 9, 11, &[]), 10, &[]).is_err());
    }

    fn thread(id: &str, body: &str, resolved: bool, outdated: bool) -> Thread {
        Thread {
            id: id.to_string(),
            path: "f.c".to_string(),
            line: (!outdated).then_some(8),
            start_line: Some(7),
            original_line: Some(8),
            original_start_line: Some(7),
            side: "RIGHT".to_string(),
            resolved,
            outdated,
            comments: vec![Comment {
                id: 42,
                node_id: "PRRC".to_string(),
                author: "coderabbitai".to_string(),
                bot: true,
                body: body.to_string(),
                created_at: Utc::now(),
                url: String::new(),
            }],
        }
    }

    #[test]
    fn select_filters_and_explains() {
        let threads = vec![
            thread("T1", "```suggestion\nx\n```", false, false),
            thread("T2", "```suggestion\nx\n```", true, false),
            thread("T3", "no block", false, false),
            thread(
                "T4",
                "```suggestion\na\n```\n```suggestion\nb\n```",
                false,
                false,
            ),
        ];
        let (picked, skipped) = select(&threads, &[]);
        assert_eq!(picked.len(), 1);
        assert_eq!((picked[0].start, picked[0].end), (7, 8));
        // Only the multi-block thread is reported without filters.
        assert_eq!(skipped.len(), 1);
        assert_eq!(skipped[0].thread_id, "T4");

        let (picked, skipped) = select(&threads, &["T2".into(), "T3".into(), "99".into()]);
        assert!(picked.is_empty());
        let reasons: Vec<&str> = skipped.iter().map(|s| s.reason.as_str()).collect();
        assert_eq!(
            reasons,
            [
                "thread is resolved",
                "no suggestion block",
                "no such thread or comment on this PR"
            ]
        );

        // A comment database ID selects its thread.
        let (picked, _) = select(&threads, &["42".into()]);
        assert_eq!(picked.len(), 1);
    }
}
