use anyhow::{Context, Result, bail};
use regex::Regex;
use serde::Serialize;
use std::sync::LazyLock;

use crate::api::Repo;
use crate::api::pulls::{self, RestComment, user_login};
use crate::cli::{CommentCommand, CommentEditArgs, CommentShowArgs};
use crate::context::Ctx;
use crate::output::{indent, print_json};
use crate::prompt;

pub fn run(ctx: &Ctx, cmd: CommentCommand) -> Result<()> {
    match cmd {
        CommentCommand::Show(args) => show(ctx, args),
        CommentCommand::Edit(args) => edit(ctx, args),
    }
}

/// Which endpoint a comment lives under.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// An inline comment on the diff, `pulls/comments/{id}`.
    ReviewComment,
    /// A comment on the conversation tab, `issues/comments/{id}`.
    IssueComment,
    /// The body of a submitted review, `pulls/{n}/reviews/{id}`.
    Review,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Kind::ReviewComment => "review_comment",
            Kind::IssueComment => "issue_comment",
            Kind::Review => "review",
        }
    }
}

/// A comment named by URL or bare ID.
#[derive(Debug, PartialEq, Eq)]
pub struct Target {
    /// Repository from the URL. Bare IDs use -R or the current repo.
    pub repo: Option<Repo>,
    /// PR or issue number from the URL.
    pub number: Option<u64>,
    /// Kind from the URL fragment. Bare IDs don't say.
    pub kind: Option<Kind>,
    pub id: u64,
}

/// Parse a comment ID, or a link such as `.../pull/42#discussion_r123`, `.../pull/42/files#r123`,
/// `.../pull/42#issuecomment-123`, or `.../pull/42#pullrequestreview-123`.
pub fn parse_target(s: &str) -> Result<Target> {
    static FRAGMENT: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"^(discussion_r|r|issuecomment-|pullrequestreview-)(\d+)$").unwrap()
    });

    let s = s.trim();
    if let Ok(id) = s.parse() {
        return Ok(Target {
            repo: None,
            number: None,
            kind: None,
            id,
        });
    }
    let bad = || anyhow::anyhow!("Expected a comment ID or comment URL, got {s:?}");
    let (url, fragment) = s.split_once('#').ok_or_else(bad)?;
    let path = url
        .split_once("://")
        .and_then(|(_, rest)| rest.split_once('/'))
        .map(|(_, path)| path)
        .ok_or_else(bad)?;
    let path = path.split('?').next().unwrap_or_default();
    let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
    let [owner, name, "pull" | "issues", number, ..] = parts.as_slice() else {
        return Err(bad());
    };
    let caps = FRAGMENT.captures(fragment).ok_or_else(bad)?;
    let kind = match &caps[1] {
        "issuecomment-" => Kind::IssueComment,
        "pullrequestreview-" => Kind::Review,
        _ => Kind::ReviewComment,
    };
    Ok(Target {
        repo: Some(Repo {
            owner: owner.to_string(),
            name: name.to_string(),
        }),
        number: Some(number.parse().map_err(|_| bad())?),
        kind: Some(kind),
        id: caps[2].parse().map_err(|_| bad())?,
    })
}

/// A comment fetched from whichever endpoint holds it.
pub struct Found {
    pub repo: Repo,
    pub kind: Kind,
    pub value: RestComment,
}

/// Fetch the comment a target names. A bare ID is tried as a review comment, then an issue
/// comment. Review IDs share no endpoint without the PR, so those need a URL.
pub fn find(ctx: &Ctx, target: &Target) -> Result<Found> {
    let repo = ctx.repo_or(target.repo.clone())?;
    let id = target.id;
    let found = |kind, value| Found {
        repo: repo.clone(),
        kind,
        value,
    };
    let client = &ctx.client;
    match target.kind {
        Some(Kind::ReviewComment) => Ok(found(
            Kind::ReviewComment,
            pulls::review_comment(client, &repo, id)?,
        )),
        Some(Kind::IssueComment) => Ok(found(
            Kind::IssueComment,
            pulls::issue_comment(client, &repo, id)?,
        )),
        Some(Kind::Review) => {
            let number = target
                .number
                .context("A review URL carries its PR number")?;
            Ok(found(
                Kind::Review,
                pulls::review(client, &repo, number, id)?,
            ))
        }
        None => {
            if let Some(v) = pulls::find_review_comment(client, &repo, id)? {
                return Ok(found(Kind::ReviewComment, v));
            }
            if let Some(v) = pulls::find_issue_comment(client, &repo, id)? {
                return Ok(found(Kind::IssueComment, v));
            }
            let hint = if ctx.repo_flag.is_none() {
                " Pass --repo OWNER/REPO if it lives elsewhere."
            } else {
                ""
            };
            bail!(
                "No review comment or issue comment {id} in {repo}. A review needs its URL \
                 (#pullrequestreview-{id}).{hint}"
            )
        }
    }
}

