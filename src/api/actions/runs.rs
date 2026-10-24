use serde::{Deserialize, Serialize};

use super::null_default;

/// A user as the Actions API embeds it.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Actor {
    pub login: String,
}

/// A workflow run.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct WorkflowRun {
    pub id: u64,
    #[serde(default, deserialize_with = "null_default")]
    pub html_url: String,
    #[serde(default)]
    pub actor: Option<Actor>,
    /// Who started this attempt, which differs from `actor` on a re-run.
    #[serde(default)]
    pub triggering_actor: Option<Actor>,
}

impl WorkflowRun {
    /// Login of whoever started the run, preferring the one who triggered this attempt.
    pub fn started_by(&self) -> Option<&str> {
        self.triggering_actor
            .as_ref()
            .or(self.actor.as_ref())
            .map(|a| a.login.as_str())
    }
}
