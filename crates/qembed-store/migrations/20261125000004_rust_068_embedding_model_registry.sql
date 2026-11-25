-- RUST-068: the global embedding registry is managed only by the platform admin role.
-- Runtime remains read-only; RUST-066 and RUST-067 consume the active projection.
REVOKE INSERT, UPDATE, DELETE ON embedding_models FROM qembed_runtime;
GRANT SELECT, INSERT, UPDATE ON embedding_models TO qembed_admin;
