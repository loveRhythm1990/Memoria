//! Cross-schema upgrade regressions. Each test owns a disposable database.
//! SQLX_OFFLINE=true DATABASE_URL=mysql://root:111@127.0.0.1:16011/test cargo test -p memoria-git --test legacy_snapshot
use memoria_git::GitForDataService;
use sqlx::{
    mysql::{MySqlConnectOptions, MySqlPoolOptions},
    MySql, MySqlPool, QueryBuilder,
};

#[tokio::test]
async fn native_operations_reject_incompatible_existing_branch_without_mutation() {
    let f = Fixture::new().await;
    sqlx::raw_sql("INSERT INTO memories VALUES (1, 'main')")
        .execute(&f.pool)
        .await
        .unwrap();
    f.git
        .create_branch("br_schema_order", "memories")
        .await
        .unwrap();
    // Simulate a previously registered branch whose startup migration could not
    // reconcile its schema. Native operations must check it independently.
    sqlx::raw_sql("ALTER TABLE memories ADD COLUMN subject_id VARCHAR(128) DEFAULT NULL AFTER id")
        .execute(&f.pool)
        .await
        .unwrap();
    sqlx::raw_sql("ALTER TABLE br_schema_order ADD COLUMN subject_id VARCHAR(128) DEFAULT NULL")
        .execute(&f.pool)
        .await
        .unwrap();
    for result in [
        f.git
            .merge_branch("br_schema_order", "memories")
            .await
            .map(|_| ()),
        f.git
            .diff_branch_rows("br_schema_order", "memories", "user", 10)
            .await
            .map(|_| ()),
    ] {
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Branch schema is incompatible"));
    }
    assert_eq!(f.rows().await, vec![(1, "main".into(), None)]);
    f.git.drop_branch("br_schema_order").await.unwrap();
    f.cleanup().await;
}

#[tokio::test]
async fn chinese_snapshot_names_round_trip_without_changing_physical_names() {
    let mut f = Fixture::new().await;
    f.snapshot.push_str("_实验");
    sqlx::raw_sql("INSERT INTO memories VALUES (1, 'historical')")
        .execute(&f.pool)
        .await
        .unwrap();
    f.snapshot().await;
    assert!(f.git.get_snapshot(&f.snapshot).await.unwrap().is_some());
    sqlx::raw_sql("UPDATE memories SET content='current'")
        .execute(&f.pool)
        .await
        .unwrap();
    f.git
        .restore_table_from_snapshot("memories", &f.snapshot)
        .await
        .unwrap();
    let content: String = sqlx::query_scalar("SELECT content FROM memories WHERE id=1")
        .fetch_one(&f.pool)
        .await
        .unwrap();
    assert_eq!(content, "historical");
    f.git.drop_snapshot(&f.snapshot).await.unwrap();
    assert!(f.git.get_snapshot(&f.snapshot).await.unwrap().is_none());
    f.cleanup().await;
}

