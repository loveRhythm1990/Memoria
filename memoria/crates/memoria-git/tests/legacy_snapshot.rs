//! Cross-schema upgrade regressions. Each test owns a disposable database.
//! SQLX_OFFLINE=true DATABASE_URL=mysql://root:111@127.0.0.1:16011/test cargo test -p memoria-git --test legacy_snapshot
use memoria_git::GitForDataService;
use sqlx::{
    mysql::{MySqlConnectOptions, MySqlPoolOptions},
    MySql, MySqlPool, QueryBuilder,
};

struct Fixture {
    pool: MySqlPool,
    admin: MySqlPool,
    db: String,
    snapshot: String,
    git: GitForDataService,
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
