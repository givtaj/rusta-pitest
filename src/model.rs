use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A frozen, reviewable contract and its generated cases. Runtime bindings are
/// supplied separately and must never be written back into this document.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Suite {
    pub suite_version: u32,
    pub source: Source,
    pub contract: Value,
    pub cases: Vec<Case>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Source {
    pub sha256: String,
    pub openapi_version: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Case {
    pub id: String,
    pub method: String,
    pub path: String,
    pub parameters: Vec<Parameter>,
    pub security: Vec<BTreeMap<String, Vec<String>>>,
    pub responses: Value,
    pub body: Option<Value>,
    pub blockers: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Parameter {
    pub name: String,
    pub location: String,
    pub required: bool,
    pub style: String,
    pub explode: bool,
    pub schema: Value,
    pub value: Option<Value>,
    pub fixture: Option<String>,
}
