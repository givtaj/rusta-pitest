//! Bounded JSON/YAML parsing shared by contract ingestion and suite loading.

use std::{cell::Cell, fmt, rc::Rc};

use serde::de::{self, DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Number, Value};

pub const MAX_DOCUMENT_BYTES: usize = 1024 * 1024;
pub const MAX_CONFIGURED_BYTES: u64 = 64 * 1024 * 1024;
const MAX_DEPTH: usize = 64;
const MAX_NODES: usize = 100_000;

struct StrictValue(Value);

impl<'de> serde::Deserialize<'de> for StrictValue {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Seed::new().deserialize(deserializer).map(Self)
    }
}

struct Seed {
    depth: usize,
    nodes: Rc<Cell<usize>>,
}

impl Seed {
    fn new() -> Self {
        Self {
            depth: 0,
            nodes: Rc::new(Cell::new(0)),
        }
    }

    fn child(&self) -> Self {
        Self {
            depth: self.depth + 1,
            nodes: Rc::clone(&self.nodes),
        }
    }
}

impl<'de> DeserializeSeed<'de> for Seed {
    type Value = Value;

    fn deserialize<D: serde::Deserializer<'de>>(self, deserializer: D) -> Result<Value, D::Error> {
        if self.depth > MAX_DEPTH {
            return Err(de::Error::custom("document nesting limit exceeded"));
        }
        let nodes = self.nodes.get() + 1;
        if nodes > MAX_NODES {
            return Err(de::Error::custom("document node limit exceeded"));
        }
        self.nodes.set(nodes);
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
        Ok(Value::String(value.to_owned()))
    }

    fn visit_string<E: de::Error>(self, value: String) -> Result<Value, E> {
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
        while let Some(key) = mapping.next_key::<String>()? {
            if values.contains_key(&key) {
                return Err(de::Error::custom("duplicate key"));
            }
            let value = mapping.next_value_seed(self.child())?;
            values.insert(key, value);
        }
        Ok(Value::Object(values))
    }
}

fn safe_error(format: &str, error: impl fmt::Display) -> String {
    let error = error.to_string();
    if error.contains("duplicate key") {
        format!("invalid {format}: duplicate object key")
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
    let value = Seed::new()
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
    yaml_serde::from_slice::<StrictValue>(bytes)
        .map(|value| value.0)
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
        assert!(parse_yaml(b"---\na: 1\n---\na: 2\n").is_err());
        assert!(parse_json(b"{} {}").is_err());
    }
}
