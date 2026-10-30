//! Compile the supported OpenAPI 3.1 schema subset without any external I/O.
//! References are kept as a graph under $defs, including productive recursion.
use std::collections::{BTreeMap, BTreeSet};

use jsonschema::{Draft, PatternOptions, Retrieve, Uri, Validator};
use serde_json::{Map, Value, json};

const MAX_DEPTH: usize = 64;
const MAX_RESOURCES: usize = 256;
const MAX_SCHEMA_NODES: usize = 10_000;

struct NoRetrieval;

impl Retrieve for NoRetrieval {
    fn retrieve(&self, _: &Uri<String>) -> Result<Value, Box<dyn std::error::Error + Send + Sync>> {
        Err("external schema retrieval is disabled".into())
    }
}

pub fn compile(contract: &Value, schema: &Value) -> Result<Validator, String> {
    if let Some(dialect) = contract.get("jsonSchemaDialect")
        && !supported_dialect(dialect)
    {
        return Err("unsupported schema dialect".into());
    }
    let mut graph = Graph {
        contract,
        names: BTreeMap::new(),
        active: BTreeMap::new(),
        definitions: Map::new(),
        nodes: 0,
    };
    let normalized = graph.schema(schema, 0, 0)?;
    ExpandedSchemaBudget {
        definitions: &graph.definitions,
        active: BTreeMap::new(),
        nodes: 0,
    }
    .check(&normalized, 0, 0)?;
    let document = json!({"$defs": graph.definitions, "allOf": [normalized]});
    jsonschema::options()
        .with_draft(Draft::Draft202012)
        .with_retriever(NoRetrieval)
        // Formats are annotations. The report/documentation must not claim
        // validation of dates, UUIDs, email addresses, or custom formats.
        .should_validate_formats(false)
        .with_pattern_options(PatternOptions::regex())
        .build(&document)
        .map_err(|_| "invalid or unsupported JSON Schema".into())
}

fn supported_dialect(value: &Value) -> bool {
    matches!(
        value.as_str(),
        Some("https://json-schema.org/draft/2020-12/schema")
            | Some("https://spec.openapis.org/oas/3.1/dialect/base")
    )
}

struct Graph<'a> {
    contract: &'a Value,
    names: BTreeMap<String, String>,
    active: BTreeMap<String, usize>,
    definitions: Map<String, Value>,
    nodes: usize,
}

