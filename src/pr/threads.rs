use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use regex::Regex;
use serde::Serialize;
use serde_json::json;
use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::io::{IsTerminal, Read};
use std::sync::LazyLock;

use crate::cli::{ReplyArgs, ResolveArgs, ThreadsArgs};
use crate::context::{Ctx, PrRef};
use crate::github::Client;
use crate::output::{first_lines, indent, print_json};
use crate::pr::model::{PrThreads, Thread, fetch_pr, fetch_threads, time_of, vars};

/// Body lines shown per comment in human output.
const BODY_LINES: usize = 12;

// -------------------------------------------------------------------------
//  threads
// -------------------------------------------------------------------------

pub fn run_threads(ctx: &Ctx, args: ThreadsArgs) -> Result<()> {
    let pr = ctx.pr(args.pr.as_deref())?;
    let info = fetch_pr(&ctx.client, &pr)?;
    let all = fetch_threads(&ctx.client, &pr)?;

    let author = match args.author.as_deref() {
        Some("@me") => Some(ctx.client.viewer()?.to_string()),
        other => other.map(str::to_string),
    };
    let since = if args.since_last_review {
        last_review_time(&ctx.client, &pr, ctx.client.viewer()?)?
    } else {
        None
    };
    let filter = Filter {
        bots: args.bots,
        humans: args.humans,
        author,
        resolved: args.resolved,
        outdated: args.outdated,
        since,
    };

    let threads: Vec<Thread> = all.threads.into_iter().filter(|t| filter.keep(t)).collect();
    let findings = if args.review_bodies && filter.keeps_coderabbit() {
        fetch_findings(&ctx.client, &pr, since)?
    } else {
        Vec::new()
    };

    if ctx.json {
        return print_json(&json!({
            "pr": { "number": info.number, "url": info.url, "author": info.author },
            "threads": threads,
            "findings": findings,
        }));
    }

    println!("{pr} {}", info.title);
    for t in &threads {
        println!();
        print_thread(t);
    }
    for f in &findings {
        println!();
        print_finding(f);
    }

    let bots = threads
        .iter()
        .filter(|t| t.starter().is_some_and(|c| c.bot))
        .count();
    let mut summary = format!(
        "{} threads ({bots} bot, {} human)",
        threads.len(),
        threads.len() - bots
    );
    if args.review_bodies {
        summary.push_str(&format!(", {} review-body findings", findings.len()));
    }
    println!("\n{summary}");
    Ok(())
}

/// Which threads `pr threads` keeps.
struct Filter {
    bots: bool,
    humans: bool,
    author: Option<String>,
    resolved: bool,
    outdated: bool,
    since: Option<DateTime<Utc>>,
}

impl Filter {
    fn keep(&self, t: &Thread) -> bool {
        if (t.resolved && !self.resolved) || (t.outdated && !self.outdated) {
            return false;
        }
        let Some(starter) = t.starter() else {
            return false;
        };
        if (self.bots && !starter.bot) || (self.humans && starter.bot) {
            return false;
        }
        if let Some(a) = &self.author
            && !same_login(&starter.author, a)
        {
            return false;
        }
        if let Some(since) = self.since
            && !t.comments.iter().any(|c| c.created_at > since)
        {
            return false;
        }
        true
    }

    /// Review-body findings come from CodeRabbit, so the starter filters apply to it too.
    fn keeps_coderabbit(&self) -> bool {
        !self.humans
            && self
                .author
                .as_deref()
                .is_none_or(|a| same_login(a, "coderabbitai"))
    }
}

/// Compare logins ignoring case and the REST `[bot]` suffix that GraphQL omits.
fn same_login(a: &str, b: &str) -> bool {
    let a = a.strip_suffix("[bot]").unwrap_or(a);
    let b = b.strip_suffix("[bot]").unwrap_or(b);
    a.eq_ignore_ascii_case(b)
}

const LAST_REVIEW_QUERY: &str = r#"
query($owner: String!, $repo: String!, $number: Int!, $author: String!) {
  repository(owner: $owner, name: $repo) {
    pullRequest(number: $number) {
      reviews(last: 1, author: $author,
              states: [APPROVED, CHANGES_REQUESTED, COMMENTED, DISMISSED]) {
        nodes { submittedAt }
      }
    }
  }
}"#;

