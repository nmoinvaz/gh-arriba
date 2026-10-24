mod alerts;
mod list;
mod merge;
mod rebase;

use anyhow::{Result, anyhow};
use chrono::{DateTime, Utc};
use regex::Regex;
use serde::Serialize;
use std::collections::{BTreeMap, HashSet};
use std::fmt;
use std::sync::LazyLock;
use std::time::Duration;

use crate::api::dependabot::{self, PrDetails, PrSummary};
use crate::api::{Client, Repo};
use crate::cli::DependabotCommand;
use crate::context::Ctx;

pub fn run(ctx: &Ctx, cmd: DependabotCommand) -> Result<()> {
    match cmd {
        DependabotCommand::List(args) => list::run(ctx, args),
        DependabotCommand::Merge(args) => merge::run(ctx, args),
        DependabotCommand::Rebase(args) => rebase::run(ctx, args),
        DependabotCommand::Alerts(args) => alerts::run_alerts(ctx, args),
        DependabotCommand::Dismiss(args) => alerts::run_dismiss(ctx, args),
    }
}

/// Comment that asks Dependabot to rebase its PR.
const REBASE_COMMENT: &str = "@dependabot rebase";

/// One repository, or every repository in an organization.
pub enum Scope {
    Repo(Repo),
    Org(String),
}

impl Scope {
    pub fn new(ctx: &Ctx, org: Option<String>) -> Result<Self> {
        match org {
            Some(o) => Ok(Scope::Org(o)),
            None => Ok(Scope::Repo(ctx.repo()?)),
        }
    }

    pub fn is_org(&self) -> bool {
        matches!(self, Scope::Org(_))
    }
}

impl fmt::Display for Scope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Scope::Repo(r) => write!(f, "{r}"),
            Scope::Org(o) => write!(f, "{o}"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Category {
    Ready,
    NeedsRebase,
    Blocked,
}

impl Category {
    pub fn label(self) -> &'static str {
        match self {
            Category::Ready => "ready",
            Category::NeedsRebase => "needs rebase 🔄",
            Category::Blocked => "blocked",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Change {
    Major,
    Minor,
    Patch,
    Other,
}

impl Change {
    pub fn badge(self) -> &'static str {
        match self {
            Change::Major => "🔴 major",
            Change::Minor => "🟡 minor",
            Change::Patch => "🟢 patch",
            Change::Other => "⚪ other",
        }
    }

    pub fn risk(self) -> &'static str {
        match self {
            Change::Major => "🔴 major, may break, review carefully",
            Change::Minor => "🟡 minor, new features, backward compatible",
            Change::Patch => "🟢 patch, bug and security fixes",
            Change::Other => "⚪ unknown, versions don't follow semver",
        }
    }
}

/// What a Dependabot PR updates, parsed from its title.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Update {
    /// Package name, or the group name for grouped updates.
    pub package: String,
    pub from: Option<String>,
    pub to: Option<String>,
    pub directory: Option<String>,
    /// Grouped PRs update several packages at once and carry no single version.
    pub group: bool,
    pub updates: Option<u64>,
    pub change: Change,
}

impl Update {
    /// Package with the group marker, as `dev-dependencies group (7 updates)`.
    pub fn name(&self) -> String {
        match (self.group, self.updates) {
            (true, Some(n)) => format!("{} group ({n} updates)", self.package),
            (true, None) => format!("{} group", self.package),
            _ => self.package.clone(),
        }
    }

