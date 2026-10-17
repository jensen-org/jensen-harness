use std::collections::HashMap;

use jensen_harness_core::providers::ProviderError;
use jensen_harness_core::providers::config::ProviderRegistry;
use jensen_harness_core::secrets::{SecretError, SecretSource};

const CONFIG: &str = r#"
default_model = "anthropic/claude-sonnet-5-5"

[providers.anthropic]
transport = "anthropic"
base_url = "https://api.anthropic.com"
api_key_keychain = {}

[providers.openai]
transport = "openai_compatible"
base_url = "https://api.openai.com/v1"
api_key_env = "OPENAI_API_KEY"

[providers.gateway]
transport = "openai_compatible"
base_url = "https://openrouter.ai/api/v1"
api_key_env = "GATEWAY_API_KEY"

[providers.deepseek]
transport = "genai"
adapter = "deepseek"
base_url = "https://api.deepseek.com"
api_key_env = "DEEPSEEK_API_KEY"
"#;

#[derive(Default)]
struct Secrets {
    env: HashMap<String, String>,
    keychain: HashMap<(String, String), String>,
}

impl Secrets {
    fn with_env(pairs: &[(&str, &str)]) -> Self {
        Self {
            env: pairs
                .iter()
                .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
                .collect(),
            keychain: HashMap::new(),
        }
    }
}

impl SecretSource for Secrets {
    fn env(&self, name: &str) -> Option<String> {
        self.env.get(name).cloned()
    }

    fn keychain(&self, service: &str, user: &str) -> Result<Option<String>, SecretError> {
        Ok(self
            .keychain
            .get(&(service.to_string(), user.to_string()))
            .cloned())
    }
}

#[test]
fn the_default_model_comes_from_the_config() {
    let registry = ProviderRegistry::from_toml(CONFIG).unwrap();
    assert_eq!(
        registry.default_model(),
        Some("anthropic/claude-sonnet-5-5")
    );
}

#[test]
fn an_address_splits_at_the_first_slash_so_gateway_model_ids_stay_whole() {
    let registry = ProviderRegistry::from_toml(CONFIG).unwrap();
    let lookup = Secrets::with_env(&[("GATEWAY_API_KEY", "k")]);

    let (provider, model) = registry
        .resolve("gateway/anthropic/claude-sonnet-5-5", &lookup)
        .unwrap();

    assert_eq!(provider.id(), "gateway");
    assert_eq!(model, "anthropic/claude-sonnet-5-5");
}

#[test]
fn each_transport_resolves_to_a_provider_with_its_own_id() {
    let registry = ProviderRegistry::from_toml(CONFIG).unwrap();
    let mut lookup = Secrets::with_env(&[("OPENAI_API_KEY", "o"), ("DEEPSEEK_API_KEY", "d")]);
    lookup.keychain.insert(
        ("jensen-harness".to_string(), "anthropic".to_string()),
        "a".to_string(),
    );
    for address in ["anthropic/m", "openai/m", "deepseek/m"] {
        let (provider, _model) = registry.resolve(address, &lookup).unwrap();
        assert_eq!(provider.id(), address.split('/').next().unwrap());
    }
}

#[test]
fn an_unset_key_variable_fails_loudly_naming_it() {
    let registry = ProviderRegistry::from_toml(CONFIG).unwrap();
    let error = registry
        .resolve("openai/gpt", &Secrets::default())
        .unwrap_err();
    let ProviderError::MissingApiKey { hint, .. } = error else {
        panic!("expected a missing key error");
    };
    assert!(hint.contains("OPENAI_API_KEY"), "{hint}");
}

#[test]
fn an_unknown_provider_and_a_bare_model_are_errors() {
    let registry = ProviderRegistry::from_toml(CONFIG).unwrap();
    assert!(matches!(
        registry.resolve("nope/m", &Secrets::default()).unwrap_err(),
        ProviderError::UnknownProvider(_)
    ));
    assert!(matches!(
        registry
            .resolve("just-a-model", &Secrets::default())
            .unwrap_err(),
        ProviderError::Config(_)
    ));
}

#[test]
fn a_genai_provider_needs_a_known_adapter() {
    let missing = r#"
[providers.x]
transport = "genai"
base_url = "https://x"
api_key_env = "K"
"#;
    let unknown = r#"
[providers.x]
transport = "genai"
adapter = "not-a-vendor"
base_url = "https://x"
api_key_env = "K"
"#;
    let stray = r#"
[providers.x]
transport = "anthropic"
adapter = "deepseek"
base_url = "https://x"
api_key_env = "K"
"#;
    for text in [missing, unknown, stray] {
        assert!(matches!(
            ProviderRegistry::from_toml(text).unwrap_err(),
            ProviderError::Config(_)
        ));
    }
}

#[test]
fn an_unknown_config_key_is_rejected() {
    let text = r#"
[providers.x]
transport = "anthropic"
base_url = "https://x"
api_key_env = "K"
api_key = "sk-oops"
"#;
    assert!(matches!(
        ProviderRegistry::from_toml(text).unwrap_err(),
        ProviderError::Config(_)
    ));
}

#[test]
fn a_keychain_backed_provider_reads_its_key_from_the_default_location() {
    let registry = ProviderRegistry::from_toml(CONFIG).unwrap();
    let mut secrets = Secrets::default();
    secrets.keychain.insert(
        ("jensen-harness".to_string(), "anthropic".to_string()),
        "sk-from-keychain".to_string(),
    );

    let (provider, _model) = registry.resolve("anthropic/m", &secrets).unwrap();

    assert_eq!(provider.id(), "anthropic");
}

#[test]
fn a_keychain_entry_that_is_not_stored_says_how_to_store_it() {
    let registry = ProviderRegistry::from_toml(CONFIG).unwrap();
    let error = registry
        .resolve("anthropic/m", &Secrets::default())
        .unwrap_err();
    let ProviderError::MissingApiKey { hint, .. } = error else {
        panic!("expected a missing key error");
    };
    assert!(hint.contains("auth set anthropic"), "{hint}");
}

#[test]
fn keychain_locations_default_to_the_provider_id_and_can_be_overridden() {
    let text = r#"
[providers.a]
transport = "anthropic"
base_url = "https://x"
api_key_keychain = {}

[providers.b]
transport = "anthropic"
base_url = "https://x"
api_key_keychain = { service = "team", user = "shared-b" }

[providers.c]
transport = "anthropic"
base_url = "https://x"
api_key_env = "C_KEY"
"#;
    let registry = ProviderRegistry::from_toml(text).unwrap();
    assert_eq!(
        registry.keychain_location("a").unwrap(),
        ("jensen-harness".to_string(), "a".to_string())
    );
    assert_eq!(
        registry.keychain_location("b").unwrap(),
        ("team".to_string(), "shared-b".to_string())
    );
    assert!(matches!(
        registry.keychain_location("c").unwrap_err(),
        ProviderError::Config(_)
    ));
}

#[test]
fn exactly_one_key_source_is_required() {
    let neither = r#"
[providers.x]
transport = "anthropic"
base_url = "https://x"
"#;
    let both = r#"
[providers.x]
transport = "anthropic"
base_url = "https://x"
api_key_env = "K"
api_key_keychain = {}
"#;
    for text in [neither, both] {
        assert!(matches!(
            ProviderRegistry::from_toml(text).unwrap_err(),
            ProviderError::Config(_)
        ));
    }
}
