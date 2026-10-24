//! Gists of the authenticated user.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};

use super::Client;

/// A gist as listed, without file contents.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct GistSummary {
    pub id: Option<String>,
    pub description: Option<String>,
}

/// A gist fetched by ID, with its files' contents.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Gist {
    pub id: String,
    pub public: Option<bool>,
    /// Files by name, in name order.
    pub files: BTreeMap<String, Option<GistFile>>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct GistFile {
    pub content: Option<String>,
}

impl Gist {
    /// A file's content, if the gist has that file.
    pub fn file_content(&self, filename: &str) -> Option<&str> {
        self.files.get(filename)?.as_ref()?.content.as_deref()
    }

    /// Every filename in the gist.
    pub fn file_names(&self) -> Vec<String> {
        self.files.keys().cloned().collect()
    }

    /// Whether the gist is private. A missing flag counts as public.
    pub fn is_private(&self) -> bool {
        !self.public.unwrap_or(true)
    }
}

#[derive(Debug, Deserialize)]
struct Created {
    id: Option<String>,
}

pub struct GistClient<'a> {
    client: &'a Client,
}

impl<'a> GistClient<'a> {
    pub fn new(client: &'a Client) -> Self {
        Self { client }
    }

    /// Search for a gist by its description, returning the gist ID if found.
    pub fn find_gist(&self, description: &str) -> Result<Option<String>> {
        let gists = self
            .client
            .get_all("gists")
            .context("Failed to list gists")?;
        let gists: Vec<GistSummary> =
            serde_json::from_value(Value::Array(gists)).context("Failed to parse gists")?;
        Ok(gists
            .into_iter()
            .find(|g| g.description.as_deref() == Some(description))
            .and_then(|g| g.id))
    }

    /// Fetch a gist by ID.
    pub fn get_gist(&self, id: &str) -> Result<Gist> {
        let value = self
            .client
            .get(&format!("gists/{id}"))
            .context("Failed to fetch gist")?;
        serde_json::from_value(value).context("Failed to parse gist")
    }

    /// Create a new gist with the given files. Returns the gist ID.
    pub fn create_gist(
        &self,
        description: &str,
        public: bool,
        files: &HashMap<String, String>,
    ) -> Result<String> {
        let mut file_map = serde_json::Map::new();
        for (name, content) in files {
            file_map.insert(name.clone(), json!({"content": content}));
        }

        let body = json!({
            "description": description,
            "public": public,
            "files": file_map,
        });

        let resp = self
            .client
            .post("gists", &body)
            .context("Failed to create gist")?;

        serde_json::from_value::<Created>(resp)
            .ok()
            .and_then(|c| c.id)
            .context("Gist ID not found in response")
    }

    /// Update gist files. Use None as the value to delete a file.
    pub fn update_files(&self, id: &str, files: &HashMap<String, Option<String>>) -> Result<()> {
        let mut file_map = serde_json::Map::new();
        for (name, content) in files {
            match content {
                Some(c) => {
                    file_map.insert(name.clone(), json!({"content": c}));
                }
                None => {
                    file_map.insert(name.clone(), Value::Null);
                }
            }
        }

        let body = json!({"files": file_map});
        self.client
            .patch(&format!("gists/{id}"), &body)
            .context("Failed to update gist")?;
        Ok(())
    }

    /// Delete a gist entirely.
    pub fn delete_gist(&self, id: &str) -> Result<()> {
        self.client
            .delete(&format!("gists/{id}"))
            .context("Failed to delete gist")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gist_files_and_visibility() {
        let gist: Gist = serde_json::from_value(json!({
            "id": "abc",
            "public": false,
            "files": {
                "b.yml": { "content": "b: 1" },
                "a.txt": { "content": "hello", "truncated": false },
                "gone": null
            }
        }))
        .unwrap();
        assert_eq!(gist.file_content("a.txt"), Some("hello"));
        assert_eq!(gist.file_content("gone"), None);
        assert_eq!(gist.file_content("missing"), None);
        assert_eq!(gist.file_names(), vec!["a.txt", "b.yml", "gone"]);
        assert!(gist.is_private());

        let gist: Gist = serde_json::from_value(json!({ "id": "x" })).unwrap();
        assert!(!gist.is_private());
        assert!(gist.file_names().is_empty());
    }

    #[test]
    fn summaries_tolerate_null_descriptions() {
        let gists: Vec<GistSummary> = serde_json::from_value(json!([
            { "id": "1", "description": null },
            { "id": "2", "description": "gh-arriba config" }
        ]))
        .unwrap();
        assert_eq!(gists[0].description, None);
        assert_eq!(gists[1].id.as_deref(), Some("2"));
    }
}
