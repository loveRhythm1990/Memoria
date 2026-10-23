//! Schema guard for the opt-in, fresh-database CAGRA experiment.
use memoria_core::MemoriaError;

// name, column type, key, extra, nullable — from information_schema.columns.
pub(crate) fn validate_memory_columns(
    columns: &[(String, String, String, String, String)],
) -> Result<(), MemoriaError> {
    let primary: Vec<_> = columns
        .iter()
        .filter(|c| c.2.eq_ignore_ascii_case("PRI"))
        .collect();
    let valid_id = primary.len() == 1
        && primary[0].0 == "id"
        && primary[0].1.to_ascii_lowercase().split(['(', ' ']).next() == Some("bigint")
        && !primary[0].1.to_ascii_lowercase().contains("unsigned")
        && primary[0].3.to_ascii_lowercase().contains("auto_increment")
        && primary[0].4.eq_ignore_ascii_case("NO");
    let valid_memory_id = columns.iter().any(|c| {
        c.0 == "memory_id"
            && c.1.eq_ignore_ascii_case("varchar(64)")
            && c.2.eq_ignore_ascii_case("UNI")
            && c.4.eq_ignore_ascii_case("NO")
    });
    if !valid_id || !valid_memory_id {
        return Err(MemoriaError::Validation(
            "cuVS experiment requires a fresh experimental database with id BIGINT AUTO_INCREMENT PRIMARY KEY and memory_id VARCHAR(64) NOT NULL UNIQUE; existing schemas are not converted".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn column(
        name: &str,
        ty: &str,
        key: &str,
        extra: &str,
    ) -> (String, String, String, String, String) {
        (
            name.into(),
            ty.into(),
            key.into(),
            extra.into(),
            "NO".into(),
        )
    }

    #[test]
    fn accepts_signed_internal_key_and_unique_business_id() {
        for ty in ["bigint", "BIGINT(64)"] {
            assert!(validate_memory_columns(&[
                column("id", ty, "PRI", "auto_increment"),
                column("memory_id", "varchar(64)", "UNI", ""),
            ])
            .is_ok());
        }
    }

    #[test]
    fn rejects_legacy_unsigned_nonunique_and_nonautomatic_schemas() {
        let valid = vec![
            column("id", "bigint", "PRI", "auto_increment"),
            column("memory_id", "varchar(64)", "UNI", ""),
        ];
        assert!(validate_memory_columns(&[]).is_err());
        assert!(validate_memory_columns(&[column("memory_id", "varchar(64)", "PRI", "")]).is_err());
        for invalid_id in [
            column("id", "bigint unsigned", "PRI", "auto_increment"),
            column("id", "int", "PRI", "auto_increment"),
            column("id", "bigint", "PRI", ""),
            column("id", "bigint", "", "auto_increment"),
        ] {
            assert!(validate_memory_columns(&[invalid_id, valid[1].clone()]).is_err());
        }
        let mut invalid = valid.clone();
        invalid[1].2 = "MUL".into();
        assert!(validate_memory_columns(&invalid).is_err());
        invalid = valid.clone();
        invalid[1].4 = "YES".into();
        assert!(validate_memory_columns(&invalid).is_err());
        invalid = valid;
        invalid[1].2 = "PRI".into();
        assert!(validate_memory_columns(&invalid).is_err());
    }
}