    /// Version change as `OLD → NEW`, empty when unknown.
    pub fn versions(&self) -> String {
        match (&self.from, &self.to) {
            (Some(f), Some(t)) => format!("{f} → {t}"),
            _ => String::new(),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct DepPr {
    /// OWNER/REPO.
    pub repo: String,
    pub number: u64,
    pub node_id: String,
    pub title: String,
    pub url: String,
    pub created_at: DateTime<Utc>,
    pub head_ref: String,
    pub mergeable: String,
    pub merge_state: String,
    pub review_decision: Option<String>,
    /// Rollup state of the head commit's checks. None when the commit has no checks.
    pub checks: Option<String>,
    pub additions: u64,
    pub deletions: u64,
    pub changed_files: u64,
    pub update: Update,
    pub category: Category,
    pub reason: Option<String>,
}

impl DepPr {
    /// `#N`, or `OWNER/REPO#N` when several repos are in view.
    pub fn label(&self, with_repo: bool) -> String {
        if with_repo {
            format!("{}#{}", self.repo, self.number)
        } else {
            format!("#{}", self.number)
        }
    }

    /// Diff size as `3 files (+10 -4)`.
    pub fn size(&self) -> String {
        let files = if self.changed_files == 1 {
            "file"
        } else {
            "files"
        };
        format!(
            "{} {files} (+{} -{})",
            self.changed_files, self.additions, self.deletions
        )
    }
}

/// PRs that update packages sharing a prefix from the same version to the same version.
#[derive(Clone, Debug, Serialize)]
pub struct LockStep {
    pub repo: String,
    pub prefix: String,
    pub from: String,
    pub to: String,
    pub change: Change,
    /// PR numbers in merge order. The first must merge before the rest.
    pub prs: Vec<u64>,
    pub status: Category,
}

/// Every open Dependabot PR in scope, plus the lock-step groups among them.
pub struct Overview {
    pub prs: Vec<DepPr>,
    pub groups: Vec<LockStep>,
}

impl Overview {
    pub fn new(prs: Vec<DepPr>) -> Self {
        let groups = build_groups(&prs);
        Self { prs, groups }
    }

    /// Fetch and classify, waiting once for GitHub to compute unknown mergeability.
    pub fn load(client: &Client, scope: &Scope) -> Result<Self> {
        Ok(Self::new(fetch_prs(client, scope, true)?))
    }

    pub fn get(&self, repo: &str, number: u64) -> Option<&DepPr> {
        self.prs
            .iter()
            .find(|p| p.repo == repo && p.number == number)
    }

    pub fn members(&self, group: &LockStep) -> Vec<&DepPr> {
        group
            .prs
            .iter()
            .filter_map(|n| self.get(&group.repo, *n))
            .collect()
    }

    pub fn is_grouped(&self, pr: &DepPr) -> bool {
        self.groups
            .iter()
            .any(|g| g.repo == pr.repo && g.prs.contains(&pr.number))
    }

    /// PRs outside every lock-step group, in the given category.
    pub fn singles(&self, category: Category) -> Vec<&DepPr> {
        self.prs
            .iter()
            .filter(|p| p.category == category && !self.is_grouped(p))
            .collect()
    }

    pub fn count(&self, category: Category) -> usize {
        self.prs.iter().filter(|p| p.category == category).count()
    }
}

/// Every open Dependabot PR in scope with just its number, title, URL, and repository.
pub fn fetch_light(client: &Client, scope: &Scope) -> Result<Vec<PrSummary>> {
    let nodes = match scope {
        Scope::Repo(r) => dependabot::open_prs(client, r)?,
        Scope::Org(o) => dependabot::search_dependabot_prs(client, o)?,
    }
    .ok_or_else(|| anyhow!("{scope} not found"))?;
    Ok(nodes
        .into_iter()
        .filter(|n| is_dependabot(n.author_login()))
        .collect())
}

/// Fetch every open Dependabot PR in scope with checks and mergeability. With `settle`, PRs whose
/// mergeability GitHub has not computed yet are fetched again after a short wait.
pub fn fetch_prs(client: &Client, scope: &Scope, settle: bool) -> Result<Vec<DepPr>> {
    // Find Dependabot's PRs cheaply, then pay for details on those alone.
    let keys: Vec<(String, u64)> = fetch_light(client, scope)?
        .into_iter()
        .map(|n| (n.repository.name_with_owner, n.number))
        .collect();
    let mut nodes = dependabot::pr_details(client, &keys)?;

    // Querying mergeability starts GitHub computing it, so a second look usually has the answer.
    if settle {
        let unknown: Vec<(String, u64)> = nodes
            .iter()
            .filter(|n| n.mergeable == "UNKNOWN")
            .map(|n| (n.repo().to_string(), n.number))
            .collect();
        if !unknown.is_empty() {
            std::thread::sleep(Duration::from_secs(2));
            for fresh in dependabot::pr_details(client, &unknown)? {
                if let Some(slot) = nodes
                    .iter_mut()
                    .find(|n| (n.repo(), n.number) == (fresh.repo(), fresh.number))
                {
                    *slot = fresh;
                }
            }
        }
    }

    Ok(nodes.into_iter().map(DepPr::from).collect())
}

/// Dependabot's login is `dependabot` in GraphQL and `dependabot[bot]` in REST.
pub fn is_dependabot(login: Option<&str>) -> bool {
    matches!(
        login,
        Some("dependabot" | "dependabot[bot]" | "app/dependabot")
    )
}

impl From<PrDetails> for DepPr {
    fn from(node: PrDetails) -> Self {
        let checks = node.checks().map(str::to_string);
        let (category, reason) =
            categorize(&node.mergeable, &node.merge_state_status, checks.as_deref());
        DepPr {
            repo: node.repository.name_with_owner,
            number: node.number,
            node_id: node.id,
            update: parse_title(&node.title),
            title: node.title,
            url: node.url,
            created_at: node.created_at,
            head_ref: node.head_ref_name,
            mergeable: node.mergeable,
            merge_state: node.merge_state_status,
            review_decision: node.review_decision,
            checks,
            additions: node.additions,
            deletions: node.deletions,
            changed_files: node.changed_files,
            category,
            reason,
        }
    }
}

/// Sort a PR into ready, needs rebase, or blocked, with a reason for blocked ones.
pub fn categorize(
    mergeable: &str,
    merge_state: &str,
    checks: Option<&str>,
) -> (Category, Option<String>) {
    match checks {
        None | Some("SUCCESS") => {}
        Some("FAILURE" | "ERROR") => return (Category::Blocked, Some("failing checks".into())),
        Some("PENDING" | "EXPECTED") => {
            return (Category::Blocked, Some("checks pending".into()));
        }
        Some(other) => {
            return (
                Category::Blocked,
                Some(format!("checks {}", other.to_lowercase())),
            );
        }
    }
    if mergeable == "CONFLICTING" || matches!(merge_state, "BEHIND" | "DIRTY") {
        return (Category::NeedsRebase, None);
    }
    if mergeable == "MERGEABLE" {
        return (Category::Ready, None);
    }
    (
        Category::Blocked,
        Some("GitHub hasn't computed mergeability yet".into()),
    )
}

static CONVENTIONAL_PREFIX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Za-z]+(?:\([^)]*\))*!?:\s*").unwrap());

static BUMP: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)^(?:bump|update)\s+(?P<pkg>\S+)(?:\s+requirement)?\s+from\s+(?P<from>\S+)\s+to\s+(?P<to>\S+)(?:\s+in\s+(?P<dir>/\S*))?",
    )
    .unwrap()
});

