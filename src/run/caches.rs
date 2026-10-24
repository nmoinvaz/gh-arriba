use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;

use crate::cli::CachesArgs;
use crate::context::{Ctx, Repo};
use crate::github;
use crate::json::{opt_time_of, str_of};
use crate::output;

/// GitHub's default cache limit per repository. Past it, the least recently used caches are evicted.
const LIMIT: u64 = 10 * 1024 * 1024 * 1024;

/// An Actions cache entry.
#[derive(Clone, Debug, Serialize)]
pub struct Cache {
    pub id: u64,
    pub key: String,
    #[serde(rename = "ref")]
    pub git_ref: String,
    pub size: u64,
    pub created_at: Option<DateTime<Utc>>,
    pub last_accessed_at: Option<DateTime<Utc>>,
}

fn parse_cache(v: &Value) -> Cache {
    Cache {
        id: v["id"].as_u64().unwrap_or(0),
        key: str_of(v, "key"),
        git_ref: str_of(v, "ref"),
        size: v["size_in_bytes"].as_u64().unwrap_or(0),
        created_at: opt_time_of(v, "created_at"),
        last_accessed_at: opt_time_of(v, "last_accessed_at"),
    }
}

/// A `--ref` value as the full ref the cache API filters on: a full ref as given, a PR number as
/// its merge ref, or a branch name.
fn full_ref(s: &str) -> String {
    if s.starts_with("refs/") {
        s.to_string()
    } else if let Ok(n) = s.trim_start_matches('#').parse::<u64>() {
        format!("refs/pull/{n}/merge")
    } else {
        format!("refs/heads/{s}")
    }
}

/// The PR number of a `refs/pull/N/merge` ref.
fn pr_number(git_ref: &str) -> Option<u64> {
    git_ref
        .strip_prefix("refs/pull/")?
        .strip_suffix("/merge")?
        .parse()
        .ok()
}

/// A ref as people say it: `PR #N`, the branch name, or the ref itself.
fn short_ref(git_ref: &str) -> String {
    match pr_number(git_ref) {
        Some(n) => format!("PR #{n}"),
        None => git_ref
            .strip_prefix("refs/heads/")
            .unwrap_or(git_ref)
            .to_string(),
    }
}

/// A key without its trailing hash segments, so the caches one step saves group together, as
/// `Linux-cargo-9f86d081884c7d65` does under `Linux-cargo`.
fn key_family(key: &str) -> &str {
    let mut family = key;
    while let Some((head, last)) = family.rsplit_once(['-', '_'])
        && last.len() >= 8
        && last.chars().all(|c| c.is_ascii_hexdigit())
    {
        family = head;
    }
    family
}

/// The repo's caches, narrowed by `--ref` and `--key` on the server.
fn fetch(ctx: &Ctx, repo: &Repo, args: &CachesArgs) -> Result<Vec<Cache>> {
    let mut path = format!("{}/actions/caches", repo.api());
    let mut sep = '?';
    if let Some(r) = &args.git_ref {
        path.push_str(&format!(
            "{sep}ref={}",
            github::encode_segment(&full_ref(r))
        ));
        sep = '&';
    }
    if let Some(k) = &args.key {
        path.push_str(&format!("{sep}key={}", github::encode_segment(k)));
    }
    Ok(ctx
        .client
        .get_all_in(&path, "actions_caches")?
        .iter()
        .map(parse_cache)
        .collect())
}

/// Caches that share a ref or a key family.
#[derive(Debug, PartialEq, Serialize)]
struct Group {
    name: String,
    count: usize,
    size: u64,
    last_used: Option<DateTime<Utc>>,
}

