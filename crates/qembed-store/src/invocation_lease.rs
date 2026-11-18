//! PostgreSQL lease acquisition, heartbeat, reclaim, and owner-fenced writes.

use std::str::FromStr;

use qembed_domain::ids::{CallerId, InvocationId, MessageId, ObjectId, SnapshotId, TenantId};
use qembed_domain::invocation::{FencingToken, Invocation, InvocationCheckpoint, InvocationStatus};
use qembed_domain::invocation_lease::InvocationLeaseStore;
use qembed_domain::scope::Scope;

use crate::invocation_worker::{InvocationStoreError, PgExecutionStore};
use crate::{begin_tenant_transaction, execute, fetch_optional};

type InvocationRow = (String, String, String, String, String, String, i64, i64);

impl InvocationLeaseStore for PgExecutionStore {
    async fn claim_invocation(
        &self,
        scope: &Scope,
        invocation_id: InvocationId,
        owner: &str,
        lease_seconds: u32,
    ) -> Result<Option<Invocation>, Self::Error> {
        validate_lease_request(owner, lease_seconds)?;
        let mut transaction = begin_tenant_transaction(&self.pool, scope).await?;
        let row = fetch_optional(
            &mut transaction,
            sqlx::query_as::<_, InvocationRow>(
                "UPDATE object_invocations SET lease_owner=$3, lease_until=now()+make_interval(secs => $4::double precision), heartbeat_at=now(), fencing_token=CASE WHEN lease_owner IS NULL THEN fencing_token ELSE fencing_token+1 END, version=CASE WHEN lease_owner IS NULL THEN version ELSE version+1 END WHERE tenant_id=$1::uuid AND id=$2::uuid AND status NOT IN ('Completed','CompletedDegraded','Failed','Cancelled') AND ((lease_owner IS NULL AND lease_until IS NULL) OR lease_until < now()) RETURNING id::text, tenant_id::text, object_id::text, caller_id::text, snapshot_id::text, status, version, fencing_token",
            )
            .bind(scope.tenant_id().to_string())
            .bind(invocation_id.to_string())
            .bind(owner)
            .bind(f64::from(lease_seconds)),
        )
        .await?;
        let invocation = row.map(reconstitute).transpose()?;
        transaction.commit().await?;
        Ok(invocation)
    }

    async fn heartbeat_invocation(
        &self,
        scope: &Scope,
        invocation_id: InvocationId,
        owner: &str,
        fencing_token: FencingToken,
        lease_seconds: u32,
    ) -> Result<bool, Self::Error> {
        validate_lease_request(owner, lease_seconds)?;
        let mut transaction = begin_tenant_transaction(&self.pool, scope).await?;
        let result = execute(
            &mut transaction,
            sqlx::query(
                "UPDATE object_invocations SET lease_until=now()+make_interval(secs => $1::double precision), heartbeat_at=now() WHERE tenant_id=$2::uuid AND id=$3::uuid AND lease_owner=$4 AND lease_until > now() AND fencing_token=$5 AND status NOT IN ('Completed','CompletedDegraded','Failed','Cancelled')",
            )
            .bind(f64::from(lease_seconds))
            .bind(scope.tenant_id().to_string())
            .bind(invocation_id.to_string())
            .bind(owner)
            .bind(to_i64(fencing_token.value(), "fencing token")?),
        )
        .await?;
        transaction.commit().await?;
        Ok(result.rows_affected() == 1)
    }

    async fn compare_and_set_owned(
        &self,
        scope: &Scope,
        owner: &str,
        expected_version: u64,
        expected_fencing_token: FencingToken,
        next: Invocation,
    ) -> Result<bool, Self::Error> {
        validate_next(scope, expected_version, expected_fencing_token, &next)?;
        if next.status().is_terminal() {
            return Err(InvocationStoreError::InvalidRequestedState(
                "terminal transitions require a dedicated atomic result operation",
            ));
        }
        let mut transaction = begin_tenant_transaction(&self.pool, scope).await?;
        let result = execute(
            &mut transaction,
            sqlx::query(
                "UPDATE object_invocations SET status=$1, version=$2 WHERE tenant_id=$3::uuid AND object_id=$4::uuid AND id=$5::uuid AND lease_owner=$6 AND lease_until > now() AND version=$7 AND fencing_token=$8 AND status NOT IN ('Completed','CompletedDegraded','Failed','Cancelled')",
            )
            .bind(status_name(next.status()))
            .bind(to_i64(next.version(), "next version")?)
            .bind(scope.tenant_id().to_string())
            .bind(next.object_id().to_string())
            .bind(next.id().to_string())
            .bind(owner)
            .bind(to_i64(expected_version, "expected version")?)
            .bind(to_i64(expected_fencing_token.value(), "fencing token")?),
        )
        .await?;
        transaction.commit().await?;
        Ok(result.rows_affected() == 1)
    }

