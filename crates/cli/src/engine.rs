use std::collections::HashMap;
use std::sync::Arc;

use jensen_harness_core::agent::Session;
use jensen_harness_core::budget::BudgetSettings;
use jensen_harness_core::providers::config::ProviderRegistry;
use jensen_harness_core::providers::{Provider, ProviderError};
use jensen_harness_core::secrets::SecretSource;
use jensen_harness_core::store::{Store, StoreError};
use tokio::sync::Mutex;

/// A provider and the `provider_id/model_id` address it was chosen by.
pub struct ResolvedModel {
    pub provider: Arc<dyn Provider>,
    pub address: String,
}

/// Where a session's provider and model come from.
pub trait ModelSource: Send + Sync {
    /// # Errors
    /// [`ProviderError`] when no model is chosen, the address is unknown, or
    /// its key is missing.
    fn resolve(&self, address: Option<&str>) -> Result<ResolvedModel, ProviderError>;
}

/// One provider and model for every session.
pub struct FixedModel {
    pub provider: Arc<dyn Provider>,
    pub model: String,
}

impl ModelSource for FixedModel {
    fn resolve(&self, _address: Option<&str>) -> Result<ResolvedModel, ProviderError> {
        Ok(ResolvedModel {
            provider: Arc::clone(&self.provider),
            address: format!("{}/{}", self.provider.id(), self.model),
        })
    }
}

/// Any model the provider config declares, chosen per session.
pub struct ConfiguredModels {
    pub registry: ProviderRegistry,
    pub secrets: Arc<dyn SecretSource>,
    pub default_model: Option<String>,
}

impl ModelSource for ConfiguredModels {
    fn resolve(&self, address: Option<&str>) -> Result<ResolvedModel, ProviderError> {
        let address = address.or(self.default_model.as_deref()).ok_or_else(|| {
            ProviderError::Config(
                "no model was chosen: pass model, or set default_model in the config".to_string(),
            )
        })?;
        let (provider, _model) = self.registry.resolve(address, self.secrets.as_ref())?;
        Ok(ResolvedModel {
            provider,
            address: address.to_string(),
        })
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
    pub store: Store,
    pub budget: BudgetSettings,
    pub sessions: Mutex<HashMap<String, Arc<Mutex<LiveSession>>>>,
}

impl Engine {
    #[must_use]
    pub fn new(models: Box<dyn ModelSource>, store: Store, budget: BudgetSettings) -> Self {
        Self {
            models,
            store,
            budget,
            sessions: Mutex::new(HashMap::new()),
        }
    }

    /// One provider and model, an in memory store and the built in budget:
    /// for tests and for embedding.
    ///
    /// # Errors
    /// [`StoreError`] when the in memory database cannot be prepared.
    pub fn with_fixed_model(
        provider: Arc<dyn Provider>,
        model: impl Into<String>,
    ) -> Result<Self, StoreError> {
        Ok(Self::new(
            Box::new(FixedModel {
                provider,
                model: model.into(),
            }),
            Store::in_memory()?,
            BudgetSettings::default(),
        ))
    }
}
