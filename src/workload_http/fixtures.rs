//! Private actor fixture values and response expectations prepared before traffic.

use std::{
    borrow::Cow,
    collections::{BTreeMap, HashSet},
    sync::Arc,
};

use serde::Deserialize;
use serde_json::Value;

use crate::{strict_json::parse_json_with_limit, workload::safe_id};

use super::validate_env_name;

const MAX_FIXTURES: usize = 32;
const MAX_FIXTURE_VALUE_BYTES: usize = 8 * 1024;
const MAX_ACTOR_FIXTURE_BYTES: usize = 64 * 1024;
const MAX_ASSERTIONS: usize = 32;
const MAX_POINTER_BYTES: usize = 1024;
const MAX_POINTER_SEGMENTS: usize = 32;
const MAX_SET_ITEMS: usize = 256;

#[derive(Clone, Deserialize)]
#[serde(try_from = "BTreeMap<String, String>")]
pub(super) struct FixtureDecl {
    env: String,
    kind: FixtureKind,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FixtureKind {
    String,
    Integer,
    Number,
    Boolean,
    Array,
    Object,
    Null,
}

impl TryFrom<BTreeMap<String, String>> for FixtureDecl {
    type Error = &'static str;

    fn try_from(mut fields: BTreeMap<String, String>) -> Result<Self, Self::Error> {
        if fields.len() != 2 {
            return Err("fixture_declaration_invalid");
        }
        let env = fields.remove("env").ok_or("fixture_declaration_invalid")?;
        let kind = match fields.get("type").map(String::as_str) {
            Some("string") => FixtureKind::String,
            Some("integer") => FixtureKind::Integer,
            Some("number") => FixtureKind::Number,
            Some("boolean") => FixtureKind::Boolean,
            Some("array") => FixtureKind::Array,
            Some("object") => FixtureKind::Object,
            Some("null") => FixtureKind::Null,
            _ => return Err("fixture_type_invalid"),
        };
        Ok(Self { env, kind })
    }
}

impl FixtureKind {
    fn accepts(self, value: &Value) -> bool {
        match self {
            Self::String => value.is_string(),
            Self::Integer => value.is_i64() || value.is_u64(),
            Self::Number => value.is_number(),
            Self::Boolean => value.is_boolean(),
            Self::Array => value.is_array(),
            Self::Object => value.is_object(),
            Self::Null => value.is_null(),
        }
    }

    fn is_request_scalar(self) -> bool {
        matches!(
            self,
            Self::String | Self::Integer | Self::Number | Self::Boolean
        )
    }
}

#[derive(Clone, Deserialize)]
#[serde(try_from = "BTreeMap<String, String>")]
pub(super) struct FixtureRef {
    fixture: String,
}

impl TryFrom<BTreeMap<String, String>> for FixtureRef {
    type Error = &'static str;

    fn try_from(mut fields: BTreeMap<String, String>) -> Result<Self, Self::Error> {
        if fields.len() != 1 {
            return Err("fixture_reference_invalid");
        }
        let fixture = fields
            .remove("fixture")
            .ok_or("fixture_reference_invalid")?;
        if !safe_id(&fixture) {
            return Err("fixture_reference_invalid");
        }
        Ok(Self { fixture })
    }
}

#[derive(Clone, Deserialize)]
#[serde(try_from = "BTreeMap<String, Value>")]
pub(super) struct FieldAssertion {
    pointer: String,
    reference: FixtureRef,
    compare_as_set: bool,
}

impl TryFrom<BTreeMap<String, Value>> for FieldAssertion {
    type Error = &'static str;