#[tokio::test]
async fn indexed_snapshot_restores_512_embeddings_and_search_indexes() {
    let f = Fixture::new().await;
    sqlx::raw_sql("ALTER TABLE memories ADD COLUMN subject_id VARCHAR(128) DEFAULT NULL; ALTER TABLE memories ADD COLUMN embedding VECF32(3); ALTER TABLE memories ADD FULLTEXT INDEX ft_content (content) WITH PARSER ngram; ALTER TABLE memories ADD UNIQUE INDEX unique_content (content)")
        .execute(&f.pool).await.unwrap();
    let mut insert = QueryBuilder::<MySql>::new("INSERT INTO memories (id, content, embedding) ");
    insert.push_values(0..512, |mut row, id| {
        row.push_bind(id)
            .push_bind(format!("historical searchable memory {id}"))
            .push_bind(format!(
                "[{}, {}, 0.5]",
                id as f32 / 512.0,
                (512 - id) as f32 / 512.0
            ));
    });
    insert.build().execute(&f.pool).await.unwrap();
    sqlx::raw_sql("CREATE INDEX memories_embedding_ivf USING ivfflat ON memories(embedding) LISTS 10 op_type 'vector_l2_ops'")
        .execute(&f.pool).await.unwrap();
    let types: Vec<String> = sqlx::query_scalar("SELECT DISTINCT INDEX_TYPE FROM information_schema.statistics WHERE table_schema = ? AND table_name = 'memories'")
        .bind(&f.db).fetch_all(&f.pool).await.unwrap();
    assert!(
        types.iter().any(|t| t.eq_ignore_ascii_case("ivfflat")),
        "{types:?}"
    );
    assert!(
        types.iter().any(|t| t.eq_ignore_ascii_case("fulltext")),
        "{types:?}"
    );
    let baseline: Vec<i32> = sqlx::query_scalar(
        "SELECT id FROM memories WHERE MATCH(content) AGAINST ('+historical' IN BOOLEAN MODE)",
    )
    .fetch_all(&f.pool)
    .await
    .expect("fulltext search before restore");
    assert_eq!(baseline.len(), 512);
    f.snapshot().await;
    sqlx::raw_sql("UPDATE memories SET content = CONCAT('current ', id), subject_id = 'current'")
        .execute(&f.pool)
        .await
        .unwrap();
    let current: Vec<i32> = sqlx::query_scalar(
        "SELECT id FROM memories WHERE MATCH(content) AGAINST ('+current' IN BOOLEAN MODE)",
    )
    .fetch_all(&f.pool)
    .await
    .expect("fulltext search before restore");
    assert_eq!(current.len(), 512);
    f.git
        .restore_table_from_snapshot("memories", &f.snapshot)
        .await
        .unwrap();
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM memories WHERE embedding IS NOT NULL AND subject_id IS NULL AND content LIKE 'historical%'")
        .fetch_one(&f.pool).await.unwrap();
    assert_eq!(count, 512);
    let matches: Vec<i32> = sqlx::query_scalar(
        "SELECT id FROM memories WHERE MATCH(content) AGAINST ('+historical' IN BOOLEAN MODE)",
    )
    .fetch_all(&f.pool)
    .await
    .unwrap();
    assert_eq!(
        matches.len(),
        512,
        "restored fulltext index must remain searchable"
    );
    let nearest: i32 = sqlx::query_scalar(
        "SELECT id FROM memories ORDER BY l2_distance(embedding, '[0, 1, 0.5]') ASC LIMIT 1",
    )
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(nearest, 0);
    let after: Vec<String> = sqlx::query_scalar("SELECT DISTINCT INDEX_TYPE FROM information_schema.statistics WHERE table_schema = ? AND table_name = 'memories'")
        .bind(&f.db).fetch_all(&f.pool).await.unwrap();
    assert!(after.iter().any(|t| t.eq_ignore_ascii_case("ivfflat")));
    assert!(after.iter().any(|t| t.eq_ignore_ascii_case("fulltext")));
    // Staging index removal must not relax the live UNIQUE constraint.
    assert!(sqlx::query(
        "INSERT INTO memories (id, content) VALUES (900, 'historical searchable memory 0')"
    )
    .execute(&f.pool)
    .await
    .is_err());
    f.assert_no_stage().await;
    f.cleanup().await;
}

struct Fixture {
    pool: MySqlPool,
    admin: MySqlPool,
    db: String,
    snapshot: String,
    git: GitForDataService,
}

