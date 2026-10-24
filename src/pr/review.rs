use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde_json::json;

use crate::api::pulls::{self, NewThread, fetch_pending};
use crate::api::{Client, PrRef};
use crate::cli::{PrArg, ReviewAddArgs, ReviewCommand, ReviewEvent, ReviewSubmitArgs, Side};
use crate::context::Ctx;
use crate::output;
use crate::prompt;

pub fn run(ctx: &Ctx, cmd: ReviewCommand) -> Result<()> {
    match cmd {
        ReviewCommand::Lines(args) => lines(ctx, args),
        ReviewCommand::Add(args) => add(ctx, args),
        ReviewCommand::Show(args) => show(ctx, args),
        ReviewCommand::Submit(args) => submit(ctx, args),
        ReviewCommand::Discard(args) => discard(ctx, args),
    }
}

/// Inclusive line range, serialized as `[start, end]`.
pub type Range = (u64, u64);

/// Lines of one file that accept inline comments, per diff side.
#[derive(Debug, Serialize)]
pub struct FileLines {
    pub path: String,
    pub right: Vec<Range>,
    pub left: Vec<Range>,
}

/// Commentable ranges for every file in the PR. Files without a patch get empty ranges.
pub fn fetch_file_lines(client: &Client, pr: &PrRef) -> Result<Vec<FileLines>> {
    let files = pulls::pr_files(client, pr)?;
    Ok(files
        .into_iter()
        .map(|f| {
            let (left, right) = f.patch.as_deref().map(hunk_ranges).unwrap_or_default();
            FileLines {
                path: f.filename,
                right,
                left,
            }
        })
        .collect())
}

/// LEFT and RIGHT ranges covered by the hunks of a unified diff patch.
pub fn hunk_ranges(patch: &str) -> (Vec<Range>, Vec<Range>) {
    let mut left = Vec::new();
    let mut right = Vec::new();
    for (old, new) in patch.lines().filter_map(parse_hunk_header) {
        left.extend(span_range(old));
        right.extend(span_range(new));
    }
    (left, right)
}

/// Parse `@@ -a,b +c,d @@ context` into `((a, b), (c, d))`. Omitted counts mean 1.
fn parse_hunk_header(line: &str) -> Option<((u64, u64), (u64, u64))> {
    let spec = line.strip_prefix("@@ ")?.split_once(" @@")?.0;
    let mut parts = spec.split_whitespace();
    let old = parse_span(parts.next()?.strip_prefix('-')?)?;
    let new = parse_span(parts.next()?.strip_prefix('+')?)?;
    Some((old, new))
}

fn parse_span(s: &str) -> Option<(u64, u64)> {
    match s.split_once(',') {
        Some((start, count)) => Some((start.parse().ok()?, count.parse().ok()?)),
        None => Some((s.parse().ok()?, 1)),
    }
}

/// A zero count is an empty side of the hunk, as in a new or deleted file.
fn span_range((start, count): (u64, u64)) -> Option<Range> {
    (count > 0).then(|| (start, start + count - 1))
}

