use jensen_harness_core::budget::{
    BudgetError, BudgetSettings, Caps, DEFAULT_MAX_TOKENS, DEFAULT_SOFT_PERCENT, Price,
    soft_threshold,
};
use jensen_harness_core::providers::config::ProviderRegistry;
use jensen_harness_core::providers::{ProviderError, Usage};

const CONFIG: &str = r#"
[providers.anthropic]
transport = "anthropic"
base_url = "https://x"
api_key_env = "K"

[pricing]
version = "2026-10"

[pricing.models."anthropic/priced"]
input = 3000000
output = 15000000
cache_read = 300000
cache_write = 3750000

[budget]
max_tokens = 1000000
soft_percent = 70
"#;

fn settings(text: &str) -> BudgetSettings {
    ProviderRegistry::from_toml(text)
        .unwrap()
        .budget_settings()
        .clone()
}

#[test]
fn a_mixed_usage_costs_what_the_hand_sum_says() {
    let price = Price {
        input: 3_000_000,
        output: 15_000_000,
        cache_read: 300_000,
        cache_write: 3_750_000,
    };
    let usage = Usage {
        input_tokens: 1_000,
        output_tokens: 500,
        cache_read_tokens: 10_000,
        cache_write_tokens: 2_000,
    };
    assert_eq!(price.cost(&usage).unwrap(), 3_000 + 7_500 + 3_000 + 7_500);
}

#[test]
fn a_cost_is_rounded_up_never_down() {
    let price = Price {
        input: 1,
        output: 0,
        cache_read: 0,
        cache_write: 0,
    };
    let usage = Usage {
        input_tokens: 1,
        ..Usage::default()
    };
    assert_eq!(price.cost(&usage).unwrap(), 1);
    assert_eq!(price.cost(&Usage::default()).unwrap(), 0);
}

#[test]
fn arithmetic_that_does_not_fit_is_an_error_not_a_wrap() {
    let price = Price {
        input: u64::MAX,
        output: u64::MAX,
        cache_read: 0,
        cache_write: 0,
    };
    let usage = Usage {
        input_tokens: u64::MAX,
        output_tokens: u64::MAX,
        ..Usage::default()
    };
    assert_eq!(price.cost(&usage), Err(BudgetError::Overflow));
    let huge = Usage {
        input_tokens: u64::MAX,
        output_tokens: 1,
        ..Usage::default()
    };
    assert_eq!(huge.total_tokens(), Err(BudgetError::Overflow));
    assert_eq!(
        Usage {
            input_tokens: 1,
            output_tokens: 2,
            cache_read_tokens: 3,
            cache_write_tokens: 4
        }
        .total_tokens(),
        Ok(10)
    );
}

#[test]
fn the_config_supplies_prices_a_version_and_default_caps() {
    let settings = settings(CONFIG);
    assert_eq!(settings.pricing.version(), "2026-10");
    assert!(settings.pricing.price_for("anthropic/priced").is_some());
    assert_eq!(settings.caps.max_tokens, Some(1_000_000));
    assert_eq!(settings.soft_percent, Some(70));
}

#[test]
fn with_no_cap_anywhere_a_session_gets_the_built_in_token_cap() {
    let budget = BudgetSettings::default()
        .resolve(None, "any/model")
        .unwrap();
    assert_eq!(budget.caps.max_tokens, Some(DEFAULT_MAX_TOKENS));
    assert_eq!(budget.caps.max_cost_micro_usd, None);
    assert_eq!(budget.soft_percent, DEFAULT_SOFT_PERCENT);
}

#[test]
fn a_request_overrides_the_config_field_by_field() {
    let settings = settings(CONFIG);
    let requested = Caps {
        max_tokens: Some(500),
        max_cost_micro_usd: None,
    };
    let budget = settings
        .resolve(Some(requested), "anthropic/priced")
        .unwrap();
    assert_eq!(budget.caps.max_tokens, Some(500));
    let other = Caps {
        max_tokens: None,
        max_cost_micro_usd: Some(9_000),
    };
    let budget = settings.resolve(Some(other), "anthropic/priced").unwrap();
    assert_eq!(budget.caps.max_tokens, Some(1_000_000));
    assert_eq!(budget.caps.max_cost_micro_usd, Some(9_000));
    assert_eq!(budget.soft_percent, 70);
    assert!(budget.price.is_some());
}

#[test]
fn a_cost_cap_on_an_unpriced_model_is_an_error_and_a_token_cap_is_fine() {
    let settings = settings(CONFIG);
    let cost_cap = Caps {
        max_tokens: None,
        max_cost_micro_usd: Some(100),
    };
    assert_eq!(
        settings
            .resolve(Some(cost_cap), "anthropic/unpriced")
            .unwrap_err(),
        BudgetError::MissingPrice {
            model: "anthropic/unpriced".to_string()
        }
    );
    let budget = settings.resolve(None, "anthropic/unpriced").unwrap();
    assert!(budget.price.is_none());
}

#[test]
fn a_zero_cap_is_refused() {
    let zero = Caps {
        max_tokens: Some(0),
        max_cost_micro_usd: None,
    };
    assert!(matches!(
        BudgetSettings::default()
            .resolve(Some(zero), "a/b")
            .unwrap_err(),
        BudgetError::Invalid(_)
    ));
}

#[test]
fn thresholds_scale_without_overflow() {
    assert_eq!(soft_threshold(1_000, 80), 800);
    assert_eq!(
        soft_threshold(u64::MAX, 99),
        u64::MAX / 100 * 99 + (u64::MAX % 100) * 99 / 100
    );
}

#[test]
fn bad_pricing_and_budget_config_is_rejected_loudly() {
    let base = "[providers.anthropic]\ntransport = \"anthropic\"\nbase_url = \"https://x\"\napi_key_env = \"K\"\n";
    let cases = [
        format!(
            "{base}[pricing.models.\"ghost/model\"]\ninput = 1\noutput = 1\ncache_read = 1\ncache_write = 1\n"
        ),
        format!(
            "{base}[pricing.models.\"no-slash\"]\ninput = 1\noutput = 1\ncache_read = 1\ncache_write = 1\n"
        ),
        format!("{base}[pricing.models.\"anthropic/m\"]\ninput = 1\n"),
        format!(
            "{base}[pricing.models.\"anthropic/m\"]\ninput = 1\noutput = 1\ncache_read = 1\ncache_write = 1\nextra = 1\n"
        ),
        format!("{base}[budget]\nsoft_percent = 100\n"),
        format!("{base}[budget]\nsoft_percent = 0\n"),
        format!("{base}[budget]\nmax_tokens = 0\n"),
        format!("{base}[budget]\nmax_dollars = 3\n"),
    ];
    for text in cases {
        assert!(
            matches!(
                ProviderRegistry::from_toml(&text).unwrap_err(),
                ProviderError::Config(_)
            ),
            "{text}"
        );
    }
}
