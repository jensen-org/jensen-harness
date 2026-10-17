use std::collections::BTreeMap;

use jensen_harness_core::store::{Store, StoreError};
use jsonschema::Validator;
use serde_json::{Value, json};
use thiserror::Error;

use crate::reducer::{self, ReduceError};
use crate::spec::NodeSpec;
use crate::validate::Workflow;

pub const ARTIFACT_THRESHOLD_BYTES: usize = 16 * 1024;
const ARTIFACT_KEY: &str = "$artifact";

#[derive(Debug, Error)]
pub enum StateError {
    #[error("channel `{0}` does not exist")]
    UnknownChannel(String),
    #[error("node `{node}` wrote channel `{channel}` which it does not declare")]
    UndeclaredWrite { node: String, channel: String },
    #[error("node `{node}` wrote channel `{channel}` against its schema: {message}")]
    SchemaViolation {
        node: String,
        channel: String,
        message: String,
    },
    #[error("channel `{channel}`: {source}")]
    Reduce {
        channel: String,
        source: ReduceError,
    },
    #[error("artifact {0} is missing from the store")]
    MissingArtifact(String),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("a value could not be encoded: {0}")]
    Encode(String),
}

struct Write {
    rank: (usize, String),
    value: Value,
}

pub struct State {
    wave_of: BTreeMap<String, usize>,
    validators: BTreeMap<String, Validator>,
    declared: BTreeMap<String, Vec<String>>,
    reducers: BTreeMap<String, crate::spec::Reducer>,
    inits: BTreeMap<String, Value>,
    writes: BTreeMap<String, Vec<Write>>,
}

impl State {
    pub fn new(workflow: &Workflow) -> Result<Self, StateError> {
        let spec = workflow.spec();
        let mut wave_of = BTreeMap::new();
        for (depth, wave) in workflow.waves().iter().enumerate() {
            for node in wave {
                wave_of.insert(node.clone(), depth);
            }
        }
        for (id, node) in &spec.nodes {
            if let NodeSpec::Loop(body) = node {
                let depth = wave_of.get(id).copied().unwrap_or_default();
                for member in &body.body {
                    wave_of.insert(member.clone(), depth);
                }
            }
        }
        let mut validators = BTreeMap::new();
        let mut reducers = BTreeMap::new();
        let mut inits = BTreeMap::new();
        for (name, channel) in &spec.channels {
            let validator = jsonschema::validator_for(&channel.schema).map_err(|error| {
                StateError::SchemaViolation {
                    node: String::new(),
                    channel: name.clone(),
                    message: error.to_string(),
                }
            })?;
            validators.insert(name.clone(), validator);
            reducers.insert(name.clone(), channel.reducer);
            if let Some(init) = &channel.init {
                inits.insert(name.clone(), init.clone());
            }
        }
        let declared = spec
            .nodes
            .iter()
            .map(|(id, node)| (id.clone(), node.writes()))
            .collect();
        Ok(Self {
            wave_of,
            validators,
            declared,
            reducers,
            inits,
            writes: BTreeMap::new(),
        })
    }

    pub fn write(&mut self, node: &str, channel: &str, value: Value) -> Result<(), StateError> {
        let validator = self
            .validators
            .get(channel)
            .ok_or_else(|| StateError::UnknownChannel(channel.to_owned()))?;
        let allowed = self
            .declared
            .get(node)
            .is_some_and(|channels| channels.iter().any(|name| name == channel));
        if !allowed {
            return Err(StateError::UndeclaredWrite {
                node: node.to_owned(),
                channel: channel.to_owned(),
            });
        }
        validator
            .validate(&value)
            .map_err(|error| StateError::SchemaViolation {
                node: node.to_owned(),
                channel: channel.to_owned(),
                message: error.to_string(),
            })?;
        let rank = (
            self.wave_of.get(node).copied().unwrap_or_default(),
            node.to_owned(),
        );
        self.writes
            .entry(channel.to_owned())
            .or_default()
            .push(Write { rank, value });
        Ok(())
    }

    pub fn value(&self, channel: &str) -> Result<Option<Value>, StateError> {
        let reducer = *self
            .reducers
            .get(channel)
            .ok_or_else(|| StateError::UnknownChannel(channel.to_owned()))?;
        let mut folded = self.inits.get(channel).cloned();
        let mut ordered: Vec<&Write> = self.writes.get(channel).into_iter().flatten().collect();
        ordered.sort_by(|a, b| a.rank.cmp(&b.rank));
        for write in ordered {
            folded = Some(
                reducer::apply(reducer, folded, write.value.clone()).map_err(|source| {
                    StateError::Reduce {
                        channel: channel.to_owned(),
                        source,
                    }
                })?,
            );
        }
        Ok(folded)
    }

    pub fn channel_hash(&self, channel: &str) -> Result<String, StateError> {
        let canonical = self.value(channel)?.unwrap_or(Value::Null).to_string();
        Ok(blake3::hash(canonical.as_bytes()).to_hex().to_string())
    }

    pub fn memo_key(&self, node: &NodeSpec, reads: &[String]) -> Result<String, StateError> {
        let mut inputs = BTreeMap::new();
        for channel in reads {
            inputs.insert(channel.clone(), self.channel_hash(channel)?);
        }
        let definition =
            serde_json::to_value(node).map_err(|error| StateError::Encode(error.to_string()))?;
        let canonical = json!({ "node": definition, "inputs": inputs }).to_string();
        Ok(blake3::hash(canonical.as_bytes()).to_hex().to_string())
    }
}

pub async fn externalize(store: &Store, value: Value) -> Result<Value, StateError> {
    let encoded = value.to_string();
    if encoded.len() <= ARTIFACT_THRESHOLD_BYTES {
        return Ok(value);
    }
    let hash = store.put_artifact(encoded.into_bytes()).await?;
    Ok(json!({ ARTIFACT_KEY: hash }))
}

pub async fn resolve(store: &Store, value: Value) -> Result<Value, StateError> {
    let hash = match &value {
        Value::Object(map) if map.len() == 1 => map.get(ARTIFACT_KEY).and_then(Value::as_str),
        _ => None,
    };
    let Some(hash) = hash else {
        return Ok(value);
    };
    let bytes = store
        .artifact(hash)
        .await?
        .ok_or_else(|| StateError::MissingArtifact(hash.to_owned()))?;
    serde_json::from_slice(&bytes).map_err(|error| StateError::Encode(error.to_string()))
}
