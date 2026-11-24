//! Tenant-scoped Caller deletion job orchestration in Runtime PostgreSQL.
//!
//! This repository manages revocation and cleanup state only. The authoritative
//! deletion tombstone is written separately through ExternalCallerDeletionLedger.

use std::fmt;

use qembed_domain::{ids::CallerId, scope::Scope};
use serde_json::json;
use uuid::Uuid;

use crate::{ContextTransactionError, PgContext};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CallerDeletionJobStatus {
    Pending,
    Running,
    Done,
    Failed,
}

impl CallerDeletionJobStatus {
    fn parse(value: &str) -> Result<Self, CallerDeletionStoreError> {
        match value {
            "Pending" => Ok(Self::Pending),
            "Running" => Ok(Self::Running),
            "Done" => Ok(Self::Done),
            "Failed" => Ok(Self::Failed),
            _ => Err(CallerDeletionStoreError::InvalidStatus),
        }
    }

    fn as_db(self) -> &'static str {
        match self {
            Self::Pending => "Pending",
            Self::Running => "Running",
            Self::Done => "Done",
            Self::Failed => "Failed",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CallerDeletionJob {
    pub id: Uuid,
    pub status: CallerDeletionJobStatus,
}

#[derive(Debug)]
pub enum CallerDeletionStoreError {
    Context(ContextTransactionError),
    Database(sqlx::Error),
    NotFound,
    InvalidStatus,
}

impl fmt::Display for CallerDeletionStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Context(error) => write!(formatter, "Caller deletion context error: {error}"),
            Self::Database(error) => write!(formatter, "Caller deletion database error: {error}"),
            Self::NotFound => formatter.write_str("Caller deletion target was not found"),
            Self::InvalidStatus => formatter.write_str("Caller deletion job has an unknown status"),
        }
    }
}

impl std::error::Error for CallerDeletionStoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Context(error) => Some(error),
            Self::Database(error) => Some(error),
            _ => None,
        }
    }
}

/// Runtime PostgreSQL adapter for the durable deletion job and Caller revocation.
#[derive(Clone)]
pub struct PgCallerDeletionRepository {
    context: PgContext,
}

impl PgCallerDeletionRepository {
    #[must_use]
    pub fn new(context: PgContext) -> Self {
        Self { context }
    }

    /// Revokes the Caller, requests cancellation of its active invocations, and
    /// creates or reuses its durable deletion job. The transaction commits before
    /// the caller contacts the external Control-plane ledger.
    pub async fn request(
        &self,
        scope: &Scope,
        caller_id: CallerId,
    ) -> Result<CallerDeletionJob, CallerDeletionStoreError> {
        let mut transaction = self
            .context
            .begin_principal(scope)
            .await
            .map_err(CallerDeletionStoreError::Context)?;
        let tenant_id = *scope.tenant_id().as_uuid();
        let caller_uuid = *caller_id.as_uuid();

        let caller_exists = sqlx::query_scalar::<_, Uuid>(
            "SELECT id FROM callers WHERE tenant_id = $1 AND id = $2 FOR UPDATE",
        )
        .bind(tenant_id)
        .bind(caller_uuid)
        .fetch_optional(transaction.connection())
        .await
        .map_err(CallerDeletionStoreError::Database)?
        .is_some();

        if !caller_exists {
            return Err(CallerDeletionStoreError::NotFound);
        }

        let revoked =
            sqlx::query_scalar::<_, bool>("SELECT qembed_crypto.request_caller_deletion($1, $2)")
                .bind(tenant_id)
                .bind(caller_uuid)
                .fetch_one(transaction.connection())
                .await
                .map_err(CallerDeletionStoreError::Database)?;

        if !revoked {
            return Err(CallerDeletionStoreError::NotFound);
        }

        let existing = sqlx::query_as::<_, (Uuid, String)>(
            "SELECT id, status FROM deletion_jobs \
             WHERE tenant_id = $1 AND caller_id = $2 \
             ORDER BY requested_at DESC LIMIT 1 FOR UPDATE",
        )
        .bind(tenant_id)
        .bind(caller_uuid)
        .fetch_optional(transaction.connection())
        .await
        .map_err(CallerDeletionStoreError::Database)?;

        let job = match existing {
            Some((id, status)) => {
                let status = CallerDeletionJobStatus::parse(&status)?;
                if matches!(
                    status,
                    CallerDeletionJobStatus::Pending
                        | CallerDeletionJobStatus::Running
                        | CallerDeletionJobStatus::Done
                ) {
                    CallerDeletionJob { id, status }
                } else {
                    self.insert_job(&mut transaction, tenant_id, caller_uuid)
                        .await?
                }
            }
            None => {
                self.insert_job(&mut transaction, tenant_id, caller_uuid)
                    .await?
            }
        };

        transaction
            .commit()
            .await
            .map_err(CallerDeletionStoreError::Database)?;
        Ok(job)
    }

