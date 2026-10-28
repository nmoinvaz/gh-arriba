//! Starts the inbox watch at login: a LaunchAgent on macOS, a systemd user service on Linux.

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::cli::NotifyWatchArgs;
use crate::context::{self, Ctx};
use crate::output::xml_escape;

const LABEL: &str = "com.github.nmoinvaz.gh-arriba.notify-watch";
const UNIT: &str = "gh-arriba-notify-watch.service";

/// Only macOS and Linux have a service manager this registers with.
fn ensure_supported() -> Result<()> {
    match std::env::consts::OS {
        "macos" | "linux" => Ok(()),
        os => bail!("Starting the watch at login is not supported on {os} yet"),
    }
}

/// Register the watch with `args` to start at every login, and start it now.
pub fn install(ctx: &Ctx, args: &NotifyWatchArgs) -> Result<()> {
    ensure_supported()?;
    let exe = std::env::current_exe().context("Failed to find the gh-arriba executable")?;
    let argv = watch_argv(&exe, args, ctx.repo_flag.as_deref(), ctx.json);
    // Services start with a bare PATH, so the current one lets the watch find gh and the notifier.
    let path = std::env::var("PATH").unwrap_or_default();
    let home = home()?;
    match std::env::consts::OS {
        "macos" => {
            let plist = launch_agent(&home);
            let log = home.join("Library/Logs/gh-arriba-notify-watch.log");
            write(&plist, &plist_xml(&argv, &path, &log))?;
            let domain = gui_domain()?;
            // Unloading first makes a second install pick up its new options.
            let _ = launchctl(&["bootout", &format!("{domain}/{LABEL}")]);
            bootstrap(&domain, &plist)?;
            println!("Started the watch, and it starts again at every login");
            println!("  Agent: {}", plist.display());
            println!("  Log:   {}", log.display());
        }
        "linux" => {
            let unit = systemd_unit(&home);
            write(&unit, &unit_file(&argv, &path))?;
            systemctl(&["daemon-reload"])?;
            systemctl(&["enable", UNIT])?;
            // Restart rather than start, so a running watch picks up new options.
            systemctl(&["restart", UNIT])?;
            println!("Started the watch, and it starts again at every login");
            println!("  Unit: {}", unit.display());
            println!("  Log:  journalctl --user -u {UNIT}");
        }
        _ => unreachable!("ensure_supported checked the OS"),
    }
    Ok(())
}

/// Stop the watch that starts at login and remove its registration. False when there was none.
pub fn uninstall() -> Result<bool> {
    ensure_supported()?;
    let home = home()?;
    let file = match std::env::consts::OS {
        "macos" => {
            // Failing to unload only means it was not loaded.
            let _ = launchctl(&["bootout", &format!("{}/{LABEL}", gui_domain()?)]);
            launch_agent(&home)
        }
        "linux" => {
            let _ = systemctl(&["disable", "--now", UNIT]);
            systemd_unit(&home)
        }
        _ => unreachable!("ensure_supported checked the OS"),
    };
    if !file.exists() {
        return Ok(false);
    }
    std::fs::remove_file(&file).with_context(|| format!("Failed to remove {}", file.display()))?;
    if std::env::consts::OS == "linux" {
        systemctl(&["daemon-reload"])?;
    }
    println!("Stopped the watch, and removed {}", file.display());
    Ok(true)
}

/// The command line the service runs: this executable and the watch options given now.
fn watch_argv(exe: &Path, args: &NotifyWatchArgs, repo: Option<&str>, json: bool) -> Vec<String> {
    let mut argv: Vec<String> = vec![
        exe.to_string_lossy().into_owned(),
        "notify".into(),
        "watch".into(),
        "--interval".into(),
        args.interval.to_string(),
    ];
    for reason in &args.reason {
        argv.extend(["--reason".into(), reason.clone()]);
    }
    if args.participating {
        argv.push("--participating".into());
    }
    if let Some(repo) = repo {
        argv.extend(["-R".into(), repo.into()]);
    }
    if json {
        argv.push("--json".into());
    }
    argv
}

fn home() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is not set")
}

fn write(path: &Path, contents: &str) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("Failed to create {}", dir.display()))?;
    }
    std::fs::write(path, contents).with_context(|| format!("Failed to write {}", path.display()))
}

fn path_str(path: &Path) -> Result<&str> {
    path.to_str()
        .with_context(|| format!("{} is not valid UTF-8", path.display()))
}

fn launch_agent(home: &Path) -> PathBuf {
    home.join(format!("Library/LaunchAgents/{LABEL}.plist"))
}

/// The launchd domain of the logged-in user's session, where agents can show notifications.
fn gui_domain() -> Result<String> {
    Ok(format!("gui/{}", context::run("id", &["-u"], None)?))
}

fn launchctl(args: &[&str]) -> Result<String> {
    context::run("launchctl", args, None)
}

/// Load the agent, retrying while launchd finishes unloading an earlier copy.
fn bootstrap(domain: &str, plist: &Path) -> Result<()> {
    let plist = path_str(plist)?;
    let mut tries = 0;
    loop {
        match launchctl(&["bootstrap", domain, plist]) {
            Ok(_) => return Ok(()),
            Err(_) if tries < 10 => {
                tries += 1;
                std::thread::sleep(Duration::from_millis(200));
            }
            Err(e) => return Err(e),
        }
    }
}

