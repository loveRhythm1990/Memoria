//! Offline recovery tool. Read-only by default; never run deletion alongside
//! Memoria servers, workers, or MCP processes. See legacy-snapshot-compatibility.md.
use sqlx::{MySql, MySqlPool, QueryBuilder};

fn candidate(name: &str) -> bool {
    if let Some(id) = name.strip_prefix("mem_restore_") {
        return id.len() == 32 && id.bytes().all(|c| c.is_ascii_hexdigit());
    }
    if let Some(branch) = name.strip_prefix("br_") {
        if let Some((id, suffix)) = branch.split_once('_') {
            return id.len() == 8
                && id.bytes().all(|c| c.is_ascii_hexdigit())
                && memoria_core::is_safe_sql_identifier(suffix);
        }
    }
    false
}

fn delete_target(args: &[String]) -> Result<Option<&str>, &'static str> {
    match args {
        [] => Ok(None),
        [flag, name, offline] if flag == "--delete" && offline == "--all-writers-stopped" => {
            if candidate(name) {
                Ok(Some(name))
            } else {
                Err("Refusing unexpected table name; use an exact generated stage/branch name")
            }
        }
        _ => Err("Usage: restore_cleanup [--delete EXACT_TABLE --all-writers-stopped]"),
    }
}

async fn registered(pool: &MySqlPool, table: &str) -> Result<bool, sqlx::Error> {
    // Fail closed if the registry cannot be read. Protect every registered
    // branch, not just 'active' branches, including an ambiguous registration.
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM mem_branches WHERE table_name = ?")
        .bind(table)
        .fetch_one(pool)
        .await?;
    Ok(count != 0)
}