/// When `login` last submitted a review on the PR, if ever.
fn last_review_time(client: &Client, pr: &PrRef, login: &str) -> Result<Option<DateTime<Utc>>> {
    let mut variables = vars(pr);
    variables["author"] = json!(login);
    let data = client.graphql(LAST_REVIEW_QUERY, variables)?;
    let nodes = &data["repository"]["pullRequest"]["reviews"]["nodes"];
    match nodes.get(0) {
        Some(review) if !review["submittedAt"].is_null() => {
            Ok(Some(time_of(review, "submittedAt")?))
        }
        _ => Ok(None),
    }
}

fn print_thread(t: &Thread) {
    let mut location = format!("{}:{}", t.path, line_span(t));
    if t.resolved {
        location.push_str(" (resolved)");
    } else if t.outdated {
        location.push_str(" (outdated)");
    }
    let Some(starter) = t.starter() else {
        return;
    };
    let bot = if starter.bot { " (bot)" } else { "" };
    println!(
        "{}  {location}  {}{bot}  comment {}",
        t.id, starter.author, starter.id
    );
    println!(
        "{}",
        indent(&first_lines(&clean_body(&starter.body), BODY_LINES), "  ")
    );
    for reply in &t.comments[1..] {
        let first = clean_body(&reply.body)
            .lines()
            .next()
            .unwrap_or_default()
            .to_string();
        println!("  ↳ {}: {first}", reply.author);
    }
}

/// Line or range of a thread, using the original lines once it is outdated.
pub(super) fn line_span(t: &Thread) -> String {
    let (start, end) = if t.line.is_some() {
        (t.start_line, t.line)
    } else {
        (t.original_start_line, t.original_line)
    };
    match (start, end) {
        (Some(s), Some(e)) if s != e => format!("{s}-{e}"),
        (_, Some(e)) => e.to_string(),
        _ => "?".to_string(),
    }
}

/// Strip HTML comments and collapsed `<details>` blocks, which hold bot prompts and metadata.
pub fn clean_body(body: &str) -> String {
    static COMMENT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?s)<!--.*?-->").unwrap());
    static DETAILS: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)<details\b[^>]*>|</details>").unwrap());

    let body = body.replace("\r\n", "\n");
    let body = COMMENT.replace_all(&body, "");
    let mut kept = String::new();
    let mut depth = 0usize;
    let mut last = 0usize;
    for m in DETAILS.find_iter(&body) {
        if depth == 0 {
            kept.push_str(&body[last..m.start()]);
        }
        if m.as_str().starts_with("</") {
            depth = depth.saturating_sub(1);
        } else {
            depth += 1;
        }
        last = m.end();
    }
    if depth == 0 {
        kept.push_str(&body[last..]);
    }

    // Collapse the blank runs left behind by removed blocks.
    let mut out: Vec<&str> = Vec::new();
    for line in kept.lines().map(str::trim_end) {
        if line.is_empty() && out.last().is_none_or(|l| l.is_empty()) {
            continue;
        }
        out.push(line);
    }
    out.join("\n").trim().to_string()
}

// -------------------------------------------------------------------------
//  review-body findings
// -------------------------------------------------------------------------

