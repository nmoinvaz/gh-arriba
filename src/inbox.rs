//! Tidies the GitHub notifications inbox.

use anyhow::Result;
use serde::Serialize;

use crate::api::notifications::{self, Thread};
use crate::api::{Client, Repo};
use crate::cli::{InboxCleanArgs, InboxCommand};
use crate::context::Ctx;
use crate::parallel;

pub fn run(ctx: &Ctx, cmd: InboxCommand) -> Result<()> {
    match cmd {
        InboxCommand::Clean(args) => clean(ctx, &args),
    }
}

/// Why a thread needs no one anymore.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
enum Finished {
    Merged,
    Closed,
    /// Its repository was deleted or is out of reach, so the thread can never update.
    Gone,
}

impl Finished {
    fn label(self) -> &'static str {
        match self {
            Self::Merged => "merged",
            Self::Closed => "closed",
            Self::Gone => "gone",
        }
    }
}

/// One line of output per finished thread.
#[derive(Serialize)]
struct Line<'a> {
    state: Finished,
    repo: &'a str,
    kind: &'a str,
    title: &'a str,
    url: String,
    done: bool,
}

/// Mark done every thread whose pull request or issue is finished, printing each.
fn clean(ctx: &Ctx, args: &InboxCleanArgs) -> Result<()> {
    let repo = ctx.repo_flag.as_deref().map(Repo::parse).transpose()?;
    let threads = notifications::inbox(&ctx.client, repo.as_ref())?;
    let states = parallel::map(&threads, |t| finished(&ctx.client, t));

    let mut count = 0;
    for (thread, state) in threads.iter().zip(states) {
        let state = match state {
            Ok(Some(state)) => state,
            Ok(None) => continue,
            // One subject that cannot be read should not keep the rest from being cleaned.
            Err(e) => {
                eprintln!("warning: {}: {e:#}", thread.subject.title);
                continue;
            }
        };
        if !args.dry_run {
            notifications::mark_done(&ctx.client, &thread.id)?;
        }
        count += 1;
        let url = thread.html_url();
        if ctx.json {
            let line = Line {
                state,
                repo: &thread.repository.full_name,
                kind: &thread.subject.kind,
                title: &thread.subject.title,
                url,
                done: !args.dry_run,
            };
            println!("{}", serde_json::to_string(&line)?);
        } else {
            println!(
                "{:<7}{} {}  {url}",
                state.label(),
                thread.repository.full_name,
                thread.subject.title
            );
        }
    }

    let summary = match (count, args.dry_run) {
        (0, _) => "No finished threads in the inbox".to_string(),
        (n, true) => format!("{n} would be marked done"),
        (n, false) => format!("Marked {n} done"),
    };
    eprintln!("{summary}");
    Ok(())
}

/// Whether the thread's pull request or issue merged, closed, or went away. Other subjects, such
/// as releases and workflow runs, have no state to finish.
fn finished(client: &Client, thread: &Thread) -> Result<Option<Finished>> {
    if !matches!(thread.subject.kind.as_str(), "PullRequest" | "Issue") {
        return Ok(None);
    }
    let Some(url) = &thread.subject.url else {
        return Ok(None);
    };
    let Some(subject) = client.get_opt(url)? else {
        return Ok(Some(Finished::Gone));
    };
    Ok(if !subject["merged_at"].is_null() {
        Some(Finished::Merged)
    } else if subject["state"] == "closed" {
        Some(Finished::Closed)
    } else {
        None
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use mockito::{Mock, Server, ServerGuard};
    use serde_json::{Value, json};

    fn ctx(server: &Server) -> Ctx {
        Ctx {
            client: Client::for_tests(&server.url()),
            repo_flag: None,
            json: false,
            yes: false,
        }
    }

    fn thread(server: &Server, id: &str, kind: &str, path: Option<&str>) -> Value {
        json!({
            "id": id, "reason": "subscribed", "updated_at": "2026-01-01T00:00:00Z",
            "subject": {
                "title": format!("Thread {id}"),
                "url": path.map(|p| format!("{}{p}", server.url())),
                "type": kind,
            },
            "repository": {"full_name": "o/r", "html_url": "https://github.com/o/r"},
        })
    }

    fn subject(server: &mut ServerGuard, path: &str, body: Option<Value>) {
        let mock = server.mock("GET", path);
        match body {
            Some(b) => mock.with_body(b.to_string()),
            None => mock.with_status(404),
        }
        .create();
    }

    /// An inbox of a merged PR, a closed issue, a thread from a deleted repo, an open PR, and a
    /// release, which has no state.
    fn inbox(server: &mut ServerGuard) {
        let threads = json!([
            thread(server, "1", "PullRequest", Some("/repos/o/r/pulls/1")),
            thread(server, "2", "Issue", Some("/repos/o/r/issues/2")),
            thread(server, "3", "PullRequest", Some("/repos/gone/r/pulls/3")),
            thread(server, "4", "PullRequest", Some("/repos/o/r/pulls/4")),
            thread(server, "5", "Release", Some("/repos/o/r/releases/5")),
        ]);
        server
            .mock("GET", "/notifications")
            .match_query(mockito::Matcher::UrlEncoded("all".into(), "true".into()))
            .with_body(threads.to_string())
            .create();
        subject(
            server,
            "/repos/o/r/pulls/1",
            Some(json!({"state": "closed", "merged_at": "2026-01-02T00:00:00Z"})),
        );
        subject(
            server,
            "/repos/o/r/issues/2",
            Some(json!({"state": "closed"})),
        );
        subject(server, "/repos/gone/r/pulls/3", None);
        subject(
            server,
            "/repos/o/r/pulls/4",
            Some(json!({"state": "open", "merged_at": null})),
        );
    }

    fn done(server: &mut ServerGuard, id: &str, times: usize) -> Mock {
        server
            .mock("DELETE", format!("/notifications/threads/{id}").as_str())
            .with_status(204)
            .expect(times)
            .create()
    }

    #[test]
    fn marks_done_the_threads_whose_subject_finished() {
        let mut server = Server::new();
        inbox(&mut server);
        let finished = ["1", "2", "3"].map(|id| done(&mut server, id, 1));
        let left = ["4", "5"].map(|id| done(&mut server, id, 0));
        clean(&ctx(&server), &InboxCleanArgs { dry_run: false }).unwrap();
        finished.iter().chain(&left).for_each(Mock::assert);
    }

    #[test]
    fn dry_runs_mark_nothing() {
        let mut server = Server::new();
        inbox(&mut server);
        let none = ["1", "2", "3", "4", "5"].map(|id| done(&mut server, id, 0));
        clean(&ctx(&server), &InboxCleanArgs { dry_run: true }).unwrap();
        none.iter().for_each(Mock::assert);
    }

    #[test]
    fn subjects_finish_by_merge_close_or_absence() {
        let mut server = Server::new();
        inbox(&mut server);
        let client = Client::for_tests(&server.url());
        let threads: Vec<Thread> = notifications::inbox(&client, None).unwrap();
        let states: Vec<_> = threads
            .iter()
            .map(|t| finished(&client, t).unwrap())
            .collect();
        assert_eq!(
            states,
            [
                Some(Finished::Merged),
                Some(Finished::Closed),
                Some(Finished::Gone),
                None,
                None
            ]
        );
    }
}
