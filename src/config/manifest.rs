use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::config::project::ProjectConfig;

#[derive(Debug, Serialize, Deserialize)]
pub struct Manifest {
    pub name: String,
    pub description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    pub url: String,
    pub files: Vec<ManifestFile>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestFile {
    pub path: String,
    pub gist: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
    #[serde(default = "epoch")]
    pub updated_at: DateTime<Utc>,
}

fn epoch() -> DateTime<Utc> {
    DateTime::<Utc>::from_timestamp(0, 0).unwrap()
}

impl Manifest {
    pub fn new(config: &ProjectConfig, files: &[ManifestFile]) -> Self {
        Self {
            name: config.name.clone(),
            description: "Configuration files synced by config-sync".to_string(),
            repo: config.repo_url(),
            url: "https://github.com/nmoinvaz/gh-arriba".to_string(),
            files: files.to_vec(),
        }
    }

    /// Create a manifest entry for a path with an optional platform and timestamp.
    pub fn entry(
        config: &ProjectConfig,
        path: &str,
        platform: Option<&str>,
        updated_at: DateTime<Utc>,
    ) -> ManifestFile {
        ManifestFile {
            path: path.to_string(),
            gist: config.gist_filename(path, platform),
            platform: platform.map(|s| s.to_string()),
            updated_at,
        }
    }

    pub fn to_yaml(&self) -> String {
        serde_yaml::to_string(self).unwrap_or_default()
    }

    pub fn from_yaml(yaml: &str) -> Option<Self> {
        serde_yaml::from_str(yaml).ok()
    }
}

/// Returns the current platform name: "macos", "linux", or "windows".
pub fn current_platform() -> &'static str {
    match std::env::consts::OS {
        "macos" => "macos",
        "linux" => "linux",
        "windows" => "windows",
        other => other,
    }
}
