use std::collections::HashMap;
use std::sync::Arc;

use jensen_harness_core::agent::Session;
use jensen_harness_core::providers::config::ProviderRegistry;
use jensen_harness_core::providers::{Provider, ProviderError};
use jensen_harness_core::secrets::SecretSource;
use tokio::sync::Mutex;

/// Where a session's provider and model come from.
pub trait ModelSource: Send + Sync {
    /// # Errors
    /// [`ProviderError`] when no model is chosen, the address is unknown, or
    /// its key is missing.
    fn resolve(&self, address: Option<&str>) -> Result<(Arc<dyn Provider>, String), ProviderError>;
}

/// One provider and model for every session.
pub struct FixedModel {
    pub provider: Arc<dyn Provider>,
    pub model: String,
}

impl ModelSource for FixedModel {
    fn resolve(
        &self,
        _address: Option<&str>,
    ) -> Result<(Arc<dyn Provider>, String), ProviderError> {
        Ok((Arc::clone(&self.provider), self.model.clone()))
    }
}

/// Any model the provider config declares, chosen per session.
pub struct ConfiguredModels {
    pub registry: ProviderRegistry,
    pub secrets: Arc<dyn SecretSource>,
    pub default_model: Option<String>,
}

impl ModelSource for ConfiguredModels {
    fn resolve(&self, address: Option<&str>) -> Result<(Arc<dyn Provider>, String), ProviderError> {
        let address = address.or(self.default_model.as_deref()).ok_or_else(|| {
            ProviderError::Config(
                "no model was chosen: pass model, or set default_model in the config".to_string(),
            )
        })?;
        self.registry.resolve(address, self.secrets.as_ref())
    }
}

pub struct LiveSession {
    pub provider: Arc<dyn Provider>,
    pub session: Session,
}

/// The session table a connection handler dispatches against. Sessions are
/// keyed by id and shared across connections, and each has its own lock, so
/// a long run on one never blocks another.
pub struct Engine {
    pub models: Box<dyn ModelSource>,
    pub sessions: Mutex<HashMap<String, Arc<Mutex<LiveSession>>>>,
}

impl Engine {
    #[must_use]
    pub fn new(models: Box<dyn ModelSource>) -> Self {
        Self {
            models,
            sessions: Mutex::new(HashMap::new()),
        }
    }

    #[must_use]
    pub fn with_fixed_model(provider: Arc<dyn Provider>, model: impl Into<String>) -> Self {
        Self::new(Box::new(FixedModel {
            provider,
            model: model.into(),
        }))
    }
}
