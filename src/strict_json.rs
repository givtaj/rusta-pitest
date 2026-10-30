//! Bounded JSON/YAML parsing shared by contract ingestion and suite loading.

use std::{cell::Cell, fmt, rc::Rc};

use serde::de::{self, DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Number, Value};

pub const MAX_DOCUMENT_BYTES: usize = 1024 * 1024;
pub const MAX_CONFIGURED_BYTES: u64 = 64 * 1024 * 1024;
const MAX_DEPTH: usize = 64;
const MAX_NODES: usize = 100_000;

struct Budget {
    nodes: Cell<usize>,
    remaining_string_bytes: Cell<usize>,
}

impl Budget {
    fn charge_string<E: de::Error>(&self, bytes: usize) -> Result<(), E> {
        let remaining = self
            .remaining_string_bytes
            .get()
            .checked_sub(bytes)
            .ok_or_else(|| E::custom("document decoded string byte limit exceeded"))?;
        self.remaining_string_bytes.set(remaining);
        Ok(())
    }
}

struct Seed {
    depth: usize,
    budget: Rc<Budget>,
}

impl Seed {
    fn new(max_bytes: usize) -> Self {
        Self {
            depth: 0,
            budget: Rc::new(Budget {
                nodes: Cell::new(0),
                remaining_string_bytes: Cell::new(max_bytes),
            }),
        }
    }

    fn child(&self) -> Self {
        Self {
            depth: self.depth + 1,
            budget: Rc::clone(&self.budget),
        }
    }
}

impl<'de> DeserializeSeed<'de> for Seed {
    type Value = Value;

    fn deserialize<D: serde::Deserializer<'de>>(self, deserializer: D) -> Result<Value, D::Error> {
        if self.depth > MAX_DEPTH {
            return Err(de::Error::custom("document nesting limit exceeded"));
        }
        let nodes = self.budget.nodes.get() + 1;
        if nodes > MAX_NODES {
            return Err(de::Error::custom("document node limit exceeded"));
        }
        self.budget.nodes.set(nodes);
        deserializer.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for Seed {
    type Value = Value;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON-compatible value with unique object keys")
    }

    fn visit_bool<E: de::Error>(self, value: bool) -> Result<Value, E> {
        Ok(Value::Bool(value))
    }

    fn visit_i64<E: de::Error>(self, value: i64) -> Result<Value, E> {
        Ok(Value::Number(value.into()))
    }

    fn visit_u64<E: de::Error>(self, value: u64) -> Result<Value, E> {
        Ok(Value::Number(value.into()))
    }

    fn visit_f64<E: de::Error>(self, value: f64) -> Result<Value, E> {
        Number::from_f64(value)
            .map(Value::Number)
            .ok_or_else(|| E::custom("non-finite number is not JSON-compatible"))
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<Value, E> {
        self.budget.charge_string(value.len())?;
        Ok(Value::String(value.to_owned()))
    }

    fn visit_string<E: de::Error>(self, value: String) -> Result<Value, E> {
        self.budget.charge_string(value.len())?;
        Ok(Value::String(value))
    }

    fn visit_none<E: de::Error>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }

    fn visit_unit<E: de::Error>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }

    fn visit_some<D: serde::Deserializer<'de>>(self, deserializer: D) -> Result<Value, D::Error> {
        self.child().deserialize(deserializer)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut sequence: A) -> Result<Value, A::Error> {
        let mut values = Vec::new();
        while let Some(value) = sequence.next_element_seed(self.child())? {
            values.push(value);
        }
        Ok(Value::Array(values))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut mapping: A) -> Result<Value, A::Error> {
        let mut values = Map::new();
        while let Some(key) = mapping.next_key_seed(KeySeed(Rc::clone(&self.budget)))? {
            if values.contains_key(&key) {
                return Err(de::Error::custom("duplicate key"));
            }
            let value = mapping.next_value_seed(self.child())?;
            values.insert(key, value);
        }
        Ok(Value::Object(values))
    }
}

// Aliased YAML keys can expand just like string values. Charge borrowed text
// before making the owned key rather than deserializing an unbudgeted String.
struct KeySeed(Rc<Budget>);

impl<'de> DeserializeSeed<'de> for KeySeed {
    type Value = String;

    fn deserialize<D: serde::Deserializer<'de>>(self, deserializer: D) -> Result<String, D::Error> {
        deserializer.deserialize_string(self)
    }
}

impl<'de> Visitor<'de> for KeySeed {
    type Value = String;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a string object key")
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<String, E> {
        self.0.charge_string(value.len())?;
        Ok(value.to_owned())
    }

    fn visit_string<E: de::Error>(self, value: String) -> Result<String, E> {
        self.0.charge_string(value.len())?;
        Ok(value)
    }
}

