use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;

use crate::config::gist::GistClient;
use crate::config::manifest::{Manifest, ManifestFile, current_platform};
use crate::config::project::ProjectConfig;

/// Tolerance for comparing local mtime against the manifest timestamp.
/// Accounts for coarser filesystem mtime resolution (e.g. FAT's two-second
/// granularity) and minor clock drift between machines.
const MTIME_TOLERANCE_SECS: i64 = 1;

/// Scan files for secrets and bail if any are found.
fn check_secrets(config: &ProjectConfig, paths: &[String]) -> Result<()> {
    let patterns = secretscan::patterns::get_all_patterns();
    let mut all_findings = Vec::new();

    for path in paths {
        let full = config.root.join(path);
        let content = match fs::read_to_string(&full) {
            Ok(c) => c,
            Err(_) => continue,
        };
        for (line_num, line) in content.lines().enumerate() {
            for (name, regex) in patterns.iter() {
                if regex.is_match(line) {
                    all_findings.push(format!(
                        "  {}:{} [{}] {}",
                        path,
                        line_num + 1,
                        name,
                        line.trim()
                    ));
                }
            }
        }
    }

    if !all_findings.is_empty() {
        eprintln!("Potential secrets found:");
        for f in &all_findings {
            eprintln!("{f}");
        }
        anyhow::bail!(
            "Aborting — {} potential secret(s) detected. Review and remove them before pushing.",
            all_findings.len()
        );
    }
    Ok(())
}

/// Read the manifest from an existing gist.
fn read_manifest(
    client: &GistClient,
    gist_id: &str,
    config: &ProjectConfig,
) -> Result<Option<Manifest>> {
    let gist = client.get_gist(gist_id)?;
    let manifest_name = config.manifest_name();
    match client.get_file_content(&gist, &manifest_name) {
        Some(yaml) => Ok(Manifest::from_yaml(yaml)),
        None => Ok(None),
    }
}

/// Check whether a gist is private (i.e. not public).
fn is_gist_private(client: &GistClient, gist_id: &str) -> Result<bool> {
    let gist = client.get_gist(gist_id)?;
    Ok(!gist["public"].as_bool().unwrap_or(true))
}

/// Print the gist URL, marking private gists.
fn print_gist_url(id: &str, private: bool) {
    let suffix = if private { " (private)" } else { "" };
    println!("Gist: https://gist.github.com/{id}{suffix}");
}

/// Read a local file's modification time as a UTC timestamp.
fn local_mtime(config: &ProjectConfig, path: &str) -> Option<DateTime<Utc>> {
    let full = config.root.join(path);
    let meta = fs::metadata(&full).ok()?;
    let mtime = meta.modified().ok()?;
    Some(DateTime::<Utc>::from(mtime))
}

/// Set a local file's modification time to the given UTC timestamp.
fn set_local_mtime(path: &Path, time: DateTime<Utc>) -> Result<()> {
    let system_time: std::time::SystemTime = time.into();
    let file = fs::File::options()
        .write(true)
        .open(path)
        .with_context(|| format!("Failed to open {} to set mtime", path.display()))?;
    file.set_modified(system_time)
        .with_context(|| format!("Failed to set mtime on {}", path.display()))?;
    Ok(())
}

/// Build the file map for creating a gist from the manifest's entries.
///
/// Keys each file by the entry's stored gist name so platform variants of the
/// same path stay distinct. Entries with no readable local file are skipped.
fn build_file_map(config: &ProjectConfig, manifest: &Manifest) -> HashMap<String, String> {
    let mut files = HashMap::new();
    files.insert(config.manifest_name(), manifest.to_yaml());
    for entry in &manifest.files {
        let full = config.root.join(&entry.path);
        if let Ok(content) = fs::read_to_string(&full) {
            files.insert(entry.gist.clone(), content);
        }
    }
    files
}

/// Remove gist files that are not in the expected set.
fn remove_stale_files(
    client: &GistClient,
    gist_id: &str,
    config: &ProjectConfig,
    manifest: &Manifest,
) -> Result<()> {
    let gist = client.get_gist(gist_id)?;
    let remote_files = client.get_file_names(&gist);

    let mut expected: HashSet<String> = HashSet::new();
    expected.insert(config.manifest_name());
    for entry in &manifest.files {
        expected.insert(entry.gist.clone());
    }

    let mut deletions: HashMap<String, Option<String>> = HashMap::new();
    for name in &remote_files {
        if !expected.contains(name) {
            deletions.insert(name.clone(), None);
            println!("  removed stale file: {name}");
        }
    }

    if !deletions.is_empty() {
        client.update_files(gist_id, &deletions)?;
    }
    Ok(())
}

