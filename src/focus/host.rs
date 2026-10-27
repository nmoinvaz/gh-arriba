//! The app, terminal, and VS Code extension host a process descends from.

use std::collections::HashMap;
use std::path::PathBuf;

/// One row of the process table.
#[derive(Debug, PartialEq)]
pub struct Proc {
    ppid: u32,
    /// The controlling terminal, as /dev/ttys003.
    tty: Option<String>,
    /// The executable path.
    comm: String,
}

/// The rows of `ps -o pid=,ppid=,tty=,comm=` by pid.
pub fn parse(table: &str) -> HashMap<u32, Proc> {
    table
        .lines()
        .filter_map(|line| {
            let (pid, rest) = field(line)?;
            let (ppid, rest) = field(rest)?;
            let (tty, comm) = field(rest)?;
            let proc = Proc {
                ppid: ppid.parse().ok()?,
                // ps shows ?? for no controlling terminal.
                tty: (tty != "??").then(|| format!("/dev/{tty}")),
                comm: comm.to_string(),
            };
            Some((pid.parse().ok()?, proc))
        })
        .collect()
}

/// The first whitespace-separated field of `s` and the rest, which keeps its inner spaces.
fn field(s: &str) -> Option<(&str, &str)> {
    let s = s.trim_start();
    let end = s.find(char::is_whitespace)?;
    Some((&s[..end], s[end..].trim_start()))
}

/// Where a process runs, as its ancestry tells it.
#[derive(Debug, Default, PartialEq)]
pub struct Host {
    /// The app bundle, as /Applications/Ghostty.app.
    pub app: Option<PathBuf>,
    /// The nearest controlling terminal.
    pub tty: Option<String>,
    /// The VS Code extension host, for a process an extension started.
    pub exthost: Option<u32>,
}

/// Walk from `pid` up to the first app bundle. Apps launched from a shell inherit its environment,
/// so the ancestry names the app where TERM_PROGRAM may not.
pub fn find(procs: &HashMap<u32, Proc>, mut pid: u32) -> Host {
    let mut host = Host::default();
    // Bounded, since pids reused between rows of a snapshot can form a cycle.
    for _ in 0..procs.len() {
        let Some(p) = procs.get(&pid) else { break };
        if host.tty.is_none() {
            host.tty = p.tty.clone();
        }
        if p.comm.ends_with("/Code Helper (Plugin)") {
            host.exthost = Some(pid);
        }
        // Helpers nest inside their app, so the outermost bundle is the app.
        if let Some(end) = p.comm.find(".app/Contents/") {
            host.app = Some(PathBuf::from(&p.comm[..end + 4]));
            break;
        }
        pid = p.ppid;
    }
    host
}

#[cfg(test)]
mod tests {
    use super::*;

    const CODE: &str = "/Applications/Visual Studio Code.app";

    fn table() -> HashMap<u32, Proc> {
        parse(&format!(
            "    1     0 ??       /sbin/launchd
              1785     1 ??       {CODE}/Contents/MacOS/Code
              2886  1785 ??       {CODE}/Contents/Frameworks/Code Helper.app/Contents/MacOS/Code Helper
             67072  2886 ttys011  /bin/zsh
             67417 67072 ttys011  claude
             68644 67417 ??       /bin/sh
             40032  1785 ??       {CODE}/Contents/Frameworks/Code Helper (Plugin).app/Contents/MacOS/Code Helper (Plugin)
              3941 40032 ??       /Users/me/.vscode/extensions/claude
               500     1 ??       /Applications/Ghostty.app/Contents/MacOS/ghostty
             19289   500 ttys002  /usr/bin/login
             19290 19289 ttys002  -/bin/zsh
               700     1 ??       tmux
               701   700 ttys009  /bin/zsh
               800   801 ??       loop
               801   800 ??       loop"
        ))
    }

    #[test]
    fn parses_rows_keeping_spaces_in_paths() {
        let procs = table();
        assert_eq!(
            procs[&67417],
            Proc {
                ppid: 67072,
                tty: Some("/dev/ttys011".into()),
                comm: "claude".into(),
            }
        );
        assert!(procs[&40032].comm.ends_with("MacOS/Code Helper (Plugin)"));
        assert_eq!(procs[&1].tty, None);
    }

    #[test]
    fn finds_the_app_and_the_nearest_terminal() {
        let procs = table();
        // A hook's shell has no terminal, so the one its session runs on counts.
        assert_eq!(
            find(&procs, 68644),
            Host {
                app: Some(CODE.into()),
                tty: Some("/dev/ttys011".into()),
                exthost: None,
            }
        );
        assert_eq!(
            find(&procs, 19290),
            Host {
                app: Some("/Applications/Ghostty.app".into()),
                tty: Some("/dev/ttys002".into()),
                exthost: None,
            }
        );
    }

    #[test]
    fn finds_the_extension_host_of_an_extension_process() {
        assert_eq!(
            find(&table(), 3941),
            Host {
                app: Some(CODE.into()),
                tty: None,
                exthost: Some(40032),
            }
        );
    }

    #[test]
    fn stops_without_an_app_at_launchd_or_a_cycle() {
        let procs = table();
        assert_eq!(find(&procs, 701).app, None);
        assert_eq!(find(&procs, 701).tty.as_deref(), Some("/dev/ttys009"));
        assert_eq!(find(&procs, 800), Host::default());
        assert_eq!(find(&procs, 999), Host::default());
    }
}
