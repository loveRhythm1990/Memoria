//! Verify ALTER lineage behavior using only disposable tables with synthetic data.
//! Never infer capability from a version label or from rel_createsql (_copy_).
use memoria_core::MemoriaError;
use sqlx::{MySqlPool, Row};
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

#[derive(Default)]
pub(crate) struct BranchAlterCapability {
    cached: Mutex<Option<(Instant, Result<(), String>)>>,
}

impl BranchAlterCapability {
    pub(crate) async fn ensure(&self, pool: &MySqlPool, schema: &str) -> Result<(), MemoriaError> {
        let pool = pool.clone();
        let schema = schema.to_owned();
        self.ensure_with(move || async move { probe(pool, &schema).await })
            .await
    }

    async fn ensure_with<F, Fut>(&self, run: F) -> Result<(), MemoriaError>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<(), String>> + Send + 'static,
    {
        let mut cache = self.cached.lock().await;
        if let Some((until, result)) = cache.as_ref() {
            if Instant::now() < *until {
                return result.clone().map_err(capability_error);
            }
        }
        // Request cancellation must not cancel the probe's cleanup. Only the
        // disposable probe task is detached; no user ALTER happens in it.
        let result = tokio::spawn(run())
            .await
            .map_err(|e| format!("capability probe task failed: {e}"))
            .and_then(|r| r);
        let ttl = if result.is_ok() { 300 } else { 30 };
        *cache = Some((Instant::now() + Duration::from_secs(ttl), result.clone()));
        result.map_err(capability_error)
    }
}

fn capability_error(reason: String) -> MemoriaError {
    MemoriaError::Database(format!(
        "Native branch ALTER capability could not be verified; user branch migration was not performed. Use a validated MatrixOne build (4.2.1-d2393868a) and check DDL permissions/connectivity. Probe: {reason}"
    ))
}

fn quote(name: &str) -> String {
    format!("`{}`", name.replace('`', "``"))
}

struct Probe {
    pool: MySqlPool,
    schema: String,
    base_name: String,
    branch_name: String,
    base: String,
    branch: String,
    base_created: bool,
    branch_created: bool,
}

impl Probe {
    fn new(pool: MySqlPool, schema: &str) -> Self {
        let id = uuid::Uuid::new_v4().simple().to_string();
        let base_name = format!("mem_lineage_probe_{id}_s");
        let branch_name = format!("mem_lineage_probe_{id}_b");
        Self {
            pool,
            schema: schema.into(),
            base: format!("{}.{}", quote(schema), quote(&base_name)),
            branch: format!("{}.{}", quote(schema), quote(&branch_name)),
            base_name,
            branch_name,
            base_created: false,
            branch_created: false,
        }
    }

    async fn exec(&self, sql: &str) -> Result<(), String> {
        tokio::time::timeout(
            Duration::from_secs(10),
            sqlx::raw_sql(sql).execute(&self.pool),
        )
        .await
        .map_err(|_| "probe statement timed out".to_string())?
        .map(|_| ())
        .map_err(|e| e.to_string())
    }

