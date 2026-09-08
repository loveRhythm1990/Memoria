//! Shared schema metadata, with separate rules for restoring rows and validating
//! a cloned table. A nullable column may be omitted from historical row data,
//! but cannot be absent from a table when current application SQL references it.
use memoria_core::MemoriaError;
use sqlx::MySqlPool;

#[derive(Debug)]
pub struct TableColumn {
    pub name: String,
    pub column_type: String,
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
    let rows: Vec<(String, String, String, Option<String>, String)> = sqlx::query_as(
        "SELECT COLUMN_NAME, COLUMN_TYPE, IS_NULLABLE, COLUMN_DEFAULT, EXTRA \
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
        .map(
            |(name, column_type, nullable, default, extra)| TableColumn {
                name,
                column_type,
                nullable: nullable == "YES",
                default,
                extra,
            },
        )
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

/// Native DATA BRANCH operations on older MatrixOne builds can silently pair
/// fields by ordinal. Be conservative across supported versions, even if a
/// newer build accepts a particular order difference. Explicit row restoration
/// has a separate compatibility rule and must not use this check.
pub fn validate_native_branch_schema(
    current: &[TableColumn],
    branch: &[TableColumn],
) -> Result<(), MemoriaError> {
    // MO CREATE TABLE LIKE reports an implicit nullable default as SQL text
    // "null", while the source metadata can use SQL NULL. They are equivalent;
    // a quoted string default ('null') must remain distinct.
    fn normalized_default(column: &TableColumn) -> Option<&str> {
        column
            .default
            .as_deref()
            .filter(|value| !value.trim().eq_ignore_ascii_case("null"))
    }
    if current.len() != branch.len()
        || current.iter().zip(branch).any(|(a, b)| {
            !a.name.eq_ignore_ascii_case(&b.name)
                || !a.column_type.eq_ignore_ascii_case(&b.column_type)
                || a.nullable != b.nullable
                || normalized_default(a) != normalized_default(b)
                || !a.extra.eq_ignore_ascii_case(&b.extra)
        })
    {
        return Err(MemoriaError::Database(
            "Branch schema is incompatible with the current table (column order, type, or constraints differ); native branch operations are disabled. Preserve the branch data and recreate a compatible branch before retrying".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn column(name: &str, ty: &str) -> TableColumn {
        TableColumn {
            name: name.into(),
            column_type: ty.into(),
            nullable: true,
            default: None,
            extra: String::new(),
        }
    }

    #[test]
    fn native_schema_requires_matching_order_types_and_constraints() {
        let current = vec![column("id", "INT"), column("embedding", "VECF32(3)")];
        assert!(validate_native_branch_schema(
            &current,
            &[column("ID", "int"), column("embedding", "vecf32(3)")]
        )
        .is_ok());
        for branch in [
            vec![column("id", "INT")],
            vec![column("embedding", "VECF32(3)"), column("id", "INT")],
            vec![
                column("id", "VARCHAR(32)"),
                column("embedding", "VECF32(3)"),
            ],
            vec![column("id", "INT"), column("embedding", "VECF32(4)")],
        ] {
            assert!(validate_native_branch_schema(&current, &branch).is_err());
        }
        let mut branch = vec![column("id", "INT"), column("embedding", "VECF32(3)")];
        branch[0].nullable = false;
        assert!(validate_native_branch_schema(&current, &branch).is_err());
        branch[0].nullable = true;
        branch[0].default = Some("0".into());
        assert!(validate_native_branch_schema(&current, &branch).is_err());
        branch[0].default = Some("null".into());
        assert!(validate_native_branch_schema(&current, &branch).is_ok());
        branch[0].default = Some("'null'".into());
        assert!(validate_native_branch_schema(&current, &branch).is_err());
    }

    #[test]
    fn row_defaults_do_not_make_missing_table_columns_safe() {
        let expected = vec![
            TableColumn {
                name: "source_event_ids".into(),
                column_type: "TEXT".into(),
                nullable: false,
                default: None,
                extra: String::new(),
            },
            TableColumn {
                name: "embedding".into(),
                column_type: "VECF32(3)".into(),
                nullable: true,
                default: None,
                extra: String::new(),
            },
            TableColumn {
                name: "is_active".into(),
                column_type: "TINYINT".into(),
                nullable: false,
                default: Some("1".into()),
                extra: String::new(),
            },
            TableColumn {
                name: "id".into(),
                column_type: "INT".into(),
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
