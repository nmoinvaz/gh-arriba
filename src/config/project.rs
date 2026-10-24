use anyhow::Result;
use std::path::{Path, PathBuf};
use std::process::Command;

pub struct ProjectConfig {
    pub root: PathBuf,
    pub name: String,
    pub public: bool,
}

impl ProjectConfig {
    pub fn new(name: Option<String>, root: Option<PathBuf>, private: bool) -> Result<Self> {
        let root = match root {
            Some(r) => expand_tilde(&r),
            None => git_toplevel().unwrap_or_else(|| std::env::current_dir().unwrap()),
        };
        // Canonicalize to resolve symlinks, trailing slashes, etc.
        let root = root.canonicalize().unwrap_or(root);
        let name = name.unwrap_or_else(|| {
            root.file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string()
        });
        Ok(Self {
            root,
            name,
            public: !private,
        })
    }

    pub fn gist_description(&self) -> String {
        format!("{} config-sync", self.name)
    }

    pub fn manifest_name(&self) -> String {
        format!(".{}-config-sync.yaml", self.name)
    }

    /// Make a path relative to the project root, stripping the root prefix
    /// and any leading ./ or /.
    pub fn relative_path(&self, path: &str) -> String {
        let root_str = self.root.to_string_lossy();
        let stripped = path
            .strip_prefix(root_str.as_ref())
            .or_else(|| path.strip_prefix("./"))
            .unwrap_or(path);
        stripped.strip_prefix('/').unwrap_or(stripped).to_string()
    }

    /// Convert a local path to a gist-safe filename using _ as directory separator.
    ///
    /// A platform-specific file gets a `[platform]` tag inserted before the
    /// extension so variants of the same path (e.g. macos vs windows) map to
    /// distinct gist files without mangling the extension
    /// (`name_dir_settings[macos].json`).
    pub fn gist_filename(&self, path: &str, platform: Option<&str>) -> String {
        let normalized = path.strip_prefix("./").unwrap_or(path);
        let base = format!("{}_{}", self.name, normalized.replace('/', "_"));
        let Some(p) = platform else { return base };
        match Path::new(normalized).extension().and_then(|e| e.to_str()) {
            Some(ext) => {
                let stem = base.strip_suffix(&format!(".{ext}")).unwrap_or(&base);
                format!("{stem}[{p}].{ext}")
            }
            None => format!("{base}[{p}]"),
        }
    }

    pub fn repo_url(&self) -> Option<String> {
        let output = Command::new("git")
            .args(["remote", "get-url", "origin"])
            .current_dir(&self.root)
            .output()
            .ok()?;
        if output.status.success() {
            let url = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if url.is_empty() { None } else { Some(url) }
        } else {
            None
        }
    }
}

fn expand_tilde(path: &Path) -> PathBuf {
    let s = path.to_string_lossy();
    if (s.starts_with("~/") || s == "~")
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home).join(s.strip_prefix("~/").unwrap_or(""));
    }
    path.to_path_buf()
}

fn git_toplevel() -> Option<PathBuf> {
    let output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()?;
    if output.status.success() {
        let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
        Some(PathBuf::from(path))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> ProjectConfig {
        ProjectConfig {
            root: PathBuf::from("/tmp/proj"),
            name: "proj".to_string(),
            public: true,
        }
    }

    #[test]
    fn gist_filename_flattens_path_separators() {
        let c = config();
        assert_eq!(
            c.gist_filename(".vscode/settings.json", None),
            "proj_.vscode_settings.json"
        );
    }

    #[test]
    fn gist_filename_tags_platform_before_extension() {
        let c = config();
        assert_eq!(
            c.gist_filename(".vscode/cmake-variants.yaml", Some("macos")),
            "proj_.vscode_cmake-variants[macos].yaml"
        );
    }

    #[test]
    fn gist_filename_tags_platform_when_no_extension() {
        let c = config();
        assert_eq!(
            c.gist_filename("Makefile", Some("linux")),
            "proj_Makefile[linux]"
        );
    }

    #[test]
    fn platform_variants_of_same_path_do_not_collide() {
        let c = config();
        let mac = c.gist_filename(".vscode/cmake-variants.yaml", Some("macos"));
        let win = c.gist_filename(".vscode/cmake-variants.yaml", Some("windows"));
        let generic = c.gist_filename(".vscode/cmake-variants.yaml", None);
        assert_ne!(mac, win);
        assert_ne!(mac, generic);
        assert_ne!(win, generic);
    }

    #[test]
    fn gist_filename_ignores_a_leading_dot_slash() {
        let c = config();
        assert_eq!(c.gist_filename("./a/b.txt", None), "proj_a_b.txt");
    }

    #[test]
    fn names_derive_from_the_project_name() {
        let c = config();
        assert_eq!(c.gist_description(), "proj config-sync");
        assert_eq!(c.manifest_name(), ".proj-config-sync.yaml");
    }

    #[test]
    fn relative_path_strips_root_and_prefixes() {
        let c = config();
        assert_eq!(
            c.relative_path("/tmp/proj/.vscode/a.json"),
            ".vscode/a.json"
        );
        assert_eq!(c.relative_path("./.vscode/a.json"), ".vscode/a.json");
        assert_eq!(c.relative_path("/a.json"), "a.json");
        assert_eq!(c.relative_path("a.json"), "a.json");
    }

    #[test]
    fn new_defaults_the_name_to_the_root_basename() {
        let root = std::env::temp_dir().join(format!("arriba-project-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();

        // The trailing `.` is canonicalized away.
        let c = ProjectConfig::new(None, Some(root.join(".")), true).unwrap();
        assert_eq!(c.root, root.canonicalize().unwrap());
        assert_eq!(c.name, format!("arriba-project-{}", std::process::id()));
        assert!(!c.public);

        let c = ProjectConfig::new(Some("named".into()), Some(root.clone()), false).unwrap();
        assert_eq!(c.name, "named");
        assert!(c.public);

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn tilde_expands_to_home() {
        let Some(home) = std::env::var_os("HOME") else {
            return;
        };
        let home = PathBuf::from(home);
        assert_eq!(expand_tilde(Path::new("~/x/y")), home.join("x/y"));
        assert_eq!(expand_tilde(Path::new("~")), home.join(""));
        assert_eq!(expand_tilde(Path::new("a/~/b")), PathBuf::from("a/~/b"));
        assert_eq!(expand_tilde(Path::new("~user")), PathBuf::from("~user"));
    }

    #[test]
    fn repo_url_is_none_outside_a_checkout() {
        let c = ProjectConfig {
            root: PathBuf::from("/nonexistent/arriba"),
            ..config()
        };
        assert_eq!(c.repo_url(), None);
    }
}
