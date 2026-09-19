use serde_json::{json, Value};

/// Preserve classification through result shaping; never infer it from text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    Input,
    Rejected,
    Backend,
}

impl ErrorKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Input => "input",
            Self::Rejected => "rejected",
            Self::Backend => "backend",
        }
    }
}

const ERROR_KIND: &str = "io.matrixorigin.memoria/errorKind";

pub(crate) fn input_error(message: impl std::fmt::Display) -> anyhow::Error {
    InputError(message.to_string()).into()
}

/// Add input classification without changing the legacy user-facing message.
#[derive(Debug)]
struct InputError(String);

impl std::fmt::Display for InputError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for InputError {}

pub fn classified_error(kind: ErrorKind, text: impl std::fmt::Display) -> Value {
    json!({"content": [{"type": "text", "text": text.to_string()}], "isError": true,
        "_meta": {(ERROR_KIND): kind.as_str()}})
}

/// An execution failure is a tool result, not a JSON-RPC protocol error.
pub fn error(text: impl std::fmt::Display) -> Value {
    classified_error(ErrorKind::Input, text)
}

pub fn error_kind(result: &Value) -> Option<ErrorKind> {
    if !is_error(result) {
        return None;
    }
    Some(match result["_meta"][ERROR_KIND].as_str() {
        Some("input") => ErrorKind::Input,
        Some("rejected") => ErrorKind::Rejected,
        _ => ErrorKind::Backend,
    })
}

#[derive(Debug)]
pub(crate) struct RemoteError {
    pub kind: ErrorKind,
    pub message: String,
}

impl std::fmt::Display for RemoteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for RemoteError {}

pub fn is_error(result: &Value) -> bool {
    result.get("isError").and_then(Value::as_bool) == Some(true)
}

pub(crate) fn execution_error(tool: &str, error: impl Into<anyhow::Error>) -> Value {
    let error = error.into();
    use memoria_core::MemoriaError;
    let kind = match error.downcast_ref::<MemoriaError>() {
        Some(
            MemoriaError::Validation(_)
            | MemoriaError::InvalidMemoryType(_)
            | MemoriaError::InvalidTrustTier(_),
        ) => ErrorKind::Input,
        Some(MemoriaError::NotFound(_) | MemoriaError::Blocked(_)) => ErrorKind::Rejected,
        _ if error.is::<InputError>() => ErrorKind::Input,
        _ => error
            .downcast_ref::<RemoteError>()
            .map(|e| e.kind)
            .unwrap_or(ErrorKind::Backend),
    };
    if kind == ErrorKind::Backend {
        tracing::warn!(tool, error = %error, "MCP backend execution failed");
    } else {
        tracing::debug!(tool, error = %error, "MCP tool input or operation rejected");
    }
    let message = match error.downcast_ref::<MemoriaError>() {
        Some(MemoriaError::Database(_)) => {
            Some("Storage operation failed. Check service health before retrying.")
        }
        Some(MemoriaError::Embedding(_)) => {
            Some("Embedding service failed. Check service health before retrying.")
        }
        Some(MemoriaError::Internal(_) | MemoriaError::Serialization(_)) => {
            Some("Tool execution failed internally. Check server logs before retrying.")
        }
        _ if error.is::<sqlx::Error>() || error.is::<reqwest::Error>() => {
            Some("A downstream request failed. Check service health before retrying.")
        }
        _ => None,
    };
    let mut message = message
        .map(str::to_string)
        .unwrap_or_else(|| error.to_string());
    if kind == ErrorKind::Backend && may_mutate(tool) {
        message
            .push_str(" A write may have partially completed; check its outcome before retrying.");
    }
    classified_error(kind, message)
}

fn may_mutate(tool: &str) -> bool {
    !matches!(
        tool,
        "memory_retrieve"
            | "memory_search"
            | "memory_list"
            | "memory_profile"
            | "memory_capabilities"
            | "memory_branches"
            | "memory_snapshots"
            | "memory_diff"
            | "memory_get_retrieval_params"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_classification_preserves_the_original_message() {
        let message = "session_id is required when session_scope is set";
        let error = input_error(message);
        assert_eq!(error.to_string(), message);
        let result = execution_error("memory_correct", error);
        assert_eq!(error_kind(&result), Some(ErrorKind::Input));
        assert_eq!(result["content"][0]["text"], message);
    }

    #[test]
    fn only_mutations_warn_about_partial_writes() {
        for tool in [
            "memory_retrieve",
            "memory_search",
            "memory_list",
            "memory_profile",
            "memory_store",
        ] {
            let result =
                execution_error(tool, memoria_core::MemoriaError::Database("private".into()));
            assert_eq!(error_kind(&result), Some(ErrorKind::Backend));
            assert_eq!(
                result.to_string().contains("write may"),
                tool == "memory_store"
            );
        }
        assert_eq!(
            error_kind(&error("query is required")),
            Some(ErrorKind::Input)
        );
        assert_eq!(error_kind(&json!({"content":[]})), None);
    }

    #[test]
    fn validation_is_actionable_and_backend_details_are_private() {
        let result = execution_error(
            "memory_apply",
            memoria_core::MemoriaError::Validation("Conflict: choose a merge strategy".into()),
        );
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
            let result = execution_error("memory_store", error);
            assert!(is_error(&result));
            assert!(!result.to_string().contains("private SQL"));
        }
    }
}
