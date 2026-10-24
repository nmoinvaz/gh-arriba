use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::{Value, json};

use crate::cli::DumpArgs;
use crate::context::Ctx;
use crate::json::{login_of, time_of};
use crate::pr::model::{PrInfo, Thread, fetch_pr, fetch_threads};
use crate::pr::threads::line_span;

/// Width of the item prefix column, so bodies line up under it.
const PREFIX: usize = 8;

pub fn run(ctx: &Ctx, args: DumpArgs) -> Result<()> {
    let pr = ctx.pr(args.pr.as_deref())?;
    let info = fetch_pr(&ctx.client, &pr)?;
    let comments = ctx
        .client
        .get_all(&format!("{}/comments", pr.issue_api()))?;
    let reviews = ctx.client.get_all(&format!("{}/reviews", pr.api()))?;
    let threads = fetch_threads(&ctx.client, &pr)?.threads;

    let items = collect(&comments, &reviews, threads)?;

    let text = if ctx.json {
        serde_json::to_string_pretty(&json!({ "pr": info, "items": items }))? + "\n"
    } else {
        render(&info, &items)
    };

    match &args.out {
        Some(path) => {
            std::fs::write(path, &text)
                .with_context(|| format!("Failed to write {}", path.display()))?;
            eprintln!("wrote {} items to {}", items.len(), path.display());
        }
        None => print!("{text}"),
    }
    Ok(())
}

#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Item {
    Comment {
        at: DateTime<Utc>,
        author: String,
        url: String,
        body: String,
    },
    Review {
        at: DateTime<Utc>,
        author: String,
        state: String,
        url: String,
        body: String,
    },
    Thread {
        at: DateTime<Utc>,
        state: String,
        #[serde(flatten)]
        thread: Thread,
    },
}

impl Item {
    fn at(&self) -> DateTime<Utc> {
        match self {
            Item::Comment { at, .. } | Item::Review { at, .. } | Item::Thread { at, .. } => *at,
        }
    }
}

/// Merge issue comments, reviews, and review threads into one chronological list.
fn collect(comments: &[Value], reviews: &[Value], threads: Vec<Thread>) -> Result<Vec<Item>> {
    let mut items = Vec::new();
    for c in comments {
        items.push(Item::Comment {
            at: time_of(c, "created_at")?,
            author: login_of(&c["user"]),
            url: text(c, "html_url"),
            body: text(c, "body"),
        });
    }
    for r in reviews {
        let state = text(r, "state");
        let body = text(r, "body");
        // Pending reviews are private drafts. Empty COMMENTED reviews only wrap inline comments,
        // which appear under their threads.
        if state == "PENDING" || (state == "COMMENTED" && body.trim().is_empty()) {
            continue;
        }
        items.push(Item::Review {
            at: time_of(r, "submitted_at")?,
            author: login_of(&r["user"]),
            state,
            url: text(r, "html_url"),
            body,
        });
    }
    for t in threads {
        let Some(first) = t.starter() else {
            continue;
        };
        let state = if t.resolved {
            "resolved"
        } else if t.outdated {
            "outdated"
        } else {
            "OPEN"
        };
        items.push(Item::Thread {
            at: first.created_at,
            state: state.to_string(),
            thread: t,
        });
    }
    items.sort_by_key(Item::at);
    Ok(items)
}

fn text(v: &Value, key: &str) -> String {
    v[key].as_str().unwrap_or_default().replace("\r\n", "\n")
}

/// Render items so each starts at column 0 with a fixed prefix and bodies sit under `| `.
fn render(pr: &PrInfo, items: &[Item]) -> String {
    let mut out = String::new();
    let state = if pr.draft {
        format!("{} draft", pr.state)
    } else {
        pr.state.clone()
    };
    line(
        &mut out,
        "pr",
        &format!(
            "#{} {} ({state}) by {}  {} <- {}  {}",
            pr.number, pr.title, pr.author, pr.base_ref, pr.head_ref, pr.url
        ),
    );
    body(&mut out, &pr.body);

    for item in items {
        match item {
            Item::Comment {
                at,
                author,
                url,
                body: b,
            } => {
                line(
                    &mut out,
                    "comment",
                    &format!("{} {author}  {url}", stamp(at)),
                );
                body(&mut out, b);
            }
            Item::Review {
                at,
                author,
                state,
                url,
                body: b,
            } => {
                line(
                    &mut out,
                    "review",
                    &format!("{} {author} {state}  {url}", stamp(at)),
                );
                body(&mut out, b);
            }
            Item::Thread { state, thread, .. } => {
                line(
                    &mut out,
                    "thread",
                    &format!(
                        "{state} {}:{} {}",
                        thread.path,
                        line_span(thread),
                        thread.id
                    ),
                );
                for c in &thread.comments {
                    line(
                        &mut out,
                        "",
                        &format!("{} {}  {}", stamp(&c.created_at), c.author, c.url),
                    );
                    body(&mut out, &c.body.replace("\r\n", "\n"));
                }
            }
        }
    }
    out
}