static GROUP: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^bump\s+the\s+(?P<group>\S+)\s+group\b(?:.*?\bwith\s+(?P<n>\d+)\s+updates?)?")
        .unwrap()
});

/// Titles without versions, as `Bump picomatch` when a package moves to different versions in
/// several directories, or `Bump x and y` for peer updates.
static BARE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^(?:bump|update)\s+(?P<pkg>.+?)(?:\s+in\s+(?P<dir>/\S*))?$").unwrap()
});

/// Parse `Bump PKG from OLD to NEW in DIR` and its variants. Unrecognized titles keep the whole
/// title as the package with no versions.
pub fn parse_title(title: &str) -> Update {
    let t = CONVENTIONAL_PREFIX.replace(title.trim(), "");
    if let Some(c) = BUMP.captures(&t) {
        let from = c["from"].to_string();
        let to = c["to"].to_string();
        return Update {
            package: c["pkg"].to_string(),
            change: semver_change(&from, &to),
            from: Some(from),
            to: Some(to),
            directory: c.name("dir").map(|d| d.as_str().to_string()),
            group: false,
            updates: None,
        };
    }
    if let Some(c) = GROUP.captures(&t) {
        return Update {
            package: c["group"].to_string(),
            from: None,
            to: None,
            directory: None,
            group: true,
            updates: c.name("n").and_then(|n| n.as_str().parse().ok()),
            change: Change::Other,
        };
    }
    let (package, directory) = match BARE.captures(&t) {
        Some(c) => (
            c["pkg"].to_string(),
            c.name("dir").map(|d| d.as_str().to_string()),
        ),
        None => (t.to_string(), None),
    };
    Update {
        package,
        from: None,
        to: None,
        directory,
        group: false,
        updates: None,
        change: Change::Other,
    }
}

/// Which semver component changed. Requirement operators and a `v` prefix are ignored, missing
/// components count as zero, and prerelease or build suffixes are dropped.
pub fn semver_change(from: &str, to: &str) -> Change {
    fn parse(v: &str) -> Option<Vec<u64>> {
        let v = v.trim_start_matches(['v', 'V', '=', '>', '<', '~', '^']);
        let core = v.split(['-', '+']).next()?;
        core.split('.').map(|p| p.parse().ok()).collect()
    }
    let (Some(a), Some(b)) = (parse(from), parse(to)) else {
        return Change::Other;
    };
    let at = |v: &[u64], i: usize| v.get(i).copied().unwrap_or(0);
    let len = a.len().max(b.len());
    if at(&a, 0) != at(&b, 0) {
        Change::Major
    } else if at(&a, 1) != at(&b, 1) {
        Change::Minor
    } else if (2..len).any(|i| at(&a, i) != at(&b, i)) {
        Change::Patch
    } else {
        Change::Other
    }
}