impl Graph<'_> {
    fn schema(
        &mut self,
        schema: &Value,
        depth: usize,
        instance_depth: usize,
    ) -> Result<Value, String> {
        self.nodes += 1;
        if depth > MAX_DEPTH || self.nodes > MAX_SCHEMA_NODES {
            return Err("schema exceeds supported complexity limits".into());
        }
        if schema.is_boolean() {
            return Ok(schema.clone());
        }
        let object = schema
            .as_object()
            .ok_or("expected a schema object or boolean")?;
        if object.contains_key("$id")
            || object.contains_key("$anchor")
            || object.contains_key("$dynamicAnchor")
            || object.contains_key("$dynamicRef")
            || object.contains_key("$vocabulary")
        {
            return Err(
                "schema identifiers, anchors and dynamic references are not supported".into(),
            );
        }
        if let Some(dialect) = object.get("$schema")
            && !supported_dialect(dialect)
        {
            return Err("unsupported schema dialect".into());
        }
        let mut normalized = object.clone();
        // We explicitly select Draft 2020-12 plus OAS annotations above.
        normalized.remove("$schema");
        if let Some(reference) = object.get("$ref") {
            let reference = reference.as_str().ok_or("invalid schema reference")?;
            let (pointer, target) = reference_target(self.contract, reference)?;
            if let Some(active_depth) = self.active.get(&pointer)
                && instance_depth <= *active_depth
            {
                return Err(
                    "recursive schema must descend into an instance property or item".into(),
                );
            }
            let name = if let Some(name) = self.names.get(&pointer) {
                name.clone()
            } else {
                if self.names.len() >= MAX_RESOURCES {
                    return Err("too many referenced schemas".into());
                }
                let name = format!("r{}", self.names.len());
                self.names.insert(pointer.clone(), name.clone());
                self.active.insert(pointer.clone(), instance_depth);
                let target = target.clone();
                let target = self.schema(&target, depth + 1, instance_depth)?;
                self.active.remove(&pointer);
                self.definitions.insert(name.clone(), target);
                name
            };
            normalized.insert("$ref".into(), Value::String(format!("#/$defs/{name}")));
        }
        for key in [
            "properties",
            "patternProperties",
            "$defs",
            "definitions",
            "dependentSchemas",
            "dependencies",
        ] {
            if let Some(value) = object.get(key) {
                let values = value.as_object().ok_or("invalid schema map")?;
                let mut result = Map::new();
                for (name, child) in values {
                    // The validator also honors this legacy keyword in the
                    // selected draft. Arrays are property-name lists; other
                    // values are schemas applied to the same instance.
                    if key == "dependencies" && child.is_array() {
                        result.insert(name.clone(), child.clone());
                        continue;
                    }
                    let descending = matches!(key, "properties" | "patternProperties");
                    result.insert(
                        name.clone(),
                        self.schema(child, depth + 1, instance_depth + usize::from(descending))?,
                    );
                }
                normalized.insert(key.into(), Value::Object(result));
            }
        }
        for key in ["allOf", "anyOf", "oneOf", "prefixItems"] {
            if let Some(value) = object.get(key) {
                let values = value.as_array().ok_or("invalid schema array")?;
                let result = values
                    .iter()
                    .map(|child| {
                        self.schema(
                            child,
                            depth + 1,
                            instance_depth + usize::from(key == "prefixItems"),
                        )
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                normalized.insert(key.into(), Value::Array(result));
            }
        }
        for key in [
            "additionalProperties",
            "unevaluatedProperties",
            "propertyNames",
            "contains",
            "items",
            "unevaluatedItems",
            "not",
            "if",
            "then",
            "else",
            "contentSchema",
        ] {
            if let Some(child) = object.get(key) {
                let descending = matches!(
                    key,
                    "additionalProperties"
                        | "unevaluatedProperties"
                        | "propertyNames"
                        | "contains"
                        | "items"
                        | "unevaluatedItems"
                );
                normalized.insert(
                    key.into(),
                    self.schema(child, depth + 1, instance_depth + usize::from(descending))?,
                );
            }
        }
        // The original keyword values and $ref siblings are otherwise retained.
        Ok(Value::Object(normalized))
    }
}

/// Charge repeated references for the schema work they can repeat at evaluation.
/// Normalization shares reference targets, so its node count alone does not bound
/// their expansion. Recheck instance descent on every expanded recursive path:
/// normalization may have cached a target through a different, productive path.
/// Productive active reference edges stop expansion, but their siblings remain
/// checked. This is an admission limit, not an instance time or memory budget.
struct ExpandedSchemaBudget<'a> {
    definitions: &'a Map<String, Value>,
    active: BTreeMap<&'a str, usize>,
    nodes: usize,
}

impl<'a> ExpandedSchemaBudget<'a> {
    fn check(
        &mut self,
        schema: &'a Value,
        depth: usize,
        instance_depth: usize,
    ) -> Result<(), String> {
        self.nodes += 1;
        if depth > MAX_DEPTH || self.nodes > MAX_SCHEMA_NODES {
            return Err("schema exceeds supported expanded complexity limits".into());
        }
        if schema.is_boolean() {
            return Ok(());
        }
        let object = schema
            .as_object()
            .ok_or("expected a schema object or boolean")?;
        if let Some(reference) = object.get("$ref") {
            // Every schema reference was normalized to this private namespace.
            let reference = reference.as_str().ok_or("invalid schema reference")?;
            let name = reference
                .strip_prefix("#/$defs/")
                .ok_or("invalid normalized schema reference")?;
            let target = self
                .definitions
                .get(name)
                .ok_or("unresolved normalized schema reference")?;
            if let Some(active_depth) = self.active.get(reference) {
                if instance_depth <= *active_depth {
                    return Err(
                        "recursive schema must descend into an instance property or item".into(),
                    );
                }
            } else {
                self.active.insert(reference, instance_depth);
                let result = self.check(target, depth + 1, instance_depth);
                self.active.remove(reference);
                result?;
            }
            // A recursive edge skips only its target, never its siblings below.
        }
        // Declarations in $defs/definitions are reached through references only.
        // Literal values (const, enum, default, examples) are not schema children.
        for key in [
            "properties",
            "patternProperties",
            "dependentSchemas",
            "dependencies",
        ] {
            if let Some(value) = object.get(key) {
                for child in value.as_object().ok_or("invalid schema map")?.values() {
                    if key == "dependencies" && child.is_array() {
                        continue;
                    }
                    let descending = matches!(key, "properties" | "patternProperties");
                    self.check(child, depth + 1, instance_depth + usize::from(descending))?;
                }
            }
        }
        for key in ["allOf", "anyOf", "oneOf", "prefixItems"] {
            if let Some(value) = object.get(key) {
                for child in value.as_array().ok_or("invalid schema array")? {
                    self.check(
                        child,
                        depth + 1,
                        instance_depth + usize::from(key == "prefixItems"),
                    )?;
                }
            }
        }
        for key in [
            "additionalProperties",
            "unevaluatedProperties",
            "propertyNames",
            "contains",
            "items",
            "unevaluatedItems",
            "not",
            "if",
            "then",
            "else",
            "contentSchema",
        ] {
            if let Some(child) = object.get(key) {
                let descending = matches!(
                    key,
                    "additionalProperties"
                        | "unevaluatedProperties"
                        | "propertyNames"
                        | "contains"
                        | "items"
                        | "unevaluatedItems"
                );
                self.check(child, depth + 1, instance_depth + usize::from(descending))?;
            }
        }
        Ok(())
    }
}

