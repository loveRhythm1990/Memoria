use futures::TryStreamExt;
use sqlx::{
    mysql::{MySqlConnectOptions, MySqlConnection, MySqlPoolOptions},
    Connection,
};

#[tokio::test]
async fn uncached_prepared_queries_preserve_protocol_state() {
    let mut conn = connect_uncached().await;
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
    let mut conn = connect_uncached().await;
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
    let url = database_url();
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
    let url = database_url();
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

fn database_url() -> String {
    std::env::var("DATABASE_URL").unwrap_or_else(|_| default_test_url())
}

async fn connect_uncached() -> MySqlConnection {
    let options: MySqlConnectOptions = database_url().parse().expect("parse local database URL");
    MySqlConnection::connect_with(&options.statement_cache_capacity(0))
        .await
        .expect("connect local database")
}

#[tokio::test]
async fn dropped_transactions_rollback_after_abandoned_rows() {
    let options: MySqlConnectOptions = database_url().parse().expect("parse local database URL");
    let pool = MySqlPoolOptions::new()
        .max_connections(1)
        .connect_with(options.statement_cache_capacity(0))
        .await
        .expect("connect pool");
    let table = format!("cache_rollback_{}", uuid::Uuid::new_v4().simple());
    sqlx::raw_sql(&format!("CREATE TABLE {table} (id BIGINT PRIMARY KEY)"))
        .execute(&pool)
        .await
        .expect("create test table");
    let select = "SELECT CAST(? AS BIGINT) UNION ALL SELECT CAST(2 AS BIGINT)";
    // Nested drop must execute rollback-to-savepoint before the outer rollback.
    let mut conn = pool.acquire().await.expect("acquire connection");
    let mut txn = conn.begin().await.expect("outer transaction");
    sqlx::query(&format!("INSERT INTO {table} VALUES (?)"))
        .bind(1_i64)
        .execute(&mut *txn)
        .await
        .expect("outer write");
    let mut nested = txn.begin().await.expect("savepoint");
    sqlx::query(&format!("INSERT INTO {table} VALUES (?)"))
        .bind(2_i64)
        .execute(&mut *nested)
        .await
        .expect("nested write");
    let mut rows = sqlx::query(select).bind(1_i64).fetch(&mut *nested);
    assert!(rows.try_next().await.expect("first row").is_some());
    drop(rows);
    drop(nested);
    drop(txn);
    // Returning to the pool must flush the rollback without waiting for another query.
    drop(conn);
    let count: i64 = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {table}")).fetch_one(&pool),
    )
    .await
    .expect("pool reuse must not hang")
    .expect("count after rollback");
    assert_eq!(count, 0);
    sqlx::raw_sql(&format!("DROP TABLE {table}"))
        .execute(&pool)
        .await
        .expect("drop test table");
    pool.close().await;
}

#[tokio::test]
async fn clear_cached_statements_drains_an_abandoned_uncached_response() {
    let options: MySqlConnectOptions = database_url().parse().expect("parse local database URL");
    let mut conn = MySqlConnection::connect_with(&options.statement_cache_capacity(10))
        .await
        .expect("connect cached session");
    let _: i64 = sqlx::query_scalar("SELECT CAST(? AS BIGINT)")
        .bind(7_i64)
        .fetch_one(&mut conn)
        .await
        .expect("warm statement cache");
    assert_eq!(conn.cached_statements_size(), 1);
    let mut rows = sqlx::query("SELECT CAST(? AS BIGINT) UNION ALL SELECT CAST(2 AS BIGINT)")
        .bind(1_i64)
        .persistent(false)
        .fetch(&mut conn);
    assert!(rows.try_next().await.expect("first row").is_some());
    drop(rows);
    conn.clear_cached_statements()
        .await
        .expect("clear after partial result");
    assert_eq!(conn.cached_statements_size(), 0);
    conn.ping().await.expect("session remains synchronized");
    conn.close().await.expect("close session");
}

#[tokio::test]
async fn graceful_and_hard_close_accept_abandoned_results() {
    for hard in [false, true] {
        let mut conn = connect_uncached().await;
        let mut rows = sqlx::query("SELECT CAST(? AS BIGINT) UNION ALL SELECT CAST(2 AS BIGINT)")
            .bind(1_i64)
            .fetch(&mut conn);
        assert!(rows.try_next().await.expect("first row").is_some());
        drop(rows);
        let close = if hard {
            conn.close_hard()
        } else {
            conn.close()
        };
        tokio::time::timeout(std::time::Duration::from_secs(5), close)
            .await
            .expect("close must complete")
            .expect("close session");
    }
}

#[path = "support/command_order_proxy.rs"]
mod command_order_proxy;

