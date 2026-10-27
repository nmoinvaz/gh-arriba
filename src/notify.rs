//! Native desktop notifications through the platform's own notifier, so long waits can end with a
//! banner instead of a terminal someone has to keep checking.

/// One desktop notification.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Note {
    pub title: String,
    pub body: String,
    /// Opened on click where the notifier supports it, otherwise appended to the body.
    pub url: Option<String>,
    /// Shown under the title where the notifier supports it.
    pub subtitle: Option<String>,
    /// Replaces the last note of the same group where the notifier supports it.
    pub group: Option<String>,
    /// Played with the note where the notifier supports it.
    pub sound: Option<String>,
}

impl Note {
    pub fn new(title: impl Into<String>, body: impl Into<String>, url: Option<String>) -> Self {
        Self {
            title: title.into(),
            body: body.into(),
            url,
            ..Self::default()
        }
    }

    /// The body with the URL on its own line, for notifiers that can't open links.
    fn body_with_url(&self) -> String {
        match &self.url {
            Some(url) if self.body.is_empty() => url.clone(),
            Some(url) => format!("{}\n{url}", self.body),
            None => self.body.clone(),
        }
    }
}

/// A notifier command: program, arguments, and environment variables.
#[derive(Debug, PartialEq)]
struct Invocation {
    program: &'static str,
    args: Vec<String>,
    env: Vec<(&'static str, String)>,
}

/// Shows a toast through the WinRT API. Title and body come from the environment so they need no
/// quoting.
const WINDOWS_TOAST: &str = "\
[Windows.UI.Notifications.ToastNotificationManager, Windows.UI.Notifications, ContentType = WindowsRuntime] | Out-Null
$template = [Windows.UI.Notifications.ToastNotificationManager]::GetTemplateContent([Windows.UI.Notifications.ToastTemplateType]::ToastText02)
$text = $template.GetElementsByTagName('text')
$text.Item(0).AppendChild($template.CreateTextNode($env:ARRIBA_NOTE_TITLE)) | Out-Null
$text.Item(1).AppendChild($template.CreateTextNode($env:ARRIBA_NOTE_BODY)) | Out-Null
$toast = [Windows.UI.Notifications.ToastNotification]::new($template)
$app = '{1AC14E77-02E7-4E5D-B744-2EB1AE5198B7}\\WindowsPowerShell\\v1.0\\powershell.exe'
[Windows.UI.Notifications.ToastNotificationManager]::CreateToastNotifier($app).Show($toast)";

/// The command that shows `note` on `os`, preferring a notifier `has` reports as installed.
fn invocation(os: &str, note: &Note, has: impl Fn(&str) -> bool) -> Option<Invocation> {
    match os {
        "macos" if has("terminal-notifier") => {
            let mut args = vec![
                "-title".into(),
                note.title.clone(),
                "-message".into(),
                note.body.clone(),
                "-group".into(),
                note.group.clone().unwrap_or_else(|| "gh-arriba".into()),
            ];
            if let Some(subtitle) = &note.subtitle {
                args.extend(["-subtitle".into(), subtitle.clone()]);
            }
            if let Some(sound) = &note.sound {
                args.extend(["-sound".into(), sound.clone()]);
            }
            if let Some(url) = &note.url {
                args.extend(["-open".into(), url.clone()]);
            }
            Some(Invocation {
                program: "terminal-notifier",
                args,
                env: Vec::new(),
            })
        }
        // Passing the text as script arguments avoids escaping it into AppleScript.
        "macos" => Some(Invocation {
            program: "osascript",
            args: vec![
                "-e".into(),
                "on run argv".into(),
                "-e".into(),
                "display notification (item 2 of argv) with title (item 1 of argv)".into(),
                "-e".into(),
                "end run".into(),
                note.title.clone(),
                note.body_with_url(),
            ],
            env: Vec::new(),
        }),
        "windows" => Some(Invocation {
            program: "powershell.exe",
            args: vec![
                "-NoProfile".into(),
                "-NonInteractive".into(),
                "-Command".into(),
                WINDOWS_TOAST.into(),
            ],
            env: vec![
                ("ARRIBA_NOTE_TITLE", note.title.clone()),
                ("ARRIBA_NOTE_BODY", note.body_with_url()),
            ],
        }),
        _ if has("notify-send") => Some(Invocation {
            program: "notify-send",
            args: vec![
                "--app-name=gh arriba".into(),
                // A title or body starting with a dash would otherwise parse as an option.
                "--".into(),
                note.title.clone(),
                note.body_with_url(),
            ],
            env: Vec::new(),
        }),
        _ => None,
    }
}

/// Whether `program` is on the PATH.
#[cfg_attr(test, allow(dead_code))]
fn on_path(program: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| {
        let file = dir.join(program);
        file.is_file() || (cfg!(windows) && file.with_extension("exe").is_file())
    })
}

