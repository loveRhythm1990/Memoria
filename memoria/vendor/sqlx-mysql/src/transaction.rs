use std::borrow::Cow;

use futures_core::future::BoxFuture;

use crate::connection::Waiting;
use crate::error::Error;
use crate::executor::Executor;
use crate::{MySql, MySqlConnection};

pub(crate) use sqlx_core::transaction::*;

/// Implementation of [`TransactionManager`] for MySQL.
pub struct MySqlTransactionManager;

impl TransactionManager for MySqlTransactionManager {
    type Database = MySql;

    fn begin<'conn>(
        conn: &'conn mut MySqlConnection,
        statement: Option<Cow<'static, str>>,
    ) -> BoxFuture<'conn, Result<(), Error>> {
        Box::pin(async move {
            let depth = conn.inner.transaction_depth;
            let statement = match statement {
                // custom `BEGIN` statements are not allowed if we're already in a transaction
                // (we need to issue a `SAVEPOINT` instead)
                Some(_) if depth > 0 => return Err(Error::InvalidSavePointStatement),
                Some(statement) => statement,
                None => begin_ansi_transaction_sql(depth),
            };
            conn.execute(&*statement).await?;
            if !conn.in_transaction() {
                return Err(Error::BeginFailed);
            }
            conn.inner.transaction_depth += 1;

            Ok(())
        })
    }

    fn commit(conn: &mut MySqlConnection) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async move {
            let depth = conn.inner.transaction_depth;

            if depth > 0 {
                conn.execute(&*commit_ansi_transaction_sql(depth)).await?;
                conn.inner.transaction_depth = depth - 1;
            }

            Ok(())
        })
    }

    fn rollback(conn: &mut MySqlConnection) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async move {
            let depth = conn.inner.transaction_depth;

            if depth > 0 {
                conn.execute(&*rollback_ansi_transaction_sql(depth)).await?;
                conn.inner.transaction_depth = depth - 1;
            }

            Ok(())
        })
    }

    fn start_rollback(conn: &mut MySqlConnection) {
        let depth = conn.inner.transaction_depth;

        if depth > 0 {
            // Drop cannot await. Queue the command after pending rows and
            // statement cleanup instead of buffering it ahead of that response.
            conn.inner.stream.waiting.push_back(Waiting::Rollback(
                rollback_ansi_transaction_sql(depth).into_owned(),
            ));

            conn.inner.transaction_depth = depth - 1;
        }
    }

    fn get_transaction_depth(conn: &MySqlConnection) -> usize {
        conn.inner.transaction_depth
    }
}