    async fn persist_assistant_output_owned(
        &self,
        scope: &Scope,
        owner: &str,
        expected_version: u64,
        expected_fencing_token: FencingToken,
        next: Invocation,
        content: String,
    ) -> Result<bool, Self::Error> {
        validate_next(scope, expected_version, expected_fencing_token, &next)?;
        if next.status() != InvocationStatus::Verifying {
            return Err(InvocationStoreError::InvalidRequestedState(
                "assistant output must move Processing to Verifying",
            ));
        }
        if content.trim().is_empty() || content.len() > 1_048_576 {
            return Err(InvocationStoreError::InvalidRequestedState(
                "assistant output is empty or too large",
            ));
        }

        let mut transaction = begin_tenant_transaction(&self.pool, scope).await?;
        let inserted = execute(
            &mut transaction,
            sqlx::query(
                "INSERT INTO interaction_messages (id, tenant_id, object_id, caller_id, invocation_id, role, content, expires_at) SELECT $7::uuid, invocation.tenant_id, invocation.object_id, invocation.caller_id, invocation.id, 'assistant', $8, input.expires_at FROM object_invocations AS invocation JOIN LATERAL (SELECT expires_at FROM interaction_messages WHERE tenant_id=invocation.tenant_id AND invocation_id=invocation.id AND role='user' ORDER BY created_at, id LIMIT 1) AS input ON TRUE WHERE invocation.tenant_id=$1::uuid AND invocation.object_id=$2::uuid AND invocation.id=$3::uuid AND invocation.lease_owner=$4 AND invocation.lease_until > now() AND invocation.version=$5 AND invocation.fencing_token=$6 AND invocation.status='Processing' AND NOT EXISTS (SELECT 1 FROM interaction_messages WHERE tenant_id=invocation.tenant_id AND invocation_id=invocation.id AND role='assistant')",
            )
            .bind(scope.tenant_id().to_string())
            .bind(next.object_id().to_string())
            .bind(next.id().to_string())
            .bind(owner)
            .bind(to_i64(expected_version, "expected version")?)
            .bind(to_i64(expected_fencing_token.value(), "fencing token")?)
            .bind(MessageId::new().to_string())
            .bind(content),
        )
        .await?;
        if inserted.rows_affected() != 1 {
            transaction.rollback().await?;
            return Ok(false);
        }

        let transitioned = execute(
            &mut transaction,
            sqlx::query(
                "UPDATE object_invocations SET status='Verifying', version=version+1 WHERE tenant_id=$1::uuid AND object_id=$2::uuid AND id=$3::uuid AND lease_owner=$4 AND lease_until > now() AND version=$5 AND fencing_token=$6 AND status='Processing'",
            )
            .bind(scope.tenant_id().to_string())
            .bind(next.object_id().to_string())
            .bind(next.id().to_string())
            .bind(owner)
            .bind(to_i64(expected_version, "expected version")?)
            .bind(to_i64(expected_fencing_token.value(), "fencing token")?),
        )
        .await?;
        if transitioned.rows_affected() != 1 {
            transaction.rollback().await?;
            return Ok(false);
        }
        transaction.commit().await?;
        Ok(true)
    }

