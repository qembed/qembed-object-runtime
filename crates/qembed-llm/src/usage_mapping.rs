//! Normalization of provider token-usage counters.
//!
//! Provider SDKs commonly use wider integer counters or omit usage fields.
//! The domain contract requires both counters as u32, so incomplete usage is
//! represented as unknown rather than filling missing values with zero.

use qembed_domain::chat::TokenUsage;

/// Failure while converting provider counters to the domain representation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UsageMappingError {
    CounterOutOfRange,
}

impl std::fmt::Display for UsageMappingError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CounterOutOfRange => {
                formatter.write_str("provider token usage exceeds the domain counter range")
            }
        }
    }
}

impl std::error::Error for UsageMappingError {}

/// Normalizes optional provider counters into domain usage.
///
/// Returns `None` when either counter is absent. A missing counter is not
/// interpreted as zero because doing so would under-report consumption.
/// Values outside the domain's u32 range are rejected instead of truncated.
pub fn normalize_usage(
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
) -> Result<Option<TokenUsage>, UsageMappingError> {
    let (Some(input_tokens), Some(output_tokens)) = (input_tokens, output_tokens) else {
        return Ok(None);
    };

    let input_tokens =
        u32::try_from(input_tokens).map_err(|_| UsageMappingError::CounterOutOfRange)?;
    let output_tokens =
        u32::try_from(output_tokens).map_err(|_| UsageMappingError::CounterOutOfRange)?;

    Ok(Some(TokenUsage::new(input_tokens, output_tokens)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_complete_usage_without_loss() {
        assert_eq!(
            normalize_usage(Some(1_234), Some(567)),
            Ok(Some(TokenUsage::new(1_234, 567)))
        );
    }

    #[test]
    fn absent_or_partial_usage_remains_unknown() {
        assert_eq!(normalize_usage(None, None), Ok(None));
        assert_eq!(normalize_usage(Some(123), None), Ok(None));
        assert_eq!(normalize_usage(None, Some(45)), Ok(None));
    }

    #[test]
    fn rejects_counters_that_cannot_fit_domain_types() {
        assert_eq!(
            normalize_usage(Some(u64::from(u32::MAX) + 1), Some(1)),
            Err(UsageMappingError::CounterOutOfRange)
        );
        assert_eq!(
            normalize_usage(Some(1), Some(u64::from(u32::MAX) + 1)),
            Err(UsageMappingError::CounterOutOfRange)
        );
    }

    #[test]
    fn accepts_maximum_domain_counter() {
        assert_eq!(
            normalize_usage(Some(u64::from(u32::MAX)), Some(0)),
            Ok(Some(TokenUsage::new(u32::MAX, 0)))
        );
    }
}
