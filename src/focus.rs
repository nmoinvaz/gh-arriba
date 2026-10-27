//! Finds the window or tab a command runs in, so clicking its notification can return there.

mod ghostty;
mod host;
mod terminal;
mod vscode;

use anyhow::Result;
use std::path::Path;

use crate::cli::FocusTarget;
use crate::context;

const GHOSTTY: &str = "com.mitchellh.ghostty";
const TERMINAL: &str = "com.apple.Terminal";
const VSCODE: &str = "com.microsoft.VSCode";

/// The command that focuses where this process runs, for a notification to run on click.
pub fn origin_command() -> Option<Vec<String>> {
    // Only the macOS notifier runs a command on click.
    if std::env::consts::OS != "macos" {
        return None;
    }
    let exe = std::env::current_exe().ok()?;
    Some(command(&exe, &origin()?))
}

/// The arguments that run `notify focus` for `target`. Clicks run under launchd's PATH, which
/// lacks gh, so the executable is named by path.
fn command(exe: &Path, target: &FocusTarget) -> Vec<String> {
    let (kind, value) = match target {
        FocusTarget::Ghostty { id } => ("ghostty", id.clone()),
        FocusTarget::Terminal { tty } => ("terminal", tty.clone()),
        FocusTarget::Vscode { dir } => ("vscode", dir.to_string_lossy().into_owned()),
        FocusTarget::App { bundle } => ("app", bundle.clone()),
    };
    let exe = exe.to_string_lossy().into_owned();
    vec![exe, "notify".into(), "focus".into(), kind.into(), value]
}

/// Where this process runs, from the app it descends from.
fn origin() -> Option<FocusTarget> {
    let table = context::run("ps", &["-axo", "pid=,ppid=,tty=,comm="], None).ok()?;
    let host = host::find(&host::parse(&table), std::process::id());
    // Under tmux or ssh no app is an ancestor, but the environment may name the one it started in.
    let bundle = host
        .app
        .as_deref()
        .and_then(bundle_id)
        .or_else(|| std::env::var("__CFBundleIdentifier").ok())?;
    let target = match bundle.as_str() {
        GHOSTTY => host
            .tty
            .as_deref()
            .and_then(ghostty::find)
            .map(|id| FocusTarget::Ghostty { id }),
        TERMINAL => host.tty.map(|tty| FocusTarget::Terminal { tty }),
        VSCODE => vscode::folder(host.exthost)
            .or_else(|| std::env::current_dir().ok())
            .map(|dir| FocusTarget::Vscode { dir }),
        _ => None,
    };
    Some(target.unwrap_or(FocusTarget::App { bundle }))
}

/// The CFBundleIdentifier of the app at `app`.
fn bundle_id(app: &Path) -> Option<String> {
    let plist = app.join("Contents/Info.plist");
    let args = ["-extract", "CFBundleIdentifier", "raw", "-o", "-"];
    let args: Vec<&str> = args.into_iter().chain([plist.to_str()?]).collect();
    context::run("plutil", &args, None).ok()
}

/// Focus `target`, or only its app when the window or tab has closed.
pub fn focus(target: &FocusTarget) -> Result<()> {
    match target {
        FocusTarget::Ghostty { id } => ghostty::focus(id).or_else(|_| activate(GHOSTTY)),
        FocusTarget::Terminal { tty } => terminal::focus(tty).or_else(|_| activate(TERMINAL)),
        FocusTarget::Vscode { dir } => vscode::focus(VSCODE, dir),
        FocusTarget::App { bundle } => activate(bundle),
    }
}

/// Bring the app with `bundle` to the front.
fn activate(bundle: &str) -> Result<()> {
    context::run("open", &["-b", bundle], None).map(drop)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{Cli, Command, NotifyCommand};
    use clap::Parser;

    #[test]
    fn commands_parse_back_into_their_targets() {
        let targets = [
            FocusTarget::Ghostty {
                id: "C2234F5B-8017".into(),
            },
            FocusTarget::Terminal {
                tty: "/dev/ttys003".into(),
            },
            FocusTarget::Vscode {
                dir: "/Users/me/My Repo".into(),
            },
            FocusTarget::App {
                bundle: "com.googlecode.iterm2".into(),
            },
        ];
        for target in targets {
            let argv = command(Path::new("/x/gh-arriba"), &target);
            assert_eq!(argv[0], "/x/gh-arriba");
            let Command::Notify(args) = Cli::try_parse_from(argv).unwrap().command else {
                panic!("not a notify command");
            };
            let Some(NotifyCommand::Focus { target: parsed }) = args.command else {
                panic!("not a focus command");
            };
            assert_eq!(parsed, target);
        }
    }
}
