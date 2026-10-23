use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde_json::{Value, json};
use serde_yaml::Value as Yaml;
use std::collections::HashSet;
use std::process::Command;
use std::time::Duration;

use crate::cli::{Bump, ReleaseArgs};
use crate::context::{Ctx, Repo};
use crate::github::Client;
use crate::{output, prompt};

// -------------------------------------------------------------------------
//  versions
// -------------------------------------------------------------------------

#[derive(Debug, PartialEq)]
struct Version {
    /// Text before the first digit, such as `v` or `release-`.
    prefix: String,
    major: u64,
    minor: u64,
    patch: u64,
}

impl Version {
    /// Parse a tag like `v1.2.3`, `1.2`, or `1.2.3-rc.1`. Prerelease and build suffixes are dropped.
    fn parse(tag: &str) -> Option<Self> {
        let start = tag.find(|c: char| c.is_ascii_digit())?;
        let core = tag[start..].split(['-', '+']).next()?;
        let parts: Vec<u64> = core
            .split('.')
            .map(|p| p.parse().ok())
            .collect::<Option<_>>()?;
        if parts.is_empty() || parts.len() > 3 {
            return None;
        }
        Some(Self {
            prefix: tag[..start].to_string(),
            major: parts[0],
            minor: parts.get(1).copied().unwrap_or(0),
            patch: parts.get(2).copied().unwrap_or(0),
        })
    }

    fn bump(&self, part: Bump) -> Self {
        let (major, minor, patch) = match part {
            Bump::Major => (self.major + 1, 0, 0),
            Bump::Minor => (self.major, self.minor + 1, 0),
            Bump::Patch => (self.major, self.minor, self.patch + 1),
        };
        Self {
            prefix: self.prefix.clone(),
            major,
            minor,
            patch,
        }
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}{}.{}.{}",
            self.prefix, self.major, self.minor, self.patch
        )
    }
}

/// Tag of the newest published release, skipping drafts.
fn latest_release_tag(client: &Client, repo: &Repo) -> Result<Option<String>> {
    let releases = client.get(&format!("{}/releases?per_page=10", repo.api()))?;
    Ok(releases
        .as_array()
        .into_iter()
        .flatten()
        .find(|r| r["draft"].as_bool() != Some(true))
        .and_then(|r| r["tag_name"].as_str())
        .map(str::to_string))
}

// -------------------------------------------------------------------------
//  workflow
// -------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct Workflow {
    id: u64,
    name: String,
    path: String,
}

impl Workflow {
    /// File name without its directory or `.yml` extension.
    fn stem(&self) -> &str {
        let base = self.path.rsplit('/').next().unwrap_or(&self.path);
        base.strip_suffix(".yml")
            .or_else(|| base.strip_suffix(".yaml"))
            .unwrap_or(base)
    }

    fn file(&self) -> &str {
        self.path.rsplit('/').next().unwrap_or(&self.path)
    }
}

/// Pick the workflow named by `-w`, or the one that looks like the release workflow.
fn pick_workflow<'a>(workflows: &'a [Workflow], name: Option<&str>) -> Result<&'a Workflow> {
    let list = || {
        workflows
            .iter()
            .map(|w| format!("{} ({})", w.name, w.path))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let one = |found: Vec<&'a Workflow>, what: &str| -> Result<&'a Workflow> {
        match found.as_slice() {
            [w] => Ok(*w),
            [] => bail!("No workflow {what}. Workflows: {}", list()),
            many => {
                let names: Vec<&str> = many.iter().map(|w| w.name.as_str()).collect();
                bail!(
                    "Several workflows {what}: {}. Pick one with -w.",
                    names.join(", ")
                )
            }
        }
    };

    if let Some(name) = name {
        let found = workflows
            .iter()
            .filter(|w| {
                w.name.eq_ignore_ascii_case(name)
                    || w.file().eq_ignore_ascii_case(name)
                    || w.stem().eq_ignore_ascii_case(name)
            })
            .collect();
        return one(found, &format!("matches {name:?}"));
    }

    let exact: Vec<&Workflow> = workflows
        .iter()
        .filter(|w| {
            w.name.eq_ignore_ascii_case("release") || w.stem().eq_ignore_ascii_case("release")
        })
        .collect();
    if !exact.is_empty() {
        return one(exact, "is named release");
    }
    let partial = workflows
        .iter()
        .filter(|w| {
            w.name.to_lowercase().contains("release") || w.stem().to_lowercase().contains("release")
        })
        .collect();
    one(partial, "mentions release")
}

#[derive(Clone, Debug, PartialEq)]
struct Input {
    name: String,
    description: String,
    required: bool,
    default: Option<String>,
    kind: String,
    options: Vec<String>,
}

