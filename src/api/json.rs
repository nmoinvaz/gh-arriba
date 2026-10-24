use serde_json::Value;

/// String field of a JSON object, empty when missing.
pub fn str_of(v: &Value, key: &str) -> String {
    v[key].as_str().unwrap_or_default().to_string()
}

/// Login of a GraphQL actor or REST user, or "ghost" for deleted accounts.
pub fn login_of(actor: &Value) -> String {
    actor["login"].as_str().unwrap_or("ghost").to_string()
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
    }
}