    async fn verify(&mut self) -> Result<(), String> {
        // No IF NOT EXISTS: only a successful CREATE gives us cleanup ownership.
        self.exec(&format!("CREATE TABLE {} (id INT PRIMARY KEY, user_id VARCHAR(64), content TEXT, is_active TINYINT DEFAULT 1, FULLTEXT INDEX ft_content (content) WITH PARSER ngram)", self.base)).await?;
        self.base_created = true;
        self.exec(&format!(
            "INSERT INTO {} VALUES (1, 'probe', 'base', 1)",
            self.base
        ))
        .await?;
        self.exec(&format!(
            "DATA BRANCH CREATE TABLE {} FROM {}",
            self.branch, self.base
        ))
        .await?;
        self.branch_created = true;
        // Both parent and child ALTER must preserve lineage. These operations
        // exercise the same copy/swap machinery used by historical clone migration.
        for table in [&self.base, &self.branch] {
            self.exec(&format!(
                "ALTER TABLE {table} ADD COLUMN subject_id VARCHAR(128) DEFAULT NULL AFTER user_id"
            ))
            .await?;
        }
        self.exec(&format!(
            "ALTER TABLE {} ADD INDEX idx_scope_subject_active (user_id,subject_id,is_active)",
            self.branch
        ))
        .await?;
        self.exec(&format!(
            "INSERT INTO {} (id,user_id,subject_id,content) VALUES (2,'probe','scope','branch')",
            self.branch
        ))
        .await?;
        let rows = sqlx::raw_sql(&format!(
            "DATA BRANCH DIFF {} AGAINST {} OUTPUT LIMIT 10",
            self.branch, self.base
        ))
        .fetch_all(&self.pool)
        .await
        .map_err(|e| e.to_string())?;
        if !rows.iter().any(|r| {
            r.try_get::<i32, _>("id").ok() == Some(2)
                && r.try_get::<String, _>("subject_id").ok().as_deref() == Some("scope")
                && r.try_get::<String, _>("content").ok().as_deref() == Some("branch")
        }) {
            return Err("native diff did not preserve field mapping after ALTER".into());
        }
        self.exec(&format!(
            "DATA BRANCH MERGE {} INTO {} WHEN CONFLICT SKIP",
            self.branch, self.base
        ))
        .await?;
        let value: Option<(String, String)> = sqlx::query_as(&format!(
            "SELECT subject_id,content FROM {} WHERE id=2",
            self.base
        ))
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| e.to_string())?;
        if value != Some(("scope".into(), "branch".into())) {
            return Err("native merge did not preserve fields after ALTER".into());
        }
        self.exec(&format!("DATA BRANCH DELETE TABLE {}", self.branch))
            .await?;
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM information_schema.tables WHERE table_schema=? AND table_name=?",
        )
        .bind(&self.schema)
        .bind(&self.branch_name)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| e.to_string())?;
        if count != 0 {
            return Err("native delete left a probe table behind".into());
        }
        self.branch_created = false;
        Ok(())
    }

    async fn cleanup(&mut self) -> Result<(), String> {
        if self.branch_created {
            let native = self
                .exec(&format!("DATA BRANCH DELETE TABLE {}", self.branch))
                .await;
            if let Err(error) = native {
                // Strictly local to this freshly-created probe, never exposed to
                // user/registry table names. Old builds may have materialized it.
                if !error.contains("is not an active branch table") {
                    return Err(error);
                }
                self.exec(&format!("DROP TABLE {}", self.branch)).await?;
            }
            self.branch_created = false;
        }
        if self.base_created {
            self.exec(&format!("DROP TABLE {}", self.base)).await?;
            self.base_created = false;
        }
        Ok(())
    }
}