/// A CodeRabbit finding that lives in a review body with no thread to reply to or resolve.
#[derive(Clone, Debug, Serialize)]
pub struct Finding {
    pub review_id: u64,
    pub review_url: String,
    pub submitted_at: DateTime<Utc>,
    #[serde(flatten)]
    pub item: BodyFinding,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct BodyFinding {
    /// nitpick, outside_diff, outside_diff_and_nitpick, or duplicate.
    pub kind: String,
    pub path: String,
    pub start_line: Option<u64>,
    pub line: u64,
    pub title: String,
    pub body: String,
}

/// Findings from every CodeRabbit review on the PR, optionally only those submitted after `since`.
fn fetch_findings(
    client: &Client,
    pr: &PrRef,
    since: Option<DateTime<Utc>>,
) -> Result<Vec<Finding>> {
    let reviews = client.get_all(&format!("{}/reviews", pr.api()))?;
    let mut findings = Vec::new();
    for review in &reviews {
        if review["user"]["login"].as_str() != Some("coderabbitai[bot]") {
            continue;
        }
        // Pending reviews have no submission time and are not visible to others anyway.
        let Ok(submitted_at) = time_of(review, "submitted_at") else {
            continue;
        };
        if since.is_some_and(|s| submitted_at <= s) {
            continue;
        }
        let body = review["body"].as_str().unwrap_or_default();
        for item in parse_review_body(body) {
            findings.push(Finding {
                review_id: review["id"].as_u64().unwrap_or(0),
                review_url: review["html_url"].as_str().unwrap_or_default().to_string(),
                submitted_at,
                item,
            });
        }
    }
    Ok(findings)
}

static SECTION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)^(?:<summary>|\*\*)[^A-Za-z<*]*(nitpick comments|outside diff range(?: and nitpick)? comments|duplicate comments|additional comments)\s*\(\d+\)\s*(?:</summary>|\*\*)",
    )
    .unwrap()
});
static SECTION_END: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)^<summary>[^<]*(review details|review info|files selected|files skipped|learnings|commits|run configuration|prompt for all review comments)",
    )
    .unwrap()
});
static OLD_FILE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^<summary>([A-Za-z0-9_./][^<>]*?) \(\d+\)</summary>").unwrap());
static OLD_FINDING: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(?:Line range hint\s+)?`(\d+)(?:-(\d+))?`:\s*(.*)$").unwrap());
static NEW_FINDING: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^`([^`\s]+):(\d+)(?:-(\d+))?`\s*$").unwrap());
static BOLD_TITLE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\*\*(.+)\*\*$").unwrap());

/// Parse findings out of a CodeRabbit review body.
///
/// Two layouts exist. The older nests findings under `<summary>PATH (N)</summary>` file blocks,
/// each starting with `` `10-16`: **title** `` (sometimes after `Line range hint`) and separated by
/// `---`. The newer sits in a
/// blockquote and gives each finding its own `<details>` block starting with
/// `` `path/to/file.c:9-23` `` on a line of its own.
pub fn parse_review_body(body: &str) -> Vec<BodyFinding> {
    let body = body.replace("\r\n", "\n");
    let mut findings = Vec::new();
    let mut section: Option<String> = None;
    let mut file: Option<String> = None;
    let mut current: Option<BodyFinding> = None;
    let mut in_fence = false;

    for raw in body.lines() {
        let line = raw
            .strip_prefix("> ")
            .or_else(|| raw.strip_prefix('>'))
            .unwrap_or(raw);
        let trimmed = line.trim();

        // Structure markers inside a code fence are part of the finding's text.
        if in_fence || trimmed.starts_with("```") {
            if trimmed.starts_with("```") {
                in_fence = !in_fence;
            }
            if let Some(f) = current.as_mut() {
                push_line(f, line);
            }
            continue;
        }

        if let Some(caps) = SECTION.captures(trimmed) {
            finish(&mut current, &mut findings);
            section = section_kind(&caps[1]);
            file = None;
            continue;
        }
        if SECTION_END.is_match(trimmed) {
            finish(&mut current, &mut findings);
            section = None;
            file = None;
            continue;
        }
        let Some(kind) = section.as_ref() else {
            continue;
        };

        if trimmed == "</blockquote></details>" {
            finish(&mut current, &mut findings);
            file = None;
            continue;
        }
        if trimmed == "---" {
            finish(&mut current, &mut findings);
            continue;
        }
        if let Some(caps) = OLD_FILE.captures(trimmed) {
            finish(&mut current, &mut findings);
            file = Some(caps[1].to_string());
            continue;
        }
        if let Some(path) = file.as_ref()
            && let Some(caps) = OLD_FINDING.captures(trimmed)
        {
            finish(&mut current, &mut findings);
            let (start_line, line) = span(&caps[1], caps.get(2).map(|m| m.as_str()));
            let rest = caps[3].trim();
            // Newer bodies put a category line here and the bold title on the next line.
            let (title, body) = match BOLD_TITLE.captures(rest) {
                Some(t) if !rest.contains(" | ") => (t[1].trim().to_string(), String::new()),
                _ => (String::new(), format!("{rest}\n")),
            };
            current = Some(BodyFinding {
                kind: kind.clone(),
                path: path.clone(),
                start_line,
                line,
                title,
                body,
            });
            continue;
        }
        if let Some(caps) = NEW_FINDING.captures(trimmed) {
            finish(&mut current, &mut findings);
            let (start_line, line) = span(&caps[2], caps.get(3).map(|m| m.as_str()));
            current = Some(BodyFinding {
                kind: kind.clone(),
                path: caps[1].to_string(),
                start_line,
                line,
                title: String::new(),
                body: String::new(),
            });
            continue;
        }

        let Some(f) = current.as_mut() else {
            continue;
        };
        // The newer layout puts the title on its own bold line after a category line.
        if f.title.is_empty()
            && !trimmed.contains(" | ")
            && let Some(caps) = BOLD_TITLE.captures(trimmed)
        {
            f.title = caps[1].trim().to_string();
        }
        push_line(f, line);
    }
    finish(&mut current, &mut findings);
    findings
}

