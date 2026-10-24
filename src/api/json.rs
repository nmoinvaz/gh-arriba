use serde_json::Value;

/// String field of a JSON object, empty when missing.
pub fn str_of(v: &Value, key: &str) -> String {
    v[key].as_str().unwrap_or_default().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn fields_with_fallbacks() {
        let v = json!({"name": "a"});
        assert_eq!(str_of(&v, "name"), "a");
        assert_eq!(str_of(&v, "missing"), "");
    }
}
