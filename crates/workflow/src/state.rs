use jensen_harness_core::store::{Store, StoreError};
use serde_json::{Value, json};
use thiserror::Error;

pub const ARTIFACT_THRESHOLD_BYTES: usize = 16 * 1024;
const ARTIFACT_KEY: &str = "$artifact";

#[derive(Debug, Error)]
pub enum StateError {
    #[error("channel `{0}` does not exist")]
    UnknownChannel(String),
    #[error("node `{node}` wrote channel `{channel}` against its schema: {message}")]
    SchemaViolation {
        node: String,
        channel: String,
        message: String,
    },
    #[error("artifact {0} is missing from the store")]
    MissingArtifact(String),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("a value could not be encoded: {0}")]
    Encode(String),
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
