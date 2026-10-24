use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde_json::{Map, Value, json};
use serde_yaml::Value as Yaml;
use std::collections::HashSet;
use std::time::Duration;

use crate::api::json::str_of;
use crate::api::{Client, Repo};

/// An Actions workflow, as the workflows API lists it.
#[derive(Clone, Debug)]
pub struct Workflow {
    pub id: u64,
    pub name: String,
    pub path: String,
}

impl Workflow {
    /// File name without its directory or `.yml` extension.
    pub fn stem(&self) -> &str {
        stem(&self.path)
    }

    pub fn file(&self) -> &str {
        file(&self.path)
    }
}

fn file(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

fn stem(path: &str) -> &str {
    let base = file(path);
    base.strip_suffix(".yml")
        .or_else(|| base.strip_suffix(".yaml"))
        .unwrap_or(base)
}

/// Whether `query` names a workflow by its display name, file name, or file stem.
pub fn matches(name: &str, path: &str, query: &str) -> bool {
    name.eq_ignore_ascii_case(query)
        || file(path).eq_ignore_ascii_case(query)
        || stem(path).eq_ignore_ascii_case(query)
}

/// Active workflows in the repository.
pub fn list(client: &Client, repo: &Repo) -> Result<Vec<Workflow>> {
    Ok(client
        .get_all_in(&format!("{}/actions/workflows", repo.api()), "workflows")?
        .iter()
        .filter(|w| w["state"].as_str() == Some("active"))
        .filter_map(|w| {
            Some(Workflow {
                id: w["id"].as_u64()?,
                name: w["name"].as_str()?.to_string(),
                path: w["path"].as_str()?.to_string(),
            })
        })
        .collect())
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
    let yaml = client.get_raw(&format!(
        "{}/contents/{}?ref={git_ref}",
        repo.api(),
        workflow.path
    ))?;
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
    let runs_path = format!(
        "{}/actions/workflows/{}/runs?event=workflow_dispatch&per_page=10",
        repo.api(),
        workflow.id
    );
    // Runs that existed before dispatching, so the new one can be told apart without clocks.
    let before: HashSet<u64> = run_ids(&client.get(&runs_path)?);

    client.post(
        &format!(
            "{}/actions/workflows/{}/dispatches",
            repo.api(),
            workflow.id
        ),
        &json!({ "ref": git_ref, "inputs": inputs }),
    )?;

    let viewer = client.viewer()?.to_string();
    for _ in 0..10 {
        std::thread::sleep(Duration::from_millis(1500));
        let runs = client.get(&runs_path)?;
        let new_run = runs["workflow_runs"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|r| {
                let id = r["id"].as_u64().unwrap_or(0);
                let actor = r["triggering_actor"]["login"]
                    .as_str()
                    .or(r["actor"]["login"].as_str());
                !before.contains(&id) && actor == Some(viewer.as_str())
            });
        if let Some(r) = new_run {
            return Ok(Some(NewRun {
                id: r["id"].as_u64().unwrap_or(0),
                url: str_of(r, "html_url"),
            }));
        }
    }
    Ok(None)
}

fn run_ids(runs: &Value) -> HashSet<u64> {
    runs["workflow_runs"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|r| r["id"].as_u64())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