/// Inputs of the `workflow_dispatch` trigger, in file order.
fn dispatch_inputs(yaml: &str) -> Result<Vec<Input>> {
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

/// The input that carries the version, by exact name, then by name containing version or tag.
fn version_input(inputs: &[Input]) -> Result<&Input> {
    if let Some(i) = inputs
        .iter()
        .find(|i| i.name.eq_ignore_ascii_case("version"))
    {
        return Ok(i);
    }
    for needle in ["version", "tag"] {
        let found: Vec<&Input> = inputs
            .iter()
            .filter(|i| i.name.to_lowercase().contains(needle))
            .collect();
        match found.as_slice() {
            [i] => return Ok(i),
            [] => continue,
            many => {
                let names: Vec<&str> = many.iter().map(|i| i.name.as_str()).collect();
                bail!(
                    "Several inputs could hold the version: {}",
                    names.join(", ")
                );
            }
        }
    }
    let names: Vec<&str> = inputs.iter().map(|i| i.name.as_str()).collect();
    bail!(
        "No version input in the workflow. Inputs: {}",
        names.join(", ")
    )
}

// -------------------------------------------------------------------------
//  plan
// -------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct PlanInput {
    name: String,
    value: Option<String>,
    /// "version", "set", "prompt", "default", or "unset". Only the first three are sent.
    source: String,
}

impl PlanInput {
    fn sent(&self) -> bool {
        matches!(self.source.as_str(), "version" | "set" | "prompt")
    }
}

#[derive(Debug, Serialize)]
struct Plan {
    repo: String,
    workflow: String,
    workflow_path: String,
    #[serde(rename = "ref")]
    git_ref: String,
    previous: Option<String>,
    version: String,
    inputs: Vec<PlanInput>,
    run_id: Option<u64>,
    run_url: Option<String>,
}