/// Prefix that ties packages released together, as `@scope` for `@scope/pkg`, the module path
/// for Go, the group ID for Maven, or the first word of a hyphenated name.
pub fn package_prefix(name: &str) -> String {
    if let Some((prefix, _)) = name.rsplit_once('/') {
        return prefix.to_string();
    }
    if let Some((prefix, _)) = name.split_once(':') {
        return prefix.to_string();
    }
    name.split(['-', '_', '.'])
        .next()
        .unwrap_or(name)
        .to_string()
}

/// Names a package answers to when matching families, its full name and its prefix, both without
/// a scope's `@`. `next` and `@next/eslint-plugin-next` share `next`, and `typescript-eslint` and
/// `@typescript-eslint/parser` share `typescript-eslint`.
fn family_keys(name: &str) -> HashSet<String> {
    [name.to_string(), package_prefix(name)]
        .into_iter()
        .map(|k| k.trim_start_matches('@').to_string())
        .collect()
}

/// Name for a family, the first member's prefix when every member shares it, else the shortest
/// key they all share, else the first member's prefix.
fn family_label(family: &[&DepPr]) -> String {
    let first = package_prefix(&family[0].update.package);
    let mut shared = family_keys(&family[0].update.package);
    for p in &family[1..] {
        let keys = family_keys(&p.update.package);
        shared.retain(|k| keys.contains(k));
    }
    if shared.contains(first.trim_start_matches('@')) {
        return first;
    }
    shared
        .into_iter()
        .min_by_key(|k| (k.len(), k.clone()))
        .unwrap_or(first)
}