#[cfg(not(test))]
fn show(note: &Note) -> bool {
    use std::process::{Command, Stdio};
    let Some(inv) = invocation(std::env::consts::OS, note, on_path) else {
        return false;
    };
    Command::new(inv.program)
        .args(&inv.args)
        .envs(inv.env)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

#[cfg(test)]
thread_local! {
    static SENT: std::cell::RefCell<Vec<Note>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Tests record notes instead of showing them.
#[cfg(test)]
fn show(note: &Note) -> bool {
    SENT.with(|s| s.borrow_mut().push(note.clone()));
    true
}

/// The notes this test thread has sent, clearing them.
#[cfg(test)]
pub fn sent() -> Vec<Note> {
    SENT.with(|s| std::mem::take(&mut *s.borrow_mut()))
}

/// Show `note`, or when no notifier works, ring the terminal bell and print it to stderr.
pub fn send(note: &Note) {
    if !show(note) {
        eprintln!(
            "\x07No desktop notifier found (install notify-send or terminal-notifier): {}: {}",
            note.title,
            note.body_with_url()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note(url: Option<&str>) -> Note {
        Note::new("CI passed", "Fix inflate", url.map(String::from))
    }

    #[test]
    fn macos_prefers_terminal_notifier_with_a_click_url() {
        let inv = invocation("macos", &note(Some("https://x/1")), |p| {
            p == "terminal-notifier"
        })
        .unwrap();
        assert_eq!(inv.program, "terminal-notifier");
        assert!(inv.args.ends_with(&["-open".into(), "https://x/1".into()]));
    }

    #[test]
    fn terminal_notifier_takes_the_group_subtitle_and_sound() {
        let has = |p: &str| p == "terminal-notifier";
        let inv = invocation("macos", &note(None), has).unwrap();
        assert!(inv.args.ends_with(&["-group".into(), "gh-arriba".into()]));

        let styled = Note {
            subtitle: Some("zlib-ng".into()),
            group: Some("gh-arriba:/src/zlib-ng".into()),
            sound: Some("Submarine".into()),
            ..note(None)
        };
        let inv = invocation("macos", &styled, has).unwrap();
        assert_eq!(
            inv.args[4..],
            [
                "-group",
                "gh-arriba:/src/zlib-ng",
                "-subtitle",
                "zlib-ng",
                "-sound",
                "Submarine"
            ]
        );
    }

    #[test]
    fn macos_falls_back_to_osascript_with_text_as_arguments() {
        let inv = invocation("macos", &note(Some("https://x/1")), |_| false).unwrap();
        assert_eq!(inv.program, "osascript");
        assert_eq!(
            inv.args[inv.args.len() - 2..],
            [
                "CI passed".to_string(),
                "Fix inflate\nhttps://x/1".to_string()
            ]
        );
    }

    #[test]
    fn linux_uses_notify_send_when_installed() {
        let inv = invocation("linux", &note(None), |p| p == "notify-send").unwrap();
        assert_eq!(inv.program, "notify-send");
        assert_eq!(inv.args[1..], ["--", "CI passed", "Fix inflate"]);
        assert_eq!(invocation("linux", &note(None), |_| false), None);
    }

    #[test]
    fn windows_passes_text_through_the_environment() {
        let inv = invocation("windows", &note(Some("https://x/1")), |_| false).unwrap();
        assert_eq!(inv.program, "powershell.exe");
        assert!(!inv.args.iter().any(|a| a.contains("Fix inflate")));
        assert_eq!(
            inv.env,
            [
                ("ARRIBA_NOTE_TITLE", "CI passed".to_string()),
                ("ARRIBA_NOTE_BODY", "Fix inflate\nhttps://x/1".to_string()),
            ]
        );
    }

    #[test]
    fn records_notes_in_tests() {
        send(&note(None));
        assert_eq!(sent(), [note(None)]);
        assert!(sent().is_empty());
    }
}
