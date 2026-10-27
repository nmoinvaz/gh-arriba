//! Adds to coding agents' hook files the hooks that run `notify` when the agent needs you, or
//! removes them.

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

use crate::notify;
use crate::prompt;

/// A hook that runs `command` for the agent's `event`.
struct Hook {
    event: &'static str,
    command: &'static str,
}

/// A coding agent whose JSON hook file nests handlers as `hooks.<event>[].hooks[]`.
struct Agent {
    name: &'static str,
    /// The variable that moves the agent's configuration directory.
    dir_var: &'static str,
    /// The configuration directory under HOME otherwise.
    default_dir: &'static str,
    file: &'static str,
    hooks: &'static [Hook],
}

/// Its hook command is the one the README gives.
const CLAUDE: Agent = Agent {
    name: "Claude Code",
    dir_var: "CLAUDE_CONFIG_DIR",
    default_dir: ".claude",
    file: "settings.json",
    hooks: &[Hook {
        event: "Notification",
        command: r#"gh arriba notify --title 'Claude Code' "$(jq -r .message)""#,
    }],
};

const AGENTS: &[Agent] = &[CLAUDE];

/// Offer each agent that is set up its hooks. An agent whose file cannot change only warns, since
/// the watch is set up either way.
pub fn offer_add(yes: bool) {
    for agent in AGENTS {
        if let Err(e) = agent.offer_add(yes) {
            eprintln!("warning: {e:#}");
        }
    }
}

/// Offer to remove each agent's hooks.
pub fn offer_remove(yes: bool) {
    for agent in AGENTS {
        if let Err(e) = agent.offer_remove(yes) {
            eprintln!("warning: {e:#}");
        }
    }
}

impl Agent {
    /// The hook file, when the agent's configuration directory exists.
    fn path(&self) -> Option<PathBuf> {
        let dir = match std::env::var_os(self.dir_var) {
            Some(dir) => PathBuf::from(dir),
            None => Path::new(&std::env::var_os("HOME")?).join(self.default_dir),
        };
        dir.is_dir().then(|| dir.join(self.file))
    }

    /// "hook" or "hooks", by how many the agent takes.
    fn hook_word(&self) -> &'static str {
        if self.hooks.len() == 1 {
            "hook"
        } else {
            "hooks"
        }
    }

    /// Offer the hooks when the agent is set up without them. `yes` adds them unasked, and without
    /// a terminal nothing changes, since the hooks are optional.
    fn offer_add(&self, yes: bool) -> Result<()> {
        let Some(path) = self.path() else {
            return Ok(());
        };
        let mut file = read(&path)?;
        if self.hooks.iter().all(|h| has_hook(&file, h.event)) {
            return Ok(());
        }
        let article = if self.hooks.len() == 1 { "a " } else { "" };
        let question = format!(
            "Also notify you when {} needs you, with {article}{} in {}?",
            self.name,
            self.hook_word(),
            path.display()
        );
        if !ask(&question, yes)? {
            return Ok(());
        }
        for hook in self.hooks {
            // An event already hooked keeps its one hook.
            if !has_hook(&file, hook.event) {
                add_hook(&mut file, hook.event, hook.command)
                    .with_context(|| format!("Failed to add a hook to {}", path.display()))?;
            }
        }
        write(&path, &file)?;
        let events: Vec<&str> = self.hooks.iter().map(|h| h.event).collect();
        println!(
            "Added {article}{} {} {} to {}",
            self.name,
            events.join(" and "),
            self.hook_word(),
            path.display()
        );
        if !notify::on_path("jq") {
            let reads = if self.hooks.len() == 1 {
                "hook reads its message"
            } else {
                "hooks read their messages"
            };
            eprintln!("warning: the {reads} with jq, which is not on the PATH");
        }
        Ok(())
    }

    /// Offer to remove the hooks. `yes` removes them unasked, and without a terminal they stay.
    fn offer_remove(&self, yes: bool) -> Result<()> {
        let Some(path) = self.path() else {
            return Ok(());
        };
        let mut file = read(&path)?;
        if !self.hooks.iter().any(|h| has_hook(&file, h.event)) {
            return Ok(());
        }
        let question = format!(
            "Also remove the gh arriba {} from {}?",
            self.hook_word(),
            path.display()
        );
        if !ask(&question, yes)? {
            return Ok(());
        }
        for hook in self.hooks {
            remove_hook(&mut file, hook.event);
        }
        write(&path, &file)?;
        println!(
            "Removed the gh arriba {} from {}",
            self.hook_word(),
            path.display()
        );
        Ok(())
    }
}

fn ask(question: &str, yes: bool) -> Result<bool> {
    if yes || !prompt::interactive() {
        return Ok(yes);
    }
    prompt::confirm(question, false)
}

/// The file's JSON, or none yet when the file does not exist.
fn read(path: &Path) -> Result<Value> {
    match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text)
            .with_context(|| format!("{} is not valid JSON", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(json!({})),
        Err(e) => Err(e).with_context(|| format!("Failed to read {}", path.display())),
    }
}