fn format_ranges(ranges: &[Range]) -> String {
    ranges
        .iter()
        .map(|&(s, e)| {
            if s == e {
                s.to_string()
            } else {
                format!("{s}-{e}")
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn lines(ctx: &Ctx, args: PrArg) -> Result<()> {
    let pr = ctx.pr(args.pr.as_deref())?;
    let files = fetch_file_lines(&ctx.client, &pr)?;
    if ctx.json {
        return output::print_json(&files);
    }
    let width = files.iter().map(|f| f.path.len()).max().unwrap_or(0);
    for f in &files {
        if f.right.is_empty() && f.left.is_empty() {
            println!("{:width$}  no diff available, not commentable", f.path);
            continue;
        }
        let mut sides = Vec::new();
        if !f.right.is_empty() {
            sides.push(format!("RIGHT {}", format_ranges(&f.right)));
        }
        if !f.left.is_empty() {
            sides.push(format!("LEFT {}", format_ranges(&f.left)));
        }
        println!("{:width$}  {}", f.path, sides.join("  "));
    }
    Ok(())
}

/// `path:line` or `path:start-end`.
fn location(path: &str, start: Option<u64>, line: Option<u64>) -> String {
    match (start, line) {
        (Some(s), Some(e)) if s != e => format!("{path}:{s}-{e}"),
        (_, Some(l)) => format!("{path}:{l}"),
        _ => path.to_string(),
    }
}

/// Parse `PATH:LINE` or `PATH:START-END` into the path and an inclusive range.
/// Split `[PR] PATH:LINE[-END] [BODY]` into its parts. The PR is present only when the first
/// argument is not a location and the second is.
fn split_add_args(args: &[String]) -> Result<(Option<&str>, &str, Option<&str>)> {
    let is_location = |i: usize| args.get(i).is_some_and(|a| parse_location(a).is_ok());
    let (pr, rest) = if is_location(0) {
        (None, args)
    } else if is_location(1) {
        (Some(args[0].as_str()), &args[1..])
    } else {
        // Report why the first argument is not a location.
        parse_location(&args[0])?;
        unreachable!("parse_location failed above");
    };
    match rest {
        [location] => Ok((pr, location, None)),
        [location, body] => Ok((pr, location, Some(body))),
        _ => bail!(
            "Expected [PR] PATH:LINE[-END] [BODY], got {} arguments",
            args.len()
        ),
    }
}

fn parse_location(s: &str) -> Result<(String, Range)> {
    let (path, spec) = s
        .rsplit_once(':')
        .with_context(|| format!("Expected PATH:LINE or PATH:START-END, got {s:?}"))?;
    let parse = |n: &str| -> Result<u64> {
        n.trim()
            .parse::<u64>()
            .ok()
            .filter(|&n| n > 0)
            .with_context(|| format!("Bad line number {n:?} in {s:?}"))
    };
    let range = match spec.split_once('-') {
        Some((a, b)) => (parse(a)?, parse(b)?),
        None => {
            let l = parse(spec)?;
            (l, l)
        }
    };
    if path.is_empty() {
        bail!("Missing path in {s:?}");
    }
    if range.0 > range.1 {
        bail!(
            "Range start {} is after its end {} in {s:?}",
            range.0,
            range.1
        );
    }
    Ok((path.to_string(), range))
}

fn side_name(side: Side) -> &'static str {
    match side {
        Side::Left => "LEFT",
        Side::Right => "RIGHT",
    }
}

/// Fail unless `range` lies inside a single hunk on `side` of `path`.
fn check_commentable(files: &[FileLines], path: &str, side: Side, range: Range) -> Result<()> {
    let Some(file) = files.iter().find(|f| f.path == path) else {
        bail!("{path} is not part of this PR's diff");
    };
    let ranges = match side {
        Side::Left => &file.left,
        Side::Right => &file.right,
    };
    if ranges.iter().any(|&(s, e)| s <= range.0 && range.1 <= e) {
        return Ok(());
    }
    let wanted = location(path, Some(range.0), Some(range.1));
    let side = side_name(side);
    if ranges.is_empty() {
        bail!("{wanted} is not commentable, {path} has no {side} lines in the diff");
    }
    bail!(
        "{wanted} is outside the diff. {side} lines that accept comments in {path}: {}",
        format_ranges(ranges)
    )
}

#[derive(Serialize)]
struct Added {
    review_id: String,
    thread_id: Option<String>,
    path: String,
    start_line: u64,
    line: u64,
    side: &'static str,
    comment_count: u64,
}

fn add(ctx: &Ctx, args: ReviewAddArgs) -> Result<()> {
    let (pr_arg, spot, body) = split_add_args(&args.args)?;
    let (path, range) = parse_location(spot)?;
    let body = prompt::read_body(body, "comment")?;
    let pr = ctx.pr(pr_arg)?;
    let client = &ctx.client;

    // Validate locally first, so a line outside the diff fails with the valid ranges.
    let files = fetch_file_lines(client, &pr)?;
    check_commentable(&files, &path, args.side, range)?;

    let state = fetch_pending(client, &pr)?;
    let (review_id, existing) = match &state.pending {
        Some(p) => (p.id.clone(), p.comment_count),
        None => (pulls::add_pending_review(client, &state.pr_id)?, 0),
    };

    let side = side_name(args.side);
    let thread_id = pulls::add_review_thread(
        client,
        &NewThread {
            review_id: &review_id,
            path: &path,
            start_line: range.0,
            line: range.1,
            side,
            body: &body,
        },
    )?;

    let added = Added {
        review_id,
        thread_id,
        path,
        start_line: range.0,
        line: range.1,
        side,
        comment_count: existing + 1,
    };
    if ctx.json {
        return output::print_json(&added);
    }
    println!(
        "added {} to pending review {} ({} comment{})",
        location(&added.path, Some(added.start_line), Some(added.line)),
        added.review_id,
        added.comment_count,
        if added.comment_count == 1 { "" } else { "s" }
    );
    Ok(())
}

fn show(ctx: &Ctx, args: PrArg) -> Result<()> {
    let pr = ctx.pr(args.pr.as_deref())?;
    let state = fetch_pending(&ctx.client, &pr)?;
    if ctx.json {
        return output::print_json(&state.pending);
    }
    let Some(p) = state.pending else {
        println!("No pending review on {pr}.");
        return Ok(());
    };
    println!(
        "Pending review {} on {pr}, {} comment{}",
        p.id,
        p.comment_count,
        if p.comment_count == 1 { "" } else { "s" }
    );
    if !p.body.trim().is_empty() {
        println!("body");
        println!("{}", output::indent(p.body.trim(), "  | "));
    }
    for c in &p.comments {
        println!(
            "{}  comment {}",
            location(&c.path, c.start_line, c.line),
            c.id
        );
        println!("{}", output::indent(c.body.trim(), "  | "));
    }
    if p.comment_count > p.comments.len() as u64 {
        println!(
            "[+{} comments not shown]",
            p.comment_count - p.comments.len() as u64
        );
    }
    Ok(())
}

fn event_name(event: ReviewEvent) -> &'static str {
    match event {
        ReviewEvent::Comment => "COMMENT",
        ReviewEvent::Approve => "APPROVE",
        ReviewEvent::RequestChanges => "REQUEST_CHANGES",
    }
}

/// What submit will do, decided before any mutation.
#[derive(Debug, PartialEq)]
enum SubmitPlan {
    /// Submit the existing pending review.
    Submit(String),
    /// Create and submit a review in one step, since none is pending.
    Create,
}

fn plan_submit(
    event: ReviewEvent,
    body: Option<&str>,
    pending: Option<(&str, u64)>,
) -> Result<SubmitPlan> {
    let has_body = body.is_some_and(|b| !b.trim().is_empty());
    let inline = pending.map(|(_, n)| n).unwrap_or(0);
    // GitHub requires a body for these events unless inline comments carry the review.
    if matches!(event, ReviewEvent::Comment | ReviewEvent::RequestChanges)
        && !has_body
        && inline == 0
    {
        if pending.is_none() && matches!(event, ReviewEvent::Comment) {
            bail!("No pending review and no --body, so there is nothing to submit");
        }
        bail!(
            "A {} review without inline comments needs --body",
            event_name(event)
        );
    }
    Ok(match pending {
        Some((id, _)) => SubmitPlan::Submit(id.to_string()),
        None => SubmitPlan::Create,
    })
}

fn submit(ctx: &Ctx, args: ReviewSubmitArgs) -> Result<()> {
    let pr = ctx.pr(args.pr.as_deref())?;
    let client = &ctx.client;
    let state = fetch_pending(client, &pr)?;

    if matches!(
        args.event,
        ReviewEvent::Approve | ReviewEvent::RequestChanges
    ) && state.author == client.viewer()?
    {
        bail!(
            "You can't {} your own pull request",
            match args.event {
                ReviewEvent::Approve => "approve",
                _ => "request changes on",
            }
        );
    }

    let pending = state
        .pending
        .as_ref()
        .map(|p| (p.id.as_str(), p.comment_count));
    let plan = plan_submit(args.event, args.body.as_deref(), pending)?;
    let event = event_name(args.event);
    let body = args
        .body
        .as_deref()
        .map(str::trim)
        .filter(|b| !b.is_empty());

    let review = match plan {
        SubmitPlan::Submit(id) => pulls::submit_review(client, &id, event, body)?,
        SubmitPlan::Create => pulls::create_review(client, &state.pr_id, event, body)?,
    };

    if ctx.json {
        return output::print_json(&review);
    }
    let comments = state.pending.as_ref().map(|p| p.comment_count).unwrap_or(0);
    let r = review.unwrap_or_default();
    println!(
        "submitted review {} as {} with {} inline comment{}: {}",
        r.id.unwrap_or_default(),
        r.state.unwrap_or_default(),
        comments,
        if comments == 1 { "" } else { "s" },
        r.url.unwrap_or_default()
    );
    Ok(())
}

fn discard(ctx: &Ctx, args: PrArg) -> Result<()> {
    let pr = ctx.pr(args.pr.as_deref())?;
    let state = fetch_pending(&ctx.client, &pr)?;
    let Some(p) = state.pending else {
        if ctx.json {
            return output::print_json(&json!({ "discarded": null }));
        }
        println!("No pending review on {pr}.");
        return Ok(());
    };

    let prompt = format!(
        "Discard pending review {} with {} comment{} on {pr}?",
        p.id,
        p.comment_count,
        if p.comment_count == 1 { "" } else { "s" }
    );
    if !prompt::confirm(&prompt, ctx.yes)? {
        println!("Kept pending review {}.", p.id);
        return Ok(());
    }
    pulls::delete_review(&ctx.client, &p.id)?;

    if ctx.json {
        return output::print_json(&json!({ "discarded": p.id, "comment_count": p.comment_count }));
    }
    println!(
        "discarded pending review {} ({} comments)",
        p.id, p.comment_count
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use mockito::{Matcher, Mock, Server};
    use serde_json::Value;

    fn ctx(server: &Server, json: bool) -> Ctx {
        Ctx {
            client: Client::for_tests(&server.url()),
            repo_flag: Some("o/r".into()),
            json,
            yes: true,
        }
    }

    fn gql(server: &mut Server, needle: &str, data: Value) -> Mock {
        server
            .mock("POST", "/graphql")
            .match_body(Matcher::Regex(needle.into()))
            .with_body(json!({ "data": data }).to_string())
            .create()
    }

    fn mock_files(server: &mut Server) -> Mock {
        server
            .mock("GET", "/repos/o/r/pulls/7/files")
            .match_query(Matcher::Any)
            .with_body(
                json!([
                    {"filename": "src/a.c", "patch": "@@ -1,3 +1,5 @@\n x\n+y\n@@ -20,2 +22,0 @@\n-z"},
                    {"filename": "logo.png"}
                ])
                .to_string(),
            )
            .create()
    }

    /// The pending-review query, with a review of `count` comments when `pending` is set.
    fn mock_pending(server: &mut Server, author: &str, pending: Option<u64>) -> Mock {
        let reviews = match pending {
            Some(count) => json!({"nodes": [{
                "id": "PRR_1", "url": "https://r/1", "body": "Overall fine.",
                "viewerDidAuthor": true,
                "comments": {"totalCount": count, "nodes": [{
                    "databaseId": 5, "path": "src/a.c", "startLine": 1, "line": 2,
                    "body": "nit"
                }]}
            }]}),
            None => json!({"nodes": []}),
        };
        gql(
            server,
            "states: PENDING",
            json!({"repository": {"pullRequest": {
                "id": "PR_7", "author": {"login": author}, "reviews": reviews
            }}}),
        )
    }

    fn mock_viewer(server: &mut Server, login: &str) -> Mock {
        gql(
            server,
            r"query \{ viewer",
            json!({"viewer": {"login": login}}),
        )
    }

    fn add_args(args: &[&str], side: Side) -> ReviewAddArgs {
        ReviewAddArgs {
            args: args.iter().map(|s| s.to_string()).collect(),
            side,
        }
    }

    #[test]
    fn lines_lists_commentable_ranges() {
        let mut server = Server::new();
        let files = mock_files(&mut server).expect(2);
        let lines_of = fetch_file_lines(&Client::for_tests(&server.url()), &pr()).unwrap();
        assert_eq!(lines_of[0].right, vec![(1, 5)]);
        assert_eq!(lines_of[0].left, vec![(1, 3), (20, 21)]);
        assert!(lines_of[1].right.is_empty() && lines_of[1].left.is_empty());
        let arg = PrArg {
            pr: Some("7".into()),
        };
        run(&ctx(&server, false), ReviewCommand::Lines(arg)).unwrap();
        files.assert();
    }

    fn pr() -> PrRef {
        PrRef {
            repo: crate::api::Repo::parse("o/r").unwrap(),
            number: 7,
        }
    }

    #[test]
    fn add_joins_the_existing_pending_review() {
        let mut server = Server::new();
        mock_files(&mut server);
        mock_pending(&mut server, "alice", Some(2));
        let start = gql(&mut server, r"pullRequestId: \$pr \}", json!({})).expect(0);
        let thread = server
            .mock("POST", "/graphql")
            .match_body(Matcher::PartialJson(json!({"variables": {"input": {
                "pullRequestReviewId": "PRR_1", "path": "src/a.c", "line": 3,
                "startLine": 2, "side": "RIGHT", "body": "Rename"
            }}})))
            .with_body(r#"{"data": {"addPullRequestReviewThread": {"thread": {"id": "PRRT_9"}}}}"#)
            .create();
        let args = add_args(&["o/r#7", "src/a.c:2-3", "Rename"], Side::Right);
        run(&ctx(&server, true), ReviewCommand::Add(args)).unwrap();
        thread.assert();
        start.assert();
    }

    #[test]
    fn add_starts_a_pending_review_when_none_exists() {
        let mut server = Server::new();
        mock_files(&mut server);
        mock_pending(&mut server, "alice", None);
        let start = server
            .mock("POST", "/graphql")
            .match_body(Matcher::AllOf(vec![
                Matcher::Regex(r"pullRequestId: \$pr \}".into()),
                Matcher::PartialJson(json!({"variables": {"pr": "PR_7"}})),
            ]))
            .with_body(
                r#"{"data": {"addPullRequestReview": {"pullRequestReview": {"id": "PRR_new"}}}}"#,
            )
            .create();
        let thread = server
            .mock("POST", "/graphql")
            .match_body(Matcher::PartialJson(json!({"variables": {"input": {
                "pullRequestReviewId": "PRR_new", "line": 21, "side": "LEFT"
            }}})))
            .with_body(r#"{"data": {"addPullRequestReviewThread": {"thread": null}}}"#)
            .create();
        let args = add_args(&["7", "src/a.c:21", "Why removed?"], Side::Left);
        run(&ctx(&server, false), ReviewCommand::Add(args)).unwrap();
        start.assert();
        thread.assert();
    }

    #[test]
    fn add_outside_the_diff_fails_before_any_mutation() {
        let mut server = Server::new();
        mock_files(&mut server);
        let pending = mock_pending(&mut server, "alice", None).expect(0);
        let args = add_args(&["7", "src/a.c:9", "x"], Side::Right);
        let err = run(&ctx(&server, true), ReviewCommand::Add(args))
            .unwrap_err()
            .to_string();
        assert_eq!(
            err,
            "src/a.c:9 is outside the diff. RIGHT lines that accept comments in src/a.c: 1-5"
        );
        let args = add_args(&["7", "logo.png:1", "x"], Side::Right);
        let err = run(&ctx(&server, true), ReviewCommand::Add(args))
            .unwrap_err()
            .to_string();
        assert_eq!(
            err,
            "logo.png:1 is not commentable, logo.png has no RIGHT lines in the diff"
        );
        pending.assert();
    }

    #[test]
    fn show_prints_the_pending_review_or_none() {
        let mut server = Server::new();
        let m = mock_pending(&mut server, "alice", Some(3));
        let arg = || PrArg {
            pr: Some("7".into()),
        };
        run(&ctx(&server, false), ReviewCommand::Show(arg())).unwrap();
        run(&ctx(&server, true), ReviewCommand::Show(arg())).unwrap();
        m.expect(2).assert();
        let mut server = Server::new();
        mock_pending(&mut server, "alice", None);
        run(&ctx(&server, false), ReviewCommand::Show(arg())).unwrap();
    }

    fn submit_args(event: ReviewEvent, body: Option<&str>) -> ReviewSubmitArgs {
        ReviewSubmitArgs {
            pr: Some("7".into()),
            event,
            body: body.map(str::to_string),
        }
    }

    #[test]
    fn submit_refuses_to_approve_your_own_pr() {
        let mut server = Server::new();
        mock_pending(&mut server, "me", Some(1));
        mock_viewer(&mut server, "me");
        let submit = gql(&mut server, "submitPullRequestReview", json!({})).expect(0);
        let args = submit_args(ReviewEvent::Approve, None);
        let err = run(&ctx(&server, true), ReviewCommand::Submit(args))
            .unwrap_err()
            .to_string();
        assert_eq!(err, "You can't approve your own pull request");
        let args = submit_args(ReviewEvent::RequestChanges, Some("x"));
        let err = run(&ctx(&server, true), ReviewCommand::Submit(args))
            .unwrap_err()
            .to_string();
        assert_eq!(err, "You can't request changes on your own pull request");
        submit.assert();
    }

    #[test]
    fn submit_sends_the_pending_review() {
        let mut server = Server::new();
        mock_pending(&mut server, "alice", Some(2));
        mock_viewer(&mut server, "me");
        let submit = server
            .mock("POST", "/graphql")
            .match_body(Matcher::PartialJson(json!({"variables": {
                "id": "PRR_1", "event": "APPROVE", "body": "LGTM"
            }})))
            .with_body(
                r#"{"data": {"submitPullRequestReview": {"pullRequestReview": {
                    "id": "PRR_1", "state": "APPROVED", "url": "u"}}}}"#,
            )
            .create();
        let args = submit_args(ReviewEvent::Approve, Some("  LGTM \n"));
        run(&ctx(&server, false), ReviewCommand::Submit(args)).unwrap();
        submit.assert();
    }

    #[test]
    fn submit_creates_a_review_when_none_is_pending() {
        let mut server = Server::new();
        mock_pending(&mut server, "alice", None);
        let create = server
            .mock("POST", "/graphql")
            .match_body(Matcher::AllOf(vec![
                Matcher::Regex(r"pullRequestId: \$pr, event".into()),
                Matcher::PartialJson(json!({"variables": {
                    "pr": "PR_7", "event": "COMMENT", "body": "Thoughts"
                }})),
            ]))
            .with_body(
                r#"{"data": {"addPullRequestReview": {"pullRequestReview": {
                    "id": "PRR_2", "state": "COMMENTED", "url": "u"}}}}"#,
            )
            .create();
        let args = submit_args(ReviewEvent::Comment, Some("Thoughts"));
        run(&ctx(&server, true), ReviewCommand::Submit(args)).unwrap();
        create.assert();
        // Nothing pending and no body is refused before any mutation.
        let args = submit_args(ReviewEvent::Comment, None);
        assert!(run(&ctx(&server, true), ReviewCommand::Submit(args)).is_err());
        create.assert();
    }

    #[test]
    fn discard_deletes_the_pending_review() {
        let mut server = Server::new();
        mock_pending(&mut server, "alice", Some(2));
        let delete = server
            .mock("POST", "/graphql")
            .match_body(Matcher::AllOf(vec![
                Matcher::Regex("deletePullRequestReview".into()),
                Matcher::PartialJson(json!({"variables": {"id": "PRR_1"}})),
            ]))
            .with_body(
                r#"{"data": {"deletePullRequestReview": {"pullRequestReview": {"id": "PRR_1"}}}}"#,
            )
            .expect(2)
            .create();
        let arg = || PrArg {
            pr: Some("7".into()),
        };
        run(&ctx(&server, true), ReviewCommand::Discard(arg())).unwrap();
        run(&ctx(&server, false), ReviewCommand::Discard(arg())).unwrap();
        delete.assert();
    }

    #[test]
    fn discard_without_a_pending_review_deletes_nothing() {
        let mut server = Server::new();
        mock_pending(&mut server, "alice", None);
        let delete = gql(&mut server, "deletePullRequestReview", json!({})).expect(0);
        let arg = || PrArg {
            pr: Some("7".into()),
        };
        run(&ctx(&server, true), ReviewCommand::Discard(arg())).unwrap();
        run(&ctx(&server, false), ReviewCommand::Discard(arg())).unwrap();
        delete.assert();
    }

    #[test]
    fn hunk_ranges_cover_both_sides() {
        let patch = "@@ -140,6 +140,7 @@ project(zlib)\n line\n+added\n@@ -960,6 +961,23 @@\n x";
        let (left, right) = hunk_ranges(patch);
        assert_eq!(left, vec![(140, 145), (960, 965)]);
        assert_eq!(right, vec![(140, 146), (961, 983)]);
    }

    #[test]
    fn hunk_ranges_skip_empty_sides_and_default_counts() {
        // New file, deleted file, and a one-line hunk with omitted counts.
        assert_eq!(hunk_ranges("@@ -0,0 +1,34 @@\n+a"), (vec![], vec![(1, 34)]));
        assert_eq!(hunk_ranges("@@ -1,10 +0,0 @@\n-a"), (vec![(1, 10)], vec![]));
        assert_eq!(
            hunk_ranges("@@ -5 +5 @@\n-a\n+b"),
            (vec![(5, 5)], vec![(5, 5)])
        );
    }

    #[test]
    fn hunk_header_ignores_content_lines() {
        // A removed line that happens to start with @@ is prefixed by -, so it never parses.
        assert_eq!(
            hunk_ranges("@@ -1,2 +1,2 @@\n-@@ -9,9 +9,9 @@\n+x"),
            (vec![(1, 2)], vec![(1, 2)])
        );
    }

    #[test]
    fn location_parses_lines_and_ranges() {
        assert_eq!(
            parse_location("src/a.c:42").unwrap(),
            ("src/a.c".into(), (42, 42))
        );
        assert_eq!(
            parse_location("src/a.c:40-42").unwrap(),
            ("src/a.c".into(), (40, 42))
        );
        assert_eq!(
            parse_location("C:/x.c:3").unwrap(),
            ("C:/x.c".into(), (3, 3))
        );
        assert!(parse_location("src/a.c").is_err());
        assert!(parse_location("src/a.c:0").is_err());
        assert!(parse_location("src/a.c:9-3").is_err());
        assert!(parse_location(":3").is_err());
        assert!(parse_location("https://github.com/o/r/pull/42").is_err());
    }

    fn files() -> Vec<FileLines> {
        vec![FileLines {
            path: "a.c".into(),
            right: vec![(10, 20), (40, 45)],
            left: vec![],
        }]
    }

    #[test]
    fn commentable_requires_one_hunk() {
        let f = files();
        assert!(check_commentable(&f, "a.c", Side::Right, (12, 18)).is_ok());
        assert!(check_commentable(&f, "a.c", Side::Right, (40, 40)).is_ok());
        // Spanning two hunks fails even though both ends are commentable.
        let err = check_commentable(&f, "a.c", Side::Right, (15, 42)).unwrap_err();
        assert!(err.to_string().contains("10-20, 40-45"), "{err}");
        assert!(check_commentable(&f, "a.c", Side::Left, (12, 12)).is_err());
        assert!(check_commentable(&f, "b.c", Side::Right, (12, 12)).is_err());
    }

    #[test]
    fn submit_plan_follows_github_body_rules() {
        use ReviewEvent::*;
        let pending = Some(("PRR_1", 2));
        let empty = Some(("PRR_1", 0));
        assert_eq!(
            plan_submit(Comment, None, pending).unwrap(),
            SubmitPlan::Submit("PRR_1".into())
        );
        assert_eq!(
            plan_submit(Approve, None, None).unwrap(),
            SubmitPlan::Create
        );
        assert_eq!(
            plan_submit(Comment, Some("lgtm"), None).unwrap(),
            SubmitPlan::Create
        );
        assert_eq!(
            plan_submit(RequestChanges, None, pending).unwrap(),
            SubmitPlan::Submit("PRR_1".into())
        );
        assert!(plan_submit(Comment, None, None).is_err());
        assert!(plan_submit(Comment, Some("  "), empty).is_err());
        assert!(plan_submit(RequestChanges, None, None).is_err());
        assert!(plan_submit(RequestChanges, None, empty).is_err());
    }

    #[test]
    fn location_formats() {
        assert_eq!(location("a.c", Some(3), Some(5)), "a.c:3-5");
        assert_eq!(location("a.c", Some(5), Some(5)), "a.c:5");
        assert_eq!(location("a.c", None, Some(5)), "a.c:5");
        assert_eq!(location("a.c", None, None), "a.c");
    }

    #[test]
    fn add_args_take_an_optional_pr_first() {
        let split = |a: &[&str]| {
            let owned: Vec<String> = a.iter().map(|s| s.to_string()).collect();
            split_add_args(&owned)
                .map(|(p, l, b)| (p.map(str::to_string), l.to_string(), b.map(str::to_string)))
        };
        let some = |s: &str| Some(s.to_string());
        assert_eq!(
            split(&["a.c:3"]).unwrap(),
            (None, "a.c:3".to_string(), None)
        );
        assert_eq!(
            split(&["a.c:3", "Fix"]).unwrap(),
            (None, "a.c:3".to_string(), some("Fix"))
        );
        assert_eq!(
            split(&["42", "a.c:3-5"]).unwrap(),
            (some("42"), "a.c:3-5".to_string(), None)
        );
        assert_eq!(
            split(&["https://github.com/o/r/pull/42", "a.c:3", "-"]).unwrap(),
            (
                some("https://github.com/o/r/pull/42"),
                "a.c:3".to_string(),
                some("-")
            )
        );
        // A body that looks like a location after the location is still the body.
        assert_eq!(
            split(&["a.c:3", "b.c:4"]).unwrap(),
            (None, "a.c:3".to_string(), some("b.c:4"))
        );
        assert!(split(&["42"]).is_err());
        assert!(split(&["42", "Fix"]).is_err());
        assert!(split(&["a.c:3", "Fix", "extra"]).is_err());
    }
}
