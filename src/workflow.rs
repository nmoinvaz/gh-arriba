use anyhow::{Result, bail};

use crate::context::Repo;
use crate::github::Client;

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_name_file_or_stem() {
        let path = ".github/workflows/build.yaml";
        assert!(matches("Build", path, "build"));
        assert!(matches("CI", path, "build.yaml"));
        assert!(matches("CI", path, "BUILD"));
        assert!(!matches("CI", path, "lint"));
    }
}