    fn try_from(mut fields: BTreeMap<String, Value>) -> Result<Self, Self::Error> {
        if fields.len() != 2 {
            return Err("fixture_assertion_invalid");
        }
        let Some(Value::String(pointer)) = fields.remove("pointer") else {
            return Err("fixture_assertion_invalid");
        };
        let (value, compare_as_set) = match fields.remove("equals") {
            Some(value) => (value, false),
            None => (
                fields
                    .remove("equals_set")
                    .ok_or("fixture_assertion_invalid")?,
                true,
            ),
        };
        if !valid_pointer(&pointer) {
            return Err("fixture_pointer_invalid");
        }
        let reference = serde_json::from_value(value).map_err(|_| "fixture_reference_invalid")?;
        Ok(Self {
            pointer,
            reference,
            compare_as_set,
        })
    }
}

fn valid_pointer(pointer: &str) -> bool {
    if pointer.is_empty() {
        return true;
    }
    if !pointer.starts_with('/') || pointer.len() > MAX_POINTER_BYTES {
        return false;
    }
    let mut segments = 0;
    let mut bytes = pointer.bytes();
    while let Some(byte) = bytes.next() {
        if byte == b'/' {
            segments += 1;
            if segments > MAX_POINTER_SEGMENTS {
                return false;
            }
        } else if byte == b'~' && !matches!(bytes.next(), Some(b'0' | b'1')) {
            return false;
        }
    }
    true
}

#[derive(Default)]
pub(super) struct BoundFixtures(BTreeMap<String, BoundFixture>);

struct BoundFixture {
    kind: FixtureKind,
    value: Option<Arc<Value>>,
}

impl BoundFixtures {
    pub(super) fn load(
        declarations: &BTreeMap<String, FixtureDecl>,
        execute: bool,
    ) -> Result<Self, String> {
        Self::load_with(declarations, execute, |variable| {
            std::env::var(variable).ok()
        })
    }

    fn load_with(
        declarations: &BTreeMap<String, FixtureDecl>,
        execute: bool,
        mut resolve: impl FnMut(&str) -> Option<String>,
    ) -> Result<Self, String> {
        if declarations.len() > MAX_FIXTURES {
            return Err("too_many_actor_fixtures".into());
        }
        let mut remaining = MAX_ACTOR_FIXTURE_BYTES;
        let mut fixtures = BTreeMap::new();
        for (name, declaration) in declarations {
            if !safe_id(name) {
                return Err("fixture_name_invalid".into());
            }
            validate_env_name(&declaration.env)?;
            let value = if execute {
                let input =
                    resolve(&declaration.env).ok_or("fixture_environment_missing_or_invalid")?;
                if input.len() > MAX_FIXTURE_VALUE_BYTES {
                    return Err("fixture_value_exceeds_limit".into());
                }
                remaining = remaining
                    .checked_sub(input.len())
                    .ok_or("actor_fixture_bytes_exceed_limit")?;
                let value = parse_json_with_limit(input.as_bytes(), MAX_FIXTURE_VALUE_BYTES)
                    .map_err(|_| "fixture_json_invalid")?;
                if !declaration.kind.accepts(&value) {
                    return Err("fixture_value_type_mismatch".into());
                }
                Some(Arc::new(value))
            } else {
                None
            };
            fixtures.insert(
                name.clone(),
                BoundFixture {
                    kind: declaration.kind,
                    value,
                },
            );
        }
        Ok(Self(fixtures))
    }

    fn get(&self, reference: &FixtureRef) -> Result<&BoundFixture, String> {
        self.0
            .get(&reference.fixture)
            .ok_or_else(|| "fixture_reference_unknown".into())
    }

    pub(super) fn scalar(&self, reference: &FixtureRef) -> Result<Option<Cow<'_, str>>, String> {
        let fixture = self.get(reference)?;
        if !fixture.kind.is_request_scalar() {
            return Err("fixture_request_scalar_required".into());
        }
        Ok(match fixture.value.as_deref() {
            None => None,
            Some(Value::String(value)) => Some(Cow::Borrowed(value)),
            Some(value) => Some(Cow::Owned(value.to_string())),
        })
    }

    pub(super) fn assertions(
        &self,
        config: &[FieldAssertion],
    ) -> Result<Vec<PreparedAssertion>, String> {
        if config.len() > MAX_ASSERTIONS {
            return Err("too_many_fixture_assertions".into());
        }
        config
            .iter()
            .map(|assertion| {
                let fixture = self.get(&assertion.reference)?;
                if assertion.compare_as_set {
                    if fixture.kind != FixtureKind::Array {
                        return Err("fixture_assertion_set_requires_array".into());
                    }
                    if let Some(value) = &fixture.value
                        && scalar_set(value.as_array().expect("validated array fixture")).is_none()
                    {
                        return Err("fixture_assertion_set_invalid".into());
                    }
                }
                Ok(PreparedAssertion {
                    pointer: assertion.pointer.clone(),
                    expected: fixture.value.clone(),
                    compare_as_set: assertion.compare_as_set,
                })
            })
            .collect()
    }
}

pub(super) struct PreparedAssertion {
    pointer: String,
    expected: Option<Arc<Value>>,
    compare_as_set: bool,
}