/// Resolve an OAS Reference Object, rather than a JSON Schema $ref. These have
/// different sibling semantics: only summary and description override a target.
pub fn resolve(contract: &Value, value: &Value) -> Result<Value, String> {
    resolve_with_policy(contract, value, false)
}

/// Path Item references have distinct sibling rules. The supported subset
/// rejects mixed definitions at every hop, including referenced Path Items.
pub fn resolve_path_item(contract: &Value, value: &Value) -> Result<Value, String> {
    resolve_with_policy(contract, value, true)
}

fn resolve_with_policy(contract: &Value, value: &Value, path_item: bool) -> Result<Value, String> {
    let mut result = value.clone();
    let mut visited = BTreeSet::new();
    let mut summary = None;
    let mut description = None;
    for _ in 0..MAX_DEPTH {
        if path_item
            && let Some(object) = result.as_object()
            && object.contains_key("$ref")
            && object.keys().any(|key| {
                !matches!(key.as_str(), "$ref" | "summary" | "description")
                    && !key.starts_with("x-")
            })
        {
            return Err(
                "path-item references with operation, parameter or server siblings are unsupported"
                    .into(),
            );
        }
        let Some(reference) = result.get("$ref") else {
            if let Some(object) = result.as_object_mut() {
                if let Some(summary) = summary {
                    object.insert("summary".into(), summary);
                }
                if let Some(description) = description {
                    object.insert("description".into(), description);
                }
            }
            return Ok(result);
        };
        let reference = reference.as_str().ok_or("invalid contract reference")?;
        let (pointer, target) = reference_target(contract, reference)?;
        if !visited.insert(pointer) {
            return Err("cyclic contract reference".into());
        }
        if summary.is_none() {
            summary = result.get("summary").cloned();
        }
        if description.is_none() {
            description = result.get("description").cloned();
        }
        result = target.clone();
    }
    Err("contract reference depth limit exceeded".into())
}