#[tokio::test]
async fn branch_index_permission_failure_is_nonfatal_but_missing_column_is_fatal() {
    let f = Fixture::new().await;
    sqlx::raw_sql("CREATE TABLE mem_memories (memory_id VARCHAR(64) PRIMARY KEY, user_id VARCHAR(64), subject_id VARCHAR(128), is_active TINYINT, memory_type VARCHAR(20), content TEXT); CREATE TABLE br_index_optional LIKE mem_memories; CREATE TABLE br_column_required LIKE br_index_optional; ALTER TABLE br_column_required DROP COLUMN subject_id")
        .execute(&f.pool).await.unwrap();
    let role = format!("restore_role_{}", uuid::Uuid::new_v4().simple());
    let user = format!("restore_user_{}", uuid::Uuid::new_v4().simple());
    let mut create_role = QueryBuilder::<MySql>::new("CREATE ROLE ");
    create_role.push(&role);
    sqlx::raw_sql(&create_role.into_sql())
        .execute(&f.admin)
        .await
        .unwrap();
    let mut create_user = QueryBuilder::<MySql>::new("CREATE USER ");
    create_user
        .push(&user)
        .push(" IDENTIFIED BY 'disposable_test_only' DEFAULT ROLE ")
        .push(&role);
    sqlx::raw_sql(&create_user.into_sql())
        .execute(&f.admin)
        .await
        .unwrap();
    for privilege in ["SELECT", "INSERT"] {
        let mut grant = QueryBuilder::<MySql>::new("GRANT ");
        grant
            .push(privilege)
            .push(" ON TABLE ")
            .push(&f.db)
            .push(".* TO ")
            .push(&role);
        sqlx::raw_sql(&grant.into_sql())
            .execute(&f.admin)
            .await
            .unwrap();
    }
    let options: MySqlConnectOptions = std::env::var("DATABASE_URL").unwrap().parse().unwrap();
    let limited = MySqlPool::connect_with(
        options
            .database(&f.db)
            .username(&user)
            .password("disposable_test_only"),
    )
    .await
    .unwrap();
    // Prove the real DDL failure, not a mock or an already-created index.
    let denied = sqlx::raw_sql("ALTER TABLE br_index_optional ADD INDEX idx_scope_subject_active (user_id, subject_id, is_active, memory_type)")
        .execute(&limited).await.unwrap_err();
    assert!(denied.to_string().contains("privilege"), "{denied}");
    let store = memoria_storage::SqlMemoryStore::new(limited.clone(), 3, "test".into());
    let optional = store.ensure_branch_subject_id("br_index_optional").await;
    let required = store.ensure_branch_subject_id("br_column_required").await;
    let write = sqlx::query("INSERT INTO br_index_optional VALUES ('memory', 'user', 'subject', 1, 'semantic', 'usable branch')")
        .execute(&limited).await;
    let content: String =
        sqlx::query_scalar("SELECT content FROM br_index_optional WHERE subject_id = 'subject'")
            .fetch_one(&limited)
            .await
            .unwrap();
    limited.close().await;
    let mut drop_user = QueryBuilder::<MySql>::new("DROP USER ");
    drop_user.push(&user);
    sqlx::raw_sql(&drop_user.into_sql())
        .execute(&f.admin)
        .await
        .unwrap();
    let mut drop_role = QueryBuilder::<MySql>::new("DROP ROLE ");
    drop_role.push(&role);
    sqlx::raw_sql(&drop_role.into_sql())
        .execute(&f.admin)
        .await
        .unwrap();
    f.cleanup().await;
    assert!(
        optional.is_ok(),
        "index failure must not reject a usable branch: {optional:?}"
    );
    assert!(required.is_err(), "missing subject_id must remain fatal");
    assert!(write.is_ok());
    assert_eq!(content, "usable branch");
}

impl Fixture {
    async fn new() -> Self {
        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL required");
        let options: MySqlConnectOptions = url.parse().unwrap();
        let admin = MySqlPoolOptions::new()
            .max_connections(2)
            .connect_with(options.clone().database("mo_catalog"))
            .await
            .unwrap();
        let db = format!("legacy_restore_{}", uuid::Uuid::new_v4().simple());
        let mut ddl = QueryBuilder::<MySql>::new("CREATE DATABASE ");
        ddl.push(&db);
        sqlx::raw_sql(ddl.sql()).execute(&admin).await.unwrap();
        let pool = MySqlPoolOptions::new()
            .max_connections(3)
            .connect_with(options.database(&db))
            .await
            .unwrap();
        sqlx::raw_sql("CREATE TABLE memories (id INT PRIMARY KEY, content VARCHAR(100) NOT NULL)")
            .execute(&pool)
            .await
            .unwrap();
        let snapshot = format!("legacy_{}", uuid::Uuid::new_v4().simple());
        let git = GitForDataService::new(pool.clone(), &db);
        Self {
            pool,
            admin,
            db,
            snapshot,
            git,
        }
    }

    async fn snapshot(&self) {
        self.git.create_snapshot(&self.snapshot).await.unwrap();
    }

    async fn rows(&self) -> Vec<(i32, String, Option<String>)> {
        sqlx::query_as("SELECT id, content, subject_id FROM memories ORDER BY id")
            .fetch_all(&self.pool)
            .await
            .unwrap()
    }

