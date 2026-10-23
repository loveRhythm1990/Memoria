//! CPU MatrixOne checks for the experimental application/schema path.
//! Does NOT prove CAGRA or GPU operation. Opt-in and loopback-only by design.
use memoria_core::interfaces::EmbeddingProvider;
use memoria_core::{MemoriaError, Memory, MemoryType, TrustTier};
use memoria_service::{MemoryService, RetrieveOptions, SessionScope};
use memoria_storage::{DbRouter, SqlMemoryStore};
use sqlx::mysql::MySqlConnectOptions;
use std::sync::Arc;

struct FixedEmbedder;

#[async_trait::async_trait]
impl EmbeddingProvider for FixedEmbedder {
    async fn embed(&self, _: &str) -> Result<Vec<f32>, MemoriaError> {
        Ok(vec![1.0, 0.0, 0.0, 0.0])
    }

    fn dimension(&self) -> usize {
        4
    }
}

fn fresh_local_url() -> (String, String) {
    let url = std::env::var("MEMORIA_CUVS_LOCAL_DATABASE_URL")
        .expect("set MEMORIA_CUVS_LOCAL_DATABASE_URL to a dedicated loopback MatrixOne");
    let opts: MySqlConnectOptions = url.parse().expect("valid local MySQL URL");
    assert!(
        matches!(opts.get_host(), "127.0.0.1" | "::1"),
        "only literal loopback hosts are allowed"
    );
    assert!(
        opts.get_database().is_some(),
        "URL must contain a database path"
    );
    assert!(
        !url.contains(['?', '#']),
        "use a plain local URL for this test"
    );
    let name = format!("memoria_cuvs_it_{}", uuid::Uuid::new_v4().simple());
    let base = url.rsplit_once('/').unwrap().0;
    (format!("{base}/{name}"), name)
}

