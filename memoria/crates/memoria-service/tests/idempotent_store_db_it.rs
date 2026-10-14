//! Atomic write receipts against isolated MatrixOne (the CI integration lane).
use memoria_core::{MemoryType, TrustTier};
use memoria_service::MemoryService;
use memoria_storage::SqlMemoryStore;
use std::sync::Arc;

async fn store(
    service: &MemoryService,
    owner: &str,
    session: &str,
    key: &str,
    content: &str,
) -> Result<memoria_core::Memory, memoria_core::MemoriaError> {
    service
        .store_memory_with_metadata_and_key_on_branch(
            owner,
            None,
            content,
            MemoryType::Episodic,
            Some(session.into()),
            Some(TrustTier::T3Inferred),
            None,
            None,
            None,
            None,
            None,
            Some(key),
        )
        .await
}

#[tokio::test]
async fn concurrent_replays_have_one_receipt_conflicts_do_not_mutate_and_deletion_is_final() {
    let url =
        std::env::var("DATABASE_URL").expect("designate isolated MatrixOne with DATABASE_URL");
    let sql = Arc::new(
        SqlMemoryStore::connect(&url, 4, uuid::Uuid::new_v4().to_string())
            .await
            .unwrap(),
    );
    sql.migrate().await.unwrap();
    let service = Arc::new(MemoryService::new_sql_with_llm(sql.clone(), None, None).await);
    let owner = format!("idem-{}", uuid::Uuid::new_v4().simple());
    let mut tasks = tokio::task::JoinSet::new();
    for _ in 0..8 {
        let service = service.clone();
        let owner = owner.clone();
        tasks.spawn(async move {
            store(
                &service,
                &owner,
                "session-a",
                "operation-a",
                "confirmed episode",
            )
            .await
            .unwrap()
        });
    }
    let mut receipt = None;
    while let Some(result) = tasks.join_next().await {
        let memory = result.unwrap();
        assert_eq!(memory.user_id, owner);
        assert_eq!(memory.session_id.as_deref(), Some("session-a"));
        if let Some(ref id) = receipt {
            assert_eq!(&memory.memory_id, id);
        } else {
            receipt = Some(memory.memory_id);
        }
    }
    let id = receipt.unwrap();
    assert_eq!(
        id,
        memoria_service::service::idempotent_memory_id(&owner, None, "operation-a").unwrap()
    );
    let replay = store(
        &service,
        &owner,
        "session-a",
        "operation-a",
        "confirmed episode",
    )
    .await
    .unwrap();
    assert_eq!(replay.memory_id, id);
    assert!(store(
        &service,
        &owner,
        "session-a",
        "operation-a",
        "changed payload"
    )
    .await
    .is_err());
    assert!(store(
        &service,
        &owner,
        "session-b",
        "operation-a",
        "confirmed episode"
    )
    .await
    .is_err());
    let table = sql.table_for_branch(&owner, None).await.unwrap();
    assert_eq!(
        sql.get_from(&table, &id).await.unwrap().unwrap().content,
        "confirmed episode"
    );
    let different_session = store(
        &service,
        &owner,
        "session-b",
        "operation-b",
        "confirmed episode",
    )
    .await
    .unwrap();
    assert_ne!(different_session.memory_id, id);
    let other = store(
        &service,
        "another-owner",
        "session-a",
        "operation-a",
        "confirmed episode",
    )
    .await
    .unwrap();
    assert_ne!(other.memory_id, id);
    assert_eq!(other.user_id, "another-owner");
    sql.soft_delete_from(&table, &id).await.unwrap();
    assert!(store(
        &service,
        &owner,
        "session-a",
        "operation-a",
        "confirmed episode"
    )
    .await
    .is_err());
    assert!(sql.get_from(&table, &id).await.unwrap().is_none());
}