fn safe_error(format: &str, error: impl fmt::Display) -> String {
    let error = error.to_string();
    if error.contains("duplicate key") {
        format!("invalid {format}: duplicate object key")
    } else if error.contains("decoded string byte limit exceeded") {
        format!("invalid {format}: decoded string and key byte limit exceeded")
    } else if error.contains("limit exceeded") || error.contains("recursion limit exceeded") {
        format!("invalid {format}: document nesting or node limit exceeded")
    } else {
        format!("invalid {format}: expected one JSON-compatible document")
    }
}

pub fn parse_json(bytes: &[u8]) -> Result<Value, String> {
    parse_json_with_limit(bytes, MAX_DOCUMENT_BYTES)
}

pub fn parse_json_with_limit(bytes: &[u8], max_bytes: usize) -> Result<Value, String> {
    if bytes.len() > max_bytes {
        return Err("document exceeds the configured size limit".into());
    }
    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    let value = Seed::new(max_bytes)
        .deserialize(&mut deserializer)
        .map_err(|error| safe_error("JSON", error))?;
    deserializer
        .end()
        .map_err(|error| safe_error("JSON", error))?;
    Ok(value)
}

#[cfg(test)]
pub fn parse_yaml(bytes: &[u8]) -> Result<Value, String> {
    parse_yaml_with_limit(bytes, MAX_DOCUMENT_BYTES)
}

pub fn parse_yaml_with_limit(bytes: &[u8], max_bytes: usize) -> Result<Value, String> {
    if bytes.len() > max_bytes {
        return Err("document exceeds the configured size limit".into());
    }
    Seed::new(max_bytes)
        .deserialize(yaml_serde::Deserializer::from_slice(bytes))
        .map_err(|error| safe_error("YAML", error))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duplicate_keys_are_rejected_without_disclosing_values() {
        for error in [
            parse_json(br#"{"secret":"first","secret":"second"}"#).unwrap_err(),
            parse_yaml(b"secret: first\nsecret: second\n").unwrap_err(),
        ] {
            assert!(error.contains("duplicate"));
            assert!(!error.contains("first") && !error.contains("second"));
        }
    }

    #[test]
    fn nested_and_multiple_documents_are_bounded() {
        let nested = format!("{}0{}", "[".repeat(70), "]".repeat(70));
        assert!(parse_json(nested.as_bytes()).is_err());
        assert!(parse_yaml(nested.as_bytes()).is_err());
        assert!(parse_yaml(b"---\na: 1\n---\na: 2\n").is_err());
        assert!(parse_json(b"{} {}").is_err());
    }

    #[test]
    fn small_yaml_aliases_preserve_string_and_scalar_key_semantics() {
        let yaml = b"name: &name value\ncopies: [*name, *name]\n42: number\ntrue: boolean\n";
        let expected = serde_json::json!({
            "name": "value",
            "copies": ["value", "value"],
            "42": "number",
            "true": "boolean",
        });
        assert_eq!(parse_yaml_with_limit(yaml, yaml.len()).unwrap(), expected);
    }

    #[test]
    fn yaml_alias_string_bytes_share_an_inclusive_configurable_budget() {
        let text = "é".repeat(32);
        let yaml = format!("base: &text {text}\ncopies: [*text, *text, *text, *text]\n");
        let decoded_bytes = "base".len() + "copies".len() + text.len() * 5;
        assert!(yaml.len() < decoded_bytes - 1);
        let value = parse_yaml_with_limit(yaml.as_bytes(), decoded_bytes).unwrap();
        assert_eq!(value["base"], text);
        assert_eq!(value["copies"], serde_json::json!([text, text, text, text]));

        for budget in [yaml.len(), decoded_bytes - 1] {
            assert_eq!(
                parse_yaml_with_limit(yaml.as_bytes(), budget).unwrap_err(),
                "invalid YAML: decoded string and key byte limit exceeded"
            );
        }
    }

    #[test]
    fn yaml_aliased_keys_share_the_value_string_budget() {
        let key = "private-key-sentinel".repeat(4);
        let yaml = format!(
            "base: &key {key}\nkeys:\n{}",
            "  - ? *key\n    : 0\n".repeat(4)
        );
        let decoded_bytes = "base".len() + "keys".len() + key.len() * 5;
        assert!(yaml.len() < decoded_bytes - 1);
        let value = parse_yaml_with_limit(yaml.as_bytes(), decoded_bytes).unwrap();
        assert_eq!(value["base"], key);
        for item in value["keys"].as_array().unwrap() {
            assert_eq!(item[&key], 0);
        }
        assert_eq!(
            parse_yaml_with_limit(yaml.as_bytes(), decoded_bytes - 1).unwrap_err(),
            "invalid YAML: decoded string and key byte limit exceeded"
        );
    }

    #[test]
    fn json_escaped_keys_and_values_still_fit_the_encoded_byte_budget() {
        let json = br#"{"\u00e9":"\u00e9","plain":["one","two",null,3,true]}"#;
        assert_eq!(
            parse_json_with_limit(json, json.len()).unwrap(),
            serde_json::json!({"é":"é","plain":["one","two",null,3,true]})
        );
        assert!(parse_json_with_limit(json, json.len() - 1).is_err());
    }
}