/// Group caches by `name_of`, largest first.
fn group_by(caches: &[Cache], name_of: impl Fn(&Cache) -> String) -> Vec<Group> {
    let mut groups: BTreeMap<String, Group> = BTreeMap::new();
    for c in caches {
        let name = name_of(c);
        let g = groups.entry(name.clone()).or_insert(Group {
            name,
            count: 0,
            size: 0,
            last_used: None,
        });
        g.count += 1;
        g.size += c.size;
        g.last_used = g.last_used.max(c.last_accessed_at);
    }
    let mut groups: Vec<Group> = groups.into_values().collect();
    groups.sort_by_key(|g| std::cmp::Reverse(g.size));
    groups
}

#[derive(Serialize)]
struct Summary<'a> {
    repo: String,
    /// Size and count of every active cache in the repo, filtered or not.
    total_bytes: u64,
    total_count: u64,
    limit_bytes: u64,
    by_ref: Vec<Group>,
    by_key: Vec<Group>,
    caches: &'a [Cache],
}

pub fn run(ctx: &Ctx, args: CachesArgs) -> Result<()> {
    let repo = ctx.repo()?;
    let caches = fetch(ctx, &repo, &args)?;
    let usage = ctx
        .client
        .get(&format!("{}/actions/cache/usage", repo.api()))?;
    let summary = Summary {
        repo: repo.to_string(),
        total_bytes: usage["active_caches_size_in_bytes"].as_u64().unwrap_or(0),
        total_count: usage["active_caches_count"].as_u64().unwrap_or(0),
        limit_bytes: LIMIT,
        by_ref: group_by(&caches, |c| short_ref(&c.git_ref)),
        by_key: group_by(&caches, |c| key_family(&c.key).to_string()),
        caches: &caches,
    };
    if ctx.json {
        return output::print_json(&summary);
    }
    let filtered = args.git_ref.is_some() || args.key.is_some();
    print!("{}", render(&summary, filtered, Utc::now()));
    Ok(())
}

fn render(s: &Summary, filtered: bool, now: DateTime<Utc>) -> String {
    let mut out = format!(
        "{}: {} use {} of the default {} limit ({}%)\n",
        s.repo,
        plural(s.total_count as usize, "cache"),
        output::size(s.total_bytes),
        output::size(s.limit_bytes),
        s.total_bytes * 100 / s.limit_bytes.max(1)
    );
    if filtered {
        let size: u64 = s.caches.iter().map(|c| c.size).sum();
        out.push_str(&format!(
            "{} match the filters, using {}\n",
            s.caches.len(),
            output::size(size)
        ));
    }
    for (title, groups) in [("By ref", &s.by_ref), ("By key", &s.by_key)] {
        if groups.is_empty() {
            continue;
        }
        let rows: Vec<Vec<String>> = groups
            .iter()
            .map(|g| {
                vec![
                    g.name.clone(),
                    plural(g.count, "cache"),
                    output::size(g.size),
                    g.last_used
                        .map(|t| format!("used {} ago", output::age(now - t)))
                        .unwrap_or_default(),
                ]
            })
            .collect();
        out.push_str(&format!("\n{title}:\n{}\n", output::table(&rows, "  ")));
    }
    out
}

