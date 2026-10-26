//! The authenticated user's notification inbox.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::{Client, Repo};

/// One notification thread: the latest activity on an issue, PR, release, or run.
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
pub struct Thread {
    pub id: String,
    /// Why it was sent, such as review_requested, mention, or ci_activity.
    pub reason: String,
    pub updated_at: DateTime<Utc>,
    pub subject: Subject,
    pub repository: ThreadRepo,
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
pub struct Subject {
    pub title: String,
    /// The API URL of the issue, PR, commit, or release, absent for runs and discussions.
    pub url: Option<String>,
    /// Issue, PullRequest, Commit, Release, CheckSuite, Discussion, and so on.
    #[serde(rename = "type")]
    pub kind: String,
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
pub struct ThreadRepo {
    pub full_name: String,
    pub html_url: String,
}

impl Thread {
    /// The web page for the thread, mapped from its API URL.
    pub fn html_url(&self) -> String {
        let base = &self.repository.html_url;
        let api_prefix = format!("/repos/{}/", self.repository.full_name);
        let rest = self
            .subject
            .url
            .as_deref()
            .and_then(|u| u.split_once(&api_prefix))
            .map(|(_, rest)| rest);
        match rest.and_then(|r| r.split_once('/')) {
            Some(("pulls", n)) => format!("{base}/pull/{n}"),
            Some(("issues", n)) => format!("{base}/issues/{n}"),
            Some(("commits", sha)) => format!("{base}/commit/{sha}"),
            Some(("releases", _)) => format!("{base}/releases"),
            _ => match self.subject.kind.as_str() {
                "CheckSuite" => format!("{base}/actions"),
                "Discussion" => format!("{base}/discussions"),
                _ => base.clone(),
            },
        }
    }
}

/// One poll of the inbox.
pub struct Poll {
    /// Unread threads, newest first, or None when nothing changed since the last poll.
    pub threads: Option<Vec<Thread>>,
    /// Pass back as `since` on the next poll.
    pub last_modified: Option<String>,
    /// Seconds GitHub asks clients to wait between polls.
    pub poll_interval: Option<u64>,
}

/// The newest page of unread notifications, all or one repo's, unless nothing changed `since` an
/// earlier poll's Last-Modified.
pub fn poll(
    client: &Client,
    repo: Option<&Repo>,
    participating: bool,
    since: Option<&str>,
) -> Result<Poll> {
    let root = repo.map(|r| format!("{}/", r.api())).unwrap_or_default();
    let mut path = format!("{root}notifications?per_page=50");
    if participating {
        path.push_str("&participating=true");
    }
    let polled = client.poll(&path, since)?;
    let threads = polled
        .body
        .map(|body| serde_json::from_value(body).context("Failed to parse notifications"))
        .transpose()?;
    Ok(Poll {
        threads,
        last_modified: polled.last_modified,
        poll_interval: polled.poll_interval,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use mockito::{Matcher, Server};
    use serde_json::{Value, json};

    fn thread(id: &str, kind: &str, url: Option<&str>) -> Value {
        json!({
            "id": id, "reason": "review_requested", "updated_at": "2026-01-01T00:00:00Z",
            "subject": {"title": "Fix inflate", "url": url, "type": kind},
            "repository": {"full_name": "o/r", "html_url": "https://github.com/o/r"},
        })
    }

    fn html(kind: &str, url: Option<&str>) -> String {
        serde_json::from_value::<Thread>(thread("1", kind, url))
            .unwrap()
            .html_url()
    }

    #[test]
    fn maps_api_urls_to_web_pages() {
        let api = "https://api.github.com/repos/o/r";
        assert_eq!(
            html("PullRequest", Some(&format!("{api}/pulls/12"))),
            "https://github.com/o/r/pull/12"
        );
        assert_eq!(
            html("Issue", Some(&format!("{api}/issues/3"))),
            "https://github.com/o/r/issues/3"
        );
        assert_eq!(
            html("Commit", Some(&format!("{api}/commits/abc"))),
            "https://github.com/o/r/commit/abc"
        );
        assert_eq!(
            html("Release", Some(&format!("{api}/releases/99"))),
            "https://github.com/o/r/releases"
        );
        assert_eq!(html("CheckSuite", None), "https://github.com/o/r/actions");
        assert_eq!(html("RepositoryAdvisory", None), "https://github.com/o/r");
    }

    #[test]
    fn polls_conditionally() {
        let mut server = Server::new();
        let first = server
            .mock("GET", "/notifications")
            .match_query(Matcher::AllOf(vec![
                Matcher::UrlEncoded("per_page".into(), "50".into()),
                Matcher::UrlEncoded("participating".into(), "true".into()),
            ]))
            .match_header("if-modified-since", Matcher::Missing)
            .with_header("Last-Modified", "Mon, 26 Oct 2026 10:00:00 GMT")
            .with_header("X-Poll-Interval", "60")
            .with_body(json!([thread("1", "Issue", None)]).to_string())
            .create();
        let again = server
            .mock("GET", "/repos/o/r/notifications")
            .match_query(Matcher::Any)
            .match_header("if-modified-since", "Mon, 26 Oct 2026 10:00:00 GMT")
            .with_status(304)
            .create();
        let client = Client::for_tests(&server.url());
        let poll1 = poll(&client, None, true, None).unwrap();
        assert_eq!(poll1.threads.unwrap()[0].id, "1");
        assert_eq!(poll1.poll_interval, Some(60));
        let repo = Repo::parse("o/r").unwrap();
        let poll2 = poll(&client, Some(&repo), false, poll1.last_modified.as_deref()).unwrap();
        assert!(poll2.threads.is_none());
        first.assert();
        again.assert();
    }
}
