use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde_json::Value;

use crate::cli::{Bump, ReleaseArgs};
use crate::context::{Ctx, Repo};
use crate::github::Client;
use crate::run::watch;
use crate::workflow::{self, Input, Workflow};
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

/// Pick the workflow named by `-w`, or the one that looks like the release workflow.
fn pick_workflow<'a>(workflows: &'a [Workflow], name: Option<&str>) -> Result<&'a Workflow> {
    if let Some(name) = name {
        return workflow::find(workflows, name);
    }

    let exact: Vec<&Workflow> = workflows
        .iter()
        .filter(|w| {
            w.name.eq_ignore_ascii_case("release") || w.stem().eq_ignore_ascii_case("release")
        })
        .collect();
    if !exact.is_empty() {
        return workflow::one_of(exact, workflows, "is named release");
    }
    let partial = workflows
        .iter()
        .filter(|w| {
            w.name.to_lowercase().contains("release") || w.stem().to_lowercase().contains("release")
        })
        .collect();
    workflow::one_of(partial, workflows, "mentions release")
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
    /// "version", "set", "prompt", "default", "unset", or "missing". Only the first three are sent.
    source: String,
    description: String,
    #[serde(rename = "type")]
    kind: String,
    required: bool,
    default: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    options: Vec<String>,
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
    if let Some((key, _)) = fields
        .iter()
        .filter_map(|f| f.split_once('='))
        .find(|(k, _)| *k == version)
    {
        bail!("Set the version with --version, not -f {key}=...");
    }
    workflow::parse_fields(fields, inputs)
}

/// Resolve every declared input's value, prompting for required ones that have none. A dry run
/// marks those as missing instead.
fn resolve_inputs(
    inputs: &[Input],
    version_name: &str,
    version: &str,
    fields: &[(String, String)],
    dry_run: bool,
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
        } else if input.required && dry_run {
            (None, "missing")
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
            description: input.description.clone(),
            kind: input.kind.clone(),
            required: input.required,
            default: input.default.clone(),
            options: input.options.clone(),
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
                "missing" => "  (required, no value)",
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

    let workflows = workflow::list(client, &repo)?;
    let workflow = pick_workflow(&workflows, args.workflow.as_deref())?;

    let inputs = workflow::inputs_at(client, &repo, workflow, &git_ref)?;
    let version_name = version_input(&inputs)?.name.clone();
    let fields = parse_fields(&args.fields, &inputs, &version_name)?;
    let resolved = resolve_inputs(&inputs, &version_name, &version, &fields, args.dry_run)?;

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

    if args.dry_run {
        if ctx.json {
            return output::print_json(&plan);
        }
        print!("{}", render(&plan));
        return Ok(());
    }

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

    let sent: serde_json::Map<String, Value> = plan
        .inputs
        .iter()
        .filter(|i| i.sent())
        .filter_map(|i| Some((i.name.clone(), Value::String(i.value.clone()?))))
        .collect();
    if let Some(run) = workflow::dispatch(client, &repo, workflow, &plan.git_ref, sent)? {
        plan.run_id = Some(run.id);
        plan.run_url = Some(run.url);
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
        let states = watch::watch(ctx, &[(repo, id)], &watch::Wait::default())?;
        // With --json the plan is the document on stdout, so the summary stays human-only.
        if !ctx.json {
            print!("{}", watch::render(&states));
        }
        watch::check(&states).with_context(|| format!("Release run {id} did not succeed"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow::dispatch_inputs;

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

        let plan = resolve_inputs(&inputs, "version", "0.4.1", &fields, false).unwrap();
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

    #[test]
    fn dry_run_marks_missing_required_inputs() {
        let yaml = "on:\n  workflow_dispatch:\n    inputs:\n      version:\n        required: true\n      notes:\n        description: Release notes\n        required: true\n";
        let inputs = dispatch_inputs(yaml).unwrap();
        let plan = resolve_inputs(&inputs, "version", "1.0.0", &[], true).unwrap();
        assert_eq!(plan[1].source, "missing");
        assert_eq!(plan[1].value, None);
        assert_eq!(plan[1].description, "Release notes");
        assert!(!plan[1].sent());
        // Without a dry run and without a terminal, the missing input is an error.
        assert!(resolve_inputs(&inputs, "version", "1.0.0", &[], false).is_err());
    }
}