fn reference_target<'a>(
    contract: &'a Value,
    reference: &str,
) -> Result<(String, &'a Value), String> {
    let fragment = reference
        .strip_prefix('#')
        .ok_or("external references are not supported")?;
    let mut decoded = Vec::new();
    let bytes = fragment.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let high = bytes.get(index + 1).and_then(|v| (*v as char).to_digit(16));
            let low = bytes.get(index + 2).and_then(|v| (*v as char).to_digit(16));
            decoded.push(
                (high.ok_or("invalid reference escape")? * 16
                    + low.ok_or("invalid reference escape")?) as u8,
            );
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    let pointer = String::from_utf8(decoded).map_err(|_| "invalid reference encoding")?;
    if !pointer.is_empty() && !pointer.starts_with('/') {
        return Err("schema anchors are not supported".into());
    }
    let mut chars = pointer.chars();
    while let Some(ch) = chars.next() {
        if ch == '~' && !matches!(chars.next(), Some('0' | '1')) {
            return Err("invalid JSON Pointer escape".into());
        }
    }
    let target = contract
        .pointer(&pointer)
        .ok_or("unresolved internal reference")?;
    Ok((pointer, target))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shared_reference_chain(levels: usize) -> (Value, Value) {
        let mut schemas = Map::new();
        schemas.insert("L0".into(), json!({"type":"string", "minLength":1}));
        for level in 1..=levels {
            let reference = format!("#/components/schemas/L{}", level - 1);
            schemas.insert(
                format!("L{level}"),
                json!({"allOf":[{"$ref":reference}, {"$ref":reference}]}),
            );
        }
        (
            json!({"components":{"schemas":schemas}}),
            json!({"$ref":format!("#/components/schemas/L{levels}")}),
        )
    }

    #[test]
    fn repeated_reference_expansion_is_bounded_before_validation() {
        let (small_contract, small_schema) = shared_reference_chain(4);
        let validator = compile(&small_contract, &small_schema).unwrap();
        assert!(validator.is_valid(&json!("ok")));
        assert!(!validator.is_valid(&json!("")));
        assert!(!validator.is_valid(&json!(42)));

        // Only 13 distinct targets, but their repeated scalar checks would
        // exceed the expanded-node limit. Rejection needs no costly evaluation.
        let (large_contract, large_schema) = shared_reference_chain(12);
        assert_eq!(
            compile(&large_contract, &large_schema).err().as_deref(),
            Some("schema exceeds supported expanded complexity limits")
        );
    }

    #[test]
    fn previously_normalized_targets_still_obey_expanded_depth_limit() {
        let mut schemas = Map::new();
        schemas.insert("L0".into(), json!({"type":"string"}));
        let mut roots = vec![json!({"$ref":"#/components/schemas/L0"})];
        for level in 1..=70 {
            schemas.insert(
                format!("L{level}"),
                json!({"$ref":format!("#/components/schemas/L{}", level - 1)}),
            );
            roots.push(json!({"$ref":format!("#/components/schemas/L{level}")}));
        }
        let contract = json!({"components":{"schemas":schemas}});
        // Short targets appear first, so normalization can reuse them without
        // traversing the full chain. The expansion check must walk it again.
        assert_eq!(
            compile(&contract, &json!({"allOf":roots})).err().as_deref(),
            Some("schema exceeds supported expanded complexity limits")
        );
    }

    #[test]
    fn productive_mutual_recursion_preserves_reference_siblings() {
        let contract = json!({"components":{"schemas":{
            "A":{"type":"object", "properties":{"next":{"$ref":"#/components/schemas/B"}}},
            "B":{"type":"object", "properties":{"next":{
                "$ref":"#/components/schemas/A", "required":["value"]
            }}}
        }}});
        let validator = compile(&contract, &json!({"$ref":"#/components/schemas/A"})).unwrap();
        assert!(validator.is_valid(&json!({"next":{"next":{"value":1}}})));
        assert!(!validator.is_valid(&json!({"next":{"next":{}}})));
        assert!(!validator.is_valid(&json!({"next":{"next":4}})));
    }

    #[test]
    fn cached_references_must_descend_on_every_recursive_path() {
        let contract = json!({"components":{"schemas":{
            "A":{"type":"object", "properties":{"next":{"$ref":"#/components/schemas/B"}}},
            "B":{"$ref":"#/components/schemas/A"}
        }}});
        let root = json!({"$ref":"#/components/schemas/A"});
        let validator = compile(&contract, &root).unwrap();
        assert!(validator.is_valid(&json!({"next":{"next":{}}})));
        assert!(!validator.is_valid(&json!({"next":4})));

        // Normalization first caches B through properties.next. Each added
        // same-instance path must still reject A -> B -> A when B is reused.
        let reference = json!({"$ref":"#/components/schemas/B"});
        for (keyword, child) in [
            ("allOf", json!([reference])),
            ("dependentSchemas", json!({"next":reference})),
            ("dependencies", json!({"next":reference})),
            ("not", reference),
        ] {
            let mut cyclic = contract.clone();
            cyclic["components"]["schemas"]["A"][keyword] = child;
            assert_eq!(
                compile(&cyclic, &root).err().as_deref(),
                Some("recursive schema must descend into an instance property or item"),
                "cached path through {keyword}"
            );
        }
    }

    #[test]
    fn active_recursive_reference_does_not_skip_expensive_siblings() {
        let (mut contract, shared_schema) = shared_reference_chain(12);
        contract["components"]["schemas"]["Node"] = json!({
            "type":"object",
            "properties":{"next":{
                "$ref":"#/components/schemas/Node",
                "allOf":[shared_schema]
            }}
        });
        assert_eq!(
            compile(&contract, &json!({"$ref":"#/components/schemas/Node"}))
                .err()
                .as_deref(),
            Some("schema exceeds supported expanded complexity limits")
        );
    }

    #[test]
    fn unused_schema_declarations_are_not_expanded() {
        let (contract, shared_schema) = shared_reference_chain(12);
        let schema = json!({"type":"string", "$defs":{"unused":shared_schema}});
        let validator = compile(&contract, &schema).unwrap();
        assert!(validator.is_valid(&json!("ok")));
        assert!(!validator.is_valid(&json!(42)));
    }

    #[test]
    fn legacy_dependencies_preserve_schema_boolean_and_property_list_semantics() {
        let contract = json!({"components":{"schemas":{
            "Age":{"properties":{"age":{"type":"integer"}}, "required":["age"]}
        }}});
        let schema = json!({"type":"object", "dependencies":{
            "name":{"$ref":"#/components/schemas/Age"},
            "enabled":false,
            "free":true,
            "left":["right"]
        }});
        let validator = compile(&contract, &schema).unwrap();
        assert!(validator.is_valid(&json!({})));
        assert!(validator.is_valid(&json!({"free":1})));
        assert!(!validator.is_valid(&json!({"enabled":true})));
        assert!(validator.is_valid(&json!({"name":"n", "age":5})));
        assert!(!validator.is_valid(&json!({"name":"n"})));
        assert!(!validator.is_valid(&json!({"name":"n", "age":"five"})));
        assert!(validator.is_valid(&json!({"left":1, "right":2})));
        assert!(!validator.is_valid(&json!({"left":1})));
    }

    #[test]
    fn legacy_dependencies_cannot_bypass_expanded_reference_limits() {
        let (mut contract, shared_schema) = shared_reference_chain(12);
        contract["components"]["schemas"]["L0"] = json!({"type":"object"});
        let schema = json!({"dependencies":{"enabled":shared_schema}});
        assert_eq!(
            compile(&contract, &schema).err().as_deref(),
            Some("schema exceeds supported expanded complexity limits")
        );
    }

    #[test]
    fn legacy_dependencies_cannot_bypass_reference_restrictions() {
        assert_eq!(
            compile(
                &json!({}),
                &json!({"dependencies":{"enabled":{"$ref":"https://example.test/schema"}}})
            )
            .err()
            .as_deref(),
            Some("external references are not supported")
        );
        let contract = json!({"components":{"schemas":{
            "Loop":{"dependencies":{"enabled":{"$ref":"#/components/schemas/Loop"}}}
        }}});
        assert_eq!(
            compile(&contract, &json!({"$ref":"#/components/schemas/Loop"}))
                .err()
                .as_deref(),
            Some("recursive schema must descend into an instance property or item")
        );
    }

    #[test]
    fn reference_siblings_and_nested_constraints_are_preserved() {
        let contract = json!({"components":{"schemas":{"Name":{"type":"string"}}}});
        let validator = compile(
            &contract,
            &json!({"$ref":"#/components/schemas/Name","minLength":3}),
        )
        .unwrap();
        assert!(validator.is_valid(&json!("abc")));
        assert!(!validator.is_valid(&json!("a")));
        assert!(!validator.is_valid(&json!(42)));
    }

    #[test]
    fn productive_recursion_is_supported_but_empty_cycles_are_blocked() {
        let contract = json!({"components":{"schemas":{"Node":{"type":"object","properties":{"next":{"$ref":"#/components/schemas/Node"}}}}}});
        let validator = compile(&contract, &json!({"$ref":"#/components/schemas/Node"})).unwrap();
        assert!(validator.is_valid(&json!({"next":{"next":{}}})));
        assert!(!validator.is_valid(&json!({"next":4})));
        let cycle = json!({"components":{"schemas":{"Loop":{"$ref":"#/components/schemas/Loop"}}}});
        assert!(compile(&cycle, &json!({"$ref":"#/components/schemas/Loop"})).is_err());
    }

    #[test]
    fn external_references_and_custom_dialects_are_blocked_without_io() {
        assert!(compile(&json!({}), &json!({"$ref":"file:///etc/passwd"})).is_err());
        assert!(
            compile(
                &json!({}),
                &json!({"$schema":"https://example.test/custom"})
            )
            .is_err()
        );
        assert!(compile(&json!({}), &json!({"$id":"https://example.test/schema"})).is_err());
    }

    #[test]
    fn references_inside_literal_values_are_not_interpreted() {
        let validator = compile(
            &json!({}),
            &json!({"const":{"$ref":"https://example.test/literal"}}),
        )
        .unwrap();
        assert!(validator.is_valid(&json!({"$ref":"https://example.test/literal"})));
    }

    #[test]
    fn json_pointer_escaping_and_uri_encoding_work() {
        let contract = json!({"components":{"schemas":{"A/B ~":{"type":"integer"}}}});
        assert!(
            compile(&contract, &json!({"$ref":"#/components/schemas/A~1B%20~0"}))
                .unwrap()
                .is_valid(&json!(3))
        );
        assert!(compile(&contract, &json!({"$ref":"#/components/schemas/A~2"})).is_err());
    }
}