async fn delete_unregistered(
    pool: &MySqlPool,
    table: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    if !candidate(table) {
        return Err("Refusing unexpected table name".into());
    }
    if registered(pool, table).await? {
        return Err("Refusing to delete a registered branch".into());
    }
    let mut ddl = QueryBuilder::<MySql>::new(if table.starts_with("br_") {
        "DATA BRANCH DELETE TABLE "
    } else {
        "DROP TABLE "
    });
    ddl.push("`").push(table).push("`");
    sqlx::raw_sql(&ddl.into_sql()).execute(pool).await?;
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let delete = delete_target(&args)?;
    let pool = MySqlPool::connect(&std::env::var("DATABASE_URL")?).await?;
    let database: String = sqlx::query_scalar("SELECT DATABASE()")
        .fetch_one(&pool)
        .await?;
    if matches!(
        database.as_str(),
        "mo_catalog" | "mysql" | "information_schema" | "system"
    ) {
        return Err("Select a single Memoria user database, not a system database".into());
    }
    let tables: Vec<String> = sqlx::query_scalar(
        "SELECT table_name FROM information_schema.tables WHERE table_schema = ? ORDER BY table_name",
    )
    .bind(&database)
    .fetch_all(&pool)
    .await?;
    if let Some(table) = delete {
        if !tables.iter().any(|t| t == table) {
            return Err("Target table does not exist in the selected database".into());
        }
        // The operator's offline confirmation is essential: absence from the
        // registry is not proof that a live restore/branch creation is abandoned.
        // This tool cannot fence other processes and is NEVER a scheduled reaper.
        delete_unregistered(&pool, table).await?;
        println!("Deleted {database}.{table}. No automatic undo; recovery requires a retained backup/snapshot.");
    } else {
        println!("Read-only inspection of {database}. Candidates may still be in use; stop ALL writers before deletion.");
        for table in tables.into_iter().filter(|t| candidate(t)) {
            let state = if registered(&pool, &table).await? {
                "protected: registered branch"
            } else {
                "unregistered: investigate; NOT proof of abandonment"
            };
            println!("{table}\t{state}");
        }
    }
    pool.close().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deletion_requires_exact_target_and_offline_confirmation() {
        assert_eq!(delete_target(&[]), Ok(None));
        let name = "mem_restore_0123456789abcdef0123456789abcdef";
        assert!(delete_target(&["--delete".into(), name.into()]).is_err());
        assert_eq!(
            delete_target(&[
                "--delete".into(),
                name.into(),
                "--all-writers-stopped".into()
            ]),
            Ok(Some(name))
        );
    }

    #[test]
    fn reject_broad_prefixes_non_generated_names_and_injection() {
        for name in [
            "mem_memories",
            "mem_restore_%",
            "mem_restore_a",
            "br_%",
            "br_live",
            "br_12345678_",
            "br_12345678_x`; DROP TABLE mem_memories",
            "br_12345678_实验`; DROP TABLE mem_memories",
            "br_12345678_实验/表",
            "memXrestore_0123456789abcdef0123456789abcdef",
        ] {
            assert!(!candidate(name), "{name}");
        }
        assert!(candidate("br_1234abcd_my_branch"));
        assert!(candidate("br_1234abcd_实验"));
        assert!(candidate("mem_restore_0123456789abcdef0123456789abcdef"));
    }

    #[tokio::test]
    #[ignore = "requires a disposable MatrixOne server via DATABASE_URL"]
    async fn offline_cleanup_protects_registered_branches_and_fails_closed() {
        let options: sqlx::mysql::MySqlConnectOptions =
            std::env::var("DATABASE_URL").unwrap().parse().unwrap();
        let admin = MySqlPool::connect_with(options.clone().database("mo_catalog"))
            .await
            .unwrap();
        let db = format!("cleanup_test_{}", uuid::Uuid::new_v4().simple());
        let mut ddl = QueryBuilder::<MySql>::new("CREATE DATABASE ");
        ddl.push(&db);
        sqlx::raw_sql(&ddl.into_sql())
            .execute(&admin)
            .await
            .unwrap();
        let pool = MySqlPool::connect_with(options.database(&db))
            .await
            .unwrap();
        sqlx::raw_sql("CREATE TABLE mem_branches (table_name VARCHAR(100), status VARCHAR(20)); CREATE TABLE br_12345678_registered (id INT); CREATE TABLE br_12345678_orphan (id INT); CREATE TABLE mem_restore_0123456789abcdef0123456789abcdef (id INT); INSERT INTO mem_branches VALUES ('br_12345678_registered', 'inactive')")
            .execute(&pool).await.unwrap();
        sqlx::raw_sql("CREATE TABLE `br_12345678_已有` (id INT); CREATE TABLE `br_12345678_孤儿` (id INT); INSERT INTO mem_branches VALUES ('br_12345678_已有', 'active')")
            .execute(&pool).await.unwrap();
        let protected = delete_unregistered(&pool, "br_12345678_registered").await;
        let unicode_protected = delete_unregistered(&pool, "br_12345678_已有").await;
        let unicode_orphan = delete_unregistered(&pool, "br_12345678_孤儿").await;
        let unexpected = delete_unregistered(&pool, "mem_branches").await;
        let orphan = delete_unregistered(&pool, "br_12345678_orphan").await;
        let stage =
            delete_unregistered(&pool, "mem_restore_0123456789abcdef0123456789abcdef").await;
        sqlx::raw_sql("DROP TABLE mem_branches; CREATE TABLE br_12345678_unverified (id INT)")
            .execute(&pool)
            .await
            .unwrap();
        let closed = delete_unregistered(&pool, "br_12345678_unverified").await;
        let tables: Vec<String> = sqlx::query_scalar("SELECT table_name FROM information_schema.tables WHERE table_schema = ? ORDER BY table_name")
            .bind(&db).fetch_all(&pool).await.unwrap();
        pool.close().await;
        let mut drop = QueryBuilder::<MySql>::new("DROP DATABASE ");
        drop.push(&db);
        sqlx::raw_sql(&drop.into_sql())
            .execute(&admin)
            .await
            .unwrap();
        assert!(protected.is_err());
        assert!(unicode_protected.is_err());
        assert!(unicode_orphan.is_ok(), "{unicode_orphan:?}");
        assert!(unexpected.is_err());
        assert!(orphan.is_ok(), "{orphan:?}");
        assert!(stage.is_ok(), "{stage:?}");
        assert!(closed.is_err());
        assert_eq!(tables.len(), 3);
        for name in [
            "br_12345678_registered",
            "br_12345678_unverified",
            "br_12345678_已有",
        ] {
            assert!(tables.iter().any(|t| t == name), "{tables:?}");
        }
    }
}