/// Resolve optional command-line file arguments into a focus set.
///
/// Files listed must already be tracked; adding new files is `gh arriba config add`.
/// Returns `None` when no files were given, meaning "all tracked files".
fn parse_focus(
    config: &ProjectConfig,
    manifest: &Manifest,
    arg_files: &[String],
) -> Result<Option<HashSet<String>>> {
    if arg_files.is_empty() {
        return Ok(None);
    }
    let tracked: HashSet<String> = manifest.files.iter().map(|f| f.path.clone()).collect();
    let mut set = HashSet::new();
    for f in arg_files {
        let norm = config.relative_path(f);
        if !tracked.contains(&norm) {
            anyhow::bail!("{norm} is not tracked. Use `gh arriba config add` to track new files.");
        }
        set.insert(norm);
    }
    Ok(Some(set))
}

/// The action sync would take for a single manifest entry.
enum Action {
    Push(DateTime<Utc>),
    Pull,
    InSync,
}

/// Classify each platform-relevant manifest entry into a sync action.
///
/// Skips entries for other platforms and, when `focus` is set, entries
/// outside the focus set. Returns the index into `manifest.files` alongside
/// the action so callers can mutate or report on the entry.
fn build_plan(
    config: &ProjectConfig,
    manifest: &Manifest,
    focus: &Option<HashSet<String>>,
) -> Vec<(usize, Action)> {
    let current = current_platform();
    let mut plan = Vec::new();
    for (idx, entry) in manifest.files.iter().enumerate() {
        let platform_match = entry.platform.is_none() || entry.platform.as_deref() == Some(current);
        if !platform_match {
            continue;
        }
        if let Some(focus) = focus
            && !focus.contains(&entry.path)
        {
            continue;
        }

        let action = match local_mtime(config, &entry.path) {
            None => Action::Pull,
            Some(local) => {
                let delta = (local - entry.updated_at).num_seconds();
                if delta > MTIME_TOLERANCE_SECS {
                    Action::Push(local)
                } else if delta < -MTIME_TOLERANCE_SECS {
                    Action::Pull
                } else {
                    Action::InSync
                }
            }
        };
        plan.push((idx, action));
    }
    plan
}

pub fn do_sync(
    client: &GistClient,
    config: &ProjectConfig,
    arg_files: &[String],
    gist_id: Option<&str>,
) -> Result<()> {
    // No gist yet: create one from the files supplied on the command line.
    let Some(id) = gist_id else {
        return create_new_gist(client, config, arg_files);
    };

    let mut manifest = read_manifest(client, id, config)?.context("No manifest found in gist")?;

    // Optionally narrow the sync to a specific subset.
    let focus = parse_focus(config, &manifest, arg_files)?;

    let plan = build_plan(config, &manifest, &focus);

    let mut push_indices: Vec<(usize, DateTime<Utc>)> = Vec::new();
    let mut pull_indices: Vec<usize> = Vec::new();
    let mut in_sync_count = 0usize;
    for (idx, action) in plan {
        match action {
            Action::Push(ts) => push_indices.push((idx, ts)),
            Action::Pull => pull_indices.push(idx),
            Action::InSync => in_sync_count += 1,
        }
    }

    // Pull first so a failed push doesn't leave local out of date.
    if !pull_indices.is_empty() {
        let gist = client.get_gist(id)?;
        for idx in &pull_indices {
            let entry = &manifest.files[*idx];
            let Some(content) = client.get_file_content(&gist, &entry.gist) else {
                eprintln!("  warning: {} not found in gist", entry.gist);
                continue;
            };
            let target = config.root.join(&entry.path);
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("Failed to create directory for {}", entry.path))?;
            }
            fs::write(&target, format!("{content}\n"))
                .with_context(|| format!("Failed to write {}", entry.path))?;
            set_local_mtime(&target, entry.updated_at)?;
            println!("  pulled {} -> {}", entry.gist, entry.path);
        }
    }

    if !push_indices.is_empty() {
        let push_paths: Vec<String> = push_indices
            .iter()
            .map(|(idx, _)| manifest.files[*idx].path.clone())
            .collect();
        check_secrets(config, &push_paths)?;

        for (idx, ts) in &push_indices {
            manifest.files[*idx].updated_at = *ts;
        }

        let mut updates: HashMap<String, Option<String>> = HashMap::new();
        updates.insert(config.manifest_name(), Some(manifest.to_yaml()));
        for (idx, _) in &push_indices {
            let entry = &manifest.files[*idx];
            let full = config.root.join(&entry.path);
            let content = fs::read_to_string(&full)
                .with_context(|| format!("Failed to read {}", entry.path))?;
            updates.insert(entry.gist.clone(), Some(content));
            println!("  pushed {}", entry.path);
        }
        client.update_files(id, &updates)?;
    }

    println!(
        "Summary: {} pushed, {} pulled, {} already in sync",
        push_indices.len(),
        pull_indices.len(),
        in_sync_count
    );
    print_gist_url(id, is_gist_private(client, id)?);

    Ok(())
}