/// One comment in a shape shared by all three kinds.
#[derive(Debug, Serialize)]
struct Shown {
    kind: &'static str,
    id: u64,
    repo: String,
    /// PR or issue number.
    number: Option<u64>,
    author: String,
    created_at: Option<String>,
    url: String,
    body: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    state: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    line: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    start_line: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    side: Option<String>,
    /// Whether the line is from the commit the comment was made on, not the PR head.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    outdated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    in_reply_to: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    diff_hunk: Option<String>,
}

/// Last path segment of an API URL as a number, as in `.../pulls/42`.
pub(super) fn trailing_number(url: Option<&str>) -> Option<u64> {
    url?.rsplit('/').next()?.parse().ok()
}

fn normalize(found: &Found) -> Shown {
    let v = &found.value;
    let number = match found.kind {
        Kind::IssueComment => trailing_number(v.issue_url.as_deref()),
        _ => trailing_number(v.pull_request_url.as_deref()),
    };
    let created_at = match found.kind {
        Kind::Review => v.submitted_at.clone(),
        _ => v.created_at.clone(),
    };
    let mut shown = Shown {
        kind: found.kind.name(),
        id: v.id,
        repo: found.repo.to_string(),
        number,
        author: user_login(v.user.as_ref()),
        created_at,
        url: v.html_url.clone(),
        body: v.body.clone(),
        state: None,
        path: None,
        line: None,
        start_line: None,
        side: None,
        outdated: false,
        in_reply_to: None,
        diff_hunk: None,
    };
    match found.kind {
        Kind::Review => shown.state = v.state.clone(),
        Kind::ReviewComment => {
            // `line` is null once the comment no longer maps onto the PR head.
            shown.outdated = v.line.is_none();
            shown.line = v.line.or(v.original_line);
            shown.start_line = if shown.outdated {
                v.original_start_line
            } else {
                v.start_line
            };
            shown.path = v.path.clone();
            shown.side = v.side.clone();
            shown.in_reply_to = v.in_reply_to_id;
            shown.diff_hunk = v.diff_hunk.clone();
        }
        Kind::IssueComment => {}
    }
    shown
}

fn render(s: &Shown) -> String {
    static HTML_COMMENT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?s)<!--.*?-->").unwrap());

    let kind = s.kind.replace('_', " ");
    let mut head = format!("{kind} {}  {}", s.id, s.author);
    if let Some(at) = &s.created_at {
        head.push_str(&format!("  {at}"));
    }
    if let Some(state) = &s.state {
        head.push_str(&format!("  {}", state.to_lowercase()));
    }
    let mut out = format!("{head}\n{}\n", s.url);
    if let Some(path) = &s.path {
        let span = match (s.start_line, s.line) {
            (Some(a), Some(b)) if a != b => format!(":{a}-{b}"),
            (_, Some(b)) => format!(":{b}"),
            _ => String::new(),
        };
        let mut line = format!("{path}{span}");
        if let Some(side) = &s.side {
            line.push_str(&format!(" ({})", side.to_lowercase()));
        }
        if s.outdated {
            line.push_str(" outdated");
        }
        if let Some(id) = s.in_reply_to {
            line.push_str(&format!("  in reply to {id}"));
        }
        out.push_str(&line);
        out.push('\n');
    }
    if let Some(hunk) = &s.diff_hunk {
        out.push_str(&indent(hunk, "    "));
        out.push('\n');
    }
    let body = HTML_COMMENT.replace_all(&s.body, "");
    let body = body.replace("\r\n", "\n");
    let body = body.trim();
    if !body.is_empty() {
        out.push('\n');
        out.push_str(body);
        out.push('\n');
    }
    out
}

fn show(ctx: &Ctx, args: CommentShowArgs) -> Result<()> {
    let target = parse_target(&args.target)?;
    let shown = normalize(&find(ctx, &target)?);
    if ctx.json {
        return print_json(&shown);
    }
    print!("{}", render(&shown));
    Ok(())
}

/// Replace a found comment's body, returning the comment as GitHub now has it.
fn edit_found(ctx: &Ctx, found: &Found, body: &str) -> Result<RestComment> {
    let client = &ctx.client;
    let id = found.value.id;
    match found.kind {
        Kind::ReviewComment => pulls::edit_review_comment(client, &found.repo, id, body),
        Kind::IssueComment => pulls::edit_issue_comment(client, &found.repo, id, body),
        Kind::Review => {
            let number = review_number(found)?;
            pulls::edit_review(client, &found.repo, number, id, body)
        }
    }
}

/// The PR a found review belongs to.
fn review_number(found: &Found) -> Result<u64> {
    trailing_number(found.value.pull_request_url.as_deref()).context("Review has no pull request")
}

