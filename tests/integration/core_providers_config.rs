use std::collections::HashMap;

use jensen_harness_core::providers::ProviderError;
use jensen_harness_core::providers::config::ProviderRegistry;

const CONFIG: &str = r#"
default_model = "anthropic/claude-sonnet-5-5"

[providers.anthropic]
transport = "anthropic"
base_url = "https://api.anthropic.com"
api_key_env = "ANTHROPIC_API_KEY"

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

fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
    let map: HashMap<String, String> = pairs
        .iter()
        .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
        .collect();
    move |name| map.get(name).cloned()
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
    let lookup = env(&[("GATEWAY_API_KEY", "k")]);

    let (provider, model) = registry
        .resolve("gateway/anthropic/claude-sonnet-5-5", &lookup)
        .unwrap();

    assert_eq!(provider.id(), "gateway");
    assert_eq!(model, "anthropic/claude-sonnet-5-5");
}

#[test]
fn each_transport_resolves_to_a_provider_with_its_own_id() {
    let registry = ProviderRegistry::from_toml(CONFIG).unwrap();
    let lookup = env(&[
        ("ANTHROPIC_API_KEY", "a"),
        ("OPENAI_API_KEY", "o"),
        ("DEEPSEEK_API_KEY", "d"),
    ]);
    for address in ["anthropic/m", "openai/m", "deepseek/m"] {
        let (provider, _model) = registry.resolve(address, &lookup).unwrap();
        assert_eq!(provider.id(), address.split('/').next().unwrap());
    }
}

#[test]
fn an_unset_key_variable_fails_loudly_naming_it() {
    let registry = ProviderRegistry::from_toml(CONFIG).unwrap();
    let error = registry.resolve("openai/gpt", &env(&[])).unwrap_err();
    let ProviderError::MissingApiKey { env_var, .. } = error else {
        panic!("expected a missing key error");
    };
    assert_eq!(env_var, "OPENAI_API_KEY");
}

#[test]
fn an_unknown_provider_and_a_bare_model_are_errors() {
    let registry = ProviderRegistry::from_toml(CONFIG).unwrap();
    assert!(matches!(
        registry.resolve("nope/m", &env(&[])).unwrap_err(),
        ProviderError::UnknownProvider(_)
    ));
    assert!(matches!(
        registry.resolve("just-a-model", &env(&[])).unwrap_err(),
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
