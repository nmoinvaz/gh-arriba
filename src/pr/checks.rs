use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde_json::Value;
use std::collections::HashSet;
use std::time::{Duration, Instant};

use crate::cli::ChecksArgs;
use crate::context::{Ctx, PrRef, git};
use crate::github::{Client, is_failed};
use crate::output;
use crate::pr::model::str_of;
use crate::pr::rollup::{self, CheckRun, latest};

/// Outcome of one check, folding check-run conclusions and commit-status states together.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
enum State {
    Pass,
    Fail,
    Pending,
    Skipped,
}

impl State {
    fn label(self) -> &'static str {
        match self {
            State::Pass => "pass",
            State::Fail => "fail",
            State::Pending => "pending",
            State::Skipped => "skipped",
        }
    }

    /// State of an Actions or app check run.
    fn of_run(run: &CheckRun) -> Self {
        if run.status != "COMPLETED" {
            return State::Pending;
        }
        match run.conclusion.as_deref() {
            Some("SUCCESS") => State::Pass,
            // Waiting on a maintainer to approve a fork's workflow blocks the PR like a failure.
            Some("ACTION_REQUIRED") => State::Fail,
            c if is_failed(c) => State::Fail,
            _ => State::Skipped,
        }
    }

    /// State of a commit status from an external service.
    fn of_status(state: &str) -> Self {
        match state {
            "SUCCESS" => State::Pass,
            "FAILURE" | "ERROR" => State::Fail,
            _ => State::Pending,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
struct Check {
    /// `Workflow / job` for Actions, the check or status name otherwise.
    name: String,
    state: State,
    url: Option<String>,
    /// Run time of a completed check run. Commit statuses carry none.
    #[serde(skip_serializing_if = "Option::is_none")]
    seconds: Option<i64>,
}

impl Check {
    /// Identity across polls. Names repeat when a workflow runs for several events.
    fn key(&self) -> String {
        format!("{}\t{}", self.name, self.url.as_deref().unwrap_or_default())
    }
}

#[derive(Debug, Default, PartialEq, Serialize)]
struct Counts {
    pass: usize,
    fail: usize,
    pending: usize,
    skipped: usize,
}

#[derive(Debug, Serialize)]
struct PrStatus {
    repo: String,
    number: u64,
    title: String,
    url: String,
    /// open, draft, closed, or merged.
    state: String,
    /// Merge state, such as clean, blocked, behind, conflicting, or unknown.
    mergeable: String,
    review: Option<String>,
    head_oid: String,
    commits: u64,
    counts: Counts,
    checks: Vec<Check>,
}

/// PR fields `pr checks` reports alongside the checks.
const STATUS_FIELDS: &str = "title url state isDraft mergeable mergeStateStatus reviewDecision headRefOid commits { totalCount }";

fn fetch_status(client: &Client, pr: &PrRef) -> Result<PrStatus> {
    let rollup = rollup::fetch(client, pr, STATUS_FIELDS)?;
    Ok(build(pr, &rollup.pr, rollup.runs, &rollup.statuses))
}

/// Fold the PR fields and its head's checks into one status.
fn build(pr: &PrRef, p: &Value, runs: Vec<CheckRun>, statuses: &[Value]) -> PrStatus {
    let mut checks: Vec<Check> = latest(runs)
        .iter()
        .map(|r| Check {
            name: if r.workflow.is_empty() {
                r.name.clone()
            } else {
                format!("{} / {}", r.workflow, r.name)
            },
            state: State::of_run(r),
            url: r.url.clone(),
            seconds: r
                .started_at
                .zip(r.completed_at)
                .map(|(start, end)| (end - start).num_seconds()),
        })
        .collect();
    checks.extend(statuses.iter().map(|s| Check {
        name: str_of(s, "context"),
        state: State::of_status(s["state"].as_str().unwrap_or_default()),
        url: s["targetUrl"].as_str().map(str::to_string),
        seconds: None,
    }));

    let mut counts = Counts::default();
    for c in &checks {
        match c.state {
            State::Pass => counts.pass += 1,
            State::Fail => counts.fail += 1,
            State::Pending => counts.pending += 1,
            State::Skipped => counts.skipped += 1,
        }
    }

    let state = match p["state"].as_str() {
        Some("OPEN") if p["isDraft"].as_bool() == Some(true) => "draft".to_string(),
        Some(s) => s.to_lowercase(),
        None => "unknown".to_string(),
    };
    PrStatus {
        repo: pr.repo.to_string(),
        number: pr.number,
        title: str_of(p, "title"),
        url: str_of(p, "url"),
        state,
        mergeable: merge_state(p),
        review: p["reviewDecision"].as_str().map(|r| r.to_lowercase()),
        head_oid: str_of(p, "headRefOid"),
        commits: p["commits"]["totalCount"].as_u64().unwrap_or(0),
        counts,
        checks,
    }
}

/// Merge state in plain words. GitHub calls a conflicting PR "dirty".
fn merge_state(p: &Value) -> String {
    if p["mergeable"].as_str() == Some("CONFLICTING") {
        return "conflicting".to_string();
    }
    match p["mergeStateStatus"].as_str() {
        Some("DIRTY") => "conflicting".to_string(),
        Some(s) => s.to_lowercase(),
        None => "unknown".to_string(),
    }
}

/// One summary line, then the failing and pending checks indented below it.
fn render(status: &PrStatus, full_ref: bool) -> String {
    let id = if full_ref {
        format!("{}#{}", status.repo, status.number)
    } else {
        format!("#{}", status.number)
    };
    let head: String = status.head_oid.chars().take(8).collect();
    let plural = if status.commits == 1 { "" } else { "s" };
    let mut line = format!(
        "{id} {}  {}  mergeable={}  head={head} ({} commit{plural})",
        status.title, status.state, status.mergeable, status.commits
    );
    if let Some(review) = &status.review {
        line.push_str(&format!("  review={review}"));
    }
    line.push_str(&format!("  checks: {}", counts_text(&status.counts)));

    let mut out = line + "\n";
    for state in [State::Fail, State::Pending] {
        for c in status.checks.iter().filter(|c| c.state == state) {
            let url = c.url.as_deref().unwrap_or_default();
            let row = format!("  {:<8} {}  {url}", state.label(), c.name);
            out.push_str(row.trim_end());
            out.push('\n');
        }
    }
    out
}

fn counts_text(c: &Counts) -> String {
    if *c == Counts::default() {
        return "none".to_string();
    }
    let mut parts = vec![
        format!("{} pass", c.pass),
        format!("{} fail", c.fail),
        format!("{} pending", c.pending),
    ];
    if c.skipped > 0 {
        parts.push(format!("{} skipped", c.skipped));
    }
    parts.join(", ")
}

pub fn run(ctx: &Ctx, args: ChecksArgs) -> Result<()> {
    let prs = resolve_prs(ctx, &args.prs)?;
    let deadline = Instant::now() + Duration::from_secs(args.timeout * 60);
    if let Some(sha) = &args.sha {
        let [pr] = prs.as_slice() else {
            bail!("--sha takes a single PR");
        };
        let status = await_head(ctx, pr, &resolve_sha(sha)?, &args, deadline)?;
        if !args.wait {
            return report(ctx, &[status]);
        }
    } else if !args.wait {
        let statuses = fetch_all(&ctx.client, &prs)?;
        return report(ctx, &statuses);
    }
    wait(ctx, &prs, &args, deadline)
}

fn fetch_all(client: &Client, prs: &[PrRef]) -> Result<Vec<PrStatus>> {
    prs.iter().map(|pr| fetch_status(client, pr)).collect()
}

/// Each PR argument, or the current branch's PR when there are none.
fn resolve_prs(ctx: &Ctx, args: &[String]) -> Result<Vec<PrRef>> {
    if args.is_empty() {
        return Ok(vec![ctx.pr(None)?]);
    }
    args.iter().map(|a| ctx.pr(Some(a))).collect()
}

/// Print the statuses, failing when any check failed so scripts can branch on the exit code.
fn report(ctx: &Ctx, statuses: &[PrStatus]) -> Result<()> {
    print_statuses(ctx, statuses)?;
    fail_on_failures(statuses)
}

fn print_statuses(ctx: &Ctx, statuses: &[PrStatus]) -> Result<()> {
    if ctx.json {
        return output::print_json(statuses);
    }
    let full_ref = spans_repos(statuses);
    for s in statuses {
        print!("{}", render(s, full_ref));
    }
    Ok(())
}

/// Whether the PRs live in more than one repository, so `#N` alone would be ambiguous.
fn spans_repos(statuses: &[PrStatus]) -> bool {
    let repos: HashSet<&str> = statuses.iter().map(|s| s.repo.as_str()).collect();
    repos.len() > 1
}

fn fail_on_failures(statuses: &[PrStatus]) -> Result<()> {
    let failed: usize = statuses.iter().map(|s| s.counts.fail).sum();
    if failed > 0 {
        let plural = if failed == 1 { "" } else { "s" };
        bail!("{failed} check{plural} failed");
    }
    Ok(())
}

/// How long a PR with no checks, or with unknown mergeability, still counts as settling. Checks
/// take a moment to register after a push, and GitHub computes mergeability lazily.
const SETTLE: Duration = Duration::from_secs(120);

/// What one PR looked like on the previous poll.
struct Seen {
    head: String,
    /// When this head was first seen, so a new push restarts the settle window.
    since: Instant,
    /// Keys of checks already reported complete.
    done: HashSet<String>,
}

/// Poll until every PR has settled, printing checks as they complete.
fn wait(ctx: &Ctx, prs: &[PrRef], args: &ChecksArgs, deadline: Instant) -> Result<()> {
    let mut seen: Vec<Option<Seen>> = prs.iter().map(|_| None).collect();
    loop {
        let statuses = fetch_all(&ctx.client, prs)?;
        let full_ref = spans_repos(&statuses);
        let mut settled = true;
        for (status, seen) in statuses.iter().zip(seen.iter_mut()) {
            for line in progress(status, seen, full_ref, prs.len() > 1) {
                // With --json, stdout carries only the final document.
                if ctx.json {
                    eprintln!("{line}");
                } else {
                    println!("{line}");
                }
            }
            let since = seen.as_ref().map_or_else(Instant::now, |s| s.since);
            settled &= is_settled(status, since.elapsed());
        }

        let failed = statuses.iter().any(|s| s.counts.fail > 0);
        if settled || (args.fail_fast && failed) {
            return report(ctx, &statuses);
        }
        if Instant::now() >= deadline {
            print_statuses(ctx, &statuses)?;
            fail_on_failures(&statuses)?;
            let pending: usize = statuses.iter().map(|s| s.counts.pending).sum();
            bail!(
                "Timed out after {} minutes with {pending} checks pending",
                args.timeout
            );
        }
        std::thread::sleep(Duration::from_secs(args.interval));
    }
}

/// Poll until the PR head is `sha`, since GitHub takes a moment to see a push.
fn await_head(
    ctx: &Ctx,
    pr: &PrRef,
    sha: &str,
    args: &ChecksArgs,
    deadline: Instant,
) -> Result<PrStatus> {
    let short: String = sha.chars().take(8).collect();
    let mut announced = false;
    loop {
        let status = fetch_status(&ctx.client, pr)?;
        if status.head_oid.starts_with(sha) {
            return Ok(status);
        }
        let head: String = status.head_oid.chars().take(8).collect();
        if Instant::now() >= deadline {
            bail!(
                "{pr} head is still {head} after {} minutes, not {short}",
                args.timeout
            );
        }
        if !announced {
            let line = format!("#{} head is {head}, waiting for {short}", pr.number);
            if ctx.json {
                eprintln!("{line}");
            } else {
                println!("{line}");
            }
            announced = true;
        }
        std::thread::sleep(Duration::from_secs(args.interval));
    }
}

/// A full or abbreviated commit hash as given, or a revision such as HEAD resolved locally.
fn resolve_sha(rev: &str) -> Result<String> {
    if let Some(sha) = as_sha(rev) {
        return Ok(sha);
    }
    let full = git(&["rev-parse", "--verify", &format!("{rev}^{{commit}}")])
        .with_context(|| format!("{rev:?} is neither a commit hash nor a local revision"))?;
    Ok(full.to_lowercase())
}

/// Lowercased hash when `s` looks like one, at least 4 hex digits as git abbreviates.
fn as_sha(s: &str) -> Option<String> {
    let s = s.trim();
    ((4..=40).contains(&s.len()) && s.chars().all(|c| c.is_ascii_hexdigit()))
        .then(|| s.to_lowercase())
}

/// Lines for checks that completed since the last poll. The first poll of a head only notes
/// what is already done, and says how much is left.
fn progress(status: &PrStatus, seen: &mut Option<Seen>, full_ref: bool, tag: bool) -> Vec<String> {
    let id = if full_ref {
        format!("{}#{}", status.repo, status.number)
    } else {
        format!("#{}", status.number)
    };
    // A lone PR's completions need no prefix, several PRs' do.
    let tag = if tag { format!("{id} ") } else { String::new() };
    let done: HashSet<String> = status
        .checks
        .iter()
        .filter(|c| c.state != State::Pending)
        .map(Check::key)
        .collect();

    let Some(prev) = seen.as_mut().filter(|s| s.head == status.head_oid) else {
        let head: String = status.head_oid.chars().take(8).collect();
        let verb = if seen.is_some() { "moved to" } else { "is" };
        *seen = Some(Seen {
            head: status.head_oid.clone(),
            since: Instant::now(),
            done,
        });
        let left = if status.checks.is_empty() {
            "no checks yet".to_string()
        } else {
            format!("{} checks pending", status.counts.pending)
        };
        return vec![format!("{id} head {verb} {head}, {left}")];
    };

    let mut lines = Vec::new();
    for c in &status.checks {
        if c.state != State::Pending && !prev.done.contains(&c.key()) {
            let took = c
                .seconds
                .map(|s| format!(" ({})", duration(s)))
                .unwrap_or_default();
            lines.push(format!("{tag}{:<8} {}{took}", c.state.label(), c.name));
        }
    }
    // A rerun turns a completed check pending again, so it reports once more when it finishes.
    prev.done = done;
    lines
}

/// Done when nothing is pending, unless the PR is still settling after a push.
fn is_settled(status: &PrStatus, waited: Duration) -> bool {
    if status.counts.pending > 0 {
        return false;
    }
    if waited >= SETTLE {
        return true;
    }
    let open = matches!(status.state.as_str(), "open" | "draft");
    !status.checks.is_empty() && !(open && status.mergeable == "unknown")
}

/// Compact duration such as `45s`, `4m12s`, or `1h3m`.
fn duration(seconds: i64) -> String {
    let s = seconds.max(0);
    match (s / 3600, s / 60 % 60, s % 60) {
        (0, 0, sec) => format!("{sec}s"),
        (0, min, sec) => format!("{min}m{sec}s"),
        (h, min, _) => format!("{h}h{min}m"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::Repo;
    use crate::pr::rollup::parse_check_run;
    use serde_json::json;

    fn pr() -> PrRef {
        PrRef {
            repo: Repo::parse("zlib-ng/zlib-ng").unwrap(),
            number: 42,
        }
    }

    fn run_node(name: &str, workflow: &str, status: &str, conclusion: Option<&str>) -> Value {
        json!({
            "__typename": "CheckRun",
            "name": name, "status": status, "conclusion": conclusion,
            "startedAt": "2026-01-01T00:00:00Z",
            "detailsUrl": format!("https://github.com/zlib-ng/zlib-ng/actions/runs/1/job/{name}"),
            "checkSuite": { "workflowRun": {
                "databaseId": 1, "event": "pull_request", "workflow": { "name": workflow }
            }}
        })
    }

    fn header() -> Value {
        json!({
            "title": "Fix thing", "url": "https://github.com/zlib-ng/zlib-ng/pull/42",
            "state": "OPEN", "isDraft": false, "mergeable": "MERGEABLE",
            "mergeStateStatus": "BLOCKED", "reviewDecision": "REVIEW_REQUIRED",
            "headRefOid": "1a2b3c4d5e6f", "commits": { "totalCount": 3 }
        })
    }

    #[test]
    fn states_fold_runs_and_statuses() {
        let runs = [
            run_node("Ubuntu", "Build", "COMPLETED", Some("SUCCESS")),
            run_node("macOS", "Build", "COMPLETED", Some("FAILURE")),
            run_node("Docs", "Docs", "IN_PROGRESS", None),
            run_node("Fuzz", "Fuzz", "COMPLETED", Some("SKIPPED")),
            run_node("Lint", "Lint", "COMPLETED", Some("NEUTRAL")),
            run_node("Fork", "CI", "COMPLETED", Some("ACTION_REQUIRED")),
        ]
        .iter()
        .map(parse_check_run)
        .collect();
        let statuses = [
            json!({"context": "ci/jenkins", "state": "ERROR", "targetUrl": "https://ci"}),
            json!({"context": "codecov", "state": "PENDING", "targetUrl": null}),
            json!({"context": "license", "state": "SUCCESS", "targetUrl": null}),
        ];
        let status = build(&pr(), &header(), runs, &statuses);
        assert_eq!(
            status.counts,
            Counts {
                pass: 2,
                fail: 3,
                pending: 2,
                skipped: 2
            }
        );
        assert_eq!(status.state, "open");
        assert_eq!(status.mergeable, "blocked");
        assert_eq!(status.review.as_deref(), Some("review_required"));
    }

    #[test]
    fn render_lists_failing_then_pending() {
        let runs = [
            run_node("Ubuntu", "Build", "COMPLETED", Some("SUCCESS")),
            run_node("macOS", "Build", "COMPLETED", Some("FAILURE")),
            run_node("Docs", "Docs", "QUEUED", None),
        ]
        .iter()
        .map(parse_check_run)
        .collect();
        let status = build(&pr(), &header(), runs, &[]);
        assert_eq!(
            render(&status, false),
            "#42 Fix thing  open  mergeable=blocked  head=1a2b3c4d (3 commits)  review=review_required  checks: 1 pass, 1 fail, 1 pending\n  \
             fail     Build / macOS  https://github.com/zlib-ng/zlib-ng/actions/runs/1/job/macOS\n  \
             pending  Docs / Docs  https://github.com/zlib-ng/zlib-ng/actions/runs/1/job/Docs\n"
        );
    }

    #[test]
    fn draft_conflicting_without_checks() {
        let mut p = header();
        p["isDraft"] = json!(true);
        p["mergeable"] = json!("CONFLICTING");
        p["mergeStateStatus"] = json!("DIRTY");
        p["reviewDecision"] = Value::Null;
        p["commits"]["totalCount"] = json!(1);
        let status = build(&pr(), &p, Vec::new(), &[]);
        assert_eq!(
            render(&status, true),
            "zlib-ng/zlib-ng#42 Fix thing  draft  mergeable=conflicting  head=1a2b3c4d (1 commit)  checks: none\n"
        );
    }

    #[test]
    fn rerun_supersedes_failed_attempt() {
        let mut failed = run_node("Ubuntu", "Build", "COMPLETED", Some("FAILURE"));
        failed["startedAt"] = json!("2026-01-01T00:00:00Z");
        let mut passed = run_node("Ubuntu", "Build", "COMPLETED", Some("SUCCESS"));
        passed["startedAt"] = json!("2026-01-01T01:00:00Z");
        let runs = [failed, passed].iter().map(parse_check_run).collect();
        let status = build(&pr(), &header(), runs, &[]);
        assert_eq!(status.counts.fail, 0);
        assert_eq!(status.counts.pass, 1);
    }

    fn status_of(nodes: &[Value]) -> PrStatus {
        build(
            &pr(),
            &header(),
            nodes.iter().map(parse_check_run).collect(),
            &[],
        )
    }

    #[test]
    fn progress_reports_each_completion_once() {
        let mut seen = None;
        let running = [
            run_node("Ubuntu", "Build", "IN_PROGRESS", None),
            run_node("macOS", "Build", "COMPLETED", Some("SUCCESS")),
        ];
        assert_eq!(
            progress(&status_of(&running), &mut seen, false, false),
            vec!["#42 head is 1a2b3c4d, 1 checks pending"]
        );

        let mut ubuntu = run_node("Ubuntu", "Build", "COMPLETED", Some("FAILURE"));
        ubuntu["completedAt"] = json!("2026-01-01T00:04:12Z");
        let finished = [
            ubuntu,
            run_node("macOS", "Build", "COMPLETED", Some("SUCCESS")),
        ];
        assert_eq!(
            progress(&status_of(&finished), &mut seen, false, false),
            vec!["fail     Build / Ubuntu (4m12s)"]
        );
        assert!(progress(&status_of(&finished), &mut seen, false, true).is_empty());

        // A push resets what has been reported.
        let mut pushed = status_of(&finished);
        pushed.head_oid = "ffff0000aaaa".to_string();
        assert_eq!(
            progress(&pushed, &mut seen, false, true),
            vec!["#42 head moved to ffff0000, 0 checks pending"]
        );
    }

    #[test]
    fn settling_waits_for_checks_and_mergeability() {
        let done = status_of(&[run_node("Ubuntu", "Build", "COMPLETED", Some("SUCCESS"))]);
        assert!(is_settled(&done, Duration::ZERO));

        let pending = status_of(&[run_node("Ubuntu", "Build", "QUEUED", None)]);
        assert!(!is_settled(&pending, SETTLE * 10));

        let empty = status_of(&[]);
        assert!(!is_settled(&empty, Duration::ZERO));
        assert!(is_settled(&empty, SETTLE));

        let mut unknown = status_of(&[run_node("Ubuntu", "Build", "COMPLETED", Some("SUCCESS"))]);
        unknown.mergeable = "unknown".to_string();
        assert!(!is_settled(&unknown, Duration::ZERO));
        unknown.state = "merged".to_string();
        assert!(is_settled(&unknown, Duration::ZERO));
    }

    #[test]
    fn sha_detection() {
        assert_eq!(as_sha("1A2B3C4D").as_deref(), Some("1a2b3c4d"));
        assert_eq!(as_sha("abc"), None);
        assert_eq!(as_sha("HEAD"), None);
        assert_eq!(as_sha(&"a".repeat(41)), None);
    }

    #[test]
    fn durations_are_compact() {
        assert_eq!(duration(45), "45s");
        assert_eq!(duration(252), "4m12s");
        assert_eq!(duration(3780), "1h3m");
        assert_eq!(duration(-3), "0s");
    }
}