async fn verify_cleanup_command_order(action: &str) {
    let options: MySqlConnectOptions = database_url().parse().expect("parse local database URL");
    let proxy =
        command_order_proxy::CommandOrderProxy::start(options.statement_cache_capacity(10)).await;
    let mut conn = MySqlConnection::connect_with(&proxy.options)
        .await
        .expect("connect via proxy");
    let _: i64 = sqlx::query_scalar("SELECT CAST(? AS BIGINT)")
        .bind(7_i64)
        .fetch_one(&mut conn)
        .await
        .expect("warm statement cache");
    let select = "SELECT CAST(? AS BIGINT) UNION ALL SELECT CAST(2 AS BIGINT)";
    if action == "rollback" {
        let mut txn = conn.begin().await.expect("begin transaction");
        proxy.arm();
        let mut rows = sqlx::query(select)
            .bind(1_i64)
            .persistent(false)
            .fetch(&mut *txn);
        assert!(rows.try_next().await.expect("first row").is_some());
        drop(rows);
        drop(txn);
        conn.ping().await.expect("drain and rollback");
    } else {
        proxy.arm();
        let mut rows = sqlx::query(select)
            .bind(1_i64)
            .persistent(false)
            .fetch(&mut conn);
        assert!(rows.try_next().await.expect("first row").is_some());
        drop(rows);
        if action == "clear" {
            conn.clear_cached_statements()
                .await
                .expect("drain and clear cache");
        }
    }
    conn.close().await.expect("drain and close");
    proxy.assert_ordered().await;
}

#[tokio::test]
async fn wire_order_rollback_waits_for_response_tail() {
    verify_cleanup_command_order("rollback").await;
}

#[tokio::test]
async fn wire_order_cache_clear_waits_for_response_tail() {
    verify_cleanup_command_order("clear").await;
}

#[tokio::test]
async fn wire_order_graceful_close_waits_for_response_tail() {
    verify_cleanup_command_order("close").await;
}

#[tokio::test]
async fn hard_close_does_not_wait_for_a_stalled_response() {
    let options: MySqlConnectOptions = database_url().parse().expect("parse local database URL");
    let proxy =
        command_order_proxy::CommandOrderProxy::start(options.statement_cache_capacity(0)).await;
    let mut conn = MySqlConnection::connect_with(&proxy.options)
        .await
        .expect("connect via proxy");
    proxy.stall_tail();
    proxy.arm();
    let mut rows = sqlx::query("SELECT CAST(? AS BIGINT) UNION ALL SELECT CAST(2 AS BIGINT)")
        .bind(1_i64)
        .fetch(&mut conn);
    assert!(rows.try_next().await.expect("first row").is_some());
    drop(rows);
    proxy.wait_until_stalled().await;
    tokio::time::timeout(std::time::Duration::from_secs(1), conn.close_hard())
        .await
        .expect("hard close must not drain stalled responses")
        .expect("hard close");
    proxy.assert_ordered().await;
}

#[tokio::test]
async fn graceful_close_times_out_and_disconnects_a_stalled_response() {
    let options: MySqlConnectOptions = database_url().parse().expect("parse local database URL");
    let proxy =
        command_order_proxy::CommandOrderProxy::start(options.statement_cache_capacity(0)).await;
    let mut conn = MySqlConnection::connect_with(&proxy.options)
        .await
        .expect("connect via proxy");
    proxy.stall_tail();
    proxy.arm();
    let mut rows = sqlx::query("SELECT CAST(? AS BIGINT) UNION ALL SELECT CAST(2 AS BIGINT)")
        .bind(1_i64)
        .fetch(&mut conn);
    assert!(rows.try_next().await.expect("first row").is_some());
    drop(rows);
    proxy.wait_until_stalled().await;
    let result = tokio::time::timeout(std::time::Duration::from_secs(7), conn.close())
        .await
        .expect("graceful close must enforce its own deadline");
    assert!(matches!(result, Err(sqlx::Error::Io(ref error))
        if error.kind() == std::io::ErrorKind::TimedOut));
    // The timeout must drop the owned socket even though no final response arrives.
    proxy.assert_ordered().await;
}

#[tokio::test]
async fn expired_connection_return_releases_capacity_after_stalled_response() {
    let options: MySqlConnectOptions = database_url().parse().expect("parse local database URL");
    let proxy =
        command_order_proxy::CommandOrderProxy::start(options.statement_cache_capacity(0)).await;
    let pool = MySqlPoolOptions::new()
        .max_connections(1)
        .max_lifetime(Some(std::time::Duration::from_secs(1)))
        .connect_with(proxy.options.clone())
        .await
        .expect("connect pool via proxy");
    let mut conn = pool.acquire().await.expect("acquire connection");
    proxy.stall_tail();
    proxy.arm();
    let mut rows = sqlx::query("SELECT CAST(? AS BIGINT) UNION ALL SELECT CAST(2 AS BIGINT)")
        .bind(1_i64)
        .fetch(&mut *conn);
    assert!(rows.try_next().await.expect("first row").is_some());
    drop(rows);
    proxy.wait_until_stalled().await;
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    // Exercise the expired-connection branch while the pool is still open.
    tokio::time::timeout(std::time::Duration::from_secs(7), conn.return_to_pool())
        .await
        .expect("expired connection must release its permit");
    assert_eq!(pool.size(), 0);
    tokio::time::timeout(std::time::Duration::from_secs(1), pool.close())
        .await
        .expect("pool shutdown must complete");
    proxy.assert_ordered().await;
}
