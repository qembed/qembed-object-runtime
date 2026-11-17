# Store migrations

SQLx migrations for this crate belong in this directory and use the standard
`<VERSION>_<description>.sql` naming convention.

The migration runner is explicit: connecting to PostgreSQL never runs migrations
as a side effect.

The owner-authentication migration persists tenant-bound owner principals and
SHA-256 digests of high-entropy API tokens. Raw tokens are never stored. The
digest is appropriate for random API credentials, not for human passwords;
password authentication would require a dedicated password-hashing scheme.
