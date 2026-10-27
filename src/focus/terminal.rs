//! Terminal.app scripts its tabs with the TTY each one runs on.

use anyhow::Result;

use crate::context;

/// Selects the tab on the TTY given as the script argument and raises its window.
const FOCUS: &str = r#"on run argv
  tell application "Terminal"
    repeat with w in windows
      repeat with t in tabs of w
        if tty of t is item 1 of argv then
          set selected of t to true
          set index of w to 1
          activate
          return
        end if
      end repeat
    end repeat
  end tell
  error "No Terminal tab runs on " & item 1 of argv
end run"#;

/// Focus the tab running on `tty`.
pub fn focus(tty: &str) -> Result<()> {
    context::run("osascript", &["-e", FOCUS, tty], None).map(drop)
}
