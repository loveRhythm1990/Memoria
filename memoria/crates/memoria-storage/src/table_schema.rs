//! Shared schema metadata, with separate rules for restoring rows and validating
//! a cloned table. A nullable column may be omitted from historical row data,
//! but cannot be absent from a table when current application SQL references it.
use memoria_core::MemoriaError;
use sqlx::MySqlPool;

#[derive(Debug)]
pub struct TableColumn {
    pub name: String,
    pub nullable: bool,
    pub default: Option<String>,
    pub extra: String,
}

impl TableColumn {
    /// Whether historical row data must supply this column. This does NOT decide
    /// whether application SQL can operate on a table that lacks the column.
    pub fn needs_snapshot_value(&self) -> bool {
        !self.nullable
            && self.default.is_none()
            && !self.extra.to_ascii_lowercase().contains("auto_increment")
    }
}

pub async fn read_table_columns(
    pool: &MySqlPool,
    schema: &str,
    table: &str,
) -> Result<Vec<TableColumn>, MemoriaError> {
    let rows: Vec<(String, String, Option<String>, String)> = sqlx::query_as(
        "SELECT COLUMN_NAME, IS_NULLABLE, COLUMN_DEFAULT, EXTRA \
         FROM information_schema.columns WHERE table_schema = ? AND table_name = ? \
         ORDER BY ORDINAL_POSITION",
    )
    .bind(schema)
    .bind(table)
    .fetch_all(pool)
    .await
    .map_err(|e| MemoriaError::Database(e.to_string()))?;
    if rows.is_empty() {
        return Err(MemoriaError::Database(format!(
            "No column metadata available for {schema}.{table}"
        )));
    }
    Ok(rows
        .into_iter()
        .map(|(name, nullable, default, extra)| TableColumn {
            name,
            nullable: nullable == "YES",
            default,
            extra,
        })
        .collect())
}

pub fn missing_columns<'a>(
    expected: &'a [TableColumn],
    present: &[String],
) -> Vec<&'a TableColumn> {
    expected
        .iter()
        .filter(|column| {
            !present
                .iter()
                .any(|name| name.eq_ignore_ascii_case(&column.name))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn row_defaults_do_not_make_missing_table_columns_safe() {
        let expected = vec![
            TableColumn {
                name: "source_event_ids".into(),
                nullable: false,
                default: None,
                extra: String::new(),
            },
            TableColumn {
                name: "embedding".into(),
                nullable: true,
                default: None,
                extra: String::new(),
            },
            TableColumn {
                name: "is_active".into(),
                nullable: false,
                default: Some("1".into()),
                extra: String::new(),
            },
            TableColumn {
                name: "id".into(),
                nullable: false,
                default: None,
                extra: "auto_increment".into(),
            },
        ];
        let missing = missing_columns(&expected, &[]);
        assert_eq!(
            missing.len(),
            4,
            "cloned tables must contain all referenced columns"
        );
        let required: Vec<_> = missing
            .into_iter()
            .filter(|c| c.needs_snapshot_value())
            .map(|c| c.name.as_str())
            .collect();
        assert_eq!(required, vec!["source_event_ids"]);
        assert!(missing_columns(
            &expected,
            &[
                "SOURCE_EVENT_IDS".into(),
                "embedding".into(),
                "is_active".into(),
                "id".into()
            ]
        )
        .is_empty());
    }
}
