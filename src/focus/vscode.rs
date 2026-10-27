//! VS Code has no scripting, but opening a folder that a window already shows focuses that window.

use anyhow::Result;
use serde::Deserialize;
use std::path::{Path, PathBuf};

use crate::context;

/// The workspace folder of the VS Code window hosting this process, when the Claude Code
/// extension is installed. Each window's extension listens on a port, names a lock file after
/// it, and sets it as CLAUDE_CODE_SSE_PORT in the window's terminals.
pub fn folder(exthost: Option<u32>) -> Option<PathBuf> {
    let ide = config_dir()?.join("ide");
    // The extension host is in the ancestry, so it outranks an inherited variable.
    let ports = match (exthost, std::env::var("CLAUDE_CODE_SSE_PORT")) {
        (Some(pid), _) => listening_ports(pid),
        (None, Ok(port)) => vec![port],
        (None, Err(_)) => return None,
    };
    ports
        .iter()
        .find_map(|port| lock_folder(&ide.join(format!("{port}.lock"))))
}

/// Claude Code's configuration directory.
fn config_dir() -> Option<PathBuf> {
    match std::env::var_os("CLAUDE_CONFIG_DIR") {
        Some(dir) => Some(dir.into()),
        None => std::env::var_os("HOME").map(|home| Path::new(&home).join(".claude")),
    }
}

/// The TCP ports `pid` listens on.
fn listening_ports(pid: u32) -> Vec<String> {
    let pid = pid.to_string();
    let args = ["-nP", "-a", "-p", &pid, "-iTCP", "-sTCP:LISTEN", "-Fn"];
    // lsof fails when the process listens on nothing.
    context::run("lsof", &args, None)
        .map(|out| ports(&out))
        .unwrap_or_default()
}

/// The ports of the `n` name lines in lsof's field output, such as n127.0.0.1:57946.
fn ports(lsof: &str) -> Vec<String> {
    lsof.lines()
        .filter_map(|line| line.strip_prefix('n')?.rsplit_once(':'))
        .map(|(_, port)| port.to_string())
        .collect()
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Lock {
    workspace_folders: Vec<PathBuf>,
}

/// The first workspace folder in an IDE lock file.
fn lock_folder(path: &Path) -> Option<PathBuf> {
    let lock: Lock = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    lock.workspace_folders.into_iter().next()
}

/// Focus the window whose workspace folder holds `dir`. Only VS Code comes forward when no window
/// does, since opening an unopened folder would add a window.
pub fn focus(bundle: &str, dir: &Path) -> Result<()> {
    let folders = open_folders();
    let mut args = vec!["-b", bundle];
    if let Some(folder) = window_folder(&folders, dir).and_then(|f| f.to_str()) {
        args.push(folder);
    }
    context::run("open", &args, None).map(drop)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Storage {
    windows_state: Option<WindowsState>,
}

/// VS Code saves the last active window, and every window only when more than one is open.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WindowsState {
    last_active_window: Option<Window>,
    #[serde(default)]
    opened_windows: Vec<Window>,
}

#[derive(Deserialize)]
struct Window {
    folder: Option<String>,
}

/// The workspace folders of open windows, from the state VS Code keeps for restoring them. It
/// saves that state whenever one of its windows loses focus, so it stays current.
fn open_folders() -> Vec<PathBuf> {
    let Some(home) = std::env::var_os("HOME") else {
        return Vec::new();
    };
    let state =
        Path::new(&home).join("Library/Application Support/Code/User/globalStorage/storage.json");
    std::fs::read_to_string(state)
        .map(|json| folders(&json))
        .unwrap_or_default()
}

fn folders(storage: &str) -> Vec<PathBuf> {
    let Ok(Storage {
        windows_state: Some(state),
    }) = serde_json::from_str(storage)
    else {
        return Vec::new();
    };
    state
        .last_active_window
        .into_iter()
        .chain(state.opened_windows)
        .filter_map(|w| file_uri_path(&w.folder?))
        .collect()
}

/// The path of a file:// URI, with percent escapes decoded.
fn file_uri_path(uri: &str) -> Option<PathBuf> {
    let encoded = uri.strip_prefix("file://")?.as_bytes();
    let mut path = Vec::with_capacity(encoded.len());
    let mut i = 0;
    while i < encoded.len() {
        let escaped = encoded
            .get(i + 1..i + 3)
            .filter(|hex| hex.iter().all(u8::is_ascii_hexdigit))
            .and_then(|hex| u8::from_str_radix(std::str::from_utf8(hex).ok()?, 16).ok());
        match (encoded[i], escaped) {
            (b'%', Some(byte)) => {
                path.push(byte);
                i += 3;
            }
            (byte, _) => {
                path.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8(path).ok().map(PathBuf::from)
}

/// The deepest of `folders` holding `dir`, for checkouts nested inside others.
fn window_folder<'a>(folders: &'a [PathBuf], dir: &Path) -> Option<&'a PathBuf> {
    folders
        .iter()
        .filter(|folder| dir.starts_with(folder))
        .max_by_key(|folder| folder.components().count())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_ports_from_lsof_fields() {
        let out = "p40032\nf31\nn127.0.0.1:43759\nf40\nn[::1]:50814";
        assert_eq!(ports(out), ["43759", "50814"]);
    }

    #[test]
    fn reads_the_first_workspace_folder_of_a_lock_file() {
        let dir = std::env::temp_dir().join(format!("gh-arriba-lock-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let lock = dir.join("57946.lock");
        std::fs::write(
            &lock,
            r#"{"pid":1785,"workspaceFolders":["/src/client","/src/other"],"ideName":"Visual Studio Code"}"#,
        )
        .unwrap();
        assert_eq!(lock_folder(&lock), Some("/src/client".into()));
        assert_eq!(lock_folder(&dir.join("1.lock")), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn lists_the_last_active_window_and_the_others() {
        let storage = r#"{"windowsState":{
            "lastActiveWindow":{"folder":"file:///src/zlib-ng"},
            "openedWindows":[{"folder":"file:///src/My%20Repo"},{"workspaceIdentifier":{}}]
        }}"#;
        assert_eq!(
            folders(storage),
            [PathBuf::from("/src/zlib-ng"), PathBuf::from("/src/My Repo")]
        );
        assert!(folders("{}").is_empty());
        assert!(folders("not json").is_empty());
    }

    #[test]
    fn decodes_percent_escapes_in_file_uris() {
        assert_eq!(
            file_uri_path("file:///src/caf%C3%A9%20bar"),
            Some("/src/café bar".into())
        );
        // Escapes that are not hex stay as they are.
        assert_eq!(
            file_uri_path("file:///src/100%+1%2"),
            Some("/src/100%+1%2".into())
        );
        assert_eq!(file_uri_path("vscode-remote://ssh/src"), None);
    }

    #[test]
    fn picks_the_deepest_folder_holding_the_directory() {
        let folders = [
            PathBuf::from("/src"),
            PathBuf::from("/src/zlib-ng"),
            PathBuf::from("/src/zlib"),
        ];
        let deepest = |dir: &str| window_folder(&folders, Path::new(dir)).cloned();
        assert_eq!(deepest("/src/zlib-ng/test"), Some("/src/zlib-ng".into()));
        assert_eq!(deepest("/src/zlib-ngx"), Some("/src".into()));
        assert_eq!(deepest("/other"), None);
    }
}