async fn write(service: &MemoryService, user: &str, session: &str, key: &str) -> Memory {
    service
        .store_memory_with_metadata_and_key_on_branch(
            user,
            None,
            "synthetic observatory memory",
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
        .unwrap()
}

#[tokio::test]
#[ignore = "requires a dedicated loopback MatrixOne; never uses DATABASE_URL"]
async fn experimental_routing_writes_retrieval_and_lifecycle() {
    let (url, shared_name) = fresh_local_url();
    let router = Arc::new(
        DbRouter::connect(&url, 4, "cuvs-local-it".into())
            .await
            .unwrap()
            .with_cuvs_experiment(true),
    );
    let mut shared = SqlMemoryStore::from_existing_pool(
        router.shared_pool().clone(),
        4,
        "cuvs-local-it".into(),
        Some(url.clone()),
        Some(router.shared_pool_max_connections()),
        "cuvs_local_shared",
    )
    .with_cuvs_experiment(true);
    shared.migrate_shared().await.unwrap();
    shared.set_db_router(router.clone());
    let shared = Arc::new(shared);
    let service = MemoryService::new(
        shared.clone(),
        Some(Arc::new(FixedEmbedder)),
        Some(shared.clone()),
    );
    // A lightweight service avoids unrelated background jobs in this test.
    let mut service = service.with_graph_retrieval_enabled(false);
    service.db_router = Some(router.clone());
    assert!(!service.graph_retrieval_enabled());

    let user_a = format!("cuvs_a_{}", uuid::Uuid::new_v4().simple());
    let user_b = format!("cuvs_b_{}", uuid::Uuid::new_v4().simple());
    let first = write(&service, &user_a, "session-a", "receipt-a").await;
    let replay = write(&service, &user_a, "session-a", "receipt-a").await;
    let second = write(&service, &user_a, "session-b", "receipt-b").await;
    let other = write(&service, &user_b, "session-a", "receipt-a").await;
    assert_eq!(first.memory_id, replay.memory_id);
    assert_ne!(first.memory_id, second.memory_id);
    assert_ne!(first.memory_id, other.memory_id);

    let a = service.user_sql_store(&user_a).await.unwrap();
    let b = service.user_sql_store(&user_b).await.unwrap();
    assert!(a.cuvs_experiment_enabled() && b.cuvs_experiment_enabled());
    assert_ne!(a.db_name(), b.db_name());
    // Repeat migration exercises the schema-version fast path too.
    a.migrate_user().await.unwrap();
    let table = a.t("mem_memories");
    let ids: Vec<i64> = sqlx::query_scalar(&format!("SELECT id FROM {table} ORDER BY id"))
        .fetch_all(a.pool())
        .await
        .unwrap();
    assert_eq!(ids.len(), 2);
    assert!(ids[0] > 0 && ids[1] > ids[0]);
    assert!(b
        .get_from(&b.t("mem_memories"), &first.memory_id)
        .await
        .unwrap()
        .is_none());

    let (memories, explain) = service
        .retrieve_explain(&user_a, "observatory", 10)
        .await
        .unwrap();
    assert!(!explain.graph_attempted);
    assert!(
        explain.vector_attempted && explain.vector_hit,
        "must exercise vectors, not only fulltext"
    );
    assert_eq!(explain.path, "hybrid");
    assert_eq!(memories.len(), 2);
    assert!(memories.iter().all(|m| m.user_id == user_a));

    let strict = RetrieveOptions::from_session_scope(Some("session-b"), Some(SessionScope::Only));
    let scoped = service
        .retrieve_with_options(&user_a, "observatory", 10, &strict)
        .await
        .unwrap();
    assert_eq!(scoped.len(), 1);
    assert_eq!(scoped[0].memory_id, second.memory_id);

    // Updating the embedding must change vector retrieval independently of text.
    sqlx::query(&format!(
        "UPDATE {table} SET embedding='[0,1,0,0]' WHERE memory_id=?"
    ))
    .bind(&first.memory_id)
    .execute(a.pool())
    .await
    .unwrap();
    let nearest = a
        .search_vector_from_filtered_scoped(
            &table,
            &user_a,
            &[0.0, 1.0, 0.0, 0.0],
            1,
            None,
            None,
            None,
            None,
        )
        .await
        .unwrap();
    assert_eq!(nearest[0].memory_id, first.memory_id);
    assert_eq!(
        a.soft_delete_from(&table, &first.memory_id).await.unwrap(),
        1
    );
    let remaining = service.retrieve(&user_a, "observatory", 10).await.unwrap();
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].memory_id, second.memory_id);
    sqlx::query(&format!("DELETE FROM {table} WHERE memory_id=?"))
        .bind(&second.memory_id)
        .execute(a.pool())
        .await
        .unwrap();
    assert!(service
        .retrieve(&user_a, "observatory", 10)
        .await
        .unwrap()
        .is_empty());

    // Background pool clones must keep both experiment policy and DB prefix.
    let bg = a.spawn_background_store(1).await.unwrap();
    assert!(bg.cuvs_experiment_enabled());
    assert_eq!(bg.db_name(), a.db_name());
    assert_eq!(bg.rebuild_vector_index("mem_memories").await.unwrap(), 0);
    let index_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM information_schema.statistics WHERE table_schema=? AND table_name='mem_memories' AND index_name='mem_memories_embedding_ivf'",
    ).bind(a.db_name().unwrap()).fetch_one(a.pool()).await.unwrap();
    assert_eq!(index_count, 0);

    assert!(service.drain_edit_log().await);
    for db in [
        a.db_name().unwrap(),
        b.db_name().unwrap(),
        shared_name.as_str(),
    ] {
        sqlx::raw_sql(&format!("DROP DATABASE `{db}`"))
            .execute(shared.pool())
            .await
            .unwrap();
    }
}

#[tokio::test]
#[ignore = "requires a dedicated loopback MatrixOne; never uses DATABASE_URL"]
async fn rejects_a_legacy_database_without_converting_it() {
    let (url, name) = fresh_local_url();
    let legacy = SqlMemoryStore::connect(&url, 4, "cuvs-legacy-it".into())
        .await
        .unwrap();
    legacy.migrate_user().await.unwrap();
    let experiment = legacy.clone().with_cuvs_experiment(true);
    let error = experiment.migrate_user().await.unwrap_err();
    assert!(matches!(error, MemoriaError::Validation(_)));
    let id_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM information_schema.columns WHERE table_schema=? AND table_name='mem_memories' AND column_name='id'",
    ).bind(&name).fetch_one(legacy.pool()).await.unwrap();
    assert_eq!(id_count, 0, "legacy schema must remain unchanged");
    sqlx::raw_sql(&format!("DROP DATABASE `{name}`"))
        .execute(legacy.pool())
        .await
        .unwrap();
}
