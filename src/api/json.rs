use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde_json::Value;

/// String field of a JSON object, empty when missing.
pub fn str_of(v: &Value, key: &str) -> String {
    v[key].as_str().unwrap_or_default().to_string()
}

/// Login of a GraphQL actor or REST user, or "ghost" for deleted accounts.
pub fn login_of(actor: &Value) -> String {
    actor["login"].as_str().unwrap_or("ghost").to_string()
}

/// RFC 3339 timestamp field that must be present.
pub fn time_of(v: &Value, key: &str) -> Result<DateTime<Utc>> {
    let s = v[key].as_str().with_context(|| format!("{key} missing"))?;
    Ok(DateTime::parse_from_rfc3339(s)
        .with_context(|| format!("Bad timestamp {s}"))?
        .with_timezone(&Utc))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn fields_with_fallbacks() {
        let v = json!({"name": "a", "at": "2026-01-02T03:04:05Z", "user": null, "bad": "x"});
        assert_eq!(str_of(&v, "name"), "a");
        assert_eq!(str_of(&v, "missing"), "");
        assert_eq!(login_of(&v["user"]), "ghost");
        assert_eq!(login_of(&json!({"login": "me"})), "me");
        assert!(time_of(&v, "at").is_ok());
        assert!(time_of(&v, "missing").is_err());
    }
}