/// Writing in place keeps a symlinked file linked and its permissions as they were.
fn write(path: &Path, file: &Value) -> Result<()> {
    let text = serde_json::to_string_pretty(file)? + "\n";
    std::fs::write(path, text).with_context(|| format!("Failed to write {}", path.display()))
}

/// A hook that runs `notify` with a message, also after someone edits its options.
fn is_ours(command: &str) -> bool {
    command.contains("arriba notify") && !command.contains("notify watch")
}

fn has_hook(file: &Value, event: &str) -> bool {
    file.get("hooks")
        .and_then(|hooks| hooks.get(event))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|group| group.get("hooks")?.as_array())
        .flatten()
        .filter_map(|hook| hook.get("command")?.as_str())
        .any(is_ours)
}

/// Append a hook for `event`, keeping the rest of the file and its order.
fn add_hook(file: &mut Value, event: &str, command: &str) -> Result<()> {
    let Some(root) = file.as_object_mut() else {
        bail!("The file is not a JSON object");
    };
    let Some(hooks) = root
        .entry("hooks")
        .or_insert_with(|| json!({}))
        .as_object_mut()
    else {
        bail!("hooks is not a JSON object");
    };
    let entry = hooks.entry(event).or_insert_with(|| json!([]));
    let Some(groups) = entry.as_array_mut() else {
        bail!("hooks.{event} is not a JSON array");
    };
    groups.push(json!({"hooks": [{"type": "command", "command": command, "async": true}]}));
    Ok(())
}

/// Remove our hook for `event`, and the group and keys that only it filled.
fn remove_hook(file: &mut Value, event: &str) {
    let Some(hooks) = file.get_mut("hooks").and_then(Value::as_object_mut) else {
        return;
    };
    let Some(groups) = hooks.get_mut(event).and_then(Value::as_array_mut) else {
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
        hooks.shift_remove(event);
    }
    if hooks.is_empty()
        && let Some(root) = file.as_object_mut()
    {
        root.shift_remove("hooks");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const COMMAND: &str = CLAUDE.hooks[0].command;

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
        assert!(!has_hook(&settings, "Notification"));
        add_hook(&mut settings, "Notification", COMMAND).unwrap();
        assert!(has_hook(&settings, "Notification"));

        let text = serde_json::to_string(&settings).unwrap();
        let ours =
            format!(r#"{{"hooks":[{{"type":"command","command":{COMMAND:?},"async":true}}]}}"#);
        assert!(text.starts_with(r#"{"model":"opus","hooks":{"PreToolUse":[],"Notification":"#));
        assert!(text.ends_with(&format!(
            r#"{ours}]}},"statusLine":{{"type":"command","command":"s.sh"}}}}"#
        )));

        let mut empty = json!({});
        add_hook(&mut empty, "Notification", COMMAND).unwrap();
        assert_eq!(
            empty,
            json!({"hooks": {"Notification": [{"hooks": [{"type": "command", "command": COMMAND, "async": true}]}]}})
        );
    }

    #[test]
    fn refuses_files_of_the_wrong_shape() {
        let add = |mut file: Value| add_hook(&mut file, "Notification", COMMAND);
        assert!(add(json!([])).is_err());
        assert!(add(json!({"hooks": []})).is_err());
        assert!(add(json!({"hooks": {"Notification": {}}})).is_err());
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
        remove_hook(&mut shared, "Notification");
        assert_eq!(
            shared,
            json!({"hooks": {"Notification": [
                {"matcher": "", "hooks": [{"type": "command", "command": "say hi"}]},
                {"matcher": "idle", "hooks": []}
            ]}})
        );

        let mut alone = json!({"model": "opus"});
        add_hook(&mut alone, "Notification", COMMAND).unwrap();
        remove_hook(&mut alone, "Notification");
        assert_eq!(alone, json!({"model": "opus"}));

        let mut other_events =
            json!({"hooks": {"Stop": [], "Notification": [{"hooks": [{"command": COMMAND}]}]}});
        remove_hook(&mut other_events, "Notification");
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
        remove_hook(&mut settings, "Notification");
        assert_eq!(
            serde_json::to_string(&settings).unwrap(),
            r#"{"model":"opus","hooks":{"Stop":[],"PreToolUse":[]},"statusLine":{},"autoMode":{}}"#
        );
        let mut emptied =
            json!({"hooks": {"Notification": [{"hooks": [{"command": COMMAND}]}]}, "a": 1, "b": 2});
        remove_hook(&mut emptied, "Notification");
        assert_eq!(serde_json::to_string(&emptied).unwrap(), r#"{"a":1,"b":2}"#);
    }

    #[test]
    fn reads_a_missing_file_as_empty() {
        let dir = std::env::temp_dir().join(format!("gh-arriba-agents-{}", std::process::id()));
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
