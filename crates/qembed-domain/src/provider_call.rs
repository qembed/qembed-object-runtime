//! Durable provider-call outcome state machine.
//!
//! A provider attempt is separate from invocation lifecycle: invocation lease
//! ownership fences local writes, while this state records whether an external
//! provider request may have escaped the process. An ambiguous outcome is never
//! eligible for automatic resend or reservation release.

use std::fmt;

/// Durable state of one logical provider call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProviderCallState {
    /// No provider network I/O has been authorized yet.
    NotStarted,
    /// The durable pre-send marker committed; the request may have escaped.
    MayHaveBeenSent,
    /// A normalized response and its durable assistant message were committed.
    ResponseReceived,
    /// The adapter has provider-contract evidence that no billable execution occurred.
    ConfirmedNotAccepted,
    /// The process cannot prove whether the provider accepted or executed the request.
    OutcomeUnknown,
    /// Cancellation committed before the pre-send marker.
    CancelledBeforeSend,
}

impl ProviderCallState {
    /// Parses the canonical PostgreSQL state spelling.
    pub fn parse(value: &str) -> Result<Self, InvalidProviderCallState> {
        match value {
            "NotStarted" => Ok(Self::NotStarted),
            "MayHaveBeenSent" => Ok(Self::MayHaveBeenSent),
            "ResponseReceived" => Ok(Self::ResponseReceived),
            "ConfirmedNotAccepted" => Ok(Self::ConfirmedNotAccepted),
            "OutcomeUnknown" => Ok(Self::OutcomeUnknown),
            "CancelledBeforeSend" => Ok(Self::CancelledBeforeSend),
            _ => Err(InvalidProviderCallState),
        }
    }

    /// Returns the canonical PostgreSQL state spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NotStarted => "NotStarted",
            Self::MayHaveBeenSent => "MayHaveBeenSent",
            Self::ResponseReceived => "ResponseReceived",
            Self::ConfirmedNotAccepted => "ConfirmedNotAccepted",
            Self::OutcomeUnknown => "OutcomeUnknown",
            Self::CancelledBeforeSend => "CancelledBeforeSend",
        }
    }

    /// Whether the attempt may be safely expired without provider reconciliation.
    #[must_use]
    pub const fn is_expiry_safe(self) -> bool {
        matches!(
            self,
            Self::NotStarted | Self::ConfirmedNotAccepted | Self::CancelledBeforeSend
        )
    }

    /// Whether another provider request must be forbidden.
    #[must_use]
    pub const fn forbids_resend(self) -> bool {
        matches!(
            self,
            Self::MayHaveBeenSent
                | Self::ResponseReceived
                | Self::ConfirmedNotAccepted
                | Self::OutcomeUnknown
                | Self::CancelledBeforeSend
        )
    }

    /// Checks the monotonic transition contract mirrored by PostgreSQL.
    #[must_use]
    pub const fn can_transition_to(self, next: Self) -> bool {
        matches!(
            (self, next),
            (Self::NotStarted, Self::MayHaveBeenSent)
                | (Self::NotStarted, Self::CancelledBeforeSend)
                | (Self::MayHaveBeenSent, Self::ResponseReceived)
                | (Self::MayHaveBeenSent, Self::ConfirmedNotAccepted)
                | (Self::MayHaveBeenSent, Self::OutcomeUnknown)
                | (Self::OutcomeUnknown, Self::ResponseReceived)
                | (Self::OutcomeUnknown, Self::ConfirmedNotAccepted)
        )
    }
}

/// An unknown or unsupported persisted provider-call state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InvalidProviderCallState;

impl fmt::Display for InvalidProviderCallState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("persisted provider call state is invalid")
    }
}

impl std::error::Error for InvalidProviderCallState {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expiry_is_limited_to_states_that_prove_no_unresolved_external_effect() {
        assert!(ProviderCallState::NotStarted.is_expiry_safe());
        assert!(ProviderCallState::ConfirmedNotAccepted.is_expiry_safe());
        assert!(ProviderCallState::CancelledBeforeSend.is_expiry_safe());
        assert!(!ProviderCallState::MayHaveBeenSent.is_expiry_safe());
        assert!(!ProviderCallState::ResponseReceived.is_expiry_safe());
        assert!(!ProviderCallState::OutcomeUnknown.is_expiry_safe());
    }

    #[test]
    fn ambiguous_outcome_can_only_reconcile_to_response_or_confirmed_rejection() {
        assert!(ProviderCallState::OutcomeUnknown
            .can_transition_to(ProviderCallState::ResponseReceived));
        assert!(ProviderCallState::OutcomeUnknown
            .can_transition_to(ProviderCallState::ConfirmedNotAccepted));
        assert!(!ProviderCallState::OutcomeUnknown
            .can_transition_to(ProviderCallState::NotStarted));
        assert!(!ProviderCallState::OutcomeUnknown
            .can_transition_to(ProviderCallState::MayHaveBeenSent));
    }

    #[test]
    fn canonical_state_names_round_trip() {
        for state in [
            ProviderCallState::NotStarted,
            ProviderCallState::MayHaveBeenSent,
            ProviderCallState::ResponseReceived,
            ProviderCallState::ConfirmedNotAccepted,
            ProviderCallState::OutcomeUnknown,
            ProviderCallState::CancelledBeforeSend,
        ] {
            assert_eq!(ProviderCallState::parse(state.as_str()), Ok(state));
        }
        assert!(ProviderCallState::parse("unknown").is_err());
    }
}