fn edit(ctx: &Ctx, args: CommentEditArgs) -> Result<()> {
    let body = prompt::read_body(args.body.as_deref(), "comment")?;
    let target = parse_target(&args.target)?;
    let found = find(ctx, &target)?;
    let resp = edit_found(ctx, &found, &body)?;
    let edited = normalize(&Found {
        repo: found.repo,
        kind: found.kind,
        value: resp,
    });
    if ctx.json {
        return print_json(&edited);
    }
    println!("{}", edited.url);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn value(v: Value) -> RestComment {
        serde_json::from_value(v).unwrap()
    }

    fn repo() -> Option<Repo> {
        Some(Repo::parse("zlib-ng/zlib-ng").unwrap())
    }

    #[test]
    fn parses_ids_and_comment_urls() {
        assert_eq!(
            parse_target("123").unwrap(),
            Target {
                repo: None,
                number: None,
                kind: None,
                id: 123
            }
        );
        let base = "https://github.com/zlib-ng/zlib-ng/pull/42";
        let cases = [
            (format!("{base}#discussion_r7"), Kind::ReviewComment),
            (format!("{base}/files#r7"), Kind::ReviewComment),
            (format!("{base}/changes#r7"), Kind::ReviewComment),
            (format!("{base}#issuecomment-7"), Kind::IssueComment),
            (format!("{base}#pullrequestreview-7"), Kind::Review),
            (
                "https://github.com/zlib-ng/zlib-ng/issues/42#issuecomment-7".to_string(),
                Kind::IssueComment,
            ),
        ];
        for (url, kind) in cases {
            assert_eq!(
                parse_target(&url).unwrap(),
                Target {
                    repo: repo(),
                    number: Some(42),
                    kind: Some(kind),
                    id: 7
                },
                "{url}"
            );
        }
    }

    #[test]
    fn rejects_links_without_a_comment() {
        assert!(parse_target("https://github.com/a/b/pull/42").is_err());
        assert!(parse_target("https://github.com/a/b/pull/42#top").is_err());
        assert!(parse_target("https://github.com/a/b#discussion_r1").is_err());
        assert!(parse_target("PRRT_abc").is_err());
    }

    #[test]
    fn review_comment_renders_location_and_hunk() {
        let found = Found {
            repo: repo().unwrap(),
            kind: Kind::ReviewComment,
            value: value(json!({
                "id": 7, "user": { "login": "alice" }, "created_at": "2026-01-02T03:04:05Z",
                "html_url": "https://github.com/zlib-ng/zlib-ng/pull/42#discussion_r7",
                "pull_request_url": "https://api.github.com/repos/zlib-ng/zlib-ng/pulls/42",
                "body": "Rename this.<!-- fingerprint -->\r\n",
                "path": "deflate.c", "line": 12, "start_line": 10, "side": "RIGHT",
                "original_line": 11, "in_reply_to_id": 6,
                "diff_hunk": "@@ -1,2 +1,3 @@\n a\n+b"
            })),
        };
        let shown = normalize(&found);
        assert_eq!(shown.number, Some(42));
        assert_eq!(
            render(&shown),
            "review comment 7  alice  2026-01-02T03:04:05Z\n\
             https://github.com/zlib-ng/zlib-ng/pull/42#discussion_r7\n\
             deflate.c:10-12 (right)  in reply to 6\n    \
             @@ -1,2 +1,3 @@\n     \
             a\n    \
             +b\n\
             \n\
             Rename this.\n"
        );
    }

    #[test]
    fn outdated_comment_uses_original_lines() {
        let found = Found {
            repo: repo().unwrap(),
            kind: Kind::ReviewComment,
            value: value(json!({
                "id": 7, "user": { "login": "alice" }, "path": "a.c",
                "line": null, "start_line": null, "original_line": 5, "original_start_line": 3
            })),
        };
        let shown = normalize(&found);
        assert!(shown.outdated);
        assert_eq!((shown.start_line, shown.line), (Some(3), Some(5)));
    }

    #[test]
    fn review_number_from_pull_request_url() {
        let found = |v| Found {
            repo: repo().unwrap(),
            kind: Kind::Review,
            value: value(v),
        };
        let pr = "https://api.github.com/repos/zlib-ng/zlib-ng/pulls/42";
        assert_eq!(
            review_number(&found(json!({ "id": 9, "pull_request_url": pr }))).unwrap(),
            42
        );
        assert!(review_number(&found(json!({ "id": 9 }))).is_err());
    }

    #[test]
    fn review_and_issue_comment_numbers() {
        let review = normalize(&Found {
            repo: repo().unwrap(),
            kind: Kind::Review,
            value: value(json!({
                "id": 9, "user": { "login": "bob" }, "state": "CHANGES_REQUESTED",
                "submitted_at": "2026-01-02T03:04:05Z", "body": "See inline.",
                "pull_request_url": "https://api.github.com/repos/zlib-ng/zlib-ng/pulls/42"
            })),
        });
        assert_eq!(review.number, Some(42));
        assert_eq!(review.created_at.as_deref(), Some("2026-01-02T03:04:05Z"));
        assert!(
            render(&review).starts_with("review 9  bob  2026-01-02T03:04:05Z  changes_requested\n")
        );

        let issue = normalize(&Found {
            repo: repo().unwrap(),
            kind: Kind::IssueComment,
            value: value(json!({
                "id": 8, "user": { "login": "carol" },
                "issue_url": "https://api.github.com/repos/zlib-ng/zlib-ng/issues/42"
            })),
        });
        assert_eq!(issue.number, Some(42));
        assert_eq!(issue.path, None);
    }
}
