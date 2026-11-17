# Store migrations

SQLx migrations for this crate belong in this directory and use the standard
`<VERSION>_<description>.sql` naming convention.

The migration runner is wired here before the first application schema
migration is introduced. This keeps database schema changes separate from
ordinary pool creation and application startup.
