use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use super::{WorkflowRun, parse, parse_all};
use crate::api::{Client, Repo};

/// An Actions workflow, as the workflows API lists it.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Workflow {
    pub id: u64,
    pub name: String,
    pub path: String,
}

impl Workflow {
    /// File name without its directory or `.yml` extension.
    pub fn stem(&self) -> &str {
        file_stem(&self.path)
    }

    pub fn file(&self) -> &str {
        file_name(&self.path)
    }
}

/// File name of a workflow path, without its directory.
pub fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// File name of a workflow path without its directory or `.yml` extension.
pub fn file_stem(path: &str) -> &str {
    let base = file_name(path);
    base.strip_suffix(".yml")
        .or_else(|| base.strip_suffix(".yaml"))
        .unwrap_or(base)
}

/// A workflow as listed, with the state that tells active ones apart.
#[derive(Deserialize)]
struct Listed {
    #[serde(flatten)]
    workflow: Workflow,
    state: Option<String>,
}

/// Active workflows in the repository.
pub fn workflows(client: &Client, repo: &Repo) -> Result<Vec<Workflow>> {
    let listed: Vec<Listed> = parse_all(
        client.get_all_in(&format!("{}/actions/workflows", repo.api()), "workflows")?,
        "workflow",
    )?;
    Ok(listed
        .into_iter()
        .filter(|w| w.state.as_deref() == Some("active"))
        .map(|w| w.workflow)
        .collect())
}

/// The workflow file at `git_ref`, as raw YAML.
pub fn workflow_file(client: &Client, repo: &Repo, path: &str, git_ref: &str) -> Result<String> {
    client.get_raw(&format!("{}/contents/{path}?ref={git_ref}", repo.api()))
}

/// The ten newest runs of a workflow started by a dispatch.
pub fn workflow_dispatch_runs(
    client: &Client,
    repo: &Repo,
    workflow_id: u64,
) -> Result<Vec<WorkflowRun>> {
    #[derive(Deserialize)]
    struct Page {
        #[serde(default)]
        workflow_runs: Vec<WorkflowRun>,
    }
    let page: Page = parse(
        client.get(&format!(
            "{}/actions/workflows/{workflow_id}/runs?event=workflow_dispatch&per_page=10",
            repo.api()
        ))?,
        "workflow runs",
    )?;
    Ok(page.workflow_runs)
}

/// Start the workflow on `git_ref` with `inputs`.
pub fn dispatch_workflow(
    client: &Client,
    repo: &Repo,
    workflow_id: u64,
    git_ref: &str,
    inputs: Map<String, Value>,
) -> Result<()> {
    client.post(
        &format!("{}/actions/workflows/{workflow_id}/dispatches", repo.api()),
        &json!({ "ref": git_ref, "inputs": inputs }),
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listing_keeps_fields_and_state() {
        let w: Listed = serde_json::from_value(json!({
            "id": 7, "name": "CI", "path": ".github/workflows/ci.yaml", "state": "active",
            "node_id": "W_1", "badge_url": "https://example.com"
        }))
        .unwrap();
        assert_eq!(w.state.as_deref(), Some("active"));
        assert_eq!((w.workflow.id, w.workflow.name.as_str()), (7, "CI"));
        assert_eq!(w.workflow.file(), "ci.yaml");
        assert_eq!(w.workflow.stem(), "ci");
    }
}
