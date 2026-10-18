use std::collections::BTreeMap;

use cel::{Context, Program, Value as Cel};
use serde_json::Value;

pub fn eval(source: &str, variables: &BTreeMap<&str, Value>) -> Result<Cel, String> {
    let program = Program::compile(source).map_err(|error| error.to_string())?;
    let mut context = Context::default();
    for (name, value) in variables {
        context
            .add_variable(*name, value)
            .map_err(|error| error.to_string())?;
    }
    program.execute(&context).map_err(|error| error.to_string())
}

pub fn truth(source: &str, variables: &BTreeMap<&str, Value>) -> Result<bool, String> {
    match eval(source, variables)? {
        Cel::Bool(verdict) => Ok(verdict),
        other => Err(format!("`{source}` gave {other:?}, not a boolean")),
    }
}

pub fn label(source: &str, variables: &BTreeMap<&str, Value>) -> Result<String, String> {
    match eval(source, variables)? {
        Cel::String(text) => Ok(text.to_string()),
        Cel::Bool(flag) => Ok(flag.to_string()),
        Cel::Int(number) => Ok(number.to_string()),
        other => Err(format!("`{source}` gave {other:?}, not a label")),
    }
}
