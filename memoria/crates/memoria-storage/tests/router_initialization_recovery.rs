//! Requires DATABASE_URL for a disposable test backend. All identities and
//! databases are synthetic; neither credentials nor packet contents are logged.
use futures::FutureExt;
use memoria_storage::{store::CURRENT_USER_SCHEMA_VERSION, DbRouter, SqlMemoryStore};
use sqlx::{mysql::MySqlPoolOptions, ConnectOptions, MySqlPool};
use std::{
    io::Write,
    panic::AssertUnwindSafe,
    sync::{Arc, Mutex},
    time::Duration,
};
use uuid::Uuid;

#[path = "support/initialization_fault_proxy.rs"]
mod initialization_fault_proxy;
use initialization_fault_proxy::InitializationFaultProxy;

struct TimeoutSetting(Option<std::ffi::OsString>);

impl TimeoutSetting {
    fn set() -> Self {
        let old = std::env::var_os("MEMORIA_ROUTER_INIT_TIMEOUT_SECS");
        // Only the current-thread database test constructs a router and reads
        // this setting; the protocol-only test does not use the environment.
        // Restore it before the concurrent database fault scenarios.
        std::env::set_var("MEMORIA_ROUTER_INIT_TIMEOUT_SECS", "10");
        Self(old)
    }
}

impl Drop for TimeoutSetting {
    fn drop(&mut self) {
        match self.0.take() {
            Some(old) => std::env::set_var("MEMORIA_ROUTER_INIT_TIMEOUT_SECS", old),
            None => std::env::remove_var("MEMORIA_ROUTER_INIT_TIMEOUT_SECS"),
        }
    }
}

#[derive(Clone)]
struct LogBuffer(Arc<Mutex<Vec<u8>>>);

impl Write for LogBuffer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

async fn table_exists(admin: &MySqlPool, db: &str, table: &str) -> bool {
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM information_schema.tables WHERE table_schema = ? AND table_name = ?",
    )
    .bind(db)
    .bind(table)
    .fetch_one(admin)
    .await
    .expect("read synthetic table catalog");
    count != 0
}

async fn stats_column_exists(admin: &MySqlPool, db: &str, column: &str) -> bool {
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM information_schema.columns WHERE table_schema = ? AND table_name = 'mem_memories_stats' AND column_name = ?",
    ).bind(db).bind(column).fetch_one(admin).await.expect("read synthetic column catalog");
    count != 0
}

async fn register_group(router: &DbRouter, db: &str) -> String {
    sqlx::raw_sql(&format!("CREATE DATABASE `{db}`"))
        .execute(router.shared_pool())
        .await
        .expect("create group fixture");
    let group = format!("grp_{}", Uuid::new_v4().simple());
    let now = chrono::Utc::now().naive_utc();
    sqlx::query(
        "INSERT INTO mem_groups (group_id, group_name, db_name, owner_user_id, status, created_at, updated_at) VALUES (?, 'Synthetic recovery group', ?, 'synthetic-owner', 'active', ?, ?)",
    ).bind(&group).bind(db).bind(now).bind(now).execute(router.shared_pool()).await.expect("register synthetic group");
    group
}

