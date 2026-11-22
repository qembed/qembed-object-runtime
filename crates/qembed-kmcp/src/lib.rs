//! Provider-neutral policy core for the RT-050 Key Management Control Plane.
//!
//! This crate contains deterministic lifecycle policy. It deliberately has no
//! HTTP, PostgreSQL, OIDC, KMS, HSM, filesystem, or network dependency.
//!
//! The caller must obtain authenticated operator/approver identities from a
//! trusted authorization adapter and persist each accepted decision using an
//! atomic compare-and-set operation that commits the registry change, immutable
//! audit event, and audit-outbox row together. This crate does not authenticate
//! identities or persist state.
//!
//! See `docs/RT-050_KMCP_CORE.md` for the trust boundaries and integration
//! contract.

#![forbid(unsafe_code)]

pub mod lifecycle;

pub use lifecycle::{
    LifecycleAction, LifecycleDecision, LifecycleError, LifecycleRequest,
    LifecycleState, plan_transition,
};
