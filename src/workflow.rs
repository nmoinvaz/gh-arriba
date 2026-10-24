use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde_json::{Map, Value};
use serde_yaml::Value as Yaml;
use std::collections::HashSet;
use std::time::Duration;

use crate::api::actions::{self, file_name as file, file_stem as stem};
use crate::api::{Client, Repo};

pub use crate::api::actions::Workflow;

/// Whether `query` names a workflow by its display name, file name, or file stem.
pub fn matches(name: &str, path: &str, query: &str) -> bool {
    name.eq_ignore_ascii_case(query)
        || file(path).eq_ignore_ascii_case(query)
        || stem(path).eq_ignore_ascii_case(query)
}

/// Active workflows in the repository.
pub fn list(client: &Client, repo: &Repo) -> Result<Vec<Workflow>> {
    actions::workflows(client, repo)
}

/// The single workflow in `found`, or an error that lists the candidates. `what` completes
/// "No workflow ..." and "Several workflows ...".
pub fn one_of<'a>(found: Vec<&'a Workflow>, all: &[Workflow], what: &str) -> Result<&'a Workflow> {
    match found.as_slice() {
        [w] => Ok(*w),
        [] => {
            let list: Vec<String> = all
                .iter()
                .map(|w| format!("{} ({})", w.name, w.path))
                .collect();
            bail!("No workflow {what}. Workflows: {}", list.join(", "))
        }
        many => {
            let names: Vec<&str> = many.iter().map(|w| w.name.as_str()).collect();
            bail!(
                "Several workflows {what}: {}. Pick one with -w.",
                names.join(", ")
            )
        }
    }
}

/// The workflow `query` names, by display name, file name, or file stem.
pub fn find<'a>(workflows: &'a [Workflow], query: &str) -> Result<&'a Workflow> {
    let found = workflows
        .iter()
        .filter(|w| matches(&w.name, &w.path, query))
        .collect();
    one_of(found, workflows, &format!("matches {query:?}"))
}

/// An input of the `workflow_dispatch` trigger.
#[derive(Clone, Debug, PartialEq)]
pub struct Input {
    pub name: String,
    pub description: String,
    pub required: bool,
    pub default: Option<String>,
    pub kind: String,
    pub options: Vec<String>,
}

/// Inputs of the `workflow_dispatch` trigger, in file order.
pub fn dispatch_inputs(yaml: &str) -> Result<Vec<Input>> {
    let doc: Yaml = serde_yaml::from_str(yaml).context("Failed to parse workflow YAML")?;
    // YAML 1.1 parsers read a bare `on` key as true, so accept either spelling.
    let on = doc
        .get("on")
        .or_else(|| doc.as_mapping().and_then(|m| m.get(Yaml::Bool(true))))
        .context("Workflow has no `on` triggers")?;
    let dispatch = match on {
        Yaml::String(s) if s == "workflow_dispatch" => return Ok(Vec::new()),
        Yaml::Sequence(items)
            if items
                .iter()
                .any(|i| i.as_str() == Some("workflow_dispatch")) =>
        {
            return Ok(Vec::new());
        }
        Yaml::Mapping(m) => m
            .get("workflow_dispatch")
            .context("Workflow has no workflow_dispatch trigger")?,
        _ => bail!("Workflow has no workflow_dispatch trigger"),
    };
    let Some(inputs) = dispatch.get("inputs").and_then(Yaml::as_mapping) else {
        return Ok(Vec::new());
    };
    Ok(inputs
        .iter()
        .filter_map(|(name, spec)| {
            Some(Input {
                name: name.as_str()?.to_string(),
                description: spec
                    .get("description")
                    .and_then(Yaml::as_str)
                    .unwrap_or_default()
                    .to_string(),
                required: spec
                    .get("required")
                    .and_then(Yaml::as_bool)
                    .unwrap_or(false),
                default: spec.get("default").and_then(scalar),
                kind: spec
                    .get("type")
                    .and_then(Yaml::as_str)
                    .unwrap_or("string")
                    .to_string(),
                options: spec
                    .get("options")
                    .and_then(Yaml::as_sequence)
                    .map(|s| s.iter().filter_map(scalar).collect())
                    .unwrap_or_default(),
            })
        })
        .collect())
}

