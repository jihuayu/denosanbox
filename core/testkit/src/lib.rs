//! Backend-neutral behavior fixture schema, shared by Rust and native bindings.
use serde::Deserialize;
use serde_json::Value;
#[derive(Debug, Deserialize)]
pub struct Fixture {
    pub id: String,
    pub source: String,
    pub entrypoint: String,
    pub steps: Vec<Step>,
}
#[derive(Debug, Deserialize)]
pub struct Step {
    pub sandbox: usize,
    pub export: String,
    pub input: Value,
    pub expected: Expected,
}
#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Expected {
    Ok { value: Value },
    Err { code: String },
}
pub fn fixtures() -> Vec<Fixture> {
    serde_json::from_str(include_str!("../../../fixtures/behavior/v1.json"))
        .expect("versioned behavior fixture schema")
}
