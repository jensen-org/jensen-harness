use std::collections::HashMap;

use jensen_harness_core::agent::Session;
use jensen_harness_core::providers::Provider;
use tokio::sync::Mutex;

/// The one provider and the in-memory session table a connection handler
/// dispatches requests against. Sessions are keyed by id and shared across
/// every connection, so a client that attaches later sees the same
/// sessions a daemon already holds.
pub struct Engine {
    pub provider: Box<dyn Provider>,
    pub sessions: Mutex<HashMap<String, Session>>,
}

impl Engine {
    #[must_use]
    pub fn new(provider: Box<dyn Provider>) -> Self {
        Self {
            provider,
            sessions: Mutex::new(HashMap::new()),
        }
    }
}
