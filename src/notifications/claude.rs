//! Adds to Claude Code's settings the Notification hook that runs `notify`, or removes it.

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

use crate::notify;
use crate::prompt;

/// The hook command the README gives.
const COMMAND: &str = r#"gh arriba notify --title 'Claude Code' "$(jq -r .message)""#;

/// Offer to add the hook when Claude Code is set up without one. `yes` adds it unasked, and
/// without a terminal nothing changes, since the hook is optional.
pub fn offer_add(yes: bool) -> Result<()> {
    let Some(path) = settings_path() else {
        return Ok(());
    };
    let mut settings = read(&path)?;
    if has_hook(&settings) {
        return Ok(());
    }
    let question = format!(
        "Also notify you when Claude Code needs you, with a hook in {}?",
        path.display()
    );
    if !ask(&question, yes)? {
        return Ok(());
    }
    add_hook(&mut settings)
        .with_context(|| format!("Failed to add a hook to {}", path.display()))?;
    write(&path, &settings)?;
    println!(
        "Added a Claude Code Notification hook to {}",
        path.display()
    );
    if !notify::on_path("jq") {
        eprintln!("warning: the hook reads its message with jq, which is not on the PATH");
    }
    Ok(())
}

/// Offer to remove the hook. `yes` removes it unasked, and without a terminal it stays.
pub fn offer_remove(yes: bool) -> Result<()> {
    let Some(path) = settings_path() else {
        return Ok(());
    };
    let mut settings = read(&path)?;
    if !has_hook(&settings) {
        return Ok(());
    }
    let question = format!("Also remove the gh arriba hook from {}?", path.display());
    if !ask(&question, yes)? {
        return Ok(());
    }
    remove_hook(&mut settings);
    write(&path, &settings)?;
    println!("Removed the gh arriba hook from {}", path.display());
    Ok(())
}

fn ask(question: &str, yes: bool) -> Result<bool> {
    if yes || !prompt::interactive() {
        return Ok(yes);
    }
    prompt::confirm(question, false)
}

/// Claude Code's settings file, when its configuration directory exists.
fn settings_path() -> Option<PathBuf> {
    let dir = match std::env::var_os("CLAUDE_CONFIG_DIR") {
        Some(dir) => PathBuf::from(dir),
        None => Path::new(&std::env::var_os("HOME")?).join(".claude"),
    };
    dir.is_dir().then(|| dir.join("settings.json"))
}

/// The settings, or none yet when the file does not exist.
fn read(path: &Path) -> Result<Value> {
    match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text)
            .with_context(|| format!("{} is not valid JSON", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(json!({})),
        Err(e) => Err(e).with_context(|| format!("Failed to read {}", path.display())),
    }
}

/// Writing in place keeps a symlinked settings file linked and its permissions as they were.
fn write(path: &Path, settings: &Value) -> Result<()> {
    let text = serde_json::to_string_pretty(settings)? + "\n";
    std::fs::write(path, text).with_context(|| format!("Failed to write {}", path.display()))
}

/// A hook that runs `notify` with a message, also after someone edits its options.
fn is_ours(command: &str) -> bool {
    command.contains("arriba notify") && !command.contains("notify watch")
}

fn has_hook(settings: &Value) -> bool {
    settings
        .pointer("/hooks/Notification")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|group| group.get("hooks")?.as_array())
        .flatten()
        .filter_map(|hook| hook.get("command")?.as_str())
        .any(is_ours)
}

/// Append the hook, keeping the rest of the settings and their order.
fn add_hook(settings: &mut Value) -> Result<()> {
    let Some(root) = settings.as_object_mut() else {
        bail!("The settings are not a JSON object");
    };
    let Some(hooks) = root
        .entry("hooks")
        .or_insert_with(|| json!({}))
        .as_object_mut()
    else {
        bail!("hooks is not a JSON object");
    };
    let entry = hooks.entry("Notification").or_insert_with(|| json!([]));
    let Some(groups) = entry.as_array_mut() else {
        bail!("hooks.Notification is not a JSON array");
    };
    groups.push(json!({"hooks": [{"type": "command", "command": COMMAND, "async": true}]}));
    Ok(())
}