async fn assert_interrupted_ddl_recovers(
    router: &DbRouter,
    admin: &MySqlPool,
    proxy: &InitializationFaultProxy,
    scope: &str,
    db: &str,
    compatibility: bool,
) {
    if compatibility {
        for column in ["feedback_useful", "feedback_irrelevant"] {
            sqlx::raw_sql(&format!(
                "ALTER TABLE `{db}`.mem_memories_stats DROP COLUMN {column}"
            ))
            .execute(admin)
            .await
            .expect("remove synthetic legacy column");
        }
        sqlx::query(&format!(
            "UPDATE `{db}`.mem_schema_meta SET schema_version = 0"
        ))
        .execute(admin)
        .await
        .expect("mark compatibility migration pending");
        router
            .invalidate_scope(scope)
            .await
            .expect("invalidate before compatibility migration");
        proxy.pause_ddl_response("alter table", "add column feedback_useful");
    } else {
        // bootstrap creates mem_memories before mem_user_state. Holding the
        // second CREATE's OK leaves a real committed prefix of the schema.
        proxy.pause_ddl_response("create table", "mem_user_state");
    }
    let initializer = router.scope_store(scope);
    tokio::pin!(initializer);
    tokio::select! {
        biased;
        _ = &mut initializer => panic!("initialization must not complete before the held DDL reply"),
        _ = proxy.ddl_committed_with_reply_held() => {},
    }
    // The backend has returned OK, but the initializer has not seen it. Verify
    // both a committed change and a later, still-missing migration step.
    if compatibility {
        assert!(stats_column_exists(admin, db, "feedback_useful").await);
        assert!(!stats_column_exists(admin, db, "feedback_irrelevant").await);
    } else {
        assert!(table_exists(admin, db, "mem_memories").await);
        assert!(table_exists(admin, db, "mem_user_state").await);
        assert!(!table_exists(admin, db, "mem_branches").await);
    }
    let error = tokio::time::timeout(Duration::from_secs(15), &mut initializer)
        .await
        .expect("router's own initialization deadline must fire")
        .err()
        .expect("pending DDL must not publish a successful store");
    assert!(
        matches!(error, memoria_core::MemoriaError::Database(ref message) if message.contains("initialization timed out"))
    );
    if compatibility {
        let version: i64 = sqlx::query_scalar(&format!(
            "SELECT schema_version FROM `{db}`.mem_schema_meta WHERE schema_key = 'user_schema'"
        ))
        .fetch_one(admin)
        .await
        .expect("read incomplete migration version");
        assert_eq!(
            version, 0,
            "interrupted migration must not publish a current version"
        );
    } else {
        assert!(!table_exists(admin, db, "mem_schema_meta").await);
    }
    proxy.release_ddl_response();
    // Reuse the same router: a leaked store/readiness cache entry would skip
    // the missing steps and make these catalog assertions fail.
    let store = router
        .scope_store(scope)
        .await
        .expect("same-router retry repairs partial schema");
    let cached = router
        .scope_store(scope)
        .await
        .expect("successful retry is cached");
    assert!(Arc::ptr_eq(&store, &cached));
    assert!(table_exists(admin, db, "mem_branches").await);
    assert!(stats_column_exists(admin, db, "feedback_useful").await);
    assert!(stats_column_exists(admin, db, "feedback_irrelevant").await);
    let version: i64 = sqlx::query_scalar(&format!(
        "SELECT schema_version FROM `{db}`.mem_schema_meta WHERE schema_key = 'user_schema'"
    ))
    .fetch_one(admin)
    .await
    .expect("read repaired migration version");
    assert_eq!(version, CURRENT_USER_SCHEMA_VERSION);
}

async fn registration_survives_failed_initialization_cleanup(
    router: &DbRouter,
    proxy: &InitializationFaultProxy,
    databases: &mut Vec<String>,
) {
    let user = format!("registration_race_{}", Uuid::new_v4().simple());
    let db = DbRouter::user_db_name_for_id(&user);
    databases.push(db.clone());
    proxy.reject_init_and_pause_cleanup();
    let initializer_router = router.clone();
    let initializer_user = user.clone();
    let initializer =
        tokio::spawn(async move { initializer_router.user_store(&initializer_user).await });
    proxy.cleanup_is_paused().await;
    let mut registration = Box::pin(router.register_user_db(&user, &db));
    let early = tokio::time::timeout(Duration::from_secs(1), &mut registration).await;
    let completed_before_cleanup = early.is_ok();
    proxy.resume_cleanup();
    match early {
        Ok(result) => result.expect("registration succeeds"),
        Err(_) => registration
            .as_mut()
            .await
            .expect("registration follows failed initialization cleanup"),
    }
    drop(registration);
    assert!(initializer.await.expect("initializer task").is_err());
    let remaining: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM mem_user_registry WHERE user_id = ? AND db_name = ? AND status = 'active'",
    ).bind(&user).bind(&db).fetch_one(router.shared_pool()).await.expect("inspect final registration");
    assert!(
        !completed_before_cleanup,
        "registration must wait for the initializer's gate"
    );
    assert_eq!(
        remaining, 1,
        "successful registration must survive compensating cleanup"
    );
    let repaired = router
        .user_store(&user)
        .await
        .expect("registered user initializes on retry");
    assert_eq!(repaired.db_name(), Some(db.as_str()));
}

async fn registration_retargets_warm_caches(
    router: &DbRouter,
    admin: &MySqlPool,
    databases: &mut Vec<String>,
) {
    let user = format!("retarget_{}", Uuid::new_v4().simple());
    let original_db = DbRouter::user_db_name_for_id(&user);
    let replacement = format!("registration_replacement_{}", Uuid::new_v4().simple());
    databases.extend([original_db.clone(), replacement.clone()]);
    let original = router
        .user_store(&user)
        .await
        .expect("warm original schema and store");
    assert_eq!(
        router
            .user_db_name(&user)
            .await
            .expect("warm original mapping"),
        original_db
    );
    sqlx::raw_sql(&format!("CREATE DATABASE `{replacement}`"))
        .execute(admin)
        .await
        .expect("create empty replacement database");
    router
        .register_user_db(&user, &replacement)
        .await
        .expect("retarget existing registration");
    assert_eq!(
        router.user_db_name(&user).await.expect("refresh mapping"),
        replacement
    );
    let refreshed = router
        .user_store(&user)
        .await
        .expect("initialize replacement schema");
    assert!(!Arc::ptr_eq(&original, &refreshed));
    assert_eq!(refreshed.db_name(), Some(replacement.as_str()));
    let version: i64 = sqlx::query_scalar(&format!("SELECT schema_version FROM `{replacement}`.mem_schema_meta WHERE schema_key = 'user_schema'"))
        .fetch_one(admin).await.expect("replacement must not reuse old schema readiness");
    assert_eq!(version, CURRENT_USER_SCHEMA_VERSION);
}