fn section_kind(heading: &str) -> Option<String> {
    let h = heading.to_ascii_lowercase();
    let kind = if h.contains("outside") && h.contains("nitpick") {
        "outside_diff_and_nitpick"
    } else if h.contains("outside") {
        "outside_diff"
    } else if h.contains("nitpick") {
        "nitpick"
    } else if h.contains("duplicate") {
        "duplicate"
    } else {
        // Additional comments are approvals of existing code, not findings.
        return None;
    };
    Some(kind.to_string())
}

/// Start and end lines from `10` or `10-16`. A single line has no start.
fn span(first: &str, second: Option<&str>) -> (Option<u64>, u64) {
    let a: u64 = first.parse().unwrap_or(0);
    match second.and_then(|s| s.parse().ok()) {
        Some(b) => (Some(a), b),
        None => (None, a),
    }
}

fn push_line(f: &mut BodyFinding, line: &str) {
    f.body.push_str(line);
    f.body.push('\n');
}

fn finish(current: &mut Option<BodyFinding>, findings: &mut Vec<BodyFinding>) {
    static COMMENT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?s)<!--.*?-->").unwrap());
    if let Some(mut f) = current.take() {
        f.body = COMMENT.replace_all(&f.body, "").trim().to_string();
        findings.push(f);
    }
}

fn print_finding(f: &Finding) {
    let span = match f.item.start_line {
        Some(s) => format!("{s}-{}", f.item.line),
        None => f.item.line.to_string(),
    };
    println!(
        "review {}  {}:{span}  coderabbitai (bot)  {}",
        f.review_id, f.item.path, f.item.kind
    );
    // The newer layout repeats the title in the body, the older keeps it on the anchor line.
    if !f.item.title.is_empty() && !f.item.body.contains(&f.item.title) {
        println!("  {}", f.item.title);
    }
    println!(
        "{}",
        indent(&first_lines(&clean_body(&f.item.body), BODY_LINES), "  ")
    );
}

// -------------------------------------------------------------------------
//  reply
// -------------------------------------------------------------------------

