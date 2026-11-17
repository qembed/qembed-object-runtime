# Qembed Object Runtime — Rust Starter

This is an intentionally minimal Rust workspace scaffold for implementing
`ROADMAP_RUST_v1.md`.

## Workspace crates

- `qembed-domain`
- `qembed-store`
- `qembed-llm`
- `qembed-tools`
- `qembed-runtime` (also hosts the `qembed-eval` binary target)
- `qembed-api`

No runtime behavior or dependencies have been implemented yet.

## Build and test

```bash
cargo check --workspace
cargo test --workspace
```

## Git

The repository is initialized and contains an initial commit
(`chore: initialize Rust workspace and roadmap`).