async fn exercise_recovery(
    router: &DbRouter,
    admin: &MySqlPool,
    proxy: &InitializationFaultProxy,
    databases: &mut Vec<String>,
) {
    registration_survives_failed_initialization_cleanup(router, proxy, databases).await;
    registration_retargets_warm_caches(router, admin, databases).await;
    SqlMemoryStore::new(router.shared_pool().clone(), 8, "synthetic-recovery".into())
        .migrate_shared()
        .await
        .expect("initialize shared group registry");
    for group in [false, true] {
        let (scope, db) = if group {
            let db = format!("router_recovery_group_{}", Uuid::new_v4().simple());
            databases.push(db.clone());
            (register_group(router, &db).await, db)
        } else {
            let user = format!("recovery_{}", Uuid::new_v4().simple());
            let db = DbRouter::user_db_name_for_id(&user);
            databases.push(db.clone());
            (user, db)
        };
        for compatibility in [false, true] {
            assert_interrupted_ddl_recovers(router, admin, proxy, &scope, &db, compatibility).await;
        }
    }

    let user = format!("cleanup_recovery_{}", Uuid::new_v4().simple());
    let db = DbRouter::user_db_name_for_id(&user);
    databases.push(db.clone());
    let bytes = Arc::new(Mutex::new(Vec::new()));
    let writer = LogBuffer(bytes.clone());
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::WARN)
        .without_time()
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .finish();
    let guard = tracing::subscriber::set_default(subscriber);
    proxy.reject_initialization_and_cleanup();
    let error = router
        .user_store(&user)
        .await
        .err()
        .expect("injected initialization failure");
    drop(guard);
    assert!(
        matches!(error, memoria_core::MemoriaError::Database(ref message) if message.contains("synthetic initialization failure"))
    );
    assert_eq!(
        proxy.cleanup_rejections(),
        1,
        "must exercise the failed compensating DELETE"
    );
    let log = String::from_utf8(bytes.lock().unwrap().clone()).unwrap();
    let warning = log
        .lines()
        .find(|line| line.contains("User registry cleanup failed"))
        .expect("cleanup failure must emit a warning");
    assert!(warning.contains("error_kind=\"database\""));
    assert!(!warning.contains(&user) && !warning.contains(&db));
    assert!(
        !warning.contains("synthetic cleanup failure"),
        "do not log raw database error text"
    );
    let active: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM mem_user_registry WHERE user_id = ? AND status = 'active'",
    )
    .bind(&user)
    .fetch_one(router.shared_pool())
    .await
    .expect("read retained registration");
    assert_eq!(active, 1);
    let repaired = router
        .user_store(&user)
        .await
        .expect("retained registration still runs migration");
    assert_eq!(repaired.db_name(), Some(db.as_str()));
    assert!(table_exists(admin, &db, "mem_memories").await);
    let version: i64 = sqlx::query_scalar(&format!(
        "SELECT schema_version FROM `{db}`.mem_schema_meta WHERE schema_key = 'user_schema'"
    ))
    .fetch_one(admin)
    .await
    .expect("read schema version after failed cleanup recovery");
    assert_eq!(version, CURRENT_USER_SCHEMA_VERSION);
}

#[tokio::test(flavor = "current_thread")]
async fn routed_initialization_recovers_after_committed_ddl_timeout_and_failed_cleanup() {
    let options = std::env::var("DATABASE_URL")
        .expect("DATABASE_URL must select a disposable test backend")
        .parse::<sqlx::mysql::MySqlConnectOptions>()
        .expect("parse test database options");
    let admin = MySqlPoolOptions::new()
        .max_connections(1)
        .connect_with(options.clone())
        .await
        .expect("connect fixture administrator");
    let shared_db = format!("router_recovery_shared_{}", Uuid::new_v4().simple());
    let proxy = InitializationFaultProxy::start(options.database(&shared_db)).await;
    let setting = TimeoutSetting::set();
    let router = DbRouter::connect(
        proxy.options.to_url_lossy().as_str(),
        8,
        "synthetic-recovery".into(),
    )
    .await
    .expect("connect router through fault proxy");
    drop(setting);
    let mut databases = Vec::new();
    let outcome = AssertUnwindSafe(exercise_recovery(&router, &admin, &proxy, &mut databases))
        .catch_unwind()
        .await;
    proxy.release_ddl_response();
    proxy.resume_cleanup();
    for db in databases.iter().chain(std::iter::once(&shared_db)) {
        sqlx::raw_sql(&format!("DROP DATABASE IF EXISTS `{db}`"))
            .execute(&admin)
            .await
            .expect("remove owned synthetic database");
    }
    drop(router);
    drop(proxy);
    admin.close().await;
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}