/// Create a new gist from the files specified on the command line.
fn create_new_gist(
    client: &GistClient,
    config: &ProjectConfig,
    arg_files: &[String],
) -> Result<()> {
    if arg_files.is_empty() {
        anyhow::bail!("No files specified and no manifest found.");
    }

    let normalized: Vec<String> = arg_files.iter().map(|p| config.relative_path(p)).collect();
    let entries: Vec<ManifestFile> = normalized
        .iter()
        .map(|p| {
            let ts = local_mtime(config, p).unwrap_or_else(Utc::now);
            Manifest::entry(config, p, None, ts)
        })
        .collect();

    let manifest = Manifest::new(config, &entries);
    let local_files: Vec<String> = normalized
        .iter()
        .filter(|p| config.root.join(p).is_file())
        .cloned()
        .collect();

    if local_files.is_empty() {
        println!("No tracked config files found and no remote gist exists.");
        return Ok(());
    }

    check_secrets(config, &local_files)?;
    let files = build_file_map(config, &manifest);
    println!("Creating gist: {}", config.gist_description());
    let id = client.create_gist(&config.gist_description(), config.public, &files)?;
    println!(
        "Pushed {} file(s): {}",
        local_files.len(),
        local_files.join(" ")
    );
    print_gist_url(&id, !config.public);
    Ok(())
}

/// Report what `sync` would do, without touching local files or the gist.
pub fn do_status(
    client: &GistClient,
    config: &ProjectConfig,
    arg_files: &[String],
    gist_id: Option<&str>,
) -> Result<()> {
    let Some(id) = gist_id else {
        println!(
            "No gist exists for this project. `gh arriba config sync FILE...` would create one."
        );
        return Ok(());
    };

    let manifest = read_manifest(client, id, config)?.context("No manifest found in gist")?;
    let focus = parse_focus(config, &manifest, arg_files)?;
    let plan = build_plan(config, &manifest, &focus);

    let mut push = 0usize;
    let mut pull = 0usize;
    let mut in_sync = 0usize;
    for (idx, action) in &plan {
        let entry = &manifest.files[*idx];
        match action {
            Action::Push(_) => {
                println!("  push  {} (local newer)", entry.path);
                push += 1;
            }
            Action::Pull => {
                let reason = if local_mtime(config, &entry.path).is_none() {
                    "missing locally"
                } else {
                    "gist newer"
                };
                println!("  pull  {} ({reason})", entry.path);
                pull += 1;
            }
            Action::InSync => in_sync += 1,
        }
    }

    // Files present in the gist but absent from the manifest. `sync` leaves
    // these alone; `gh arriba config cleanup` removes them.
    let gist = client.get_gist(id)?;
    let mut expected: HashSet<String> = HashSet::new();
    expected.insert(config.manifest_name());
    for entry in &manifest.files {
        expected.insert(entry.gist.clone());
    }
    let mut stale = 0usize;
    for name in client.get_file_names(&gist) {
        if !expected.contains(&name) {
            println!("  stale {name} (not in manifest, run `gh arriba config cleanup`)");
            stale += 1;
        }
    }

    if push == 0 && pull == 0 && stale == 0 {
        println!("Everything is in sync ({in_sync} file(s)).");
    } else {
        println!("Summary: {push} to push, {pull} to pull, {in_sync} in sync, {stale} stale");
    }
    print_gist_url(id, is_gist_private(client, id)?);

    Ok(())
}