    async fn assert_no_stage(&self) {
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM information_schema.tables WHERE table_schema = ? AND table_name LIKE 'mem_restore_%'")
            .bind(&self.db).fetch_one(&self.pool).await.unwrap();
        assert_eq!(count, 0, "restore must clean up private staging tables");
    }

    async fn cleanup(&self) {
        let _ = self.git.drop_snapshot(&self.snapshot).await;
        self.pool.close().await;
        let mut ddl = QueryBuilder::<MySql>::new("DROP DATABASE IF EXISTS ");
        ddl.push(&self.db);
        sqlx::raw_sql(ddl.sql()).execute(&self.admin).await.unwrap();
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let pool = self.admin.clone();
        let mut snap = QueryBuilder::<MySql>::new("DROP SNAPSHOT IF EXISTS ");
        snap.push(&self.snapshot);
        let snap = snap.sql().to_string();
        let mut db = QueryBuilder::<MySql>::new("DROP DATABASE IF EXISTS ");
        db.push(&self.db);
        let db = db.sql().to_string();
        tokio::spawn(async move {
            let _ = sqlx::raw_sql(&snap).execute(&pool).await;
            let _ = sqlx::raw_sql(&db).execute(&pool).await;
        });
    }
}

#[tokio::test]
async fn legacy_snapshot_restores_by_column_name_and_current_defaults() {
    let f = Fixture::new().await;
    sqlx::query("INSERT INTO memories VALUES (1, 'historical')")
        .execute(&f.pool)
        .await
        .unwrap();
    f.snapshot().await;
    sqlx::raw_sql("ALTER TABLE memories ADD COLUMN subject_id VARCHAR(128) DEFAULT NULL AFTER id")
        .execute(&f.pool)
        .await
        .unwrap();
    sqlx::raw_sql("ALTER TABLE memories ADD COLUMN generation INT NOT NULL DEFAULT 7")
        .execute(&f.pool)
        .await
        .unwrap();
    sqlx::query(
        "UPDATE memories SET content = 'current', subject_id = 'current-subject', generation = 9",
    )
    .execute(&f.pool)
    .await
    .unwrap();
    for _ in 0..2 {
        let result = f
            .git
            .restore_table_from_snapshot("memories", &f.snapshot)
            .await;
        assert!(
            result.is_ok(),
            "{result:?}; current rows: {:?}",
            f.rows().await
        );
        assert_eq!(f.rows().await, vec![(1, "historical".into(), None)]);
        let generation: i32 = sqlx::query_scalar("SELECT generation FROM memories WHERE id = 1")
            .fetch_one(&f.pool)
            .await
            .unwrap();
        assert_eq!(generation, 7);
        f.assert_no_stage().await;
    }
    f.cleanup().await;
}

