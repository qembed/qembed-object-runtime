pub mod anonymous_invocation;
//! HTTP handlers.
//!
//! Keep transport handlers limited to qembed-domain contracts and
//! qembed-runtime services. Concrete adapters belong in composition.

pub mod health;
pub mod invocation;
pub mod owner;
pub mod principal_invocation;