pub fn do_restore(
    client: &GistClient,
    config: &ProjectConfig,
    arg_files: &[String],
    gist_id: Option<&str>,
) -> Result<()> {
    let id = gist_id.context("No gist found for this project")?;
    let manifest = read_manifest(client, id, config)?.context("No manifest found in gist")?;

    let focus = parse_focus(config, &manifest, arg_files)?;

    let current = current_platform();
    let targets: Vec<&ManifestFile> = manifest
        .files
        .iter()
        .filter(|entry| {
            let platform_match =
                entry.platform.is_none() || entry.platform.as_deref() == Some(current);
            let focus_match = focus.as_ref().is_none_or(|f| f.contains(&entry.path));
            platform_match && focus_match
        })
        .collect();

    if targets.is_empty() {
        println!("No files to restore.");
        return Ok(());
    }

    let gist = client.get_gist(id)?;
    let mut restored = 0usize;
    let mut in_sync = 0usize;
    for entry in &targets {
        // Skip when local mtime already matches the manifest timestamp.
        if let Some(local) = local_mtime(config, &entry.path)
            && (local - entry.updated_at).num_seconds().abs() <= MTIME_TOLERANCE_SECS
        {
            in_sync += 1;
            continue;
        }

        let Some(content) = client.get_file_content(&gist, &entry.gist) else {
            eprintln!("  warning: {} not found in gist", entry.gist);
            continue;
        };
        let target = config.root.join(&entry.path);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create directory for {}", entry.path))?;
        }
        fs::write(&target, format!("{content}\n"))
            .with_context(|| format!("Failed to write {}", entry.path))?;
        set_local_mtime(&target, entry.updated_at)?;
        println!("  restored {} -> {}", entry.gist, entry.path);
        restored += 1;
    }

    println!("Summary: {restored} restored, {in_sync} already in sync");
    print_gist_url(id, is_gist_private(client, id)?);

    Ok(())
}

pub fn do_add(
    client: &GistClient,
    config: &ProjectConfig,
    files: &[String],
    platform_specific: bool,
    gist_id: Option<&str>,
) -> Result<()> {
    if files.is_empty() {
        anyhow::bail!("Usage: gh arriba config add FILE...");
    }

    let platform = if platform_specific {
        Some(current_platform())
    } else {
        None
    };

    // Start with existing manifest entries, then merge new ones.
    let mut entries: Vec<ManifestFile> = if let Some(id) = gist_id {
        read_manifest(client, id, config)?
            .map(|m| m.files)
            .unwrap_or_default()
    } else {
        Vec::new()
    };

    // An entry is identified by (path, platform): the same path may carry one
    // entry per platform. Newly created entries accumulate in `added`.
    let platform_str = platform.map(|s| s.to_string());
    let mut added: Vec<ManifestFile> = Vec::new();
    for f in files {
        let normalized = config.relative_path(f);

        // Already tracked for this exact platform scope, nothing to do.
        if entries
            .iter()
            .chain(added.iter())
            .any(|e| e.path == normalized && e.platform == platform_str)
        {
            println!("  skipped {normalized} (already tracked)");
            continue;
        }
        // A platform-agnostic entry and a platform-specific one for the same
        // path would both match on sync. Reject the mix instead.
        if entries
            .iter()
            .chain(added.iter())
            .any(|e| e.path == normalized)
        {
            anyhow::bail!(
                "{normalized} is already tracked with a different platform scope. \
                 Run `gh arriba config remove {normalized}` first to change it."
            );
        }

        let ts = local_mtime(config, &normalized).unwrap_or_else(Utc::now);
        added.push(Manifest::entry(config, &normalized, platform, ts));
    }

    if added.is_empty() {
        return Ok(());
    }

    entries.extend(added.iter().cloned());
    let manifest = Manifest::new(config, &entries);
    let added_paths: Vec<String> = added.iter().map(|e| e.path.clone()).collect();
    check_secrets(config, &added_paths)?;

    if let Some(id) = gist_id {
        let mut updates: HashMap<String, Option<String>> = HashMap::new();
        updates.insert(config.manifest_name(), Some(manifest.to_yaml()));
        for entry in &added {
            let full = config.root.join(&entry.path);
            let suffix = entry
                .platform
                .as_deref()
                .map(|p| format!(" ({p})"))
                .unwrap_or_default();
            if full.is_file() {
                let content = fs::read_to_string(&full)
                    .with_context(|| format!("Failed to read {}", entry.path))?;
                updates.insert(entry.gist.clone(), Some(content));
                println!("  added {}{suffix}", entry.path);
            } else {
                println!("  skipped {} (not found locally)", entry.path);
            }
        }
        client.update_files(id, &updates)?;
        print_gist_url(id, is_gist_private(client, id)?);
    } else {
        let file_map = build_file_map(config, &manifest);
        println!("Creating gist: {}", config.gist_description());
        let id = client.create_gist(&config.gist_description(), config.public, &file_map)?;
        for entry in &added {
            let suffix = entry
                .platform
                .as_deref()
                .map(|p| format!(" ({p})"))
                .unwrap_or_default();
            println!("  added {}{suffix}", entry.path);
        }
        print_gist_url(&id, !config.public);
    }

    Ok(())
}

