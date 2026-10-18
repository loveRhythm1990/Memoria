//! Model delayed cross-session DDL visibility without relying on timing or a
//! particular MatrixOne build. The proxy forwards to a real local test database
//! but rejects metadata access on sessions that have not observed its CREATE.
use memoria_storage::SqlMemoryStore;
use sqlx::mysql::{MySqlConnectOptions, MySqlPoolOptions, MySqlSslMode};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use tokio::{
    io::AsyncWriteExt,
    net::{TcpListener, TcpStream},
    sync::Mutex,
};
use uuid::Uuid;

#[path = "support/mysql_packet.rs"]
mod mysql_packet;
use mysql_packet::packet;

async fn proxy_session(client: TcpStream, backend: String, rejected: Arc<AtomicUsize>) {
    let Ok(server) = TcpStream::connect(backend).await else {
        return;
    };
    let (mut sr, mut sw) = server.into_split();
    let (mut cr, cw) = client.into_split();
    let cw = Arc::new(Mutex::new(cw));
    let output = cw.clone();
    let replies = async move {
        while let Ok(bytes) = packet(&mut sr).await {
            if output.lock().await.write_all(&bytes).await.is_err() {
                break;
            }
        }
    };
    let requests = async move {
        let mut saw_create = false;
        while let Ok(bytes) = packet(&mut cr).await {
            // COM_QUERY / COM_STMT_PREPARE, not authentication or bound parameters.
            if bytes.len() > 5 && bytes[3] == 0 && matches!(bytes[4], 0x03 | 0x16) {
                let sql = String::from_utf8_lossy(&bytes[5..]).to_ascii_lowercase();
                if sql.contains("mem_schema_meta") {
                    if sql.trim_start().starts_with("create table") {
                        saw_create = true;
                    } else if !saw_create
                        && (sql.starts_with("insert ") || sql.starts_with("select "))
                    {
                        rejected.fetch_add(1, Ordering::SeqCst);
                        let mut payload = vec![0xff, 0x7a, 0x04]; // ER_NO_SUCH_TABLE (1146)
                        payload.extend_from_slice(b"#42S02no such table: mem_schema_meta");
                        let mut response = vec![payload.len() as u8, 0, 0, 1];
                        response.extend_from_slice(&payload);
                        if cw.lock().await.write_all(&response).await.is_err() {
                            break;
                        }
                        continue;
                    }
                }
            }
            if sw.write_all(&bytes).await.is_err() {
                break;
            }
        }
    };
    tokio::select! {
        _ = replies => {},
        _ = requests => {},
    }
}

#[tokio::test]
async fn schema_marker_create_and_use_stay_on_the_same_session() {
    let url = std::env::var("DATABASE_URL").unwrap_or_else(|_| default_test_url());
    // Preserve credentials and database parameters; only replace the authority's
    // host/port with the localhost fault-injection proxy.
    let (prefix, rest) = url.split_once("://").expect("database URL scheme");
    let (authority, path) = rest.split_once('/').expect("database URL path");
    let (credentials, host) = authority
        .rsplit_once('@')
        .expect("test database credentials");
    let backend = if host.contains(':') {
        host.to_string()
    } else {
        format!("{host}:3306")
    };
    let admin = MySqlPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
        .expect("connect test database");
    let db = format!("schema_visibility_{}", Uuid::new_v4().simple());
    sqlx::raw_sql(&format!("CREATE DATABASE `{db}`"))
        .execute(&admin)
        .await
        .expect("create isolated test database");
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind proxy");
    let addr = listener.local_addr().unwrap();
    let rejected = Arc::new(AtomicUsize::new(0));
    let counter = rejected.clone();
    let proxy = tokio::spawn(async move {
        let mut sessions = tokio::task::JoinSet::new();
        while let Ok((client, _)) = listener.accept().await {
            sessions.spawn(proxy_session(client, backend.clone(), counter.clone()));
        }
    });
    let proxy_url = format!("{prefix}://{credentials}@{addr}/{path}");
    let mut outcomes = Vec::new();
    for phase in 0..5 {
        if phase == 1 {
            // Interrupted initialization: ordinary tables exist, metadata does not.
            sqlx::raw_sql(&format!("DROP TABLE `{db}`.mem_schema_meta"))
                .execute(&admin)
                .await
                .expect("remove test schema marker");
        } else if phase == 2 {
            sqlx::query(&format!(
                "UPDATE `{db}`.mem_schema_meta SET schema_version = 0"
            ))
            .execute(&admin)
            .await
            .expect("make test schema marker stale");
        }
        let options: MySqlConnectOptions = proxy_url.parse().expect("parse proxy URL");
        let connections = if phase == 4 { 1 } else { 2 };
        let pool = MySqlPoolOptions::new()
            .min_connections(connections)
            .max_connections(connections)
            .connect_with(options.ssl_mode(MySqlSslMode::Disabled))
            .await
            .expect("connect proxy");
        // Prewarm two sessions so separate pool executions can switch sessions.
        let a = pool.acquire().await.unwrap();
        let b = if connections > 1 {
            Some(pool.acquire().await.unwrap())
        } else {
            None
        };
        drop(a);
        drop(b);
        let mut store = SqlMemoryStore::new(pool.clone(), 8, "schema-visibility-test".into());
        store.set_db_name(db.clone());
        let result = store.migrate_user().await;
        pool.close().await;
        let failed = result.is_err();
        outcomes.push(result);
        if failed {
            break;
        }
        let version: i64 = sqlx::query_scalar(&format!(
            "SELECT schema_version FROM `{db}`.mem_schema_meta WHERE schema_key = 'user_schema'"
        ))
        .fetch_one(&admin)
        .await
        .expect("read test schema version");
        assert_eq!(version, memoria_storage::store::CURRENT_USER_SCHEMA_VERSION);
    }
    proxy.abort();
    sqlx::raw_sql(&format!("DROP DATABASE `{db}`"))
        .execute(&admin)
        .await
        .expect("remove isolated test database");
    admin.close().await;
    for (phase, result) in outcomes.iter().enumerate() {
        assert!(
            result.is_ok(),
            "migration phase {phase} must tolerate cross-session DDL visibility: {result:?}"
        );
    }
    assert_eq!(outcomes.len(), 5);
    assert_eq!(
        rejected.load(Ordering::SeqCst),
        0,
        "metadata access crossed sessions"
    );
}

fn default_test_url() -> String {
    "mysql://root:111@localhost:6001/memoria_test".to_string()
}