/// A LaunchAgent that runs `argv` at login with `path`, restarting it a minute after it exits, as
/// when the first poll runs before the network is up.
fn plist_xml(argv: &[String], path: &str, log: &Path) -> String {
    let args: String = argv
        .iter()
        .map(|a| format!("    <string>{}</string>\n", xml_escape(a)))
        .collect();
    let path = xml_escape(path);
    let log = xml_escape(&log.to_string_lossy());
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{LABEL}</string>
  <key>ProgramArguments</key>
  <array>
{args}  </array>
  <key>EnvironmentVariables</key>
  <dict>
    <key>PATH</key>
    <string>{path}</string>
  </dict>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <true/>
  <key>ThrottleInterval</key>
  <integer>60</integer>
  <key>StandardOutPath</key>
  <string>{log}</string>
  <key>StandardErrorPath</key>
  <string>{log}</string>
</dict>
</plist>
"#
    )
}

fn systemd_unit(home: &Path) -> PathBuf {
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".config"));
    config.join("systemd/user").join(UNIT)
}

fn systemctl(args: &[&str]) -> Result<String> {
    let args: Vec<&str> = ["--user"].into_iter().chain(args.iter().copied()).collect();
    context::run("systemctl", &args, None)
}

/// A user service that runs `argv` with `path` from login, restarting it a minute after it exits.
/// The journal keeps its output.
fn unit_file(argv: &[String], path: &str) -> String {
    let exec: Vec<String> = argv.iter().map(|a| exec_word(a)).collect();
    format!(
        "[Unit]
Description=gh arriba notify watch

[Service]
ExecStart={}
Environment={}
Restart=always
RestartSec=60

[Install]
WantedBy=default.target
",
        exec.join(" "),
        quoted(&format!("PATH={}", path.replace('%', "%%")))
    )
}

/// One word of an ExecStart line. systemd expands % specifiers and $ variables there, so both are
/// doubled, and a word with spaces or quotes is quoted.
fn exec_word(word: &str) -> String {
    let word = word.replace('%', "%%").replace('$', "$$");
    if word.contains(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '\\' | ';')) {
        quoted(&word)
    } else {
        word
    }
}

fn quoted(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', r"\\").replace('"', "\\\""))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args() -> NotifyWatchArgs {
        NotifyWatchArgs {
            interval: 90,
            reason: vec!["mention".into(), "review_requested".into()],
            participating: true,
            all: false,
            install: true,
            uninstall: false,
        }
    }

    #[test]
    fn the_service_runs_the_watch_with_the_options_given_at_install() {
        let exe = Path::new("/x/gh-arriba");
        assert_eq!(
            watch_argv(exe, &args(), Some("o/r"), true),
            [
                "/x/gh-arriba",
                "notify",
                "watch",
                "--interval",
                "90",
                "--reason",
                "mention",
                "--reason",
                "review_requested",
                "--participating",
                "-R",
                "o/r",
                "--json",
            ]
        );
        let bare = NotifyWatchArgs {
            interval: 60,
            reason: Vec::new(),
            participating: false,
            ..args()
        };
        assert_eq!(
            watch_argv(exe, &bare, None, false),
            ["/x/gh-arriba", "notify", "watch", "--interval", "60"]
        );
    }

    #[test]
    fn launch_agents_escape_their_strings() {
        let argv = vec!["/Apps/A&B/gh-arriba".to_string(), "<watch>".to_string()];
        let xml = plist_xml(&argv, "/opt/homebrew/bin:/usr/bin", Path::new("/u/l.log"));
        assert!(xml.contains("    <string>/Apps/A&amp;B/gh-arriba</string>\n"));
        assert!(xml.contains("    <string>&lt;watch&gt;</string>\n  </array>"));
        assert!(xml.contains("<string>/opt/homebrew/bin:/usr/bin</string>"));
        assert!(xml.contains(&format!("<string>{LABEL}</string>")));
        assert_eq!(xml.matches("<string>/u/l.log</string>").count(), 2);
    }

    #[test]
    fn units_quote_and_escape_their_words() {
        assert_eq!(exec_word("--interval"), "--interval");
        assert_eq!(exec_word("/My Apps/gh-arriba"), r#""/My Apps/gh-arriba""#);
        assert_eq!(exec_word("100%"), "100%%");
        assert_eq!(exec_word("$HOME"), "$$HOME");
        assert_eq!(exec_word(r#"say "hi""#), r#""say \"hi\"""#);

        let argv = vec!["/x/gh arriba".to_string(), "notify".to_string()];
        let unit = unit_file(&argv, "/usr/bin:/a%b");
        assert!(unit.contains("\nExecStart=\"/x/gh arriba\" notify\n"));
        assert!(unit.contains("\nEnvironment=\"PATH=/usr/bin:/a%%b\"\n"));
        assert!(unit.contains("\nRestart=always\nRestartSec=60\n"));
        assert!(unit.ends_with("[Install]\nWantedBy=default.target\n"));
    }

    /// systemd checks the unit itself wherever it is installed, as on the Linux CI runners.
    #[test]
    fn units_pass_systemd_analyze_verify() {
        use std::process::Command;
        if Command::new("systemd-analyze")
            .arg("--version")
            .output()
            .is_err()
        {
            return;
        }
        let dir = std::env::temp_dir().join(format!("gh-arriba-unit-{}", std::process::id()));
        // A path with a space exercises the quoting.
        let exe = dir.join("My Apps/gh-arriba");
        std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
        std::fs::copy("/bin/sleep", &exe).unwrap();
        let unit = dir.join(UNIT);
        let argv = [exe.to_string_lossy().into_owned(), "100".to_string()];
        std::fs::write(&unit, unit_file(&argv, "/usr/bin:/a%b")).unwrap();

        let out = Command::new("systemd-analyze")
            .args(["verify", unit.to_str().unwrap()])
            .output()
            .unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}