/// Group PRs within a repo that move related packages between the same two versions. Packages
/// are related when their family keys overlap, transitively.
pub fn build_groups(prs: &[DepPr]) -> Vec<LockStep> {
    let mut buckets: BTreeMap<(String, String, String), Vec<&DepPr>> = BTreeMap::new();
    for pr in prs {
        let (Some(from), Some(to)) = (&pr.update.from, &pr.update.to) else {
            continue;
        };
        buckets
            .entry((pr.repo.clone(), from.clone(), to.clone()))
            .or_default()
            .push(pr);
    }

    let mut groups = Vec::new();
    for ((repo, from, to), mut members) in buckets {
        members.sort_by_key(|p| p.number);
        let keys: Vec<HashSet<String>> = members
            .iter()
            .map(|p| family_keys(&p.update.package))
            .collect();

        // Union-find over members whose keys overlap.
        let mut parent: Vec<usize> = (0..members.len()).collect();
        fn root(parent: &mut [usize], mut i: usize) -> usize {
            while parent[i] != i {
                parent[i] = parent[parent[i]];
                i = parent[i];
            }
            i
        }
        for i in 0..members.len() {
            for j in i + 1..members.len() {
                if !keys[i].is_disjoint(&keys[j]) {
                    let (a, b) = (root(&mut parent, i), root(&mut parent, j));
                    parent[b.max(a)] = a.min(b);
                }
            }
        }
        let mut families: BTreeMap<usize, Vec<&DepPr>> = BTreeMap::new();
        for (i, pr) in members.iter().enumerate() {
            families.entry(root(&mut parent, i)).or_default().push(pr);
        }

        for family in families.into_values().filter(|f| f.len() >= 2) {
            // A blocked member blocks the group, otherwise the first PR decides how it merges.
            let status = if family.iter().any(|p| p.category == Category::Blocked) {
                Category::Blocked
            } else {
                family[0].category
            };
            groups.push(LockStep {
                repo: repo.clone(),
                prefix: family_label(&family),
                from: from.clone(),
                to: to.clone(),
                change: family[0].update.change,
                prs: family.iter().map(|p| p.number).collect(),
                status,
            });
        }
    }
    // Oldest group first, the order a person would merge them.
    groups.sort_by(|a, b| (&a.repo, a.prs[0]).cmp(&(&b.repo, b.prs[0])));
    groups
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::cli::DependabotListArgs;
    use mockito::{Matcher, Mock, Server, ServerGuard};
    use serde_json::{Value, json};

    /// A context against the test server, scoped to o/r and answering yes to every prompt.
    pub fn ctx(server: &ServerGuard, json: bool) -> Ctx {
        Ctx {
            client: Client::for_tests(&server.url()),
            repo_flag: Some("o/r".into()),
            json,
            yes: true,
        }
    }

    /// A Dependabot PR as the details query returns it.
    pub fn node(
        repo: &str,
        number: u64,
        title: &str,
        mergeable: &str,
        merge_state: &str,
        checks: Option<&str>,
    ) -> Value {
        json!({
            "id": format!("PR_{number}"), "number": number, "title": title,
            "url": format!("https://github.com/{repo}/pull/{number}"),
            "createdAt": "2026-01-02T03:04:05Z",
            "headRefName": format!("dependabot/cargo/pr-{number}"),
            "mergeable": mergeable, "mergeStateStatus": merge_state, "reviewDecision": null,
            "additions": 2, "deletions": 1, "changedFiles": 1,
            "author": { "login": "dependabot" },
            "repository": { "nameWithOwner": repo },
            "commits": { "nodes": [ { "commit": {
                "statusCheckRollup": checks.map(|s| json!({ "state": s }))
            } } ] }
        })
    }

    /// A ready PR in o/r.
    pub fn ready(number: u64, title: &str) -> Value {
        node("o/r", number, title, "MERGEABLE", "CLEAN", Some("SUCCESS"))
    }

    /// Mock the cheap listing (the repo query, or the org search with `org`) and the details
    /// query for `nodes`, which Dependabot authored.
    pub fn mock_prs(server: &mut ServerGuard, org: bool, nodes: &[Value]) -> (Mock, Mock) {
        let summaries: Vec<Value> = nodes
            .iter()
            .map(|n| {
                json!({
                    "number": n["number"], "title": n["title"], "url": n["url"],
                    "author": { "login": "dependabot" }, "repository": n["repository"]
                })
            })
            .collect();
        let page = json!({ "pageInfo": { "hasNextPage": false }, "nodes": summaries });
        let (pattern, data) = if org {
            (r"search\(query", json!({ "search": page }))
        } else {
            (
                r"pullRequests\(states: OPEN",
                json!({ "repository": { "pullRequests": page } }),
            )
        };
        let light = server
            .mock("POST", "/graphql")
            .match_body(Matcher::Regex(pattern.into()))
            .with_body(json!({ "data": data }).to_string())
            .create();
        let aliased: serde_json::Map<String, Value> = nodes
            .iter()
            .enumerate()
            .map(|(i, n)| (format!("p{i}"), json!({ "pullRequest": n })))
            .collect();
        let details = server
            .mock("POST", "/graphql")
            .match_body(Matcher::Regex("fragment Dep on PullRequest".into()))
            .with_body(json!({ "data": aliased }).to_string())
            .create();
        (light, details)
    }

    #[test]
    fn fetch_light_keeps_dependabot_prs() {
        let mut server = Server::new();
        let author = |n: u64, login: &str| {
            json!({ "number": n, "title": "t", "url": "u", "author": { "login": login },
                    "repository": { "nameWithOwner": "o/r" } })
        };
        server
            .mock("POST", "/graphql")
            .with_body(
                json!({ "data": { "repository": { "pullRequests": {
                    "pageInfo": { "hasNextPage": false },
                    "nodes": [author(1, "dependabot"), author(2, "someone"),
                              author(3, "dependabot[bot]"), { "number": 4, "author": null }]
                } } } })
                .to_string(),
            )
            .create();
        let client = Client::for_tests(&server.url());
        let scope = Scope::Repo(Repo::parse("o/r").unwrap());
        let numbers: Vec<u64> = fetch_light(&client, &scope)
            .unwrap()
            .iter()
            .map(|n| n.number)
            .collect();
        assert_eq!(numbers, vec![1, 3]);
    }

    #[test]
    fn fetch_light_reports_missing_scope() {
        let mut server = Server::new();
        server
            .mock("POST", "/graphql")
            .match_body(Matcher::Regex(r"pullRequests\(states".into()))
            .with_body(r#"{"data": {"repository": null}}"#)
            .create();
        server
            .mock("POST", "/graphql")
            .match_body(Matcher::Regex(r"search\(query".into()))
            .with_body(r#"{"data": {"search": null}}"#)
            .create();
        let client = Client::for_tests(&server.url());
        let repo = Scope::Repo(Repo::parse("o/gone").unwrap());
        let err = fetch_light(&client, &repo).unwrap_err();
        assert_eq!(err.to_string(), "o/gone not found");
        let err = fetch_light(&client, &Scope::Org("acme".into())).unwrap_err();
        assert_eq!(err.to_string(), "acme not found");
    }

    #[test]
    fn fetch_prs_classifies_details() {
        let mut server = Server::new();
        let (light, details) = mock_prs(
            &mut server,
            false,
            &[
                ready(1, "Bump serde from 1.0.1 to 1.0.2"),
                node(
                    "o/r",
                    2,
                    "Bump tokio from 1.0.0 to 1.1.0",
                    "CONFLICTING",
                    "DIRTY",
                    Some("SUCCESS"),
                ),
                node(
                    "o/r",
                    3,
                    "Bump pino from 9.0.0 to 10.0.0",
                    "MERGEABLE",
                    "UNSTABLE",
                    Some("FAILURE"),
                ),
                node("o/r", 4, "Bump the dev group", "MERGEABLE", "CLEAN", None),
            ],
        );
        let client = Client::for_tests(&server.url());
        let scope = Scope::Repo(Repo::parse("o/r").unwrap());
        // Nothing has unknown mergeability, so settling fetches no second time.
        let o = Overview::load(&client, &scope).unwrap();
        light.assert();
        details.assert();

        let got: Vec<(u64, Category, Option<&str>)> = o
            .prs
            .iter()
            .map(|p| (p.number, p.category, p.reason.as_deref()))
            .collect();
        assert_eq!(
            got,
            vec![
                (1, Category::Ready, None),
                (2, Category::NeedsRebase, None),
                (3, Category::Blocked, Some("failing checks")),
                (4, Category::Ready, None),
            ]
        );
        let p = o.get("o/r", 1).unwrap();
        assert_eq!(p.node_id, "PR_1");
        assert_eq!(p.head_ref, "dependabot/cargo/pr-1");
        assert_eq!(p.checks.as_deref(), Some("SUCCESS"));
        assert_eq!(p.update.package, "serde");
        assert_eq!(p.size(), "1 file (+2 -1)");
        assert_eq!(p.label(true), "o/r#1");
        assert_eq!(o.get("o/r", 4).unwrap().checks, None);
        assert!(o.get("x/y", 1).is_none());
    }

    #[test]
    fn scopes_and_labels() {
        let server = Server::new();
        let c = ctx(&server, false);
        let s = Scope::new(&c, None).unwrap();
        assert!(!s.is_org());
        assert_eq!(s.to_string(), "o/r");
        let s = Scope::new(&c, Some("acme".into())).unwrap();
        assert!(s.is_org());
        assert_eq!(s.to_string(), "acme");

        assert_eq!(Category::NeedsRebase.label(), "needs rebase 🔄");
        assert_eq!(Change::Patch.badge(), "🟢 patch");
        assert!(Change::Other.risk().contains("semver"));
        assert!(is_dependabot(Some("app/dependabot")));
        assert!(!is_dependabot(None));

        let mut p = pr("o/r", 2, "Bump a from 1 to 2", Category::Ready);
        p.changed_files = 3;
        assert_eq!(p.size(), "3 files (+1 -1)");
        assert_eq!(p.label(false), "#2");
        assert_eq!(p.update.name(), "a");
        assert_eq!(
            parse_title("Bump the dev group across 1 directory").name(),
            "dev group"
        );
    }

    #[test]
    fn run_dispatches_to_subcommands() {
        let mut server = Server::new();
        let (light, _) = mock_prs(&mut server, false, &[]);
        let args = DependabotListArgs {
            org: None,
            security: false,
        };
        run(&ctx(&server, true), DependabotCommand::List(args)).unwrap();
        light.assert();
    }

    /// A PR for tests, parsed from a title with the given state.
    pub fn pr(repo: &str, number: u64, title: &str, category: Category) -> DepPr {
        DepPr {
            repo: repo.to_string(),
            number,
            node_id: format!("PR_{number}"),
            title: title.to_string(),
            url: format!("https://github.com/{repo}/pull/{number}"),
            created_at: DateTime::<Utc>::from_timestamp(0, 0).unwrap(),
            head_ref: format!("dependabot/npm_and_yarn/{number}"),
            mergeable: "MERGEABLE".into(),
            merge_state: "CLEAN".into(),
            review_decision: None,
            checks: Some("SUCCESS".into()),
            additions: 1,
            deletions: 1,
            changed_files: 1,
            update: parse_title(title),
            category,
            reason: None,
        }
    }

    #[test]
    fn title_plain_bump() {
        let u = parse_title("Bump @prisma/client from 6.19.3 to 7.10.1");
        assert_eq!(u.package, "@prisma/client");
        assert_eq!(u.from.as_deref(), Some("6.19.3"));
        assert_eq!(u.to.as_deref(), Some("7.10.1"));
        assert_eq!(u.change, Change::Major);
        assert!(!u.group);
    }

    #[test]
    fn title_conventional_prefixes_and_directory() {
        let u = parse_title("build(deps-dev): bump ipython from 8.17.2 to 9.17.1");
        assert_eq!(u.package, "ipython");
        assert_eq!(u.change, Change::Major);

        let u = parse_title("fix(deps)(deps): bump fastapi from 0.149.0 to 0.156.2 in /ml-service");
        assert_eq!(u.package, "fastapi");
        assert_eq!(u.directory.as_deref(), Some("/ml-service"));
        assert_eq!(u.change, Change::Minor);

        let u = parse_title(
            "build(deps): Bump body-parser from 2.3.0 to 2.4.0 in /workers/q in the npm_and_yarn group across 1 directory",
        );
        assert_eq!(u.package, "body-parser");
        assert_eq!(u.directory.as_deref(), Some("/workers/q"));
        assert!(!u.group);
    }

    #[test]
    fn title_requirement_update() {
        let u = parse_title(
            "fix(deps)(deps): update fastapi requirement from >=0.149.0 to >=0.156.2 in /ml-service/onnx-serving",
        );
        assert_eq!(u.package, "fastapi");
        assert_eq!(u.from.as_deref(), Some(">=0.149.0"));
        assert_eq!(u.change, Change::Minor);
    }

    #[test]
    fn title_group_update() {
        let u = parse_title("Bump the dev-dependencies group with 7 updates");
        assert!(u.group);
        assert_eq!(u.package, "dev-dependencies");
        assert_eq!(u.updates, Some(7));
        assert_eq!(u.name(), "dev-dependencies group (7 updates)");
        assert_eq!(u.versions(), "");

        let u = parse_title(
            "chore(deps): Bump the cargo-minor-and-patch group across 1 directory with 3 updates",
        );
        assert_eq!(u.package, "cargo-minor-and-patch");
        assert_eq!(u.updates, Some(3));
    }

    #[test]
    fn title_without_versions() {
        let u = parse_title("Bump picomatch");
        assert_eq!(u.package, "picomatch");
        assert_eq!(u.from, None);
        assert!(!u.group);

        let u = parse_title("chore(deps): bump webpack and webpack-cli in /web");
        assert_eq!(u.package, "webpack and webpack-cli");
        assert_eq!(u.directory.as_deref(), Some("/web"));
    }

    #[test]
    fn title_unrecognized_keeps_text() {
        let u = parse_title("Upgrade everything");
        assert_eq!(u.package, "Upgrade everything");
        assert_eq!(u.from, None);
        assert_eq!(u.change, Change::Other);
    }

    #[test]
    fn semver_components() {
        assert_eq!(semver_change("3", "4"), Change::Major);
        assert_eq!(semver_change("v1.2.3", "v1.3.0"), Change::Minor);
        assert_eq!(semver_change("1.2.3", "1.2.4"), Change::Patch);
        assert_eq!(semver_change("1.2", "1.2.1"), Change::Patch);
        assert_eq!(semver_change("1.2.3.4", "1.2.3.5"), Change::Patch);
        assert_eq!(semver_change("4.0.0-rc.18", "4.0.0"), Change::Other);
        assert_eq!(semver_change("latest", "1.0.0"), Change::Other);
        assert_eq!(semver_change("2024.7.4", "2026.10.19"), Change::Major);
    }

    #[test]
    fn categories() {
        assert_eq!(
            categorize("MERGEABLE", "CLEAN", Some("SUCCESS")).0,
            Category::Ready
        );
        assert_eq!(categorize("MERGEABLE", "BLOCKED", None).0, Category::Ready);
        assert_eq!(
            categorize("CONFLICTING", "DIRTY", Some("SUCCESS")).0,
            Category::NeedsRebase
        );
        assert_eq!(
            categorize("MERGEABLE", "BEHIND", Some("SUCCESS")).0,
            Category::NeedsRebase
        );
        assert_eq!(
            categorize("CONFLICTING", "DIRTY", Some("FAILURE")),
            (Category::Blocked, Some("failing checks".into()))
        );
        assert_eq!(
            categorize("MERGEABLE", "UNSTABLE", Some("PENDING")),
            (Category::Blocked, Some("checks pending".into()))
        );
        assert_eq!(
            categorize("UNKNOWN", "UNKNOWN", Some("SUCCESS")).0,
            Category::Blocked
        );
    }

    #[test]
    fn prefixes() {
        assert_eq!(
            package_prefix("@typescript-eslint/parser"),
            "@typescript-eslint"
        );
        assert_eq!(package_prefix("golang.org/x/net"), "golang.org/x");
        assert_eq!(
            package_prefix("io.github.revxrsal:lamp.common"),
            "io.github.revxrsal"
        );
        assert_eq!(package_prefix("serde_json"), "serde");
        assert_eq!(package_prefix("react-dom"), "react");
        assert_eq!(package_prefix("react"), "react");
    }

    #[test]
    fn lockstep_groups_need_matching_versions() {
        let prs = vec![
            pr(
                "o/r",
                13,
                "Bump @typescript-eslint/eslint-plugin from 6.0.0 to 7.0.0",
                Category::NeedsRebase,
            ),
            pr(
                "o/r",
                12,
                "Bump @typescript-eslint/parser from 6.0.0 to 7.0.0",
                Category::Ready,
            ),
            pr(
                "o/r",
                14,
                "Bump @typescript-eslint/utils from 6.0.0 to 7.1.0",
                Category::Ready,
            ),
            pr(
                "o/r",
                9,
                "Bump pino-http from 10.5.0 to 11.0.0",
                Category::Ready,
            ),
            pr(
                "o/r",
                10,
                "Bump pino from 9.14.0 to 10.4.0",
                Category::Ready,
            ),
            pr(
                "o/other",
                2,
                "Bump @typescript-eslint/parser from 6.0.0 to 7.0.0",
                Category::Ready,
            ),
        ];
        let groups = build_groups(&prs);
        assert_eq!(groups.len(), 1);
        let g = &groups[0];
        assert_eq!(g.prefix, "@typescript-eslint");
        assert_eq!(g.prs, vec![12, 13]);
        assert_eq!(g.status, Category::Ready);
        assert_eq!(g.change, Change::Major);
    }

    #[test]
    fn lockstep_joins_scoped_and_bare_names() {
        let prs = vec![
            pr(
                "o/r",
                13,
                "Bump next from 15.5.28 to 16.4.2 in /apps/web",
                Category::Ready,
            ),
            pr(
                "o/r",
                15,
                "Bump @next/eslint-plugin-next from 15.5.28 to 16.4.2",
                Category::Ready,
            ),
            pr(
                "o/r",
                20,
                "Bump typescript-eslint from 8.0.0 to 8.1.0",
                Category::Ready,
            ),
            pr(
                "o/r",
                21,
                "Bump @typescript-eslint/parser from 8.0.0 to 8.1.0",
                Category::Ready,
            ),
            pr(
                "o/r",
                22,
                "Bump @typescript-eslint/utils from 8.0.0 to 8.1.0",
                Category::Ready,
            ),
            pr(
                "o/r",
                30,
                "Bump nextra from 15.5.28 to 16.4.2",
                Category::Ready,
            ),
        ];
        let groups = build_groups(&prs);
        let found: Vec<(String, Vec<u64>)> = groups
            .iter()
            .map(|g| (g.prefix.clone(), g.prs.clone()))
            .collect();
        assert_eq!(
            found,
            vec![
                ("next".to_string(), vec![13, 15]),
                ("typescript-eslint".to_string(), vec![20, 21, 22]),
            ]
        );
    }

    #[test]
    fn lockstep_status_follows_first_or_blocked_member() {
        let prs = vec![
            pr(
                "o/r",
                1,
                "Bump serde from 1.0.1 to 1.0.2",
                Category::NeedsRebase,
            ),
            pr(
                "o/r",
                2,
                "Bump serde_json from 1.0.1 to 1.0.2",
                Category::Ready,
            ),
        ];
        assert_eq!(build_groups(&prs)[0].status, Category::NeedsRebase);

        let prs = vec![
            pr("o/r", 1, "Bump serde from 1.0.1 to 1.0.2", Category::Ready),
            pr(
                "o/r",
                2,
                "Bump serde_json from 1.0.1 to 1.0.2",
                Category::Blocked,
            ),
        ];
        assert_eq!(build_groups(&prs)[0].status, Category::Blocked);
    }

    #[test]
    fn overview_singles_exclude_grouped() {
        let o = Overview::new(vec![
            pr("o/r", 1, "Bump serde from 1.0.1 to 1.0.2", Category::Ready),
            pr(
                "o/r",
                2,
                "Bump serde_json from 1.0.1 to 1.0.2",
                Category::Ready,
            ),
            pr("o/r", 3, "Bump tokio from 1.0.0 to 1.1.0", Category::Ready),
        ]);
        let singles: Vec<u64> = o
            .singles(Category::Ready)
            .iter()
            .map(|p| p.number)
            .collect();
        assert_eq!(singles, vec![3]);
        assert_eq!(o.count(Category::Ready), 3);
    }
}