/// Remove the hook, and the group and keys that only it filled.
fn remove_hook(settings: &mut Value) {
    let Some(hooks) = settings.get_mut("hooks").and_then(Value::as_object_mut) else {
        return;
    };
    let Some(groups) = hooks.get_mut("Notification").and_then(Value::as_array_mut) else {
        return;
    };
    groups.retain_mut(|group| {
        let Some(list) = group.get_mut("hooks").and_then(Value::as_array_mut) else {
            return true;
        };
        let before = list.len();
        list.retain(|hook| {
            !hook
                .get("command")
                .and_then(Value::as_str)
                .is_some_and(is_ours)
        });
        list.len() == before || !list.is_empty()
    });
    // remove swaps the last key into the gap, so shift_remove keeps the order.
    if groups.is_empty() {
        hooks.shift_remove("Notification");
    }
    if hooks.is_empty()
        && let Some(root) = settings.as_object_mut()
    {
        root.shift_remove("hooks");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adds_the_hook_keeping_the_rest_in_order() {
        let mut settings = json!({
            "model": "opus",
            "hooks": {
                "PreToolUse": [],
                "Notification": [{"matcher": "", "hooks": [{"type": "command", "command": "say hi"}]}]
            },
            "statusLine": {"type": "command", "command": "s.sh"}
        });
        assert!(!has_hook(&settings));
        add_hook(&mut settings).unwrap();
        assert!(has_hook(&settings));

        let text = serde_json::to_string(&settings).unwrap();
        let ours =
            format!(r#"{{"hooks":[{{"type":"command","command":{COMMAND:?},"async":true}}]}}"#);
        assert!(text.starts_with(r#"{"model":"opus","hooks":{"PreToolUse":[],"Notification":"#));
        assert!(text.ends_with(&format!(
            r#"{ours}]}},"statusLine":{{"type":"command","command":"s.sh"}}}}"#
        )));

        let mut empty = json!({});
        add_hook(&mut empty).unwrap();
        assert_eq!(
            empty,
            json!({"hooks": {"Notification": [{"hooks": [{"type": "command", "command": COMMAND, "async": true}]}]}})
        );
    }

    #[test]
    fn refuses_settings_of_the_wrong_shape() {
        assert!(add_hook(&mut json!([])).is_err());
        assert!(add_hook(&mut json!({"hooks": []})).is_err());
        assert!(add_hook(&mut json!({"hooks": {"Notification": {}}})).is_err());
    }

    #[test]
    fn edited_hooks_count_as_ours_but_the_watch_does_not() {
        assert!(is_ours(COMMAND));
        assert!(is_ours(
            r#"gh arriba notify --title 'Claude Code' --sound Submarine "$(jq -r .message)""#
        ));
        assert!(!is_ours("gh arriba notify watch"));
        assert!(!is_ours("~/.claude/notify.sh"));
    }

    #[test]
    fn removes_only_the_hook_and_what_it_alone_filled() {
        let mut shared = json!({"hooks": {"Notification": [
            {"matcher": "", "hooks": [
                {"type": "command", "command": "say hi"},
                {"type": "command", "command": COMMAND}
            ]},
            {"matcher": "idle", "hooks": []}
        ]}});
        remove_hook(&mut shared);
        assert_eq!(
            shared,
            json!({"hooks": {"Notification": [
                {"matcher": "", "hooks": [{"type": "command", "command": "say hi"}]},
                {"matcher": "idle", "hooks": []}
            ]}})
        );

        let mut alone = json!({"model": "opus"});
        add_hook(&mut alone).unwrap();
        remove_hook(&mut alone);
        assert_eq!(alone, json!({"model": "opus"}));

        let mut other_events =
            json!({"hooks": {"Stop": [], "Notification": [{"hooks": [{"command": COMMAND}]}]}});
        remove_hook(&mut other_events);
        assert_eq!(other_events, json!({"hooks": {"Stop": []}}));
    }

    #[test]
    fn removing_keeps_the_keys_after_it_in_order() {
        let mut settings = json!({
            "model": "opus",
            "hooks": {"Notification": [{"hooks": [{"command": COMMAND}]}], "Stop": [], "PreToolUse": []},
            "statusLine": {},
            "autoMode": {}
        });
        remove_hook(&mut settings);
        assert_eq!(
            serde_json::to_string(&settings).unwrap(),
            r#"{"model":"opus","hooks":{"Stop":[],"PreToolUse":[]},"statusLine":{},"autoMode":{}}"#
        );
        let mut emptied =
            json!({"hooks": {"Notification": [{"hooks": [{"command": COMMAND}]}]}, "a": 1, "b": 2});
        remove_hook(&mut emptied);
        assert_eq!(serde_json::to_string(&emptied).unwrap(), r#"{"a":1,"b":2}"#);
    }

    #[test]
    fn reads_a_missing_file_as_empty_settings() {
        let dir = std::env::temp_dir().join(format!("gh-arriba-claude-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        assert_eq!(read(&path).unwrap(), json!({}));
        std::fs::write(&path, "{ not json").unwrap();
        assert!(read(&path).is_err());
        write(&path, &json!({"b": 1, "a": 2})).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "{\n  \"b\": 1,\n  \"a\": 2\n}\n"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
