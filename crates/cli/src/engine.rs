use std::collections::HashMap;
use std::sync::Arc;

use jensen_harness_core::agent::Session;
use jensen_harness_core::providers::Provider;
use tokio::sync::Mutex;

/// The one provider and model, and the in-memory session table a connection
/// handler dispatches requests against. Sessions are keyed by id and shared
/// across every connection, so a client that attaches later sees the same
/// sessions a daemon already holds.
pub struct Engine {
    pub provider: Arc<dyn Provider>,
    pub model: String,
    pub sessions: Mutex<HashMap<String, Session>>,
}

impl Engine {
    #[must_use]
    pub fn new(provider: Arc<dyn Provider>, model: impl Into<String>) -> Self {
        Self {
            provider,
            model: model.into(),
            sessions: Mutex::new(HashMap::new()),
        }
    }
}