    async fn complete_invocation_owned(
        &self,
        scope: &Scope,
        owner: &str,
        expected_version: u64,
        expected_fencing_token: FencingToken,
        next: Invocation,
    ) -> Result<bool, Self::Error> {
        validate_next(scope, expected_version, expected_fencing_token, &next)?;
        if next.status() != InvocationStatus::Completed {
            return Err(InvocationStoreError::InvalidRequestedState(
                "completion must transition to Completed",
            ));
        }
        let mut transaction = begin_tenant_transaction(&self.pool, scope).await?;

        // The canonical SQL trigger permits release of an expired lease on a
        // terminal transition, but rejects changing the owner of a live lease.
        // Expire it and release it in two updates under the same row lock and
        // transaction so no competing worker can observe the intermediate row.
        let expired = execute(
            &mut transaction,
            sqlx::query(
                "UPDATE object_invocations SET lease_until=now()-interval '1 microsecond' WHERE tenant_id=$1::uuid AND object_id=$2::uuid AND id=$3::uuid AND lease_owner=$4 AND lease_until > now() AND version=$5 AND fencing_token=$6 AND status='Verifying'",
            )
            .bind(scope.tenant_id().to_string())
            .bind(next.object_id().to_string())
            .bind(next.id().to_string())
            .bind(owner)
            .bind(to_i64(expected_version, "expected version")?)
            .bind(to_i64(expected_fencing_token.value(), "fencing token")?),
        )
        .await?;
        if expired.rows_affected() != 1 {
            transaction.rollback().await?;
            return Ok(false);
        }

        let result = execute(
            &mut transaction,
            sqlx::query(
                "UPDATE object_invocations SET status='Completed', finished_at=now(), version=version+1, lease_owner=NULL, lease_until=NULL WHERE tenant_id=$1::uuid AND object_id=$2::uuid AND id=$3::uuid AND lease_owner=$4 AND lease_until <= now() AND version=$5 AND fencing_token=$6 AND status='Verifying' AND EXISTS (SELECT 1 FROM interaction_messages WHERE tenant_id=$1::uuid AND object_id=$2::uuid AND invocation_id=$3::uuid AND role='assistant')",
            )
            .bind(scope.tenant_id().to_string())
            .bind(next.object_id().to_string())
            .bind(next.id().to_string())
            .bind(owner)
            .bind(to_i64(expected_version, "expected version")?)
            .bind(to_i64(expected_fencing_token.value(), "fencing token")?),
        )
        .await?;
        if result.rows_affected() != 1 {
            transaction.rollback().await?;
            return Ok(false);
        }
        transaction.commit().await?;
        Ok(true)
    }

    async fn fail_invocation_owned(
        &self,
        scope: &Scope,
        owner: &str,
        expected_version: u64,
        expected_fencing_token: FencingToken,
        next: Invocation,
        error_code: &'static str,
    ) -> Result<bool, Self::Error> {
        validate_next(scope, expected_version, expected_fencing_token, &next)?;
        if next.status() != InvocationStatus::Failed
            || !matches!(
                error_code,
                "provider_timeout"
                    | "provider_rate_limited"
                    | "provider_unavailable"
                    | "provider_malformed_output"
                    | "provider_other"
            )
        {
            return Err(InvocationStoreError::InvalidRequestedState(
                "failure must use a stable allowlisted provider error code",
            ));
        }
        let mut transaction = begin_tenant_transaction(&self.pool, scope).await?;

        let expired = execute(
            &mut transaction,
            sqlx::query(
                "UPDATE object_invocations SET lease_until=now()-interval '1 microsecond' WHERE tenant_id=$1::uuid AND object_id=$2::uuid AND id=$3::uuid AND lease_owner=$4 AND lease_until > now() AND version=$5 AND fencing_token=$6 AND status NOT IN ('Completed','CompletedDegraded','Failed','Cancelled')",
            )
            .bind(scope.tenant_id().to_string())
            .bind(next.object_id().to_string())
            .bind(next.id().to_string())
            .bind(owner)
            .bind(to_i64(expected_version, "expected version")?)
            .bind(to_i64(expected_fencing_token.value(), "fencing token")?),
        )
        .await?;
        if expired.rows_affected() != 1 {
            transaction.rollback().await?;
            return Ok(false);
        }

        let result = execute(
            &mut transaction,
            sqlx::query(
                "UPDATE object_invocations SET status='Failed', error_code=$1, finished_at=now(), version=version+1, lease_owner=NULL, lease_until=NULL WHERE tenant_id=$2::uuid AND object_id=$3::uuid AND id=$4::uuid AND lease_owner=$5 AND lease_until <= now() AND version=$6 AND fencing_token=$7 AND status NOT IN ('Completed','CompletedDegraded','Failed','Cancelled')",
            )
            .bind(error_code)
            .bind(scope.tenant_id().to_string())
            .bind(next.object_id().to_string())
            .bind(next.id().to_string())
            .bind(owner)
            .bind(to_i64(expected_version, "expected version")?)
            .bind(to_i64(expected_fencing_token.value(), "fencing token")?),
        )
        .await?;
        if result.rows_affected() != 1 {
            transaction.rollback().await?;
            return Ok(false);
        }
        transaction.commit().await?;
        Ok(true)
    }

}