pub fn run_reply(ctx: &Ctx, args: ReplyArgs) -> Result<()> {
    let body = read_body(args.body.as_deref())?;

    let (comment, thread_id) = match comment_id(&args.target)? {
        Some(id) => {
            let c = lookup_comment(ctx, id)?;
            // Replies attach to the thread's top-level comment, replies to replies fail.
            let path = format!("{}/comments/{}/replies", c.pr.api(), c.top_id);
            let resp = ctx.client.post(&path, &json!({ "body": body }))?;
            let reply = NewComment {
                id: resp["id"].as_u64().unwrap_or(0),
                url: resp["html_url"].as_str().unwrap_or_default().to_string(),
            };
            let thread_id = if args.resolve {
                let threads = fetch_threads(&ctx.client, &c.pr)?;
                Some(find_thread(&threads, &id.to_string())?.id.clone())
            } else {
                None
            };
            (reply, thread_id)
        }
        None => {
            let data = ctx
                .client
                .graphql(REPLY_MUTATION, json!({ "id": args.target, "body": body }))?;
            let c = &data["addPullRequestReviewThreadReply"]["comment"];
            let reply = NewComment {
                id: c["databaseId"].as_u64().unwrap_or(0),
                url: c["url"].as_str().unwrap_or_default().to_string(),
            };
            (reply, args.resolve.then(|| args.target.clone()))
        }
    };

    let resolved = match thread_id {
        Some(id) => Some(set_resolved(&ctx.client, &id, true)?),
        None => None,
    };

    if ctx.json {
        return print_json(&json!({ "comment": comment, "resolved": resolved }));
    }
    println!("{}", comment.url);
    if let Some(r) = resolved {
        print_resolved(&r);
    }
    Ok(())
}

const REPLY_MUTATION: &str = r#"
mutation($id: ID!, $body: String!) {
  addPullRequestReviewThreadReply(input: {pullRequestReviewThreadId: $id, body: $body}) {
    comment { databaseId url }
  }
}"#;

#[derive(Serialize)]
struct NewComment {
    id: u64,
    url: String,
}

/// Reply text from the argument, or stdin for `-` or a piped stdin.
fn read_body(arg: Option<&str>) -> Result<String> {
    let body = match arg {
        Some("-") => read_stdin()?,
        Some(text) => text.to_string(),
        None if std::io::stdin().is_terminal() => {
            bail!("Pass the text as an argument, or pipe it on stdin")
        }
        None => read_stdin()?,
    };
    let body = body.trim_end_matches(['\n', '\r']).to_string();
    if body.trim().is_empty() {
        bail!("The reply is empty");
    }
    Ok(body)
}

fn read_stdin() -> Result<String> {
    let mut s = String::new();
    std::io::stdin()
        .read_to_string(&mut s)
        .context("Failed to read stdin")?;
    Ok(s)
}

// -------------------------------------------------------------------------
//  resolve
// -------------------------------------------------------------------------

pub fn run_resolve(ctx: &Ctx, args: ResolveArgs) -> Result<()> {
    let resolve = !args.undo;
    // Threads per PR number, so several comment IDs on one PR cost one fetch.
    let mut cache: HashMap<u64, PrThreads> = HashMap::new();
    let mut results = Vec::new();

    for target in &args.targets {
        let thread_id = match comment_id(target)? {
            None => target.clone(),
            Some(id) => {
                let c = lookup_comment(ctx, id)?;
                let threads = match cache.entry(c.pr.number) {
                    Entry::Occupied(e) => e.into_mut(),
                    Entry::Vacant(e) => e.insert(fetch_threads(&ctx.client, &c.pr)?),
                };
                find_thread(threads, target)?.id.clone()
            }
        };
        let r = set_resolved(&ctx.client, &thread_id, resolve)?;
        if !ctx.json {
            print_resolved(&r);
        }
        results.push(r);
    }

    if ctx.json {
        print_json(&results)?;
    }
    Ok(())
}

#[derive(Serialize)]
struct Resolved {
    thread_id: String,
    path: String,
    line: Option<u64>,
    resolved: bool,
}

const RESOLVE_MUTATION: &str = r#"
mutation($id: ID!) {
  resolveReviewThread(input: {threadId: $id}) {
    thread { id isResolved path line originalLine }
  }
}"#;

const UNRESOLVE_MUTATION: &str = r#"
mutation($id: ID!) {
  unresolveReviewThread(input: {threadId: $id}) {
    thread { id isResolved path line originalLine }
  }
}"#;

fn set_resolved(client: &Client, thread_id: &str, resolve: bool) -> Result<Resolved> {
    let (query, field) = if resolve {
        (RESOLVE_MUTATION, "resolveReviewThread")
    } else {
        (UNRESOLVE_MUTATION, "unresolveReviewThread")
    };
    let data = client.graphql(query, json!({ "id": thread_id }))?;
    let t = &data[field]["thread"];
    Ok(Resolved {
        thread_id: t["id"].as_str().unwrap_or(thread_id).to_string(),
        path: t["path"].as_str().unwrap_or_default().to_string(),
        line: t["line"].as_u64().or_else(|| t["originalLine"].as_u64()),
        resolved: t["isResolved"].as_bool().unwrap_or(resolve),
    })
}

