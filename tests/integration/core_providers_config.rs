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

#[test]
fn a_role_names_a_declared_provider_and_model() {
    let with_roles = format!(
        "{CONFIG}\n[roles]\nimplement = \"openai/gpt-x\"\nreview = \"gateway/vendor/model\"\n"
    );
    let registry = ProviderRegistry::from_toml(&with_roles).unwrap();
    assert_eq!(registry.role("implement"), Some("openai/gpt-x"));
    assert_eq!(registry.role("review"), Some("gateway/vendor/model"));
    assert_eq!(registry.role("missing"), None);
}

#[test]
fn a_role_bound_to_an_undeclared_provider_is_refused() {
    let broken = format!("{CONFIG}\n[roles]\nimplement = \"ghost/model\"\n");
    let error = ProviderRegistry::from_toml(&broken).unwrap_err();
    assert!(matches!(error, ProviderError::Config(message) if message.contains("implement")));
}

#[test]
fn forges_take_a_token_source_and_an_optional_base_url() {
    let config = format!(
        "{CONFIG}\n[forges.github]\napi_key_keychain = {{}}\n\n[forges.gitlab]\nbase_url = \"https://git.corp.example\"\napi_key_env = \"GL\"\n"
    );
    let registry = ProviderRegistry::from_toml(&config).unwrap();
    assert_eq!(
        registry.keychain_location("github").unwrap(),
        ("jensen-harness".to_owned(), "forge-github".to_owned())
    );
    assert_eq!(
        registry.forge("gitlab").unwrap().base_url.as_deref(),
        Some("https://git.corp.example")
    );
    let secrets = Secrets::with_env(&[("GL", "token-value")]);
    assert_eq!(
        registry.forge_token("gitlab", &secrets).unwrap(),
        "token-value"
    );
    assert!(matches!(
        registry.forge_token("github", &secrets),
        Err(ProviderError::MissingApiKey { .. })
    ));
}

#[test]
fn a_forge_must_be_github_or_gitlab_with_exactly_one_token_source() {
    let unknown = format!("{CONFIG}\n[forges.bitbucket]\napi_key_env = \"X\"\n");
    assert!(matches!(
        ProviderRegistry::from_toml(&unknown),
        Err(ProviderError::Config(_))
    ));
    let both = format!("{CONFIG}\n[forges.github]\napi_key_env = \"X\"\napi_key_keychain = {{}}\n");
    assert!(matches!(
        ProviderRegistry::from_toml(&both),
        Err(ProviderError::Config(_))
    ));
    let none = format!("{CONFIG}\n[forges.github]\n");
    assert!(matches!(
        ProviderRegistry::from_toml(&none),
        Err(ProviderError::Config(_))
    ));
}

#[test]
fn language_servers_are_declared_with_a_language_and_a_closed_network_by_default() {
    let config = format!(
        "{CONFIG}\n[lsp.servers.rust]\ncommand = \"rust-analyzer\"\nlanguage = \"rust\"\nenv_allow = [\"RUSTUP_HOME\"]\ntimeout_secs = 120\n"
    );
    let registry = ProviderRegistry::from_toml(&config).unwrap();
    let server = registry.lsp_server("rust").unwrap();
    assert_eq!(server.command, "rust-analyzer");
    assert_eq!(server.language, "rust");
    assert_eq!(server.timeout_secs, Some(120));
    assert!(registry.lsp_server("go").is_none());
    let missing = format!("{CONFIG}\n[lsp.servers.go]\ncommand = \"gopls\"\n");
    assert!(
        ProviderRegistry::from_toml(&missing).is_err(),
        "a language is required"
    );
}
