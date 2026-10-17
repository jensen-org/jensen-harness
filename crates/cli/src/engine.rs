use std::collections::HashMap;
use std::sync::Arc;

use jensen_harness_core::agent::Session;
use jensen_harness_core::budget::BudgetSettings;
use jensen_harness_core::providers::config::ProviderRegistry;
use jensen_harness_core::providers::{Provider, ProviderError};
use jensen_harness_core::secrets::SecretSource;
use jensen_harness_core::store::{Store, StoreError};
use jensen_harness_promote::PromoteError;
use jensen_harness_promote::forge::{ForgeKind, ForgeTarget, Secret, parse_remote};
use jensen_harness_workflow::{CancellationToken, MemoryRunStore, RunError, RunStore};
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

    /// The `provider_id/model_id` a workflow role is bound to, if any.
    fn role(&self, role: &str) -> Option<String>;

    /// # Errors
    /// [`RunError`] when the remote is not a forge this config knows or its
    /// token is missing.
    fn forge_target(&self, remote_url: &str) -> Result<ForgeTarget, RunError>;
}

/// One provider and model for every session.
pub struct FixedModel {
    pub provider: Arc<dyn Provider>,
    pub model: String,
}

impl ModelSource for FixedModel {
    fn forge_target(&self, remote_url: &str) -> Result<ForgeTarget, RunError> {
        Err(PromoteError::UnknownForge(remote_url.to_owned()).into())
    }

    fn role(&self, _role: &str) -> Option<String> {
        Some(format!("{}/{}", self.provider.id(), self.model))
    }

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
    fn forge_target(&self, remote_url: &str) -> Result<ForgeTarget, RunError> {
        let remote = parse_remote(remote_url)?;
        for (name, kind, default_host) in [
            ("github", ForgeKind::GitHub, "github.com"),
            ("gitlab", ForgeKind::GitLab, "gitlab.com"),
        ] {
            let Some(entry) = self.registry.forge(name) else {
                continue;
            };
            let host = match &entry.base_url {
                Some(base) => url::Url::parse(base)
                    .ok()
                    .and_then(|parsed| parsed.host_str().map(str::to_ascii_lowercase))
                    .unwrap_or_default(),
                None => default_host.to_owned(),
            };
            if host == remote.host || (entry.base_url.is_some() && default_host == remote.host) {
                let token = self.registry.forge_token(name, self.secrets.as_ref())?;
                return Ok(ForgeTarget {
                    kind,
                    api_base: entry.base_url.clone(),
                    project: remote.project,
                    token: Secret::new(token),
                });
            }
        }
        Err(PromoteError::UnknownForge(remote_url.to_owned()).into())
    }

    fn role(&self, role: &str) -> Option<String> {
        self.registry.role(role).map(str::to_string)
    }

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
    pub checkpoints: Arc<dyn RunStore>,
    pub workspaces: std::path::PathBuf,
    pub active_workflows: Mutex<HashMap<String, CancellationToken>>,
}

impl Engine {
    #[must_use]
    pub fn new(models: Box<dyn ModelSource>, store: Store, budget: BudgetSettings) -> Self {
        Self {
            models,
            store,
            budget,
            sessions: Mutex::new(HashMap::new()),
            checkpoints: Arc::new(MemoryRunStore::new()),
            workspaces: std::env::temp_dir().join(format!(
                "jensen-harness-workspaces-{}",
                uuid::Uuid::new_v4()
            )),
            active_workflows: Mutex::new(HashMap::new()),
        }
    }

    #[must_use]
    pub fn with_workspaces(mut self, workspaces: std::path::PathBuf) -> Self {
        self.workspaces = workspaces;
        self
    }

    /// Keeps workflow checkpoints in `checkpoints`, so a run survives a
    /// restart.
    #[must_use]
    pub fn with_checkpoints(mut self, checkpoints: Arc<dyn RunStore>) -> Self {
        self.checkpoints = checkpoints;
        self
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
        Ok(Self::with_fixed_model_on(
            provider,
            model,
            Store::in_memory()?,
        ))
    }

    /// Like [`Engine::with_fixed_model`] over an existing store, so a second
    /// engine over the same store stands in for a restarted daemon.
    #[must_use]
    pub fn with_fixed_model_on(
        provider: Arc<dyn Provider>,
        model: impl Into<String>,
        store: Store,
    ) -> Self {
        Self::new(
            Box::new(FixedModel {
                provider,
                model: model.into(),
            }),
            store,
            BudgetSettings::default(),
        )
    }
}