/// A YAML scalar as the string the dispatch API expects.
fn scalar(v: &Yaml) -> Option<String> {
    match v {
        Yaml::String(s) => Some(s.clone()),
        Yaml::Bool(b) => Some(b.to_string()),
        Yaml::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// Inputs the workflow declares at `git_ref`.
pub fn inputs_at(
    client: &Client,
    repo: &Repo,
    workflow: &Workflow,
    git_ref: &str,
) -> Result<Vec<Input>> {
    let yaml = actions::workflow_file(client, repo, &workflow.path, git_ref)?;
    dispatch_inputs(&yaml)
        .map_err(|e| anyhow::anyhow!("{} ({}): {e}", workflow.name, workflow.path))
}

/// Parse `-f KEY=VALUE` pairs, rejecting inputs the workflow does not declare.
pub fn parse_fields(fields: &[String], inputs: &[Input]) -> Result<Vec<(String, String)>> {
    let mut parsed = Vec::new();
    for f in fields {
        let (key, value) = f
            .split_once('=')
            .with_context(|| format!("Expected KEY=VALUE, got {f:?}"))?;
        // GitHub rejects the whole dispatch when it gets an input the workflow does not declare.
        if !inputs.iter().any(|i| i.name == key) {
            let names: Vec<&str> = inputs.iter().map(|i| i.name.as_str()).collect();
            bail!(
                "{key} is not an input of this workflow. Inputs: {}",
                names.join(", ")
            );
        }
        parsed.push((key.to_string(), value.to_string()));
    }
    Ok(parsed)
}

/// Required inputs with no default that `fields` leaves unset.
pub fn missing<'a>(inputs: &'a [Input], fields: &[(String, String)]) -> Vec<&'a str> {
    inputs
        .iter()
        .filter(|i| i.required && i.default.is_none())
        .filter(|i| !fields.iter().any(|(k, _)| *k == i.name))
        .map(|i| i.name.as_str())
        .collect()
}

/// A run started by a dispatch.
#[derive(Clone, Debug, Serialize)]
pub struct NewRun {
    pub id: u64,
    pub url: String,
}

