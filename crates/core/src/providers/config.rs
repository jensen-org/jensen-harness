//! `providers.toml`: which providers exist and how to reach them. A model is
//! addressed as `provider_id/model_id`, split at the first slash so a
//! gateway's own `vendor/model` ids stay intact.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde::Deserialize;

use super::genai::{GenaiProvider, parse_adapter};
use super::{Provider, ProviderError, require_api_key};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Transport {
    Anthropic,
    OpenaiCompatible,
    Genai,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderEntry {
    pub transport: Transport,
    pub adapter: Option<String>,
    pub base_url: String,
    pub api_key_env: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigFile {
    default_model: Option<String>,
    providers: BTreeMap<String, ProviderEntry>,
}

#[derive(Debug)]
pub struct ProviderRegistry {
    default_model: Option<String>,
    providers: BTreeMap<String, ProviderEntry>,
}

impl ProviderRegistry {
    /// # Errors
    /// [`ProviderError::Config`] when the TOML is malformed, a `genai`
    /// provider names no adapter or an unknown one, or an adapter is set on
    /// a transport that takes none.
    pub fn from_toml(text: &str) -> Result<Self, ProviderError> {
        let file: ConfigFile =
            toml::from_str(text).map_err(|error| ProviderError::Config(error.to_string()))?;
        for (id, entry) in &file.providers {
            match (entry.transport, entry.adapter.as_deref()) {
                (Transport::Genai, None) => {
                    return Err(ProviderError::Config(format!(
                        "provider {id} uses the genai transport and needs an adapter"
                    )));
                }
                (Transport::Genai, Some(name)) if parse_adapter(name).is_none() => {
                    return Err(ProviderError::Config(format!(
                        "provider {id} names an unknown adapter {name:?}"
                    )));
                }
                (Transport::Anthropic | Transport::OpenaiCompatible, Some(_)) => {
                    return Err(ProviderError::Config(format!(
                        "provider {id} sets an adapter, which only the genai transport takes"
                    )));
                }
                _ => {}
            }
        }
        Ok(Self {
            default_model: file.default_model,
            providers: file.providers,
        })
    }

    #[must_use]
    pub fn default_model(&self) -> Option<&str> {
        self.default_model.as_deref()
    }

    /// Builds the provider an address names and returns it with the model id.
    ///
    /// # Errors
    /// [`ProviderError::Config`] for an address with no slash,
    /// [`ProviderError::UnknownProvider`] for an id the config lacks, and
    /// [`ProviderError::MissingApiKey`] when its key variable has no value.
    pub fn resolve(
        &self,
        address: &str,
        lookup: &dyn Fn(&str) -> Option<String>,
    ) -> Result<(Arc<dyn Provider>, String), ProviderError> {
        let (provider_id, model) = address.split_once('/').ok_or_else(|| {
            ProviderError::Config(format!("{address:?} is not provider_id/model_id"))
        })?;
        let entry = self
            .providers
            .get(provider_id)
            .ok_or_else(|| ProviderError::UnknownProvider(provider_id.to_string()))?;
        let api_key = require_api_key(provider_id, &entry.api_key_env, lookup)?;
        let provider: Arc<dyn Provider> = match entry.transport {
            Transport::Anthropic | Transport::OpenaiCompatible | Transport::Genai => {
                let name = match entry.transport {
                    Transport::Anthropic => "anthropic",
                    Transport::OpenaiCompatible => "openai",
                    Transport::Genai => entry.adapter.as_deref().ok_or_else(|| {
                        ProviderError::Config(format!("provider {provider_id} needs an adapter"))
                    })?,
                };
                let adapter = parse_adapter(name)
                    .ok_or_else(|| ProviderError::Config(format!("unknown adapter {name:?}")))?;
                Arc::new(GenaiProvider::new(
                    provider_id,
                    adapter,
                    &entry.base_url,
                    api_key,
                ))
            }
        };
        Ok((provider, model.to_string()))
    }
}