/// Parse `-f KEY=VALUE` pairs, rejecting the version input and undeclared inputs.
fn parse_fields(
    fields: &[String],
    inputs: &[Input],
    version: &str,
) -> Result<Vec<(String, String)>> {
    let mut parsed = Vec::new();
    for f in fields {
        let (key, value) = f
            .split_once('=')
            .with_context(|| format!("Expected KEY=VALUE, got {f:?}"))?;
        if key == version {
            bail!("Set the version with --version, not -f {key}=...");
        }
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

/// Resolve every declared input's value, prompting for required ones that have none.
fn resolve_inputs(
    inputs: &[Input],
    version_name: &str,
    version: &str,
    fields: &[(String, String)],
) -> Result<Vec<PlanInput>> {
    let mut plan = Vec::new();
    let mut missing = Vec::new();
    for input in inputs {
        let set = fields.iter().rev().find(|(k, _)| *k == input.name);
        let (value, source) = if input.name == version_name {
            (Some(version.to_string()), "version")
        } else if let Some((_, v)) = set {
            (Some(v.clone()), "set")
        } else if let Some(d) = &input.default {
            (Some(d.clone()), "default")
        } else if input.required {
            if !prompt::interactive() {
                missing.push(input.name.clone());
                continue;
            }
            (Some(ask(input)?), "prompt")
        } else {
            (None, "unset")
        };
        plan.push(PlanInput {
            name: input.name.clone(),
            value,
            source: source.to_string(),
        });
    }
    if !missing.is_empty() {
        bail!(
            "Required inputs have no value: {}. Set them with -f KEY=VALUE.",
            missing.join(", ")
        );
    }
    Ok(plan)
}

fn ask(input: &Input) -> Result<String> {
    let label = if input.description.is_empty() {
        input.name.clone()
    } else {
        format!("{} ({})", input.name, input.description)
    };
    let choices: Vec<String> = match input.kind.as_str() {
        "boolean" => vec!["true".to_string(), "false".to_string()],
        "choice" => input.options.clone(),
        _ => Vec::new(),
    };
    if choices.is_empty() {
        return Ok(dialoguer::Input::<String>::new()
            .with_prompt(label)
            .interact_text()?);
    }
    let idx = prompt::select(&label, &choices, 0)?;
    Ok(choices[idx].clone())
}

fn render(plan: &Plan) -> String {
    let mut out = format!(
        "Repository: {}\nWorkflow:   {} ({})\nRef:        {}\nPrevious:   {}\nVersion:    {}\n",
        plan.repo,
        plan.workflow,
        plan.workflow_path,
        plan.git_ref,
        plan.previous.as_deref().unwrap_or("none"),
        plan.version,
    );
    if !plan.inputs.is_empty() {
        out.push_str("Inputs:\n");
        let width = plan.inputs.iter().map(|i| i.name.len()).max().unwrap_or(0);
        for i in &plan.inputs {
            let value = i.value.as_deref().unwrap_or("");
            let note = match i.source.as_str() {
                "default" => "  (default)",
                "unset" => "  (unset)",
                _ => "",
            };
            out.push_str(&format!("  {:<width$}  {value}{note}\n", i.name));
        }
    }
    out
}

// -------------------------------------------------------------------------
//  run
// -------------------------------------------------------------------------

pub fn run(ctx: &Ctx, args: ReleaseArgs) -> Result<()> {
    let repo = ctx.repo()?;
    let client = &ctx.client;

    let info = client.get(&repo.api())?;
    let git_ref = match args.git_ref {
        Some(r) => r,
        None => info["default_branch"]
            .as_str()
            .context("Repository has no default branch")?
            .to_string(),
    };

    let previous = latest_release_tag(client, &repo)?;
    let version = match (&args.version, &previous) {
        (Some(v), _) => v.clone(),
        (None, Some(tag)) => Version::parse(tag)
            .with_context(|| format!("Can't read a version from tag {tag}. Pass --version."))?
            .bump(args.bump)
            .to_string(),
        (None, None) => "1.0.0".to_string(),
    };

    let workflows: Vec<Workflow> = client
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
        .collect();
    let workflow = pick_workflow(&workflows, args.workflow.as_deref())?;

    let yaml = client.get_raw(&format!(
        "{}/contents/{}?ref={git_ref}",
        repo.api(),
        workflow.path
    ))?;
    let inputs = dispatch_inputs(&yaml)
        .map_err(|e| anyhow::anyhow!("{} ({}): {e}", workflow.name, workflow.path))?;
    let version_name = version_input(&inputs)?.name.clone();
    let fields = parse_fields(&args.fields, &inputs, &version_name)?;
    let resolved = resolve_inputs(&inputs, &version_name, &version, &fields)?;

    let mut plan = Plan {
        repo: repo.to_string(),
        workflow: workflow.name.clone(),
        workflow_path: workflow.path.clone(),
        git_ref,
        previous,
        version,
        inputs: resolved,
        run_id: None,
        run_url: None,
    };

    // With --json the plan goes to stderr, so stdout carries only the final document.
    if ctx.json {
        eprint!("{}", render(&plan));
    } else {
        print!("{}", render(&plan));
    }
    let question = format!("Dispatch {} for {}?", plan.workflow, plan.version);
    if !prompt::confirm(&question, ctx.yes)? {
        println!("Cancelled");
        return Ok(());
    }

    let runs_path = format!(
        "{}/actions/workflows/{}/runs?event=workflow_dispatch&per_page=10",
        repo.api(),
        workflow.id
    );
    // Runs that existed before dispatching, so the new one can be told apart without clocks.
    let before: HashSet<u64> = run_ids(&client.get(&runs_path)?);

    let sent: serde_json::Map<String, Value> = plan
        .inputs
        .iter()
        .filter(|i| i.sent())
        .filter_map(|i| Some((i.name.clone(), Value::String(i.value.clone()?))))
        .collect();
    client.post(
        &format!(
            "{}/actions/workflows/{}/dispatches",
            repo.api(),
            workflow.id
        ),
        &json!({ "ref": plan.git_ref, "inputs": sent }),
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
            plan.run_id = r["id"].as_u64();
            plan.run_url = r["html_url"].as_str().map(str::to_string);
            break;
        }
    }

    if ctx.json {
        output::print_json(&plan)?;
    } else {
        match &plan.run_url {
            Some(url) => println!("Dispatched {} for {}: {url}", plan.workflow, plan.version),
            None => println!(
                "Dispatched {} for {}. The run is not visible yet, see https://github.com/{}/actions/workflows/{}",
                plan.workflow,
                plan.version,
                plan.repo,
                workflow.file()
            ),
        }
    }

    if args.watch {
        let id = plan
            .run_id
            .context("Can't watch, the dispatched run did not appear in time")?;
        let status = Command::new("gh")
            .args([
                "run",
                "watch",
                &id.to_string(),
                "--repo",
                &plan.repo,
                "--exit-status",
            ])
            .status()
            .context("Failed to run gh run watch")?;
        if !status.success() {
            bail!("Release run {id} did not succeed");
        }
    }
    Ok(())
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
    fn version_parse_and_bump() {
        let v = Version::parse("v1.2.3").unwrap();
        assert_eq!(v.bump(Bump::Patch).to_string(), "v1.2.4");
        assert_eq!(v.bump(Bump::Minor).to_string(), "v1.3.0");
        assert_eq!(v.bump(Bump::Major).to_string(), "v2.0.0");
        assert_eq!(
            Version::parse("0.4.0")
                .unwrap()
                .bump(Bump::Patch)
                .to_string(),
            "0.4.1"
        );
        assert_eq!(
            Version::parse("1.2").unwrap().bump(Bump::Patch).to_string(),
            "1.2.1"
        );
        assert_eq!(
            Version::parse("release-2.0.0-rc.1+b5")
                .unwrap()
                .bump(Bump::Minor)
                .to_string(),
            "release-2.1.0"
        );
        assert_eq!(Version::parse("latest"), None);
        assert_eq!(Version::parse("1.2.3.4"), None);
    }

    fn wf(id: u64, name: &str, path: &str) -> Workflow {
        Workflow {
            id,
            name: name.to_string(),
            path: path.to_string(),
        }
    }

    #[test]
    fn workflow_selection() {
        let all = vec![
            wf(1, "CI", ".github/workflows/ci.yml"),
            wf(2, "Release", ".github/workflows/release.yml"),
            wf(3, "Release Notes", ".github/workflows/notes.yml"),
        ];
        assert_eq!(pick_workflow(&all, None).unwrap().id, 2);
        assert_eq!(pick_workflow(&all, Some("notes.yml")).unwrap().id, 3);
        assert_eq!(pick_workflow(&all, Some("ci")).unwrap().id, 1);
        assert!(pick_workflow(&all, Some("deploy")).is_err());

        let partial = vec![
            wf(1, "CI", ".github/workflows/ci.yml"),
            wf(4, "Publish release", ".github/workflows/publish.yml"),
        ];
        assert_eq!(pick_workflow(&partial, None).unwrap().id, 4);

        let ambiguous = vec![
            wf(4, "Publish release", ".github/workflows/publish.yml"),
            wf(5, "Draft release", ".github/workflows/draft.yml"),
        ];
        assert!(pick_workflow(&ambiguous, None).is_err());
    }

    const GCS: &str = r#"
name: Release

on:
  workflow_dispatch:
    inputs:
      version:
        description: "Version to release"
        required: true
        type: string
      channel:
        type: choice
        options: [stable, beta]
        default: stable
      dry_run:
        type: boolean
        default: false
"#;

    #[test]
    fn inputs_from_workflow_file() {
        let inputs = dispatch_inputs(GCS).unwrap();
        let names: Vec<&str> = inputs.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, vec!["version", "channel", "dry_run"]);
        assert!(inputs[0].required);
        assert_eq!(inputs[1].options, vec!["stable", "beta"]);
        assert_eq!(inputs[2].default.as_deref(), Some("false"));
        assert_eq!(version_input(&inputs).unwrap().name, "version");
    }

    #[test]
    fn trigger_forms_without_inputs() {
        assert!(dispatch_inputs("on: workflow_dispatch").unwrap().is_empty());
        assert!(
            dispatch_inputs("on: [push, workflow_dispatch]")
                .unwrap()
                .is_empty()
        );
        assert!(dispatch_inputs("on: [push]").is_err());
        assert!(dispatch_inputs("on:\n  push:\n").is_err());
    }

    #[test]
    fn version_input_fallbacks() {
        let input = |name: &str| Input {
            name: name.to_string(),
            description: String::new(),
            required: true,
            default: None,
            kind: "string".to_string(),
            options: Vec::new(),
        };
        assert_eq!(
            version_input(&[input("new_version")]).unwrap().name,
            "new_version"
        );
        assert_eq!(
            version_input(&[input("tag_name")]).unwrap().name,
            "tag_name"
        );
        assert!(version_input(&[input("a_version"), input("b_version")]).is_err());
        assert!(version_input(&[input("channel")]).is_err());
    }

    #[test]
    fn fields_and_resolution() {
        let inputs = dispatch_inputs(GCS).unwrap();
        let fields = parse_fields(&["channel=beta".to_string()], &inputs, "version").unwrap();
        assert!(parse_fields(&["version=1".to_string()], &inputs, "version").is_err());
        assert!(parse_fields(&["nope=1".to_string()], &inputs, "version").is_err());
        assert!(parse_fields(&["channel".to_string()], &inputs, "version").is_err());

        let plan = resolve_inputs(&inputs, "version", "0.4.1", &fields).unwrap();
        let rows: Vec<(&str, Option<&str>, &str, bool)> = plan
            .iter()
            .map(|i| {
                (
                    i.name.as_str(),
                    i.value.as_deref(),
                    i.source.as_str(),
                    i.sent(),
                )
            })
            .collect();
        assert_eq!(
            rows,
            vec![
                ("version", Some("0.4.1"), "version", true),
                ("channel", Some("beta"), "set", true),
                ("dry_run", Some("false"), "default", false),
            ]
        );
    }
}
