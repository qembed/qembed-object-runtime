#[allow(dead_code)]
mod support;

use qembed_api::provenance::ApplicationProvenanceVerifier;
use qembed_domain::object::Object;
use qembed_domain::{
    citation_verifier::{CitationClaim, CitationDocument, VerificationRule},
    ids::{KnowledgeChunkId, KnowledgeDocumentId, ObjectId, TenantId},
    knowledge::{PinnedKnowledgeRevision, RevisionNumber},
    scope::Scope,
    test_support::scope_for_tenant,
};
use qembed_store::{
    PgPoolConfig, connect_pool, object_repository::ObjectRepository, run_migrations,
};

#[tokio::test]
async fn application_verifier_rejects_forged_citation_ids_and_accepts_persisted_evidence() {
    let database_url = std::env::var("DATABASE_URL")
        .expect("DATABASE_URL must be set by scripts/test-postgres.sh");
    let pool = connect_pool(&database_url, PgPoolConfig::default())
        .await
        .expect("PostgreSQL test database accepts connections");
    run_migrations(&pool).await.expect("migrations apply");

    let context = support::signed_context(&pool).await;
    let tenant_id = TenantId::new();
    let scope: Scope = scope_for_tenant(tenant_id);
    let object_id = ObjectId::new();
    ObjectRepository::new(context.clone())
        .create_object(&scope, &Object::new(object_id, tenant_id))
        .await
        .expect("Object created");

    let document_id = KnowledgeDocumentId::new();
    let chunk_id = KnowledgeChunkId::new();
    sqlx::query(
        "INSERT INTO knowledge_documents \
         (id, tenant_id, object_id, doc_key, title, status, valid_from_revision, valid_to_revision) \
         VALUES ($1::uuid, $2::uuid, $3::uuid, 'rust074-evidence', 'RUST-074 evidence', 'live', 1, NULL)",
    )
    .bind(document_id.to_string())
    .bind(tenant_id.to_string())
    .bind(object_id.to_string())
    .execute(&pool)
    .await
    .expect("Knowledge document fixture inserted");
    sqlx::query(
        "INSERT INTO knowledge_chunks \
         (id, tenant_id, object_id, document_id, content, erased, position, valid_from_revision, valid_to_revision) \
         VALUES ($1::uuid, $2::uuid, $3::uuid, $4::uuid, 'The supported value is 42.', false, 0, 1, NULL)",
    )
    .bind(chunk_id.to_string())
    .bind(tenant_id.to_string())
    .bind(object_id.to_string())
    .bind(document_id.to_string())
    .execute(&pool)
    .await
    .expect("Knowledge chunk fixture inserted");
    sqlx::query(
        "UPDATE knowledge_revisions SET current_revision = 1 \
          WHERE tenant_id = $1::uuid AND object_id = $2::uuid",
    )
    .bind(tenant_id.to_string())
    .bind(object_id.to_string())
    .execute(&pool)
    .await
    .expect("Knowledge revision advanced for fixture");

    let verifier = ApplicationProvenanceVerifier::new(context);
    let pinned = PinnedKnowledgeRevision::new(tenant_id, object_id, RevisionNumber::new(1));

    let valid = CitationDocument {
        claims: vec![CitationClaim {
            id: "claim-1".to_owned(),
            text: "The supported value is 42.".to_owned(),
            citations: vec![chunk_id],
        }],
    };
    let valid_report = verifier
        .verify(&scope, pinned, &valid)
        .await
        .expect("persisted evidence can be loaded");
    assert!(valid_report.is_accepted());

    let forged = CitationDocument {
        claims: vec![CitationClaim {
            id: "claim-forged".to_owned(),
            text: "The supported value is 42.".to_owned(),
            citations: vec![KnowledgeChunkId::new()],
        }],
    };
    let forged_report = verifier
        .verify(&scope, pinned, &forged)
        .await
        .expect("verification returns a report for unresolved citations");
    assert!(!forged_report.is_accepted());
    assert!(
        forged_report
            .violations()
            .iter()
            .any(|violation| violation.rule == VerificationRule::V4CitationResolution)
    );

    pool.close().await;
}
