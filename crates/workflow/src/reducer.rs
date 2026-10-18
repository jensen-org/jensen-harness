use serde_json::{Map, Value};
use thiserror::Error;

use crate::spec::Reducer;

#[derive(Debug, Error, PartialEq, Eq)]
#[error("{reducer:?} cannot combine {found} into {holding}")]
pub struct ReduceError {
    pub reducer: Reducer,
    pub holding: &'static str,
    pub found: &'static str,
}

fn kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

pub fn apply(reducer: Reducer, current: Option<Value>, write: Value) -> Result<Value, ReduceError> {
    if reducer == Reducer::SetUnion {
        return set_union(current, write);
    }
    let Some(current) = current else {
        return Ok(write);
    };
    let mismatch = |current: &Value, write: &Value| ReduceError {
        reducer,
        holding: kind(current),
        found: kind(write),
    };
    match (reducer, current, write) {
        (Reducer::Overwrite, _, write) => Ok(write),
        (Reducer::Append, Value::Array(mut items), Value::Array(more)) => {
            items.extend(more);
            Ok(Value::Array(items))
        }
        (Reducer::MergeMap, Value::Object(mut map), Value::Object(more)) => {
            let more: Map<String, Value> = more;
            map.extend(more);
            Ok(Value::Object(map))
        }
        (Reducer::Max, Value::Number(held), Value::Number(new)) => {
            let larger = match (held.as_f64(), new.as_f64()) {
                (Some(a), Some(b)) if b > a => Value::Number(new),
                _ => Value::Number(held),
            };
            Ok(larger)
        }
        (_, current, write) => Err(mismatch(&current, &write)),
    }
}

fn set_union(current: Option<Value>, write: Value) -> Result<Value, ReduceError> {
    let held = current.unwrap_or_else(|| Value::Array(Vec::new()));
    match (held, write) {
        (Value::Array(items), Value::Array(more)) => {
            let mut union: Vec<Value> = Vec::new();
            for item in items.into_iter().chain(more) {
                if !union.contains(&item) {
                    union.push(item);
                }
            }
            Ok(Value::Array(union))
        }
        (held, write) => Err(ReduceError {
            reducer: Reducer::SetUnion,
            holding: kind(&held),
            found: kind(&write),
        }),
    }
}
