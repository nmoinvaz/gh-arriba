use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::{null_default, opt_time, parse, parse_all};
use crate::api::{Client, Repo, encode_segment};

/// An Actions cache entry. It serializes with `ref` and `size`, the names `run caches --json`
/// has always printed.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Cache {
    pub id: u64,
    #[serde(default, deserialize_with = "null_default")]
    pub key: String,
    #[serde(rename = "ref", default, deserialize_with = "null_default")]
    pub git_ref: String,
    #[serde(
        rename(serialize = "size", deserialize = "size_in_bytes"),
        default,
        deserialize_with = "null_default"
    )]
    pub size: u64,
    #[serde(default, deserialize_with = "opt_time")]
    pub created_at: Option<DateTime<Utc>>,
    #[serde(default, deserialize_with = "opt_time")]
    pub last_accessed_at: Option<DateTime<Utc>>,
}

/// Size and count of every active cache in the repository.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CacheUsage {
    #[serde(default, deserialize_with = "null_default")]
    pub active_caches_size_in_bytes: u64,
    #[serde(default, deserialize_with = "null_default")]
    pub active_caches_count: u64,
}

/// The repository's caches, narrowed on the server to a full ref and a key prefix when given.
pub fn caches(
    client: &Client,
    repo: &Repo,
    git_ref: Option<&str>,
    key: Option<&str>,
) -> Result<Vec<Cache>> {
    let mut path = format!("{}/actions/caches", repo.api());
    let mut sep = '?';
    if let Some(r) = git_ref {
        path.push_str(&format!("{sep}ref={}", encode_segment(r)));
        sep = '&';
    }
    if let Some(k) = key {
        path.push_str(&format!("{sep}key={}", encode_segment(k)));
    }
    parse_all(client.get_all_in(&path, "actions_caches")?, "cache")
}

pub fn cache_usage(client: &Client, repo: &Repo) -> Result<CacheUsage> {
    parse(
        client.get(&format!("{}/actions/cache/usage", repo.api()))?,
        "cache usage",
    )
}

pub fn delete_cache(client: &Client, repo: &Repo, id: u64) -> Result<()> {
    client.delete(&format!("{}/actions/caches/{id}", repo.api()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use mockito::{Matcher, Server};
    use serde_json::json;

    #[test]
    fn parses_cache_fields_and_keeps_output_names() {
        let c: Cache = serde_json::from_value(json!({
            "id": 3, "key": "Linux-cargo-abc", "ref": "refs/heads/main", "size_in_bytes": 2048,
            "created_at": "2026-10-01T00:00:00Z", "last_accessed_at": "2026-10-20T00:00:00Z",
            "version": "v1"
        }))
        .unwrap();
        assert_eq!(
            (c.id, c.key.as_str(), c.git_ref.as_str(), c.size),
            (3, "Linux-cargo-abc", "refs/heads/main", 2048)
        );
        assert_eq!(
            c.last_accessed_at,
            Some("2026-10-20T00:00:00Z".parse().unwrap())
        );
        assert_eq!(
            serde_json::to_value(&c).unwrap(),
            json!({
                "id": 3, "key": "Linux-cargo-abc", "ref": "refs/heads/main", "size": 2048,
                "created_at": "2026-10-01T00:00:00Z", "last_accessed_at": "2026-10-20T00:00:00Z"
            })
        );
    }

    #[test]
    fn usage_counts() {
        let u: CacheUsage = serde_json::from_value(json!({
            "full_name": "o/r", "active_caches_size_in_bytes": 10, "active_caches_count": 2
        }))
        .unwrap();
        assert_eq!(
            (u.active_caches_size_in_bytes, u.active_caches_count),
            (10, 2)
        );
    }

    #[test]
    fn listing_filters_are_encoded() {
        let mut server = Server::new();
        let all = server
            .mock("GET", "/repos/o/r/actions/caches")
            .match_query(Matcher::Exact("per_page=100".into()))
            .with_body(r#"{"total_count": 1, "actions_caches": [{"id": 1, "size_in_bytes": 5}]}"#)
            .create();
        let filtered = server
            .mock("GET", "/repos/o/r/actions/caches")
            .match_query(Matcher::Exact(
                "ref=refs%2Fpull%2F3%2Fmerge&key=Linux%20cargo&per_page=100".into(),
            ))
            .with_body(r#"{"actions_caches": []}"#)
            .create();
        let key_only = server
            .mock("GET", "/repos/o/r/actions/caches")
            .match_query(Matcher::Exact("key=sccache&per_page=100".into()))
            .with_body(r#"{"actions_caches": [{"id": 2}]}"#)
            .create();
        let client = Client::for_tests(&server.url());
        let repo = Repo::parse("o/r").unwrap();
        assert_eq!(caches(&client, &repo, None, None).unwrap()[0].size, 5);
        assert!(
            caches(
                &client,
                &repo,
                Some("refs/pull/3/merge"),
                Some("Linux cargo")
            )
            .unwrap()
            .is_empty()
        );
        assert_eq!(
            caches(&client, &repo, None, Some("sccache")).unwrap()[0].id,
            2
        );
        for m in [all, filtered, key_only] {
            m.assert();
        }
    }

    #[test]
    fn usage_and_delete() {
        let mut server = Server::new();
        server
            .mock("GET", "/repos/o/r/actions/cache/usage")
            .with_body(r#"{"active_caches_size_in_bytes": 100, "active_caches_count": null}"#)
            .create();
        let delete = server
            .mock("DELETE", "/repos/o/r/actions/caches/4")
            .with_status(204)
            .create();
        let client = Client::for_tests(&server.url());
        let repo = Repo::parse("o/r").unwrap();
        let usage = cache_usage(&client, &repo).unwrap();
        assert_eq!(
            (usage.active_caches_size_in_bytes, usage.active_caches_count),
            (100, 0)
        );
        delete_cache(&client, &repo, 4).unwrap();
        delete.assert();
    }
}