/// Dispatch the workflow on `git_ref`, then wait briefly for the run it starts. None when the
/// run does not show up in time.
pub fn dispatch(
    client: &Client,
    repo: &Repo,
    workflow: &Workflow,
    git_ref: &str,
    inputs: Map<String, Value>,
) -> Result<Option<NewRun>> {
    // Runs that existed before dispatching, so the new one can be told apart without clocks.
    let before: HashSet<u64> = actions::workflow_dispatch_runs(client, repo, workflow.id)?
        .iter()
        .map(|r| r.id)
        .collect();

    actions::dispatch_workflow(client, repo, workflow.id, git_ref, inputs)?;

    let viewer = client.viewer()?.to_string();
    for _ in 0..10 {
        std::thread::sleep(Duration::from_millis(1500));
        let runs = actions::workflow_dispatch_runs(client, repo, workflow.id)?;
        let new_run = runs
            .into_iter()
            .find(|r| !before.contains(&r.id) && r.started_by() == Some(viewer.as_str()));
        if let Some(r) = new_run {
            return Ok(Some(NewRun {
                id: r.id,
                url: r.html_url,
            }));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mockito::{Matcher, Server};
    use serde_json::json;

    fn wf(id: u64, name: &str, path: &str) -> Workflow {
        Workflow {
            id,
            name: name.to_string(),
            path: path.to_string(),
        }
    }

    #[test]
    fn fields_must_be_declared_inputs() {
        let inputs = dispatch_inputs(
            "on:\n  workflow_dispatch:\n    inputs:\n      version:\n        required: true\n",
        )
        .unwrap();
        assert_eq!(
            parse_fields(&["version=1.2=3".to_string()], &inputs).unwrap(),
            vec![("version".to_string(), "1.2=3".to_string())]
        );
        assert!(parse_fields(&["channel=beta".to_string()], &inputs).is_err());
        assert!(parse_fields(&["version".to_string()], &inputs).is_err());

        assert_eq!(missing(&inputs, &[]), vec!["version"]);
        let set = vec![("version".to_string(), "1.0".to_string())];
        assert!(missing(&inputs, &set).is_empty());
    }

    #[test]
    fn matches_name_file_or_stem() {
        let path = ".github/workflows/build.yaml";
        assert!(matches("Build", path, "build"));
        assert!(matches("CI", path, "build.yaml"));
        assert!(matches("CI", path, "BUILD"));
        assert!(!matches("CI", path, "lint"));
    }

    #[test]
    fn find_names_the_candidates() {
        let all = vec![
            wf(1, "Build", ".github/workflows/build.yml"),
            wf(2, "Release", ".github/workflows/release.yml"),
            wf(3, "release", ".github/workflows/publish.yml"),
        ];
        assert_eq!(find(&all, "build.yml").unwrap().id, 1);
        assert_eq!(
            find(&all, "lint").unwrap_err().to_string(),
            "No workflow matches \"lint\". Workflows: Build (.github/workflows/build.yml), \
             Release (.github/workflows/release.yml), release (.github/workflows/publish.yml)"
        );
        assert_eq!(
            find(&all, "release").unwrap_err().to_string(),
            "Several workflows matches \"release\": Release, release. Pick one with -w."
        );
    }

    #[test]
    fn dispatch_inputs_in_every_trigger_shape() {
        let inputs = dispatch_inputs(
            "name: Release\n\
             on:\n  push:\n  workflow_dispatch:\n    inputs:\n      \
             bump:\n        description: Part to bump\n        type: choice\n        \
             options: [patch, minor, 3]\n        default: patch\n      \
             dry:\n        type: boolean\n        default: false\n      \
             count:\n        default: 2\n",
        )
        .unwrap();
        assert_eq!(
            inputs[0],
            Input {
                name: "bump".to_string(),
                description: "Part to bump".to_string(),
                required: false,
                default: Some("patch".to_string()),
                kind: "choice".to_string(),
                options: vec!["patch".into(), "minor".into(), "3".into()],
            }
        );
        assert_eq!(inputs[1].default.as_deref(), Some("false"));
        assert_eq!(inputs[1].kind, "boolean");
        assert_eq!(
            (inputs[2].kind.as_str(), inputs[2].default.as_deref()),
            ("string", Some("2"))
        );

        assert!(
            dispatch_inputs("on: workflow_dispatch\n")
                .unwrap()
                .is_empty()
        );
        assert!(
            dispatch_inputs("on: [push, workflow_dispatch]\n")
                .unwrap()
                .is_empty()
        );
        assert!(
            dispatch_inputs("on:\n  workflow_dispatch:\n")
                .unwrap()
                .is_empty()
        );
        // `true:` is how YAML 1.1 reads a bare `on`.
        assert!(
            dispatch_inputs("true:\n  workflow_dispatch: {}\n")
                .unwrap()
                .is_empty()
        );
        for bad in [
            "on: push\n",
            "on:\n  push: {}\n",
            "jobs: {}\n",
            "on: [push]\n",
        ] {
            assert!(dispatch_inputs(bad).is_err(), "{bad}");
        }
        assert!(dispatch_inputs("on: [").is_err());
    }

    #[test]
    fn inputs_at_reads_the_file_at_the_ref() {
        let mut server = Server::new();
        server
            .mock("GET", "/repos/o/r/contents/.github/workflows/ci.yml")
            .match_query(Matcher::UrlEncoded("ref".into(), "v1".into()))
            .with_body("on: push\n")
            .create();
        let client = Client::for_tests(&server.url());
        let repo = Repo::parse("o/r").unwrap();
        let err = inputs_at(
            &client,
            &repo,
            &wf(1, "CI", ".github/workflows/ci.yml"),
            "v1",
        )
        .unwrap_err()
        .to_string();
        assert_eq!(
            err,
            "CI (.github/workflows/ci.yml): Workflow has no workflow_dispatch trigger"
        );
    }

    #[test]
    fn dispatch_returns_the_viewers_new_run() {
        let mut server = Server::new();
        server
            .mock("GET", "/repos/o/r/actions/workflows/7/runs")
            .match_query(Matcher::Any)
            .with_body(r#"{"workflow_runs": [{"id": 1, "actor": {"login": "me"}}]}"#)
            .expect(1)
            .create();
        server
            .mock("GET", "/repos/o/r/actions/workflows/7/runs")
            .match_query(Matcher::Any)
            .with_body(
                r#"{"workflow_runs": [
                    {"id": 3, "actor": {"login": "someone"}},
                    {"id": 2, "triggering_actor": {"login": "me"}, "html_url": "https://h/2"},
                    {"id": 1, "actor": {"login": "me"}}
                ]}"#,
            )
            .create();
        let sent = server
            .mock("POST", "/repos/o/r/actions/workflows/7/dispatches")
            .match_body(Matcher::Json(json!({"ref": "dev", "inputs": {}})))
            .with_status(204)
            .create();
        server
            .mock("POST", "/graphql")
            .with_body(r#"{"data": {"viewer": {"login": "me"}}}"#)
            .create();
        let client = Client::for_tests(&server.url());
        let repo = Repo::parse("o/r").unwrap();
        let run = dispatch(
            &client,
            &repo,
            &wf(7, "Release", ".github/workflows/release.yml"),
            "dev",
            Map::new(),
        )
        .unwrap()
        .unwrap();
        assert_eq!((run.id, run.url.as_str()), (2, "https://h/2"));
        sent.assert();
    }
}