fn line(out: &mut String, prefix: &str, rest: &str) {
    out.push_str(&format!("{prefix:<PREFIX$}{rest}\n"));
}

fn body(out: &mut String, text: &str) {
    for l in text.trim().lines() {
        out.push_str(&format!("{:<PREFIX$}| {l}\n", ""));
    }
}

fn stamp(at: &DateTime<Utc>) -> String {
    at.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pr::model::Comment;

    fn info() -> PrInfo {
        PrInfo {
            node_id: "PR_1".into(),
            number: 7,
            title: "Fix it".into(),
            url: "https://github.com/o/r/pull/7".into(),
            state: "OPEN".into(),
            draft: false,
            body: "Line one\r\nLine two".replace("\r\n", "\n"),
            author: "alice".into(),
            created_at: "2026-01-01T00:00:00Z".parse().unwrap(),
            base_ref: "main".into(),
            head_ref: "fix".into(),
            head_oid: "abc".into(),
            head_repo: Some("o/r".into()),
            cross_repository: false,
        }
    }

    fn thread() -> Thread {
        Thread {
            id: "PRRT_1".into(),
            path: "a.c".into(),
            line: Some(4),
            start_line: Some(2),
            original_line: Some(4),
            original_start_line: Some(2),
            side: "RIGHT".into(),
            resolved: false,
            outdated: false,
            comments: vec![Comment {
                id: 9,
                node_id: "PRRC_9".into(),
                author: "bob".into(),
                bot: false,
                body: "Rename this.".into(),
                created_at: "2026-01-02T00:00:00Z".parse().unwrap(),
                url: "https://github.com/o/r/pull/7#discussion_r9".into(),
            }],
        }
    }

    #[test]
    fn collect_sorts_and_skips_wrapper_reviews() {
        let comments = vec![json!({
            "user": {"login": "carol"}, "created_at": "2026-01-03T00:00:00Z",
            "html_url": "c", "body": "Thanks"
        })];
        let reviews = vec![
            json!({"user": {"login": "bob"}, "submitted_at": "2026-01-02T00:00:01Z",
                   "state": "COMMENTED", "html_url": "r1", "body": ""}),
            json!({"user": {"login": "bob"}, "submitted_at": "2026-01-04T00:00:00Z",
                   "state": "APPROVED", "html_url": "r2", "body": ""}),
            json!({"user": {"login": "me"}, "state": "PENDING", "html_url": "r3", "body": "x"}),
        ];
        let items = collect(&comments, &reviews, vec![thread()]).unwrap();
        let kinds: Vec<&str> = items
            .iter()
            .map(|i| match i {
                Item::Comment { .. } => "comment",
                Item::Review { .. } => "review",
                Item::Thread { .. } => "thread",
            })
            .collect();
        assert_eq!(kinds, ["thread", "comment", "review"]);
    }

    #[test]
    fn render_prefixes_every_item_and_body_line() {
        let comments = vec![json!({
            "user": {"login": "carol"}, "created_at": "2026-01-03T00:00:00Z",
            "html_url": "https://c", "body": "Thanks\r\nagain"
        })];
        let items = collect(&comments, &[], vec![thread()]).unwrap();
        let out = render(&info(), &items);
        let expected = "\
pr      #7 Fix it (OPEN) by alice  main <- fix  https://github.com/o/r/pull/7
        | Line one
        | Line two
thread  OPEN a.c:2-4 PRRT_1
        2026-01-02T00:00:00Z bob  https://github.com/o/r/pull/7#discussion_r9
        | Rename this.
comment 2026-01-03T00:00:00Z carol  https://c
        | Thanks
        | again
";
        assert_eq!(out, expected);
    }
}
