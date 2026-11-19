//! Durable state of one logical provider call.

use crate::chat::TokenUsage;

/// Persisted state and optional normalized usage for one provider call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProviderCallAttempt {
    state: ProviderCallAttemptState,
    usage: Option<TokenUsage>,
}

impl ProviderCallAttempt {
    /// Reconstitutes a value read from the tenant-scoped store.
    #[must_use]
    pub const fn from_persisted(
        state: ProviderCallAttemptState,
        usage: Option<TokenUsage>,
    ) -> Self {
        Self { state, usage }
    }

    #[must_use]
    pub const fn state(self) -> ProviderCallAttemptState {
        self.state
    }

    /// None means unavailable; Some(TokenUsage::new(0, 0)) is explicit zero.
    #[must_use]
    pub const fn usage(self) -> Option<TokenUsage> {
        self.usage
    }
}

/// Database-backed state that decides whether a provider request may be sent
/// and whether its token reservation can be safely released.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProviderCallAttemptState {
    /// No network request has been authorized yet.
    NotStarted,
    /// The durable pre-send marker committed; the request may have reached
    /// the provider and must never be sent again automatically.
    MayHaveBeenSent,
    /// A provider response and its assistant message were durably committed.
    ResponseReceived,
    /// The adapter proved that the provider did not accept or execute the call.
    ConfirmedNotAccepted,
    /// The outcome cannot be determined; reservation remains held.
    OutcomeUnknown,
    /// Cancellation was durably recorded before the pre-send marker.
    CancelledBeforeSend,
}

impl ProviderCallAttemptState {
    /// Whether expiry may safely release the associated token reservation.
    #[must_use]
    pub const fn permits_reservation_expiry(self) -> bool {
        matches!(
            self,
            Self::NotStarted | Self::ConfirmedNotAccepted | Self::CancelledBeforeSend
        )
    }

    /// Whether the state proves that a request might have been accepted.
    #[must_use]
    pub const fn may_have_been_accepted(self) -> bool {
        matches!(
            self,
            Self::MayHaveBeenSent | Self::ResponseReceived | Self::OutcomeUnknown
        )
    }
}

#[cfg(test)]
mod tests {
    use super::ProviderCallAttemptState;

    #[test]
    fn missing_usage_is_distinct_from_explicit_zero_usage() {
        use crate::chat::TokenUsage;
        use super::{ProviderCallAttempt, ProviderCallAttemptState};

        let missing = ProviderCallAttempt::from_persisted(
            ProviderCallAttemptState::ResponseReceived,
            None,
        );
        let explicit_zero = ProviderCallAttempt::from_persisted(
            ProviderCallAttemptState::ResponseReceived,
            Some(TokenUsage::new(0, 0)),
        );

        assert_eq!(missing.usage(), None);
        assert_eq!(explicit_zero.usage(), Some(TokenUsage::new(0, 0)));
        assert_ne!(missing, explicit_zero);
    }

    #[test]
    fn only_proven_pre_send_or_non_acceptance_states_allow_expiry() {
        for state in [
            ProviderCallAttemptState::NotStarted,
            ProviderCallAttemptState::ConfirmedNotAccepted,
            ProviderCallAttemptState::CancelledBeforeSend,
        ] {
            assert!(state.permits_reservation_expiry());
        }

        for state in [
            ProviderCallAttemptState::MayHaveBeenSent,
            ProviderCallAttemptState::ResponseReceived,
            ProviderCallAttemptState::OutcomeUnknown,
        ] {
            assert!(!state.permits_reservation_expiry());
            assert!(state.may_have_been_accepted());
        }
    }
}
