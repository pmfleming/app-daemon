use std::collections::HashSet;

use serde_json::Value;

pub fn load_fixture(source: &str) -> serde_json::Result<Value> {
    serde_json::from_str(source)
}

pub fn validate_unique_names(values: &[&str]) -> Result<(), String> {
    let mut seen = HashSet::new();
    for value in values {
        if !seen.insert(*value) {
            return Err(format!("duplicate protocol name: {value}"));
        }
    }
    Ok(())
}

pub fn fixture_names<'a>(fixture: &'a Value, section: &str) -> Result<Vec<&'a str>, String> {
    let registry = fixture
        .get("registry")
        .ok_or_else(|| "fixture registry is missing".to_owned())?;
    registry_names(registry, section)
}

pub fn registry_names<'a>(registry: &'a Value, section: &str) -> Result<Vec<&'a str>, String> {
    registry
        .get(section)
        .and_then(Value::as_array)
        .ok_or_else(|| format!("protocol registry section is missing: {section}"))?
        .iter()
        .map(|item| {
            item.get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| format!("protocol registry name is invalid: {section}"))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{fixture_names, validate_unique_names};

    #[test]
    fn validates_names_and_extracts_fixture_sections() -> Result<(), String> {
        assert!(validate_unique_names(&["one", "two"]).is_ok());
        assert!(validate_unique_names(&["one", "one"]).is_err());
        let fixture = json!({ "registry": { "methods": [{ "name": "one" }] } });
        assert_eq!(fixture_names(&fixture, "methods")?, ["one"]);
        Ok(())
    }
}
