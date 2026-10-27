//! Ghostty 1.3 scripts its terminals by ID, but does not tell which TTY each one runs on.

use anyhow::Result;
use std::io::Write;
use std::time::{Duration, Instant};

use crate::context;

/// Lists each terminal as its ID and title, separated by a tab. Inside the tell block `tab` names
/// Ghostty's tab class, so the separator is spelled as a character ID.
const LIST: &str = r#"set sep to character id 9
tell application "Ghostty"
  set out to ""
  repeat with t in terminals
    set out to out & (id of t) & sep & (name of t) & linefeed
  end repeat
  return out
end tell"#;

/// Each terminal's ID and title.
fn terminals() -> Option<Vec<(String, String)>> {
    context::run("osascript", &["-e", LIST], None)
        .ok()
        .map(|out| parse(&out))
}

fn parse(out: &str) -> Vec<(String, String)> {
    out.lines()
        .filter_map(|line| line.split_once('\t'))
        .map(|(id, title)| (id.to_string(), title.to_string()))
        .collect()
}

/// The ID of the terminal running on `tty`. A unique title marks the terminal until a listing
/// shows it, then the old title comes back.
pub fn find(tty: &str) -> Option<String> {
    let before = terminals()?;
    let marker = format!("gh-arriba-{}", std::process::id());
    set_title(tty, &marker).ok()?;

    // Ghostty applies titles on its own thread, so the marker can take a moment to show.
    let deadline = Instant::now() + Duration::from_secs(1);
    let id = loop {
        let found = terminals()
            .unwrap_or_default()
            .into_iter()
            .find(|(_, title)| *title == marker);
        if let Some((id, _)) = found {
            break id;
        }
        // A marker that never shows was overwritten already, so no title needs restoring.
        if Instant::now() > deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(50));
    };

    let old = before.iter().find(|(i, _)| *i == id).map_or("", |(_, t)| t);
    // The title is cosmetic, so failing to restore it still leaves the ID usable.
    let _ = set_title(tty, old);
    Some(id)
}

/// Set the title of the terminal on `tty` with OSC 2.
fn set_title(tty: &str, title: &str) -> std::io::Result<()> {
    let mut terminal = std::fs::OpenOptions::new().write(true).open(tty)?;
    write!(terminal, "\x1b]2;{title}\x07")
}

/// Focus the terminal with `id`, bringing its window to the front. The ID is a script argument,
/// so it needs no AppleScript quoting.
pub fn focus(id: &str) -> Result<()> {
    let script = [
        "-e",
        "on run argv",
        "-e",
        "tell application \"Ghostty\" to focus terminal id (item 1 of argv)",
        "-e",
        "end run",
    ];
    let args: Vec<&str> = script.into_iter().chain([id]).collect();
    context::run("osascript", &args, None).map(drop)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ids_and_titles_keeping_tabs_out() {
        let out = "D5C8-BADA\tgh arriba notify watch\n7374-EB7A\t✳ Session notes\n\nstray";
        assert_eq!(
            parse(out),
            [
                (
                    "D5C8-BADA".to_string(),
                    "gh arriba notify watch".to_string()
                ),
                ("7374-EB7A".to_string(), "✳ Session notes".to_string()),
            ]
        );
    }
}