/// `1 cache` or `3 caches`.
fn plural(n: usize, noun: &str) -> String {
    if n == 1 {
        format!("1 {noun}")
    } else {
        format!("{n} {noun}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn at(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    fn cache(id: u64, key: &str, git_ref: &str, size: u64, used: &str) -> Cache {
        Cache {
            id,
            key: key.to_string(),
            git_ref: git_ref.to_string(),
            size,
            created_at: Some(at("2026-10-01T00:00:00Z")),
            last_accessed_at: Some(at(used)),
        }
    }

    #[test]
    fn parses_cache_fields() {
        let c = parse_cache(&json!({
            "id": 3, "key": "Linux-cargo-abc", "ref": "refs/heads/main", "size_in_bytes": 2048,
            "created_at": "2026-10-01T00:00:00Z", "last_accessed_at": "2026-10-20T00:00:00Z",
            "version": "v1"
        }));
        assert_eq!(
            (c.id, c.key.as_str(), c.git_ref.as_str(), c.size),
            (3, "Linux-cargo-abc", "refs/heads/main", 2048)
        );
        assert_eq!(c.last_accessed_at, Some(at("2026-10-20T00:00:00Z")));
    }

    #[test]
    fn refs_in_both_directions() {
        assert_eq!(full_ref("main"), "refs/heads/main");
        assert_eq!(full_ref("feature/x"), "refs/heads/feature/x");
        assert_eq!(full_ref("42"), "refs/pull/42/merge");
        assert_eq!(full_ref("#42"), "refs/pull/42/merge");
        assert_eq!(full_ref("refs/tags/v1"), "refs/tags/v1");
        assert_eq!(short_ref("refs/pull/42/merge"), "PR #42");
        assert_eq!(short_ref("refs/heads/feature/x"), "feature/x");
        assert_eq!(short_ref("refs/tags/v1"), "refs/tags/v1");
        assert_eq!(pr_number("refs/pull/7/merge"), Some(7));
        assert_eq!(pr_number("refs/heads/7"), None);
    }

    #[test]
    fn key_families_drop_trailing_hashes() {
        assert_eq!(key_family("Linux-cargo-9f86d081884c7d65"), "Linux-cargo");
        assert_eq!(
            key_family("setup-python-Linux-3.12-pip-0123456789abcdef-fedcba98"),
            "setup-python-Linux-3.12-pip"
        );
        assert_eq!(key_family("node-modules-1700000000"), "node-modules");
        assert_eq!(key_family("ccache-gcc-13"), "ccache-gcc-13");
        assert_eq!(key_family("deadbeef00"), "deadbeef00");
    }

    #[test]
    fn groups_sum_and_sort_by_size() {
        let caches = vec![
            cache(
                1,
                "a-11111111",
                "refs/heads/main",
                100,
                "2026-10-20T00:00:00Z",
            ),
            cache(
                2,
                "a-22222222",
                "refs/pull/5/merge",
                300,
                "2026-10-21T00:00:00Z",
            ),
            cache(3, "b", "refs/heads/main", 50, "2026-10-22T00:00:00Z"),
        ];
        let by_ref = group_by(&caches, |c| short_ref(&c.git_ref));
        assert_eq!(
            by_ref
                .iter()
                .map(|g| (g.name.as_str(), g.count, g.size))
                .collect::<Vec<_>>(),
            vec![("PR #5", 1, 300), ("main", 2, 150)]
        );
        assert_eq!(by_ref[1].last_used, Some(at("2026-10-22T00:00:00Z")));
        let by_key = group_by(&caches, |c| key_family(&c.key).to_string());
        assert_eq!(
            by_key
                .iter()
                .map(|g| (g.name.as_str(), g.size))
                .collect::<Vec<_>>(),
            vec![("a", 400), ("b", 50)]
        );
    }

    #[test]
    fn renders_usage_and_groups() {
        let caches = vec![cache(
            1,
            "a-11111111",
            "refs/heads/main",
            1024 * 1024 * 1024,
            "2026-10-23T00:00:00Z",
        )];
        let summary = Summary {
            repo: "o/r".to_string(),
            total_bytes: 2 * 1024 * 1024 * 1024,
            total_count: 2,
            limit_bytes: LIMIT,
            by_ref: group_by(&caches, |c| short_ref(&c.git_ref)),
            by_key: group_by(&caches, |c| key_family(&c.key).to_string()),
            caches: &caches,
        };
        assert_eq!(
            render(&summary, true, at("2026-10-24T00:00:00Z")),
            "o/r: 2 caches use 2.0 GB of the default 10.0 GB limit (20%)\n\
             1 match the filters, using 1.0 GB\n\
             \n\
             By ref:\n  \
             main  1 cache  1.0 GB  used 1d ago\n\
             \n\
             By key:\n  \
             a  1 cache  1.0 GB  used 1d ago\n"
        );
    }
}
