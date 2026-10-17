use sqlx::{
    mysql::{MySqlConnectOptions, MySqlConnection, MySqlPoolOptions},
    Connection, Executor, Row,
};
use std::time::Duration;

#[path = "support/prepare_pause_proxy.rs"]
mod prepare_pause_proxy;
use prepare_pause_proxy::PreparePauseProxy;

const SQL: &str = "SELECT CAST(? AS BIGINT), CAST(? AS BIGINT)";

fn options(capacity: usize) -> MySqlConnectOptions {
    std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| default_test_url())
        .parse::<MySqlConnectOptions>()
        .expect("parse local database URL")
        .statement_cache_capacity(capacity)
}

#[derive(Clone, Copy)]
enum Operation {
    Execute,
    Prepare,
    Describe,
}

async fn operation(conn: &mut MySqlConnection, operation: Operation) -> Result<(), sqlx::Error> {
    match operation {
        Operation::Execute => {
            sqlx::query(SQL)
                .bind(1_i64)
                .bind(2_i64)
                .execute(conn)
                .await?;
        }
        Operation::Prepare => {
            conn.prepare(SQL).await?;
        }
        Operation::Describe => {
            conn.describe(SQL).await?;
        }
    }
    Ok(())
}

async fn cancel_at_pause(conn: &mut MySqlConnection, proxy: &PreparePauseProxy, kind: Operation) {
    proxy.arm();
    let request = operation(conn, kind);
    tokio::pin!(request);
    tokio::select! {
        biased;
        _ = &mut request => panic!("operation completed before paused response"),
        _ = proxy.paused() => {},
    }
    // Let the client consume any forwarded header/metadata before cancellation.
    assert!(
        tokio::time::timeout(Duration::from_millis(20), &mut request)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn interrupted_prepare_rejects_reuse_without_reading_leftover_packets() {
    for capacity in [0, 100] {
        for boundary in [0, 1, 2] {
            for kind in [Operation::Execute, Operation::Prepare, Operation::Describe] {
                let proxy = PreparePauseProxy::start(options(capacity), boundary).await;
                let mut conn = MySqlConnection::connect_with(&proxy.options)
                    .await
                    .expect("connect via proxy");
                conn.prepare("SELECT CAST(9 AS BIGINT)")
                    .await
                    .expect("warm statement cache");
                cancel_at_pause(&mut conn, &proxy, kind).await;
                for _ in 0..2 {
                    let result = tokio::time::timeout(Duration::from_secs(1), conn.ping())
                        .await
                        .expect("interrupted preparation must not drain as an execute result");
                    assert!(matches!(result, Err(sqlx::Error::Protocol(ref message))
                        if message.contains("interrupted statement preparation")));
                }
                for retry_kind in [Operation::Execute, Operation::Prepare, Operation::Describe] {
                    let result = tokio::time::timeout(
                        Duration::from_secs(1),
                        operation(&mut conn, retry_kind),
                    )
                    .await
                    .expect("interrupted session must reject further operations");
                    assert!(matches!(result, Err(sqlx::Error::Protocol(_))));
                }
                let result = conn.clear_cached_statements().await;
                assert!(matches!(result, Err(sqlx::Error::Protocol(_))));
                let result = tokio::time::timeout(Duration::from_secs(1), conn.close())
                    .await
                    .expect("interrupted session must disconnect without draining");
                assert!(matches!(result, Err(sqlx::Error::Protocol(_))));
                proxy.assert_disconnected_without_more_commands().await;
            }
        }
    }
}

#[tokio::test]
async fn pool_replaces_connection_after_cancelled_prepare() {
    for capacity in [0, 100] {
        for boundary in [0, 1, 2] {
            let proxy = PreparePauseProxy::start(options(capacity), boundary).await;
            let pool = MySqlPoolOptions::new()
                .max_connections(1)
                .connect_with(proxy.options.clone())
                .await
                .expect("connect pool");
            let mut conn = pool.acquire().await.expect("acquire connection");
            cancel_at_pause(&mut conn, &proxy, Operation::Execute).await;
            drop(conn);
            let mut replacement = tokio::time::timeout(Duration::from_secs(2), pool.acquire())
                .await
                .expect("cancelled preparation must release pool capacity")
                .expect("acquire replacement");
            // Use the text protocol so this regression is independent of the
            // separate uncached-execution compatibility fix.
            let row = (&mut *replacement)
                .fetch_one("SELECT CAST(42 AS BIGINT)")
                .await
                .expect("query on replacement");
            let value: i64 = row.try_get(0).expect("decode replacement result");
            assert_eq!(value, 42);
            assert_eq!(proxy.connection_count(), 2);
            proxy.assert_disconnected_without_more_commands().await;
            drop(replacement);
            pool.close().await;
        }
    }
}

#[tokio::test]
async fn successful_and_server_rejected_prepares_keep_connection_reusable() {
    let mut conn = MySqlConnection::connect_with(&options(100))
        .await
        .expect("connect local database");
    for _ in 0..3 {
        let value: i64 = sqlx::query_scalar("SELECT CAST(? AS BIGINT)")
            .bind(42_i64)
            .fetch_one(&mut conn)
            .await
            .expect("prepared query");
        assert_eq!(value, 42);
    }
    let missing = format!("prepare_missing_{}", uuid::Uuid::new_v4().simple());
    let result = sqlx::query(&format!("SELECT ? FROM {missing}"))
        .bind(1_i64)
        .execute(&mut conn)
        .await;
    assert!(matches!(result, Err(sqlx::Error::Database(_))));
    conn.ping()
        .await
        .expect("server error must not poison preparation state");
    let value: i64 = sqlx::query_scalar("SELECT CAST(? AS BIGINT)")
        .bind(7_i64)
        .fetch_one(&mut conn)
        .await
        .expect("query after server error");
    assert_eq!(value, 7);
    conn.close().await.expect("close session");
}

#[tokio::test]
async fn pool_discards_cancelled_prepare_without_flushing_blocked_writes() {
    let proxy = PreparePauseProxy::stall_prepare_write(options(0)).await;
    let pool = MySqlPoolOptions::new()
        .max_connections(1)
        .connect_with(proxy.options.clone())
        .await
        .expect("connect pool");
    let mut conn = pool.acquire().await.expect("acquire connection");
    // Synthetic SQL larger than local TCP buffering. The proxy reads only its
    // command byte, so cancellation leaves bytes in SQLx's write buffer.
    let sql = format!("SELECT 1 /*{}*/", "x".repeat(32 * 1024 * 1024));
    proxy.arm();
    {
        let request = conn.prepare(&sql);
        tokio::pin!(request);
        tokio::select! {
            biased;
            _ = &mut request => panic!("PREPARE completed despite a blocked request body"),
            _ = proxy.paused() => {},
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut request)
                .await
                .is_err()
        );
    }
    assert!(conn.should_flush(), "must exercise buffered PREPARE bytes");
    drop(conn);
    let mut replacement = tokio::time::timeout(Duration::from_secs(2), pool.acquire())
        .await
        .expect("discard must not flush a blocked write")
        .expect("replacement connection");
    let row = (&mut *replacement)
        .fetch_one("SELECT CAST(42 AS BIGINT)")
        .await
        .expect("query on replacement");
    assert_eq!(row.try_get::<i64, _>(0).expect("replacement result"), 42);
    assert_eq!(proxy.connection_count(), 2);
    drop(replacement);
    pool.close().await;
}

fn default_test_url() -> String {
    "mysql://root:111@localhost:6001/memoria_test".to_string()
}
