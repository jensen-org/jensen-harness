//! What a session may spend. Prices and caps are config; every amount is an
//! integer, in tokens or in micro dollars, so nothing rounds silently.

pub mod estimate;
pub mod pricing;

use serde::{Deserialize, Serialize};

pub use pricing::{Price, PricingTable};

/// A session with no cap anywhere gets this one. Execution has no human in
/// it, so a runaway loop has to be stopped by default.
pub const DEFAULT_MAX_TOKENS: u64 = 5_000_000;
pub const DEFAULT_SOFT_PERCENT: u8 = 80;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BudgetError {
    #[error("the cost does not fit in a 64 bit amount")]
    Overflow,
    #[error("a cost cap needs a price for {model}, and the pricing table has none")]
    MissingPrice { model: String },
    #[error("{0}")]
    Invalid(String),
}

/// Hard caps. `None` means unset, never zero.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Caps {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_cost_micro_usd: Option<u64>,
}

impl Caps {
    /// # Errors
    /// [`BudgetError::Invalid`] for a cap of zero, which would refuse every call.
    pub fn validated(self) -> Result<Self, BudgetError> {
        if self.max_tokens == Some(0) || self.max_cost_micro_usd == Some(0) {
            return Err(BudgetError::Invalid(
                "a budget cap of zero would refuse every call".to_string(),
            ));
        }
        Ok(self)
    }

    /// `self` wins field by field over `base`.
    #[must_use]
    pub fn over(self, base: Self) -> Self {
        Self {
            max_tokens: self.max_tokens.or(base.max_tokens),
            max_cost_micro_usd: self.max_cost_micro_usd.or(base.max_cost_micro_usd),
        }
    }

    #[must_use]
    pub fn is_unset(&self) -> bool {
        self.max_tokens.is_none() && self.max_cost_micro_usd.is_none()
    }
}

/// The soft threshold of a cap: the amount at which the run is told to wrap up.
#[must_use]
pub fn soft_threshold(cap: u64, soft_percent: u8) -> u64 {
    let scaled = u128::from(cap) * u128::from(soft_percent) / 100;
    u64::try_from(scaled).unwrap_or(cap)
}

/// Everything budget related a daemon is configured with.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BudgetSettings {
    pub pricing: PricingTable,
    pub caps: Caps,
    pub soft_percent: Option<u8>,
}

/// The caps one session runs under, once the request, the config and the
/// built in default have been combined.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionBudget {
    pub caps: Caps,
    pub soft_percent: u8,
    pub price: Option<Price>,
}

impl BudgetSettings {
    /// Precedence: the request, then the config, then the built in token cap.
    ///
    /// # Errors
    /// [`BudgetError::Invalid`] for a zero cap and [`BudgetError::MissingPrice`]
    /// for a cost cap on a model the pricing table does not know.
    pub fn resolve(
        &self,
        requested: Option<Caps>,
        model_address: &str,
    ) -> Result<SessionBudget, BudgetError> {
        let requested = requested.unwrap_or_default().validated()?;
        let mut caps = requested.over(self.caps);
        if caps.is_unset() {
            caps.max_tokens = Some(DEFAULT_MAX_TOKENS);
        }
        let price = self.pricing.price_for(model_address).copied();
        if caps.max_cost_micro_usd.is_some() && price.is_none() {
            return Err(BudgetError::MissingPrice {
                model: model_address.to_string(),
            });
        }
        Ok(SessionBudget {
            caps,
            soft_percent: self.soft_percent.unwrap_or(DEFAULT_SOFT_PERCENT),
            price,
        })
    }
}