#[tokio::test]
async fn legacy_empty_snapshot_restores_to_empty_table() {
    let f = Fixture::new().await;
    f.snapshot().await;
    sqlx::raw_sql("ALTER TABLE memories ADD COLUMN subject_id VARCHAR(128) DEFAULT NULL")
        .execute(&f.pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO memories VALUES (1, 'current', 'subject')")
        .execute(&f.pool)
        .await
        .unwrap();
    f.git
        .restore_table_from_snapshot("memories", &f.snapshot)
        .await
        .unwrap();
    assert!(f.rows().await.is_empty());
    f.assert_no_stage().await;
    f.cleanup().await;
}

#[tokio::test]
async fn restore_without_unique_keys_preserves_duplicate_row_count() {
    let f = Fixture::new().await;
    sqlx::raw_sql("CREATE TABLE unkeyed (content VARCHAR(100) NOT NULL); INSERT INTO unkeyed VALUES ('historical'), ('historical')")
        .execute(&f.pool).await.unwrap();
    f.snapshot().await;
    sqlx::raw_sql("DELETE FROM unkeyed; INSERT INTO unkeyed VALUES ('current')")
        .execute(&f.pool)
        .await
        .unwrap();
    for _ in 0..2 {
        f.git
            .restore_table_from_snapshot("unkeyed", &f.snapshot)
            .await
            .unwrap();
        let rows: Vec<String> = sqlx::query_scalar("SELECT content FROM unkeyed")
            .fetch_all(&f.pool)
            .await
            .unwrap();
        assert_eq!(rows, vec!["historical", "historical"]);
        f.assert_no_stage().await;
    }
    f.cleanup().await;
}

#[tokio::test]
async fn legacy_snapshot_constraint_failure_keeps_current_rows() {
    let f = Fixture::new().await;
    sqlx::query("INSERT INTO memories VALUES (1, 'duplicate'), (2, 'duplicate')")
        .execute(&f.pool)
        .await
        .unwrap();
    f.snapshot().await;
    sqlx::query("DELETE FROM memories")
        .execute(&f.pool)
        .await
        .unwrap();
    sqlx::raw_sql("ALTER TABLE memories ADD COLUMN subject_id VARCHAR(128) DEFAULT NULL")
        .execute(&f.pool)
        .await
        .unwrap();
    sqlx::raw_sql("ALTER TABLE memories ADD UNIQUE KEY unique_content (content)")
        .execute(&f.pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO memories VALUES (3, 'keep current', 'subject')")
        .execute(&f.pool)
        .await
        .unwrap();
    assert!(f
        .git
        .restore_table_from_snapshot("memories", &f.snapshot)
        .await
        .is_err());
    assert_eq!(
        f.rows().await,
        vec![(3, "keep current".into(), Some("subject".into()))]
    );
    f.assert_no_stage().await;
    f.cleanup().await;
}

#[tokio::test]
async fn missing_snapshot_keeps_current_rows() {
    let f = Fixture::new().await;
    sqlx::raw_sql("ALTER TABLE memories ADD COLUMN subject_id VARCHAR(128) DEFAULT NULL")
        .execute(&f.pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO memories VALUES (1, 'keep current', NULL)")
        .execute(&f.pool)
        .await
        .unwrap();
    assert!(f
        .git
        .restore_table_from_snapshot("memories", &f.snapshot)
        .await
        .is_err());
    assert_eq!(f.rows().await, vec![(1, "keep current".into(), None)]);
    f.assert_no_stage().await;
    f.cleanup().await;
}

#[tokio::test]
async fn legacy_snapshot_missing_required_column_fails_before_deletion() {
    let f = Fixture::new().await;
    sqlx::query("INSERT INTO memories VALUES (1, 'historical')")
        .execute(&f.pool)
        .await
        .unwrap();
    f.snapshot().await;
    sqlx::query("DELETE FROM memories")
        .execute(&f.pool)
        .await
        .unwrap();
    sqlx::raw_sql("ALTER TABLE memories ADD COLUMN subject_id VARCHAR(128) NOT NULL")
        .execute(&f.pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO memories VALUES (2, 'current', 'must keep')")
        .execute(&f.pool)
        .await
        .unwrap();
    let error = f
        .git
        .restore_table_from_snapshot("memories", &f.snapshot)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("required column"), "{error}");
    assert_eq!(
        f.rows().await,
        vec![(2, "current".into(), Some("must keep".into()))]
    );
    f.assert_no_stage().await;
    f.cleanup().await;
}

#[tokio::test]
async fn old_select_star_restore_reproduces_column_mismatch() {
    let f = Fixture::new().await;
    sqlx::query("INSERT INTO memories VALUES (1, 'historical')")
        .execute(&f.pool)
        .await
        .unwrap();
    f.snapshot().await;
    sqlx::raw_sql("ALTER TABLE memories ADD COLUMN subject_id VARCHAR(128) DEFAULT NULL")
        .execute(&f.pool)
        .await
        .unwrap();
    // Reproduce the old path only on this test-owned table, then recover it.
    sqlx::query("DELETE FROM memories")
        .execute(&f.pool)
        .await
        .unwrap();
    let mut old_insert =
        QueryBuilder::<MySql>::new("INSERT INTO memories SELECT * FROM memories {SNAPSHOT = '");
    old_insert.push(&f.snapshot).push("'}");
    let error = sqlx::raw_sql(old_insert.sql())
        .execute(&f.pool)
        .await
        .unwrap_err();
    assert!(
        error.to_string().to_lowercase().contains("column"),
        "{error}"
    );
    assert!(
        f.rows().await.is_empty(),
        "old path left the current table empty"
    );
    f.git
        .restore_table_from_snapshot("memories", &f.snapshot)
        .await
        .unwrap();
    assert_eq!(f.rows().await, vec![(1, "historical".into(), None)]);
    f.cleanup().await;
}
