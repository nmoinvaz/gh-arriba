use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::collections::HashMap;

use crate::github::Client;

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
        Ok(gists
            .iter()
            .find(|g| g["description"].as_str() == Some(description))
            .and_then(|g| g["id"].as_str())
            .map(str::to_string))
    }

    /// Fetch a gist by ID.
    pub fn get_gist(&self, id: &str) -> Result<Value> {
        self.client
            .get(&format!("gists/{id}"))
            .context("Failed to fetch gist")
    }

    /// Extract a file's content from a gist response.
    pub fn get_file_content<'v>(&self, gist: &'v Value, filename: &str) -> Option<&'v str> {
        gist["files"][filename]["content"].as_str()
    }

    /// List all filenames in a gist response.
    pub fn get_file_names(&self, gist: &Value) -> Vec<String> {
        gist["files"]
            .as_object()
            .map(|files| files.keys().cloned().collect())
            .unwrap_or_default()
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

        resp["id"]
            .as_str()
            .map(|s| s.to_string())
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
