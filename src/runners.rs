use anyhow::{Result, bail};
use serde::Serialize;
use serde_json::Value;

use crate::cli::RunnersArgs;
use crate::context::Ctx;
use crate::dependabot::{Scope, table};
use crate::output;

// -------------------------------------------------------------------------
//  runners
// -------------------------------------------------------------------------

/// A self-hosted runner as the runners API reports it.
#[derive(Debug, PartialEq, Serialize)]
struct Runner {
    id: u64,
    name: String,
    /// "online" or "offline".
    status: String,
    busy: bool,
    os: String,
    labels: Vec<String>,
}

impl Runner {
    /// Status column: busy, idle, or offline.
    fn state(&self) -> &str {
        if self.status != "online" {
            "offline"
        } else if self.busy {
            "busy"
        } else {
            "idle"
        }
    }
}

fn parse_runner(v: &Value) -> Runner {
    Runner {
        id: v["id"].as_u64().unwrap_or(0),
        name: v["name"].as_str().unwrap_or_default().to_string(),
        status: v["status"].as_str().unwrap_or_default().to_string(),
        busy: v["busy"].as_bool().unwrap_or(false),
        os: v["os"].as_str().unwrap_or_default().to_string(),
        labels: v["labels"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|l| l["name"].as_str().map(str::to_string))
            .collect(),
    }
}

/// Runners API path for the scope.
fn runners_path(scope: &Scope) -> String {
    match scope {
        Scope::Repo(r) => format!("{}/actions/runners", r.api()),
        Scope::Org(o) => format!("orgs/{o}/actions/runners"),
    }
}

/// Whether an API error means the token lacks the access the runners API needs.
fn is_forbidden(e: &anyhow::Error) -> bool {
    let text = format!("{e:#}");
    text.contains("GitHub API 403") || text.contains("GitHub API 404")
}

/// What to do when the runners API refuses the token.
fn access_hint(scope: &Scope) -> String {
    match scope {
        Scope::Org(o) => format!(
            "Listing runners for {o} needs an org admin and the admin:org scope. Run `gh auth refresh -s admin:org`."
        ),
        Scope::Repo(r) => format!("Listing runners for {r} needs admin access to the repo."),
    }
}

// -------------------------------------------------------------------------
//  run
// -------------------------------------------------------------------------

#[derive(Serialize)]
struct Report {
    scope: String,
    runners: Vec<Runner>,
}

pub fn run(ctx: &Ctx, args: RunnersArgs) -> Result<()> {
    let scope = Scope::new(ctx, args.org)?;
    let mut runners: Vec<Runner> = match ctx.client.get_all_in(&runners_path(&scope), "runners") {
        Ok(list) => list.iter().map(parse_runner).collect(),
        Err(e) if is_forbidden(&e) => bail!("{}\n{e:#}", access_hint(&scope)),
        Err(e) => return Err(e),
    };
    runners.sort_by_key(|r| r.name.to_lowercase());

    let report = Report {
        scope: scope.to_string(),
        runners,
    };
    if ctx.json {
        return output::print_json(&report);
    }
    print!("{}", render(&report, &scope));
    Ok(())
}

/// Runner table followed by the count summary.
fn render(report: &Report, scope: &Scope) -> String {
    if report.runners.is_empty() {
        let hint = match scope {
            Scope::Repo(_) => " Runners registered to the organization show with --org ORG.",
            Scope::Org(_) => "",
        };
        return format!("No self-hosted runners in {}.{hint}\n", report.scope);
    }
    let mut rows = vec![vec![
        "NAME".to_string(),
        "STATUS".to_string(),
        "OS".to_string(),
        "LABELS".to_string(),
    ]];
    for r in &report.runners {
        rows.push(vec![
            r.name.clone(),
            r.state().to_string(),
            r.os.clone(),
            r.labels.join(", "),
        ]);
    }
    format!("{}\n{}\n", table(&rows, ""), summary(&report.runners))
}

/// Counts line, as `12 runners: 9 online (4 busy), 3 offline`.
fn summary(runners: &[Runner]) -> String {
    let online = runners.iter().filter(|r| r.status == "online").count();
    let busy = runners
        .iter()
        .filter(|r| r.status == "online" && r.busy)
        .count();
    let offline = runners.len() - online;
    let noun = if runners.len() == 1 {
        "runner"
    } else {
        "runners"
    };
    format!(
        "{} {noun}: {online} online ({busy} busy), {offline} offline",
        runners.len()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::Repo;
    use serde_json::json;

    fn runner(name: &str, status: &str, busy: bool) -> Runner {
        parse_runner(&json!({
            "id": 1, "name": name, "os": "Linux", "status": status, "busy": busy,
            "labels": [
                { "id": 1, "name": "self-hosted", "type": "read-only" },
                { "id": 2, "name": "linux", "type": "custom" }
            ]
        }))
    }

    #[test]
    fn parses_runner_and_state() {
        let r = runner("build-1", "online", true);
        assert_eq!(r.labels, vec!["self-hosted", "linux"]);
        assert_eq!(r.state(), "busy");
        assert_eq!(runner("b", "online", false).state(), "idle");
        // An offline runner can still report busy from its last job.
        assert_eq!(runner("c", "offline", true).state(), "offline");
    }

    #[test]
    fn summary_counts() {
        let runners = vec![
            runner("a", "online", true),
            runner("b", "online", false),
            runner("c", "offline", true),
        ];
        assert_eq!(summary(&runners), "3 runners: 2 online (1 busy), 1 offline");
        assert_eq!(
            summary(&runners[..1]),
            "1 runner: 1 online (1 busy), 0 offline"
        );
    }

    #[test]
    fn render_table_and_empty_hint() {
        let scope = Scope::Repo(Repo::parse("a/b").unwrap());
        let report = Report {
            scope: "a/b".to_string(),
            runners: vec![runner("build-1", "online", false)],
        };
        assert_eq!(
            render(&report, &scope),
            "NAME     STATUS  OS     LABELS\n\
             build-1  idle    Linux  self-hosted, linux\n\
             1 runner: 1 online (0 busy), 0 offline\n"
        );
        let empty = Report {
            scope: "a/b".to_string(),
            runners: Vec::new(),
        };
        assert!(render(&empty, &scope).contains("--org ORG"));
    }

    #[test]
    fn forbidden_detection() {
        assert!(is_forbidden(&anyhow::anyhow!(
            "GitHub API 403 on GET orgs/x/actions/runners: Must have admin rights"
        )));
        assert!(!is_forbidden(&anyhow::anyhow!(
            "GitHub API 500 on GET x: oops"
        )));
        assert!(access_hint(&Scope::Org("x".to_string())).contains("admin:org"));
    }
}
