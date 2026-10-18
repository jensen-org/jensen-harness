//! `providers.toml`: which providers exist and how to reach them. A model is
//! addressed as `provider_id/model_id`, split at the first slash so a
//! gateway's own `vendor/model` ids stay intact.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde::Deserialize;

use super::genai::{GenaiProvider, parse_adapter};
use super::{Provider, ProviderError};
use crate::budget::{BudgetSettings, Caps, PricingTable};
use crate::secrets::SecretSource;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Transport {
    Anthropic,
    OpenaiCompatible,
    Genai,
}

pub const DEFAULT_KEYCHAIN_SERVICE: &str = "jensen-harness";

/// Where a key sits in the OS keychain. `service` defaults to
/// [`DEFAULT_KEYCHAIN_SERVICE`] and `user` to the provider id.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeychainRef {
    pub service: Option<String>,
    pub user: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderEntry {
    pub transport: Transport,
    pub adapter: Option<String>,
    pub base_url: String,
    pub api_key_env: Option<String>,
    pub api_key_keychain: Option<KeychainRef>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForgeEntry {
    pub base_url: Option<String>,
    pub api_key_env: Option<String>,
    pub api_key_keychain: Option<KeychainRef>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BudgetFile {
    max_tokens: Option<u64>,
    max_cost_micro_usd: Option<u64>,
    soft_percent: Option<u8>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpNetwork {
    #[default]
    None,
    Full,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpAccess {
    #[default]
    ReadOnly,
    ReadWrite,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpServerEntry {
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env_allow: Vec<String>,
    pub url: Option<String>,
    #[serde(default)]
    pub network: McpNetwork,
    #[serde(default)]
    pub access: McpAccess,
    #[serde(default)]
    pub mutating: bool,
    #[serde(default)]
    pub tools: Vec<String>,
    pub timeout_secs: Option<u64>,
    pub api_key_env: Option<String>,
    pub api_key_keychain: Option<KeychainRef>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LspServerEntry {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env_allow: Vec<String>,
    pub language: String,
    #[serde(default)]
    pub network: McpNetwork,
    pub timeout_secs: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct LspFile {
    #[serde(default)]
    servers: BTreeMap<String, LspServerEntry>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct McpFile {
    #[serde(default)]
    servers: BTreeMap<String, McpServerEntry>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct LibraryFile {
    #[serde(default)]
    paths: Vec<std::path::PathBuf>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct SkillsFile {
    #[serde(default)]
    allow_project: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigFile {
    default_model: Option<String>,
    providers: BTreeMap<String, ProviderEntry>,
    #[serde(default)]
    roles: BTreeMap<String, String>,
    #[serde(default)]
    forges: BTreeMap<String, ForgeEntry>,
    #[serde(default)]
    library: LibraryFile,
    #[serde(default)]
    skills: SkillsFile,
    #[serde(default)]
    mcp: McpFile,
    #[serde(default)]
    lsp: LspFile,
    #[serde(default)]
    pricing: PricingTable,
    budget: Option<BudgetFile>,
}

#[derive(Debug)]
pub struct ProviderRegistry {
    default_model: Option<String>,
    providers: BTreeMap<String, ProviderEntry>,
    roles: BTreeMap<String, String>,
    forges: BTreeMap<String, ForgeEntry>,
    library: Vec<std::path::PathBuf>,
    allow_project_skills: bool,
    mcp_servers: BTreeMap<String, McpServerEntry>,
    lsp_servers: BTreeMap<String, LspServerEntry>,
    budget: BudgetSettings,
}

fn budget_settings(file: &ConfigFile) -> Result<BudgetSettings, ProviderError> {
    for address in file.pricing.addresses() {
        let known = address
            .split_once('/')
            .is_some_and(|(provider, _model)| file.providers.contains_key(provider));
        if !known {
            return Err(ProviderError::Config(format!(
                "pricing names {address:?}, which is not provider_id/model_id of a declared provider"
            )));
        }
    }
    let Some(budget) = &file.budget else {
        return Ok(BudgetSettings {
            pricing: file.pricing.clone(),
            ..BudgetSettings::default()
        });
    };
    if budget
        .soft_percent
        .is_some_and(|percent| !(1..=99).contains(&percent))
    {
        return Err(ProviderError::Config(
            "budget soft_percent must be between 1 and 99".to_string(),
        ));
    }
    let caps = Caps {
        max_tokens: budget.max_tokens,
        max_cost_micro_usd: budget.max_cost_micro_usd,
    }
    .validated()
    .map_err(|error| ProviderError::Config(error.to_string()))?;
    Ok(BudgetSettings {
        pricing: file.pricing.clone(),
        caps,
        soft_percent: budget.soft_percent,
    })
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
            if entry.api_key_env.is_some() == entry.api_key_keychain.is_some() {
                return Err(ProviderError::Config(format!(
                    "provider {id} needs exactly one of api_key_env and api_key_keychain"
                )));
            }
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
        for (role, address) in &file.roles {
            let known = address
                .split_once('/')
                .is_some_and(|(provider, _model)| file.providers.contains_key(provider));
            if !known {
                return Err(ProviderError::Config(format!(
                    "role {role} names {address:?}, which is not provider_id/model_id of a declared provider"
                )));
            }
        }
        for (name, entry) in &file.mcp.servers {
            let plain = !name.is_empty()
                && !name.contains("__")
                && name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'));
            if !plain {
                return Err(ProviderError::Config(format!(
                    "mcp server name {name:?} must be letters, digits, dashes and single underscores"
                )));
            }
            if entry.url.is_some() && entry.network == McpNetwork::None {
                return Err(ProviderError::Config(format!(
                    "mcp server {name} is reached over the network; network = \"none\" contradicts its url"
                )));
            }
            if entry.command.is_some() == entry.url.is_some() {
                return Err(ProviderError::Config(format!(
                    "mcp server {name} needs exactly one of command and url"
                )));
            }
            if entry.api_key_env.is_some() && entry.api_key_keychain.is_some() {
                return Err(ProviderError::Config(format!(
                    "mcp server {name} takes at most one of api_key_env and api_key_keychain"
                )));
            }
            if entry.command.is_some()
                && (entry.api_key_env.is_some() || entry.api_key_keychain.is_some())
            {
                return Err(ProviderError::Config(format!(
                    "mcp server {name} runs as a command; pass a token with env_allow instead"
                )));
            }
        }
        for (name, entry) in &file.forges {
            if !matches!(name.as_str(), "github" | "gitlab") {
                return Err(ProviderError::Config(format!(
                    "forge {name} is not github or gitlab"
                )));
            }
            if entry.api_key_env.is_some() == entry.api_key_keychain.is_some() {
                return Err(ProviderError::Config(format!(
                    "forge {name} needs exactly one of api_key_env and api_key_keychain"
                )));
            }
        }
        let budget = budget_settings(&file)?;
        Ok(Self {
            default_model: file.default_model,
            providers: file.providers,
            roles: file.roles,
            forges: file.forges,
            library: file.library.paths,
            allow_project_skills: file.skills.allow_project,
            mcp_servers: file.mcp.servers,
            lsp_servers: file.lsp.servers,
            budget,
        })
    }

    #[must_use]
    pub fn budget_settings(&self) -> &BudgetSettings {
        &self.budget
    }

    #[must_use]
    pub fn default_model(&self) -> Option<&str> {
        self.default_model.as_deref()
    }

    /// The model a workflow role is bound to, as `provider_id/model_id`.
    #[must_use]
    pub fn role(&self, name: &str) -> Option<&str> {
        self.roles.get(name).map(String::as_str)
    }

    /// The keychain service and user a provider's key is stored under.
    ///
    /// # Errors
    /// [`ProviderError::UnknownProvider`] for an id the config lacks and
    /// [`ProviderError::Config`] for a provider that reads its key from the
    /// environment instead.
    pub fn keychain_location(&self, provider_id: &str) -> Result<(String, String), ProviderError> {
        if !self.providers.contains_key(provider_id)
            && !self.forges.contains_key(provider_id)
            && let Some(server) = self.mcp_servers.get(provider_id)
        {
            let reference = server.api_key_keychain.as_ref().ok_or_else(|| {
                ProviderError::Config(format!(
                    "mcp server {provider_id} does not read its token from the keychain"
                ))
            })?;
            return Ok((
                reference
                    .service
                    .clone()
                    .unwrap_or_else(|| DEFAULT_KEYCHAIN_SERVICE.to_string()),
                reference
                    .user
                    .clone()
                    .unwrap_or_else(|| format!("mcp-{provider_id}")),
            ));
        }
        if !self.providers.contains_key(provider_id)
            && let Some(forge) = self.forges.get(provider_id)
        {
            let reference = forge.api_key_keychain.as_ref().ok_or_else(|| {
                ProviderError::Config(format!(
                    "forge {provider_id} reads its key from the environment, not the keychain"
                ))
            })?;
            return Ok((
                reference
                    .service
                    .clone()
                    .unwrap_or_else(|| DEFAULT_KEYCHAIN_SERVICE.to_string()),
                reference
                    .user
                    .clone()
                    .unwrap_or_else(|| format!("forge-{provider_id}")),
            ));
        }
        let entry = self
            .providers
            .get(provider_id)
            .ok_or_else(|| ProviderError::UnknownProvider(provider_id.to_string()))?;
        let reference = entry.api_key_keychain.as_ref().ok_or_else(|| {
            ProviderError::Config(format!(
                "provider {provider_id} reads its key from the environment, not the keychain"
            ))
        })?;
        Ok((
            reference
                .service
                .clone()
                .unwrap_or_else(|| DEFAULT_KEYCHAIN_SERVICE.to_string()),
            reference
                .user
                .clone()
                .unwrap_or_else(|| provider_id.to_string()),
        ))
    }

    #[must_use]
    pub fn lsp_server(&self, name: &str) -> Option<&LspServerEntry> {
        self.lsp_servers.get(name)
    }

    #[must_use]
    pub fn mcp_server(&self, name: &str) -> Option<&McpServerEntry> {
        self.mcp_servers.get(name)
    }

    pub fn mcp_token(
        &self,
        name: &str,
        secrets: &dyn SecretSource,
    ) -> Result<Option<String>, ProviderError> {
        let entry = self
            .mcp_servers
            .get(name)
            .ok_or_else(|| ProviderError::UnknownProvider(name.to_string()))?;
        if let Some(variable) = &entry.api_key_env {
            return secrets
                .env(variable)
                .map(Some)
                .ok_or_else(|| ProviderError::MissingApiKey {
                    provider: name.to_string(),
                    hint: format!("set the {variable} environment variable"),
                });
        }
        let Some(reference) = &entry.api_key_keychain else {
            return Ok(None);
        };
        let service = reference
            .service
            .clone()
            .unwrap_or_else(|| DEFAULT_KEYCHAIN_SERVICE.to_string());
        let user = reference
            .user
            .clone()
            .unwrap_or_else(|| format!("mcp-{name}"));
        secrets
            .keychain(&service, &user)?
            .map(Some)
            .ok_or_else(|| ProviderError::MissingApiKey {
                provider: name.to_string(),
                hint: format!(
                    "store it with `jensen-harness auth set {name}` (keychain {service}/{user})"
                ),
            })
    }

    #[must_use]
    pub fn allow_project_skills(&self) -> bool {
        self.allow_project_skills
    }

    #[must_use]
    pub fn library_paths(&self) -> &[std::path::PathBuf] {
        &self.library
    }

    #[must_use]
    pub fn forge(&self, name: &str) -> Option<&ForgeEntry> {
        self.forges.get(name)
    }

    pub fn forge_token(
        &self,
        name: &str,
        secrets: &dyn SecretSource,
    ) -> Result<String, ProviderError> {
        let entry = self
            .forges
            .get(name)
            .ok_or_else(|| ProviderError::UnknownProvider(name.to_string()))?;
        if let Some(variable) = &entry.api_key_env {
            return secrets
                .env(variable)
                .ok_or_else(|| ProviderError::MissingApiKey {
                    provider: name.to_string(),
                    hint: format!("set the {variable} environment variable"),
                });
        }
        let (service, user) = self.keychain_location(name)?;
        secrets
            .keychain(&service, &user)?
            .ok_or_else(|| ProviderError::MissingApiKey {
                provider: name.to_string(),
                hint: format!(
                    "store it with `jensen-harness auth set {name}` (keychain {service}/{user})"
                ),
            })
    }

    fn api_key(
        &self,
        provider_id: &str,
        secrets: &dyn SecretSource,
    ) -> Result<String, ProviderError> {
        let entry = self
            .providers
            .get(provider_id)
            .ok_or_else(|| ProviderError::UnknownProvider(provider_id.to_string()))?;
        if let Some(variable) = &entry.api_key_env {
            return secrets
                .env(variable)
                .ok_or_else(|| ProviderError::MissingApiKey {
                    provider: provider_id.to_string(),
                    hint: format!("set the {variable} environment variable"),
                });
        }
        let (service, user) = self.keychain_location(provider_id)?;
        secrets
            .keychain(&service, &user)?
            .ok_or_else(|| ProviderError::MissingApiKey {
                provider: provider_id.to_string(),
                hint: format!(
                    "store it with `jensen-harness auth set {provider_id}` (keychain {service}/{user})"
                ),
            })
    }

    /// Builds the provider an address names and returns it with the model id.
    ///
    /// # Errors
    /// [`ProviderError::Config`] for an address with no slash,
    /// [`ProviderError::UnknownProvider`] for an id the config lacks,
    /// [`ProviderError::MissingApiKey`] when its key is not there and
    /// [`ProviderError::Secret`] when the keychain itself fails.
    pub fn resolve(
        &self,
        address: &str,
        secrets: &dyn SecretSource,
    ) -> Result<(Arc<dyn Provider>, String), ProviderError> {
        let (provider_id, model) = address.split_once('/').ok_or_else(|| {
            ProviderError::Config(format!("{address:?} is not provider_id/model_id"))
        })?;
        let entry = self
            .providers
            .get(provider_id)
            .ok_or_else(|| ProviderError::UnknownProvider(provider_id.to_string()))?;
        let api_key = self.api_key(provider_id, secrets)?;
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