fn print_resolved(r: &Resolved) {
    let verb = if r.resolved { "resolved" } else { "unresolved" };
    let line = r.line.map(|l| format!(":{l}")).unwrap_or_default();
    println!("{verb} {} ({}{line})", r.thread_id, r.path);
}

// -------------------------------------------------------------------------
//  shared
// -------------------------------------------------------------------------

/// Database ID of a review comment, or None for a thread node ID.
fn comment_id(target: &str) -> Result<Option<u64>> {
    if target.starts_with("PRRT_") {
        return Ok(None);
    }
    target
        .parse()
        .map(Some)
        .with_context(|| format!("Expected a comment ID or a PRRT_ thread ID, got {target:?}"))
}

/// The PR a review comment belongs to and the top-level comment of its thread.
struct CommentRef {
    pr: PrRef,
    top_id: u64,
}

fn lookup_comment(ctx: &Ctx, id: u64) -> Result<CommentRef> {
    let repo = ctx.repo()?;
    let path = format!("{}/pulls/comments/{id}", repo.api());
    let comment = ctx.client.get_opt(&path)?.with_context(|| {
        // Comment IDs carry no repo, so a lookup in the wrong one is the likely mistake.
        let hint = if ctx.repo_flag.is_none() {
            " Pass --repo OWNER/REPO."
        } else {
            ""
        };
        format!("Review comment {id} not found in {repo}.{hint}")
    })?;
    let number = comment["pull_request_url"]
        .as_str()
        .and_then(|u| u.rsplit('/').next())
        .and_then(|n| n.parse().ok())
        .with_context(|| format!("Review comment {id} has no pull request"))?;
    Ok(CommentRef {
        pr: PrRef { repo, number },
        top_id: comment["in_reply_to_id"].as_u64().unwrap_or(id),
    })
}