pub fn do_remove(
    client: &GistClient,
    config: &ProjectConfig,
    files: &[String],
    platform_specific: bool,
    gist_id: Option<&str>,
) -> Result<()> {
    if files.is_empty() {
        anyhow::bail!("Usage: gh arriba config remove FILE...");
    }

    let id = gist_id.context("No gist found")?;

    let manifest = read_manifest(client, id, config)?.context("No manifest found in gist")?;

    let remove_set: HashSet<String> = files.iter().map(|f| config.relative_path(f)).collect();

    // Without --platform, removing by path drops every variant of that path.
    // With it, only the variant scoped to the current platform is removed.
    // Either way, delete each matched entry's stored gist file.
    let current = current_platform();
    let (removed, remaining): (Vec<ManifestFile>, Vec<ManifestFile>) =
        manifest.files.into_iter().partition(|e| {
            remove_set.contains(&e.path)
                && (!platform_specific || e.platform.as_deref() == Some(current))
        });

    if removed.is_empty() {
        if platform_specific {
            anyhow::bail!("None of the given files are tracked for {current}.");
        }
        anyhow::bail!("None of the given files are tracked.");
    }

    let mut updates: HashMap<String, Option<String>> = HashMap::new();
    for entry in &removed {
        updates.insert(entry.gist.clone(), None);
        let suffix = entry
            .platform
            .as_deref()
            .map(|p| format!(" ({p})"))
            .unwrap_or_default();
        println!("  removed {}{suffix}", entry.path);
    }
    let new_manifest = Manifest::new(config, &remaining);
    updates.insert(config.manifest_name(), Some(new_manifest.to_yaml()));

    client.update_files(id, &updates)?;
    print_gist_url(id, is_gist_private(client, id)?);

    Ok(())
}

pub fn do_cleanup(
    client: &GistClient,
    config: &ProjectConfig,
    gist_id: Option<&str>,
) -> Result<()> {
    let id = gist_id.context("No gist found")?;
    let manifest = read_manifest(client, id, config)?.context("No manifest found in gist")?;
    remove_stale_files(client, id, config, &manifest)?;
    print_gist_url(id, is_gist_private(client, id)?);
    Ok(())
}

pub fn do_delete(client: &GistClient, gist_id: Option<&str>) -> Result<()> {
    let id = gist_id.context("No gist found")?;
    client.delete_gist(id)?;
    println!("Deleted gist: https://gist.github.com/{id}");
    Ok(())
}

pub fn do_open(gist_id: Option<&str>) -> Result<()> {
    let id = gist_id.context("No gist found")?;
    let url = format!("https://gist.github.com/{id}");
    println!("Opening {url}");

    let program = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(target_os = "windows") {
        "explorer"
    } else {
        "xdg-open"
    };

    std::process::Command::new(program)
        .arg(&url)
        .status()
        .with_context(|| format!("Failed to launch {program}"))?;

    Ok(())
}
