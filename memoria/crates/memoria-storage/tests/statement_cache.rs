use futures::TryStreamExt;
use sqlx::{
    mysql::{MySqlConnectOptions, MySqlConnection, MySqlPoolOptions},
    Connection,
};

#[tokio::test]
async fn uncached_prepared_queries_preserve_protocol_state() {
    let url = std::env::var("DATABASE_URL").unwrap_or_else(|_| default_test_url());
    let options: MySqlConnectOptions = url.parse().expect("parse local database URL");
    let mut conn = MySqlConnection::connect_with(&options.statement_cache_capacity(0))
        .await
        .expect("connect local database");
    for n in 0_i64..10 {
        let got: i64 = sqlx::query_scalar("SELECT CAST(? AS BIGINT)")
            .bind(n)
            .fetch_one(&mut conn)
            .await
            .expect("uncached select");
        assert_eq!(got, n);
    }
    assert_eq!(conn.cached_statements_size(), 0);
    conn.close().await.expect("close database session");
}

#[tokio::test]
async fn uncached_queries_recover_after_errors_and_dropped_results() {
    let url = std::env::var("DATABASE_URL").unwrap_or_else(|_| default_test_url());
    let options: MySqlConnectOptions = url.parse().expect("parse local database URL");
    let mut conn = MySqlConnection::connect_with(&options.statement_cache_capacity(0))
        .await
        .expect("connect local database");
    let table = format!("cache_probe_{}", uuid::Uuid::new_v4().simple());
    sqlx::raw_sql(&format!("CREATE TABLE {table} (id BIGINT PRIMARY KEY)"))
        .execute(&mut conn)
        .await
        .expect("create test table");
    for id in 0_i64..4 {
        sqlx::query(&format!("INSERT INTO {table} VALUES (?)"))
            .bind(id)
            .execute(&mut conn)
            .await
            .expect("uncached insert");
    }
    let insert = format!("INSERT INTO {table} VALUES (?)");
    assert!(sqlx::query(&insert)
        .bind(0_i64)
        .execute(&mut conn)
        .await
        .is_err());
    // Resume the same session after a server-side error and after abandoning rows.
    let select = format!("SELECT id FROM {table} WHERE id >= ? ORDER BY id");
    let mut rows = sqlx::query(&select).bind(0_i64).fetch(&mut conn);
    assert!(rows.try_next().await.expect("first row").is_some());
    drop(rows);
    let count: i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {table} WHERE id >= ?"))
        .bind(0_i64)
        .fetch_one(&mut conn)
        .await
        .expect("query after dropped stream");
    assert_eq!(count, 4);
    let empty = sqlx::query(&select)
        .bind(99_i64)
        .fetch_optional(&mut conn)
        .await
        .expect("empty result");
    assert!(empty.is_none());
    // Transaction commands must drain pending results/close before changing state.
    let mut txn = conn.begin().await.expect("begin transaction");
    sqlx::query(&insert)
        .bind(5_i64)
        .execute(&mut *txn)
        .await
        .expect("transaction insert");
    txn.rollback().await.expect("rollback");
    let count: i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {table}"))
        .fetch_one(&mut conn)
        .await
        .expect("query after rollback");
    assert_eq!(count, 4);
    assert_eq!(conn.cached_statements_size(), 0);
    sqlx::raw_sql(&format!("DROP TABLE {table}"))
        .execute(&mut conn)
        .await
        .expect("drop test table");
    conn.close().await.expect("close session");
}

#[tokio::test]
async fn pool_reuse_and_explicit_nonpersistent_queries_preserve_results() {
    let url = std::env::var("DATABASE_URL").unwrap_or_else(|_| default_test_url());
    for capacity in [0, 100] {
        let options: MySqlConnectOptions = url.parse().expect("parse local database URL");
        let pool = MySqlPoolOptions::new()
            .max_connections(1)
            .connect_with(options.statement_cache_capacity(capacity))
            .await
            .expect("connect pool");
        for n in 0_i64..10 {
            let got: i64 = sqlx::query_scalar("SELECT CAST(? AS BIGINT)")
                .bind(n)
                .persistent(false)
                .fetch_one(&pool)
                .await
                .expect("nonpersistent query");
            assert_eq!(got, n);
        }
        let mut conn = pool.acquire().await.expect("reacquire pooled session");
        assert_eq!(conn.cached_statements_size(), 0);
        // Check both normal caching and the disabled-cache path after reuse.
        let got: i64 = sqlx::query_scalar("SELECT CAST(? AS BIGINT)")
            .bind(42_i64)
            .fetch_one(&mut *conn)
            .await
            .expect("persistent query");
        assert_eq!(got, 42);
        assert_eq!(
            conn.cached_statements_size(),
            if capacity == 0 { 0 } else { 1 }
        );
        drop(conn);
        pool.close().await;
    }
}

#[tokio::test]
async fn routed_user_pool_really_disables_statement_cache() {
    let url = std::env::var("DATABASE_URL").unwrap_or_else(|_| default_test_url());
    let suffix_start = url.find(['?', '#']).unwrap_or(url.len());
    let (authority_and_path, suffix) = url.split_at(suffix_start);
    let (base, _) = authority_and_path.rsplit_once('/').expect("database URL");
    let database = format!("cache_router_{}", uuid::Uuid::new_v4().simple());
    let url = format!("{base}/{database}{suffix}");
    let router = memoria_storage::DbRouter::connect(&url, 8, "cache-test".into())
        .await
        .expect("connect router");
    let mut conn = router
        .global_user_pool()
        .acquire()
        .await
        .expect("acquire user session");
    for n in 0_i64..3 {
        let got: i64 = sqlx::query_scalar("SELECT CAST(? AS BIGINT)")
            .bind(n)
            .fetch_one(&mut *conn)
            .await
            .expect("routed query");
        assert_eq!(got, n);
    }
    let cache_size = conn.cached_statements_size();
    drop(conn);
    sqlx::raw_sql(&format!("DROP DATABASE {database}"))
        .execute(router.shared_pool())
        .await
        .expect("drop test database");
    assert_eq!(
        cache_size, 0,
        "routed connections must not retain prepared statements"
    );
}

fn default_test_url() -> String {
    "mysql://root:111@localhost:6001/memoria_test".into()
}