impl PreparedAssertion {
    pub(super) fn matches(&self, body: &Value) -> bool {
        let (Some(expected), Some(actual)) =
            (self.expected.as_deref(), body.pointer(&self.pointer))
        else {
            return false;
        };
        if self.compare_as_set {
            let expected = expected.as_array().expect("validated array fixture");
            let Some(actual) = actual
                .as_array()
                .filter(|items| items.len() == expected.len())
            else {
                return false;
            };
            let Some(actual) = scalar_set(actual) else {
                return false;
            };
            expected.iter().all(|item| actual.contains(item))
        } else {
            actual == expected
        }
    }
}

fn scalar_set(items: &[Value]) -> Option<HashSet<&Value>> {
    if items.len() > MAX_SET_ITEMS {
        return None;
    }
    let mut remaining = MAX_FIXTURE_VALUE_BYTES;
    let mut set = HashSet::with_capacity(items.len());
    for value in items {
        match value {
            Value::Array(_) | Value::Object(_) => return None,
            Value::String(text) => remaining = remaining.checked_sub(text.len())?,
            _ => {}
        }
        if !set.insert(value) {
            return None;
        }
    }
    Some(set)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn load_fixture(kind: &str, value: &str) -> Result<BoundFixtures, String> {
        let declaration = serde_json::from_value(json!({
            "env": "RUSTA_FIXTURE_UNIT_SYNTHETIC",
            "type": kind,
        }))
        .unwrap();
        let declarations = BTreeMap::from([("value".into(), declaration)]);
        BoundFixtures::load_with(&declarations, true, |_| Some(value.into()))
    }

    fn fixture_ref() -> FixtureRef {
        serde_json::from_value(json!({"fixture": "value"})).unwrap()
    }

    fn assertion(config: Value) -> FieldAssertion {
        serde_json::from_value(config).unwrap()
    }

    #[test]
    fn preview_validates_names_types_and_references_without_resolving_values() {
        let declarations = serde_json::from_value(json!({
            "value": {"env": "RUSTA_FIXTURE_UNIT_SYNTHETIC", "type": "string"},
            "array": {"env": "RUSTA_FIXTURE_UNIT_SYNTHETIC", "type": "array"},
        }))
        .unwrap();
        let fixtures =
            BoundFixtures::load_with(&declarations, false, |_| panic!("preview read env")).unwrap();
        assert!(fixtures.scalar(&fixture_ref()).unwrap().is_none());
        let array: FixtureRef = serde_json::from_value(json!({"fixture": "array"})).unwrap();
        assert!(fixtures.scalar(&array).is_err());
        let unknown = FixtureRef {
            fixture: "unknown".into(),
        };
        assert!(fixtures.scalar(&unknown).is_err());
        let set = assertion(json!({"pointer": "", "equals_set": {"fixture": "value"}}));
        assert!(fixtures.assertions(&[set]).is_err());
    }

    #[test]
    fn plans_require_strict_objects_and_json_pointer_syntax() {
        for invalid in [
            json!(["/id", {"fixture": "value"}]),
            json!({"pointer": "/id", "equals": null}),
            json!({"pointer": "/id", "equals": {"fixture": "value"}, "equals_set": null}),
            json!({"pointer": "/id", "other": {"fixture": "value"}}),
            json!({"pointer": "/id", "equals": ["value"]}),
            json!({"pointer": "/id", "equals": {"fixture": "bad name"}}),
            json!({"pointer": "id", "equals": {"fixture": "value"}}),
            json!({"pointer": "/a~2b", "equals": {"fixture": "value"}}),
            json!({"pointer": "/a~", "equals": {"fixture": "value"}}),
            json!({"pointer": "/a".repeat(MAX_POINTER_SEGMENTS + 1), "equals": {"fixture": "value"}}),
            json!({"pointer": format!("/{}", "a".repeat(MAX_POINTER_BYTES)), "equals": {"fixture": "value"}}),
        ] {
            assert!(serde_json::from_value::<FieldAssertion>(invalid).is_err());
        }
        for invalid in [
            json!(["RUSTA_FIXTURE_UNIT_SYNTHETIC", "string"]),
            json!({"env": "RUSTA_FIXTURE_UNIT_SYNTHETIC", "type": "unsupported"}),
            json!({"env": "RUSTA_FIXTURE_UNIT_SYNTHETIC", "type": "string", "other": true}),
        ] {
            assert!(serde_json::from_value::<FixtureDecl>(invalid).is_err());
        }
    }

    #[test]
    fn validates_typed_json_and_renders_only_wire_scalars() {
        for (kind, input, scalar) in [
            ("string", r#""a/b?x=✓""#, "a/b?x=✓"),
            ("integer", "18446744073709551615", "18446744073709551615"),
            ("integer", "-22", "-22"),
            ("number", "0.25", "0.25"),
            ("boolean", "false", "false"),
        ] {
            let fixtures = load_fixture(kind, input).unwrap();
            assert_eq!(
                fixtures.scalar(&fixture_ref()).unwrap().as_deref(),
                Some(scalar)
            );
        }
        for (kind, input) in [("null", "null"), ("object", "{}"), ("array", "[]")] {
            let fixtures = load_fixture(kind, input).unwrap();
            assert!(fixtures.scalar(&fixture_ref()).is_err());
        }
        assert!(load_fixture("integer", "1.0").is_err());
        assert!(load_fixture("string", "true").is_err());
        assert!(load_fixture("number", r#""12""#).is_err());
        let malformed = load_fixture(
            "object",
            r#"{"private":"synthetic-a","private":"synthetic-b"}"#,
        );
        assert_eq!(malformed.err().unwrap(), "fixture_json_invalid");
    }

    #[test]
    fn private_values_and_assertion_resources_are_bounded() {
        let literal = format!(r#""{}""#, "a".repeat(MAX_FIXTURE_VALUE_BYTES - 2));
        assert!(load_fixture("string", &literal).is_ok());
        assert_eq!(
            load_fixture("string", &(literal.clone() + " "))
                .err()
                .unwrap(),
            "fixture_value_exceeds_limit",
        );
        let declarations = (0..(MAX_ACTOR_FIXTURE_BYTES / MAX_FIXTURE_VALUE_BYTES + 1))
            .map(|i| {
                (
                    format!("fixture-{i}"),
                    FixtureDecl {
                        env: "RUSTA_FIXTURE_UNIT_SYNTHETIC".into(),
                        kind: FixtureKind::String,
                    },
                )
            })
            .collect();
        assert_eq!(
            BoundFixtures::load_with(&declarations, true, |_| Some(literal.clone()))
                .err()
                .unwrap(),
            "actor_fixture_bytes_exceed_limit",
        );
        let assert = assertion(json!({"pointer": "", "equals": {"fixture": "value"}}));
        let fixtures = load_fixture("array", "[]").unwrap();
        assert!(
            fixtures
                .assertions(&vec![assert; MAX_ASSERTIONS + 1])
                .is_err()
        );
    }

    #[test]
    fn matching_distinguishes_missing_fields_explicit_null_and_types() {
        let fixture = load_fixture("null", "null").unwrap();
        let config = assertion(json!({
            "pointer": "/a~1b~0/0",
            "equals": {"fixture": "value"},
        }));
        let prepared = fixture.assertions(&[config]).unwrap();
        assert!(prepared[0].matches(&json!({"a/b~": [null]})));
        assert!(!prepared[0].matches(&json!({"a/b~": []})));
        assert!(!prepared[0].matches(&json!({"a/b~": ["null"]})));
    }

    #[test]
    fn set_matching_is_order_independent_typed_and_duplicate_free() {
        let config = assertion(json!({"pointer": "/tenants", "equals_set": {"fixture": "value"}}));
        let fixtures = load_fixture("array", r#"["tenant-a", 13, null]"#).unwrap();
        let prepared = fixtures.assertions(std::slice::from_ref(&config)).unwrap();
        assert!(prepared[0].matches(&json!({"tenants": [null, 13, "tenant-a"]})));
        for actual in [
            json!({"tenants": ["tenant-b", 13, null]}),
            json!({"tenants": ["tenant-a", 13, 13]}),
            json!({"tenants": ["tenant-a", "13", null]}),
            json!({"tenants": ["tenant-a", 13.0, null]}),
            json!({"tenants": ["tenant-a", 13, {}]}),
            json!({"tenants": ["tenant-a", 13]}),
            json!({"tenants": vec![true; MAX_SET_ITEMS + 1]}),
            json!({"tenants": [json!("x".repeat(MAX_FIXTURE_VALUE_BYTES + 1)), json!(13), Value::Null]}),
        ] {
            assert!(!prepared[0].matches(&actual));
        }
        for expected in ["[false,false]", "[{}]", "[[]]"] {
            let fixtures = load_fixture("array", expected).unwrap();
            assert!(fixtures.assertions(std::slice::from_ref(&config)).is_err());
        }
        let oversized = serde_json::to_string(&(0..=MAX_SET_ITEMS).collect::<Vec<_>>()).unwrap();
        let fixtures = load_fixture("array", &oversized).unwrap();
        assert!(fixtures.assertions(&[config]).is_err());
    }
}
