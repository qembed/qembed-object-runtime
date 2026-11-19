//! HTTP/API boundary and application composition root.
//!
//! Concrete adapters are wired in the composition module. Handler modules are
//! kept separate and may depend only on domain contracts and runtime services.

pub mod composition;
pub mod errors;
pub mod handlers;
