//! Provider-neutral token budget calculations for one invocation.

use crate::chat::TokenUsage;

/// Invalid or unrepresentable token budget input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TokenBudgetError {
    ZeroLimit,
    ArithmeticOverflow,
    NoOutputCapacity,
}

/// Outcome of settling a reservation when provider usage may be unavailable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TokenSettlement {
    actual_tokens: u64,
    degraded: bool,
}

impl TokenSettlement {
    #[must_use]
    pub const fn actual_tokens(self) -> u64 {
        self.actual_tokens
    }

    /// Missing provider usage is explicitly marked degraded; it is never zero.
    #[must_use]
    pub const fn is_degraded(self) -> bool {
        self.degraded
    }
}

/// Returns the immutable snapshot cap tightened by the current live object cap.
pub fn effective_token_budget(
    snapshot_limit: u64,
    live_object_limit: u64,
) -> Result<u64, TokenBudgetError> {
    if snapshot_limit == 0 || live_object_limit == 0 {
        return Err(TokenBudgetError::ZeroLimit);
    }
    Ok(snapshot_limit.min(live_object_limit))
}

/// Estimates input tokens using the mock-path contract:
/// ceil(input UTF-8 bytes / 3) * 1.3, rounded upward to a whole token.
///
/// Integer arithmetic avoids floating-point rounding differences.
pub fn estimate_input_tokens(input_utf8_bytes: usize) -> Result<u64, TokenBudgetError> {
    let bytes =
        u64::try_from(input_utf8_bytes).map_err(|_| TokenBudgetError::ArithmeticOverflow)?;
    let base = bytes
        .checked_add(2)
        .ok_or(TokenBudgetError::ArithmeticOverflow)?
        / 3;
    base.checked_mul(13)
        .and_then(|value| value.checked_add(9))
        .map(|value| value / 10)
        .ok_or(TokenBudgetError::ArithmeticOverflow)
}

/// Calculates the provider output allowance after reserving the estimated input.
pub fn output_token_allowance(
    effective_limit: u64,
    estimated_input_tokens: u64,
    configured_output_limit: u32,
) -> Result<u32, TokenBudgetError> {
    if effective_limit == 0 {
        return Err(TokenBudgetError::ZeroLimit);
    }
    let remaining = effective_limit.saturating_sub(estimated_input_tokens);
    let allowance = remaining.min(u64::from(configured_output_limit));
    if allowance == 0 {
        return Err(TokenBudgetError::NoOutputCapacity);
    }
    u32::try_from(allowance).map_err(|_| TokenBudgetError::ArithmeticOverflow)
}

/// Uses normalized provider usage when present; otherwise settles the non-zero
/// reservation estimate and marks the invocation degraded.
#[must_use]
pub const fn settle_token_usage(
    reserved_estimate: u64,
    provider_usage: Option<TokenUsage>,
) -> TokenSettlement {
    match provider_usage {
        Some(usage) => TokenSettlement {
            actual_tokens: usage.input_tokens() as u64 + usage.output_tokens() as u64,
            degraded: false,
        },
        None => TokenSettlement {
            actual_tokens: reserved_estimate,
            degraded: true,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effective_limit_is_the_tighter_snapshot_or_live_limit() {
        assert_eq!(effective_token_budget(2_000, 1_500), Ok(1_500));
        assert_eq!(effective_token_budget(1_000, 1_500), Ok(1_000));
    }

    #[test]
    fn zero_budget_fails_closed() {
        assert_eq!(
            effective_token_budget(0, 1_000),
            Err(TokenBudgetError::ZeroLimit)
        );
        assert_eq!(
            effective_token_budget(1_000, 0),
            Err(TokenBudgetError::ZeroLimit)
        );
    }

    #[test]
    fn mock_input_estimate_uses_utf8_bytes_and_rounds_up() {
        assert_eq!(estimate_input_tokens(0), Ok(0));
        assert_eq!(estimate_input_tokens(1), Ok(2));
        assert_eq!(estimate_input_tokens(3), Ok(2));
        assert_eq!(estimate_input_tokens(4), Ok(3));
        assert_eq!(estimate_input_tokens("я".len()), Ok(2));
    }

    #[test]
    fn output_allowance_never_exceeds_remaining_effective_budget() {
        assert_eq!(output_token_allowance(100, 40, 80), Ok(60));
        assert_eq!(output_token_allowance(100, 90, 80), Ok(10));
        assert_eq!(
            output_token_allowance(100, 100, 80),
            Err(TokenBudgetError::NoOutputCapacity)
        );
    }

    #[test]
    fn provider_usage_replaces_estimate() {
        let settled = settle_token_usage(20, Some(TokenUsage::new(12, 7)));
        assert_eq!(settled.actual_tokens(), 19);
        assert!(!settled.is_degraded());
    }

    #[test]
    fn missing_provider_usage_uses_estimate_and_marks_degraded() {
        let settled = settle_token_usage(20, None);
        assert_eq!(settled.actual_tokens(), 20);
        assert!(settled.is_degraded());
    }
}