    async fn insert_job(
        &self,
        transaction: &mut crate::TenantTransaction,
        tenant_id: Uuid,
        caller_id: Uuid,
    ) -> Result<CallerDeletionJob, CallerDeletionStoreError> {
        let id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO deletion_jobs (id, tenant_id, caller_id, status) \
             VALUES ($1, $2, $3, 'Pending')",
        )
        .bind(id)
        .bind(tenant_id)
        .bind(caller_id)
        .execute(transaction.connection())
        .await
        .map_err(CallerDeletionStoreError::Database)?;

        Ok(CallerDeletionJob {
            id,
            status: CallerDeletionJobStatus::Pending,
        })
    }

    /// Replays physical cleanup after the external tombstone is durably committed.
    /// A false SQL result means active work is still draining; the job remains
    /// Running and a later DELETE retry can safely resume it.
    pub async fn process(
        &self,
        scope: &Scope,
        caller_id: CallerId,
        job_id: Uuid,
        control_plane_sequence_id: i64,
    ) -> Result<CallerDeletionJob, CallerDeletionStoreError> {
        let mut transaction = self
            .context
            .begin_principal(scope)
            .await
            .map_err(CallerDeletionStoreError::Context)?;
        let tenant_id = *scope.tenant_id().as_uuid();
        let caller_uuid = *caller_id.as_uuid();

        let existing_status = sqlx::query_scalar::<_, String>(
            "SELECT status FROM deletion_jobs \
             WHERE tenant_id = $1 AND caller_id = $2 AND id = $3 FOR UPDATE",
        )
        .bind(tenant_id)
        .bind(caller_uuid)
        .bind(job_id)
        .fetch_optional(transaction.connection())
        .await
        .map_err(CallerDeletionStoreError::Database)?;

        let Some(existing_status) = existing_status else {
            return Err(CallerDeletionStoreError::NotFound);
        };
        let existing_status = CallerDeletionJobStatus::parse(&existing_status)?;
        if existing_status == CallerDeletionJobStatus::Done {
            transaction
                .commit()
                .await
                .map_err(CallerDeletionStoreError::Database)?;
            return Ok(CallerDeletionJob {
                id: job_id,
                status: CallerDeletionJobStatus::Done,
            });
        }

        let complete =
            sqlx::query_scalar::<_, bool>("SELECT qembed_crypto.erase_caller_data($1, $2)")
                .bind(tenant_id)
                .bind(caller_uuid)
                .fetch_one(transaction.connection())
                .await
                .map_err(CallerDeletionStoreError::Database)?;

        let status = if complete {
            CallerDeletionJobStatus::Done
        } else {
            CallerDeletionJobStatus::Running
        };
        let progress = json!({
            "control_plane_sequence_id": control_plane_sequence_id,
            "erasure_complete": complete
        });

        sqlx::query(
            "UPDATE deletion_jobs \
             SET status = $4, progress = $5, \
                 finished_at = CASE WHEN $4 = 'Done' THEN clock_timestamp() ELSE NULL END \
             WHERE tenant_id = $1 AND caller_id = $2 AND id = $3",
        )
        .bind(tenant_id)
        .bind(caller_uuid)
        .bind(job_id)
        .bind(status.as_db())
        .bind(progress)
        .execute(transaction.connection())
        .await
        .map_err(CallerDeletionStoreError::Database)?;

        transaction
            .commit()
            .await
            .map_err(CallerDeletionStoreError::Database)?;

        Ok(CallerDeletionJob { id: job_id, status })
    }
}