fn validate_lease_request(owner: &str, lease_seconds: u32) -> Result<(), InvocationStoreError> {
    if owner.trim().is_empty() || owner.len() > 128 || !(3..=3600).contains(&lease_seconds) {
        return Err(InvocationStoreError::InvalidRequestedState(
            "lease owner must be 1..128 bytes and lease duration 1..3600 seconds",
        ));
    }
    Ok(())
}

fn validate_next(
    scope: &Scope,
    expected_version: u64,
    expected_fencing_token: FencingToken,
    next: &Invocation,
) -> Result<(), InvocationStoreError> {
    if next.tenant_id() != scope.tenant_id() {
        return Err(InvocationStoreError::InvalidRequestedState(
            "invocation tenant differs from trusted Scope",
        ));
    }
    if next.version()
        != expected_version
            .checked_add(1)
            .ok_or(InvocationStoreError::InvalidRequestedState(
                "expected version overflow",
            ))?
    {
        return Err(InvocationStoreError::InvalidRequestedState(
            "next invocation version must advance exactly once",
        ));
    }
    if next.fencing_token() != expected_fencing_token {
        return Err(InvocationStoreError::InvalidRequestedState(
            "fencing token cannot change during a lifecycle transition",
        ));
    }
    Ok(())
}

fn reconstitute(row: InvocationRow) -> Result<Invocation, InvocationStoreError> {
    let status = match row.5.as_str() {
        "Received" => InvocationStatus::Received,
        "ContextLoading" => InvocationStatus::ContextLoading,
        "Processing" => InvocationStatus::Processing,
        "ExecutingTool" => InvocationStatus::ExecutingTool,
        "Verifying" => InvocationStatus::Verifying,
        "WaitingForInput" => InvocationStatus::WaitingForInput,
        "WaitingForApproval" => InvocationStatus::WaitingForApproval,
        "Completed" => InvocationStatus::Completed,
        "CompletedDegraded" => InvocationStatus::CompletedDegraded,
        "Failed" => InvocationStatus::Failed,
        "Cancelled" => InvocationStatus::Cancelled,
        _ => return Err(InvocationStoreError::InvalidPersistedValue("status")),
    };
    let version =
        u64::try_from(row.6).map_err(|_| InvocationStoreError::InvalidPersistedValue("version"))?;
    let token = u64::try_from(row.7)
        .map_err(|_| InvocationStoreError::InvalidPersistedValue("fencing_token"))?;
    Ok(Invocation::from_persisted(
        parse_id::<InvocationId>(&row.0, "id")?,
        parse_id::<TenantId>(&row.1, "tenant_id")?,
        parse_id::<ObjectId>(&row.2, "object_id")?,
        parse_id::<CallerId>(&row.3, "caller_id")?,
        parse_id::<SnapshotId>(&row.4, "snapshot_id")?,
        InvocationCheckpoint::new(status, version, FencingToken::from_value(token)),
    ))
}

fn parse_id<T: FromStr>(value: &str, field: &'static str) -> Result<T, InvocationStoreError> {
    value
        .parse()
        .map_err(|_| InvocationStoreError::InvalidPersistedValue(field))
}

fn to_i64(value: u64, field: &'static str) -> Result<i64, InvocationStoreError> {
    i64::try_from(value).map_err(|_| InvocationStoreError::InvalidRequestedState(field))
}

const fn status_name(status: InvocationStatus) -> &'static str {
    match status {
        InvocationStatus::Received => "Received",
        InvocationStatus::ContextLoading => "ContextLoading",
        InvocationStatus::Processing => "Processing",
        InvocationStatus::ExecutingTool => "ExecutingTool",
        InvocationStatus::Verifying => "Verifying",
        InvocationStatus::WaitingForInput => "WaitingForInput",
        InvocationStatus::WaitingForApproval => "WaitingForApproval",
        InvocationStatus::Completed => "Completed",
        InvocationStatus::CompletedDegraded => "CompletedDegraded",
        InvocationStatus::Failed => "Failed",
        InvocationStatus::Cancelled => "Cancelled",
    }
}