async fn probe(pool: MySqlPool, schema: &str) -> Result<(), String> {
    let mut probe = Probe::new(pool, schema);
    let result = tokio::time::timeout(Duration::from_secs(60), probe.verify())
        .await
        .unwrap_or_else(|_| Err("capability probe timed out".into()));
    let cleanup = probe.cleanup().await;
    if result.is_err() || cleanup.is_err() {
        // An ambiguous CREATE response or process/DB failure may leave only
        // these named probe artifacts. Never scan/delete a broad table prefix.
        tracing::warn!(schema, base = %probe.base_name, branch = %probe.branch_name,
            error = ?result.as_ref().err(), cleanup_error = ?cleanup.as_ref().err(),
            "branch ALTER probe failed; inspect exact probe names if cleanup was interrupted");
    }
    // Never cache success if cleanup failed; preserve the original failure.
    result.and(cleanup)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    #[tokio::test]
    async fn concurrent_checks_share_success_and_expired_failures_retry() {
        let capability = BranchAlterCapability::default();
        let calls = Arc::new(AtomicUsize::new(0));
        let run = || {
            let calls = calls.clone();
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        };
        let (a, b) = tokio::join!(capability.ensure_with(run), capability.ensure_with(run));
        a.unwrap();
        b.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        *capability.cached.lock().await = Some((
            Instant::now() + Duration::from_secs(30),
            Err("unavailable".into()),
        ));
        assert!(capability.ensure_with(run).await.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        capability.cached.lock().await.as_mut().unwrap().0 = Instant::now();
        capability.ensure_with(run).await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn cancelled_waiter_does_not_cancel_probe_cleanup() {
        let capability = Arc::new(BranchAlterCapability::default());
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (resume_tx, resume_rx) = tokio::sync::oneshot::channel();
        let (cleaned_tx, cleaned_rx) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            capability
                .ensure_with(|| async move {
                    started_tx.send(()).unwrap();
                    resume_rx.await.unwrap();
                    cleaned_tx.send(()).unwrap();
                    Ok(())
                })
                .await
        });
        started_rx.await.unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        resume_tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(1), cleaned_rx)
            .await
            .unwrap()
            .unwrap();
    }

    #[test]
    fn probe_names_are_private_unique_and_qualified() {
        let pool = sqlx::mysql::MySqlPoolOptions::new()
            .max_lifetime(None)
            .idle_timeout(None)
            .connect_lazy_with(sqlx::mysql::MySqlConnectOptions::new());
        let a = Probe::new(pool.clone(), "db`name");
        let b = Probe::new(pool, "db`name");
        assert_ne!(a.branch_name, b.branch_name);
        assert!(a.branch.starts_with("`db``name`.`mem_lineage_probe_"));
        assert!(memoria_core::is_safe_sql_identifier(&a.branch_name));
        assert!(!a.base_created && !a.branch_created);
    }

    #[tokio::test]
    #[ignore = "requires disposable MatrixOne via DATABASE_URL; run explicitly in DB CI"]
    async fn materialized_probe_cleanup_preserves_unrelated_tables() {
        let options: sqlx::mysql::MySqlConnectOptions =
            std::env::var("DATABASE_URL").unwrap().parse().unwrap();
        let admin = MySqlPool::connect_with(options.clone().database("mo_catalog"))
            .await
            .unwrap();
        let schema = format!("probe_cleanup_{}", uuid::Uuid::new_v4().simple());
        sqlx::raw_sql(&format!("CREATE DATABASE {}", quote(&schema)))
            .execute(&admin)
            .await
            .unwrap();
        let pool = MySqlPool::connect_with(options.database(&schema))
            .await
            .unwrap();
        let mut probe = Probe::new(pool.clone(), &schema);
        probe
            .exec(&format!("CREATE TABLE {} (id INT PRIMARY KEY)", probe.base))
            .await
            .unwrap();
        probe.base_created = true;
        // Emulate an old engine materializing the branch into a regular table.
        // It is still owned solely by this probe, never a user branch.
        probe
            .exec(&format!(
                "CREATE TABLE {} (id INT PRIMARY KEY)",
                probe.branch
            ))
            .await
            .unwrap();
        probe.branch_created = true;
        sqlx::raw_sql(
            "CREATE TABLE user_data (id INT PRIMARY KEY); INSERT INTO user_data VALUES (1)",
        )
        .execute(&pool)
        .await
        .unwrap();
        probe.cleanup().await.unwrap();
        let remaining: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM information_schema.tables WHERE table_schema=? AND table_name IN (?,?)")
            .bind(&schema).bind(&probe.base_name).bind(&probe.branch_name).fetch_one(&pool).await.unwrap();
        assert_eq!(remaining, 0);
        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM user_data")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(rows, 1);
        pool.close().await;
        sqlx::raw_sql(&format!("DROP DATABASE {}", quote(&schema)))
            .execute(&admin)
            .await
            .unwrap();
    }
}
