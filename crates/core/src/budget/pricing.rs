use std::collections::BTreeMap;

use serde::Deserialize;

use super::BudgetError;
use crate::providers::Usage;

const TOKENS_PER_MILLION: u128 = 1_000_000;

/// Micro dollars per million tokens, per bucket.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Price {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
}

impl Price {
    /// The cost of `usage` in micro dollars, rounded up so an estimate is
    /// never under the bill.
    ///
    /// # Errors
    /// [`BudgetError::Overflow`] when the amount does not fit in 64 bits.
    pub fn cost(&self, usage: &Usage) -> Result<u64, BudgetError> {
        let buckets = [
            (usage.input_tokens, self.input),
            (usage.output_tokens, self.output),
            (usage.cache_read_tokens, self.cache_read),
            (usage.cache_write_tokens, self.cache_write),
        ];
        let mut scaled: u128 = 0;
        for (tokens, price) in buckets {
            scaled = u128::from(tokens)
                .checked_mul(u128::from(price))
                .and_then(|part| scaled.checked_add(part))
                .ok_or(BudgetError::Overflow)?;
        }
        let micros = scaled.div_ceil(TOKENS_PER_MILLION);
        u64::try_from(micros).map_err(|_too_large| BudgetError::Overflow)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PricingTable {
    #[serde(default)]
    version: String,
    #[serde(default)]
    models: BTreeMap<String, Price>,
}

impl PricingTable {
    #[must_use]
    pub fn version(&self) -> &str {
        &self.version
    }

    #[must_use]
    pub fn price_for(&self, model_address: &str) -> Option<&Price> {
        self.models.get(model_address)
    }

    pub fn addresses(&self) -> impl Iterator<Item = &str> {
        self.models.keys().map(String::as_str)
    }
}

impl Usage {
    /// All four buckets summed; what a token cap counts.
    ///
    /// # Errors
    /// [`BudgetError::Overflow`] when the sum does not fit in 64 bits.
    pub fn total_tokens(&self) -> Result<u64, BudgetError> {
        [
            self.input_tokens,
            self.output_tokens,
            self.cache_read_tokens,
            self.cache_write_tokens,
        ]
        .into_iter()
        .try_fold(0u64, u64::checked_add)
        .ok_or(BudgetError::Overflow)
    }
}