fn find_thread<'a>(threads: &'a PrThreads, id: &str) -> Result<&'a Thread> {
    threads
        .threads
        .iter()
        .find(|t| t.matches(id))
        .with_context(|| format!("No review thread contains comment {id}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pr::model::Comment;

    /// Trimmed from a 2024 CodeRabbit review on zlib-ng/zlib-ng#1813.
    const OLD_BODY: &str = r#"**Actionable comments posted: 2**

<details>
<summary>🧹 Outside diff range and nitpick comments (3)</summary><blockquote>

<details>
<summary>arch/s390/dfltcc_common.c (2)</summary><blockquote>

`10-16`: **Document thread-safety considerations for global variables.**

These variables are initialized once during the library constructor.

```diff
+/* These variables are initialized once
 int env_dfltcc_disabled;
```

---

Line range hint `45`: **Define constant for maximum compression level.**

The maximum value of 10 should be a constant.

<details>
<summary>🧰 Tools</summary>
<details>
<summary>🪛 cppcheck (2)</summary>
[error] 24-24: unknown macro
</details>
</details>

</blockquote></details>
<details>
<summary>CMakeLists.txt (1)</summary><blockquote>

`120-121`: **Quote the variable.**

Unquoted expansion breaks on paths with spaces.

<!-- This is an auto-generated comment by CodeRabbit -->

</blockquote></details>

</blockquote></details>
<details>
<summary>🔇 Additional comments (1)</summary><blockquote>

<details>
<summary>arch/s390/dfltcc_deflate.c (1)</summary><blockquote>

`5-9`: **LGTM!**

Looks fine.

</blockquote></details>

</blockquote></details>

<details>
<summary>📜 Review details</summary>

**Configuration used: CodeRabbit UI**
</details>"#;

    /// Trimmed from a 2026 CodeRabbit review on zlib-ng/zlib-ng#2472.
    const NEW_BODY: &str = r#"> [!CAUTION]
> Some comments are outside the diff and can’t be posted inline due to GitHub limitations.
>
> **⚠️ Outside diff range comments (1)**
>
> <details>
> <summary><strong>🟡 Minor</strong> · Initialize DFLTCC for native builds. · <code>dfltcc_common.c:9-23</code></summary><blockquote>
>
> `arch/s390/dfltcc_common.c:9-23`
> **🎯 Functional Correctness** | **🟡 Minor** | **⚡ Quick win**
>
> **Initialize DFLTCC for native builds.**
>
> Native builds define `FUNCTABLE_INIT` as a no-op.
>
> <details><summary>Suggested fix</summary>
>
> ```diff
> -#  define FUNCTABLE_INIT ((void)0)
> ---
> +#  define FUNCTABLE_INIT PREFIX(dfltcc_init_env)()
> ```
>
> </details>
>
> <!-- cr-comment:v1:7fbe407489fc76ac0cd5d193 -->
>
> </blockquote></details>

---

<details>
<summary>ℹ️ Review info</summary>

`ignored/path.c:1`
</details>"#;

    /// Trimmed from a 2026 CodeRabbit review on calcom/cal.com#30135.
    const NITPICK_BODY: &str = r#"**Actionable comments posted: 1**

<details>
<summary>🧹 Nitpick comments (1)</summary><blockquote>

<details>
<summary>packages/embeds/embed-core/src/embed.test.ts (1)</summary><blockquote>

`823-826`: _🎯 Functional Correctness_ | _🔵 Trivial_ | _⚡ Quick win_

**Assert the guard's state and queue invariants.**

The test does not verify that `iframeReady` remains falsy.

<details>
<summary>🤖 Prompt for AI Agents</summary>

```
In `@packages/embeds/embed-core/src/embed.test.ts` around lines 823 - 826, extend the test.
```

</details>

</blockquote></details>

</blockquote></details>

<details>
<summary>🤖 Prompt for all review comments with AI agents</summary>

```
Outside diff range comments:
`packages/other.ts:5`
```

</details>"#;

    #[test]
    fn parses_nitpick_layout_with_category_line() {
        let f = parse_review_body(NITPICK_BODY);
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].kind, "nitpick");
        assert_eq!(f[0].path, "packages/embeds/embed-core/src/embed.test.ts");
        assert_eq!((f[0].start_line, f[0].line), (Some(823), 826));
        assert_eq!(f[0].title, "Assert the guard's state and queue invariants.");
        assert!(f[0].body.starts_with("_🎯 Functional Correctness_"));
    }

    #[test]
    fn parses_old_layout_by_file_block() {
        let f = parse_review_body(OLD_BODY);
        assert_eq!(f.len(), 3);
        assert_eq!(f[0].kind, "outside_diff_and_nitpick");
        assert_eq!(f[0].path, "arch/s390/dfltcc_common.c");
        assert_eq!((f[0].start_line, f[0].line), (Some(10), 16));
        assert_eq!(
            f[0].title,
            "Document thread-safety considerations for global variables."
        );
        assert!(f[0].body.contains("int env_dfltcc_disabled;"));
        assert_eq!((f[1].start_line, f[1].line), (None, 45));
        assert_eq!(f[1].title, "Define constant for maximum compression level.");
        // A tool summary with a count inside a finding is not a file block.
        assert!(f[1].body.contains("unknown macro"));
        assert_eq!(f[1].path, "arch/s390/dfltcc_common.c");
        assert_eq!(f[2].path, "CMakeLists.txt");
        assert!(!f[2].body.contains("auto-generated"));
    }

    #[test]
    fn parses_new_layout_inside_blockquote() {
        let f = parse_review_body(NEW_BODY);
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].kind, "outside_diff");
        assert_eq!(f[0].path, "arch/s390/dfltcc_common.c");
        assert_eq!((f[0].start_line, f[0].line), (Some(9), 23));
        assert_eq!(f[0].title, "Initialize DFLTCC for native builds.");
        // A `---` inside a code fence does not end the finding.
        assert!(f[0].body.contains("PREFIX(dfltcc_init_env)()"));
        assert!(!f[0].body.contains("cr-comment"));
    }

    #[test]
    fn body_without_sections_has_no_findings() {
        assert!(parse_review_body("**Actionable comments posted: 3**\n\n`a.c:1`").is_empty());
    }

    #[test]
    fn clean_body_drops_nested_details_and_comments() {
        let body = "_⚠️ Potential issue_\r\n\r\n**Fix it.**\r\n\r\n<details>\r\n<summary>Prompt</summary>\r\n<details>inner</details>\r\nprompt text\r\n</details>\r\n\r\n<!-- fingerprint -->\r\nTail.";
        assert_eq!(
            clean_body(body),
            "_⚠️ Potential issue_\n\n**Fix it.**\n\nTail."
        );
    }

    fn comment(author: &str, bot: bool, at: &str) -> Comment {
        Comment {
            id: 1,
            node_id: "PRRC_1".into(),
            author: author.into(),
            bot,
            body: "b".into(),
            created_at: at.parse().unwrap(),
            url: "u".into(),
        }
    }

    fn thread(resolved: bool, outdated: bool, comments: Vec<Comment>) -> Thread {
        Thread {
            id: "PRRT_1".into(),
            path: "a.c".into(),
            line: Some(3),
            start_line: None,
            original_line: Some(3),
            original_start_line: None,
            side: "RIGHT".into(),
            resolved,
            outdated,
            comments,
        }
    }

    fn filter() -> Filter {
        Filter {
            bots: false,
            humans: false,
            author: None,
            resolved: false,
            outdated: false,
            since: None,
        }
    }

    #[test]
    fn filter_defaults_to_open_current_threads() {
        let c = || vec![comment("alice", false, "2026-01-01T00:00:00Z")];
        assert!(filter().keep(&thread(false, false, c())));
        assert!(!filter().keep(&thread(true, false, c())));
        assert!(!filter().keep(&thread(false, true, c())));
        let wide = Filter {
            resolved: true,
            outdated: true,
            ..filter()
        };
        assert!(wide.keep(&thread(true, true, c())));
    }

    #[test]
    fn filter_by_starter_kind_and_login() {
        let bot = thread(
            false,
            false,
            vec![comment("coderabbitai", true, "2026-01-01T00:00:00Z")],
        );
        let human = thread(
            false,
            false,
            vec![comment("alice", false, "2026-01-01T00:00:00Z")],
        );
        let bots = Filter {
            bots: true,
            ..filter()
        };
        let humans = Filter {
            humans: true,
            ..filter()
        };
        assert!(bots.keep(&bot) && !bots.keep(&human));
        assert!(humans.keep(&human) && !humans.keep(&bot));
        let by = Filter {
            author: Some("coderabbitai[bot]".into()),
            ..filter()
        };
        assert!(by.keep(&bot) && !by.keep(&human));
        assert!(by.keeps_coderabbit());
        assert!(!humans.keeps_coderabbit());
    }

    #[test]
    fn filter_since_keeps_threads_with_newer_comments() {
        let since = Some("2026-01-02T00:00:00Z".parse().unwrap());
        let f = Filter { since, ..filter() };
        let old = thread(
            false,
            false,
            vec![comment("a", false, "2026-01-01T00:00:00Z")],
        );
        let replied = thread(
            false,
            false,
            vec![
                comment("a", false, "2026-01-01T00:00:00Z"),
                comment("b", false, "2026-01-03T00:00:00Z"),
            ],
        );
        assert!(!f.keep(&old));
        assert!(f.keep(&replied));
    }

    #[test]
    fn line_span_falls_back_to_original_lines() {
        let mut t = thread(false, true, vec![]);
        t.line = None;
        t.original_start_line = Some(4);
        t.original_line = Some(9);
        assert_eq!(line_span(&t), "4-9");
        t.line = Some(12);
        t.start_line = None;
        assert_eq!(line_span(&t), "12");
    }

    #[test]
    fn comment_id_accepts_numbers_and_thread_ids() {
        assert_eq!(comment_id("123").unwrap(), Some(123));
        assert_eq!(comment_id("PRRT_kwDO").unwrap(), None);
        assert!(comment_id("abc").is_err());
    }
}
