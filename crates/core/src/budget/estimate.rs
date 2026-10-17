//! What a model call is expected to cost before it is made. This is a
//! ceiling, not a bill: it counts the whole request at full price and adds a
//! typical reply, never the provider's `max_tokens`. The provider's own usage
//! settles the real amount afterwards.

use super::{BudgetError, Price};
use crate::compact::tokens;
use crate::providers::{CompletionRequest, ContentBlock, Usage};

/// What a reply is expected to cost at the high end of normal.
pub const OUTPUT_ALLOWANCE_TOKENS: u64 = 4_096;

/// Tokens in the request, counted piece by piece.
#[must_use]
pub fn request_tokens(request: &CompletionRequest) -> u64 {
    let mut total = tokens(&request.system);
    for tool in &request.tools {
        total +=
            tokens(&tool.name) + tokens(&tool.description) + tokens(&tool.input_schema.to_string());
    }
    for turn in &request.turns {
        for block in &turn.content {
            total += match block {
                ContentBlock::Text(text) => tokens(text),
                ContentBlock::ToolUse { name, input, .. } => {
                    tokens(name) + tokens(&input.to_string())
                }
                ContentBlock::ToolResult { content, .. } => tokens(content),
                ContentBlock::Opaque(value) => tokens(&value.to_string()),
            };
        }
    }
    u64::try_from(total).unwrap_or(u64::MAX)
}

/// The tokens and micro dollars to hold for one call.
///
/// # Errors
/// [`BudgetError::Overflow`] when the amounts do not fit in 64 bits.
pub fn reservation(
    request: &CompletionRequest,
    price: Option<&Price>,
) -> Result<(u64, u64), BudgetError> {
    let input = request_tokens(request);
    let tokens = input
        .checked_add(OUTPUT_ALLOWANCE_TOKENS)
        .ok_or(BudgetError::Overflow)?;
    let cost = match price {
        Some(price) => price.cost(&Usage {
            input_tokens: input,
            output_tokens: OUTPUT_ALLOWANCE_TOKENS,
            ..Usage::default()
        })?,
        None => 0,
    };
    Ok((tokens, cost))
}
