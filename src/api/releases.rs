//! Releases of a repository.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::{Client, Repo};

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Release {
    pub tag_name: Option<String>,
    pub draft: Option<bool>,
}

/// The ten newest releases, drafts included.
pub fn recent(client: &Client, repo: &Repo) -> Result<Vec<Release>> {
    let value = client.get(&format!("{}/releases?per_page=10", repo.api()))?;
    serde_json::from_value(value).context("Failed to parse releases")
}

/// Tag of the newest published release, skipping drafts.
pub fn latest_tag(client: &Client, repo: &Repo) -> Result<Option<String>> {
    Ok(latest_published(recent(client, repo)?))
}

fn latest_published(releases: Vec<Release>) -> Option<String> {
    releases
        .into_iter()
        .find(|r| r.draft != Some(true))
        .and_then(|r| r.tag_name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mockito::{Matcher, Server};
    use serde_json::json;

    #[test]
    fn latest_skips_drafts() {
        let releases: Vec<Release> = serde_json::from_value(json!([
            { "tag_name": "v2.0.0", "draft": true },
            { "tag_name": "v1.1.0", "draft": false },
            { "tag_name": "v1.0.0" }
        ]))
        .unwrap();
        assert_eq!(latest_published(releases).as_deref(), Some("v1.1.0"));
        assert_eq!(latest_published(Vec::new()), None);
    }

    #[test]
    fn latest_tag_reads_the_newest_ten() {
        let mut server = Server::new();
        let mock = server
            .mock("GET", "/repos/o/r/releases")
            .match_query(Matcher::Exact("per_page=10".into()))
            .with_body(
                r#"[{"tag_name": "v3.0.0", "draft": true}, {"tag_name": "v2.1.0", "draft": false}]"#,
            )
            .create();
        let client = Client::for_tests(&server.url());
        let repo = Repo::parse("o/r").unwrap();
        assert_eq!(
            latest_tag(&client, &repo).unwrap().as_deref(),
            Some("v2.1.0")
        );
        mock.assert();
    }
}
