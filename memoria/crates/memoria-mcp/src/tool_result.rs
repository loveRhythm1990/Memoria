use serde_json::{json, Value};

/// An execution failure is a tool result, not a JSON-RPC protocol error.
pub fn error(text: impl std::fmt::Display) -> Value {
    json!({"content": [{"type": "text", "text": text.to_string()}], "isError": true})
}

pub fn is_error(result: &Value) -> bool {
    result.get("isError").and_then(Value::as_bool) == Some(true)
}

pub(crate) fn execution_error(error: impl Into<anyhow::Error>) -> Value {
    let error = error.into();
    tracing::warn!(error = %error, "MCP tool execution failed");
    use memoria_core::MemoriaError;
    let message = match error.downcast_ref::<MemoriaError>() {
        Some(MemoriaError::Database(_)) => Some("Storage operation failed. Check service health before retrying; a write may have partially completed."),
        Some(MemoriaError::Embedding(_)) => Some("Embedding service failed. Check service health and retry the retrieval when available."),
        Some(MemoriaError::Internal(_) | MemoriaError::Serialization(_)) => Some("Tool execution failed internally. Check server logs before retrying; a write may have partially completed."),
        _ if error.is::<sqlx::Error>() || error.is::<reqwest::Error>() => Some("A downstream request failed. Check service health before retrying; a write may have partially completed."),
        _ => None,
    };
    self::error(
        message
            .map(str::to_string)
            .unwrap_or_else(|| error.to_string()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validation_is_actionable_and_backend_details_are_private() {
        let result = execution_error(memoria_core::MemoriaError::Validation(
            "Conflict: choose a merge strategy".into(),
        ));
        assert!(is_error(&result));
        assert!(result["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("choose a merge strategy"));
        for error in [
            memoria_core::MemoriaError::Database("private SQL".into()),
            memoria_core::MemoriaError::Internal("private SQL".into()),
            memoria_core::MemoriaError::Embedding("private SQL".into()),
        ] {
            let result = execution_error(error);
            assert!(is_error(&result));
            assert!(!result.to_string().contains("private SQL"));
        }
    }
}
