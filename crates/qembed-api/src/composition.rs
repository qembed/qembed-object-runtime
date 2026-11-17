//! Application composition root.
//!
//! This is the only API-layer module intended to construct and wire concrete
//! adapters (store, LLM, and tools) with the runtime. The actual constructors
//! will be added alongside the corresponding ports and adapters in later
//! roadmap tasks. HTTP handlers must not import these adapter crates directly.
