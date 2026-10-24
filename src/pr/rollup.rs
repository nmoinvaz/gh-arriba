use chrono::{DateTime, Utc};
use std::collections::HashMap;

use crate::api::pulls::CheckRun;

/// Keep the latest attempt of each check, as `gh pr checks` does, so reruns supersede failures.
pub fn latest(runs: Vec<CheckRun>) -> Vec<CheckRun> {
    let mut by_key: HashMap<(String, String, String), CheckRun> = HashMap::new();
    for run in runs {
        let key = (run.workflow.clone(), run.name.clone(), run.event.clone());
        match by_key.get(&key) {
            Some(existing) if recency(existing) >= recency(&run) => {}
            _ => {
                by_key.insert(key, run);
            }
        }
    }
    let mut runs: Vec<CheckRun> = by_key.into_values().collect();
    runs.sort_by_key(|r| (r.workflow.to_lowercase(), r.name.to_lowercase()));
    runs
}

/// Ordering key for attempts. A queued rerun has no start time yet but is the newest attempt.
fn recency(run: &CheckRun) -> (bool, Option<DateTime<Utc>>) {
    (run.started_at.is_none(), run.started_at)
}
