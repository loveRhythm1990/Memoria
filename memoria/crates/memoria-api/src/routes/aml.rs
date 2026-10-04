//! AML textual-memory adapter. Protocol-specific code stays at the HTTP boundary.
use axum::{
    extract::{FromRequestParts, State},
    http::{request::Parts, StatusCode},
    routing::post,
    Json, Router,
};
use chrono::{DateTime, Datelike, Utc};
use memoria_core::{MemoriaError, Memory, MemoryType, TrustTier};
use memoria_service::RetrieveOptions;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use subtle::ConstantTimeEq;

use super::memory::api_err_typed;
use crate::state::AppState;

type ApiResult<T> = Result<Json<T>, (StatusCode, String)>;

pub fn router(state: &AppState) -> Router<AppState> {
    if state.aml_api_key.is_none() {
        return Router::new();
    }
    Router::new()
        .route("/aml/add", post(add))
        .route("/aml/search", post(search))
}

/// An explicit, dedicated multi-sample credential; ordinary Memoria keys do
/// not grant access to arbitrary caller-supplied evaluation scopes.
struct AmlAuth;

#[async_trait::async_trait]
impl FromRequestParts<AppState> for AmlAuth {
    type Rejection = (StatusCode, String);

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let expected = state
            .aml_api_key
            .as_deref()
            .ok_or((StatusCode::NOT_FOUND, "AML adapter is disabled".into()))?;
        let mut values = parts.headers.get_all("authorization").iter();
        let supplied = values
            .next()
            .filter(|_| values.next().is_none())
            .and_then(|header| header.to_str().ok())
            .and_then(|value| value.split_once(' '))
            .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
            .map(|(_, token)| token);
        if !supplied.is_some_and(|token| {
            token.len() == expected.len() && bool::from(token.as_bytes().ct_eq(expected.as_bytes()))
        }) {
            return Err((StatusCode::UNAUTHORIZED, "Invalid AML Bearer token".into()));
        }
        Ok(Self)
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct AddRequest {
    request_id: String,
    user_id: String,
    session_id: String,
    messages: Vec<Message>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Role {
    User,
    Assistant,
}

#[derive(Debug, Serialize, Deserialize)]
struct Message {
    role: Role,
    content: String,
    timestamp: Option<i64>,
}

#[derive(Serialize)]
struct AddResponse {
    success: bool,
    request_id: String,
    user_id: String,
    session_id: String,
}

#[derive(Deserialize)]
struct SearchRequest {
    query: String,
    user_id: String,
    top_k: u32,
    options: Option<Vec<String>>,
}

#[derive(Serialize)]
struct SearchResponse {
    data: Vec<MemoryItem>,
}

#[derive(Serialize)]
struct MemoryItem {
    id: String,
    content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    score: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    created_at: Option<DateTime<Utc>>,
}

fn invalid(message: impl Into<String>) -> (StatusCode, String) {
    (StatusCode::UNPROCESSABLE_ENTITY, message.into())
}

fn validate_text(field: &str, value: &str) -> Result<(), (StatusCode, String)> {
    if value.trim().is_empty() || value.contains('\0') {
        return Err(invalid(format!(
            "{field} must be non-empty and contain no NUL"
        )));
    }
    Ok(())
}

// Length-framed, domain-separated hashing preserves exact external identifiers
// without trusting them as database identifiers or exceeding VARCHAR(64).
fn hash_parts(domain: &str, parts: &[&str]) -> String {
    let mut hash = Sha256::new();
    hash.update(domain.as_bytes());
    for part in parts {
        hash.update((part.len() as u64).to_be_bytes());
        hash.update(part.as_bytes());
    }
    format!("{:x}", hash.finalize())
}

pub fn scope_id(user_id: &str) -> String {
    format!(
        "aml_{}",
        &hash_parts("memoria/aml/user/v1", &[user_id])[..60]
    )
}

fn timestamp(value: Option<i64>) -> Result<Option<DateTime<Utc>>, (StatusCode, String)> {
    value
        .map(|millis| {
            DateTime::from_timestamp_millis(millis)
                .filter(|date| (1000..=9999).contains(&date.year()))
                .ok_or_else(|| invalid("timestamp is outside the supported UTC date range"))
        })
        .transpose()
}

struct PreparedAdd {
    scope: String,
    request_key: String,
    payload_hash: String,
    memories: Vec<Memory>,
}

impl AddRequest {
    fn prepare(&self) -> Result<PreparedAdd, (StatusCode, String)> {
        validate_text("request_id", &self.request_id)?;
        validate_text("user_id", &self.user_id)?;
        validate_text("session_id", &self.session_id)?;
        if self.messages.is_empty() {
            return Err(invalid("messages must not be empty"));
        }
        let scope = scope_id(&self.user_id);
        let request_key = hash_parts(
            "memoria/aml/request/v1",
            &[&scope, &self.session_id, &self.request_id],
        );
        let payload_hash = format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(self).map_err(invalid_json)?)
        );
        let session = format!(
            "aml_{}",
            &hash_parts("memoria/aml/session/v1", &[&self.session_id])[..60]
        );
        let now = Utc::now();
        let mut memories = Vec::new();
        for (message_index, message) in self.messages.iter().enumerate() {
            validate_text("messages[].content", &message.content)?;
            let source_timestamp = timestamp(message.timestamp)?;
            let role = match message.role {
                Role::User => "user",
                Role::Assistant => "assistant",
            };
            let prefix = source_timestamp
                .map(|time| {
                    format!(
                        "[{role} at {}]\nSource time refers to this message/session.\n",
                        time.to_rfc3339()
                    )
                })
                .unwrap_or_else(|| format!("[{role}]\nSource time: unspecified.\n"));
            for (chunk_index, chunk) in chunks(&message.content).into_iter().enumerate() {
                let mut metadata = HashMap::new();
                metadata.insert("aml_user_id".into(), serde_json::json!(self.user_id));
                metadata.insert("aml_session_id".into(), serde_json::json!(self.session_id));
                metadata.insert("aml_request_id".into(), serde_json::json!(self.request_id));
                metadata.insert("role".into(), serde_json::json!(role));
                metadata.insert("timestamp".into(), serde_json::json!(message.timestamp));
                metadata.insert(
                    "source_time_kind".into(),
                    serde_json::json!("message_or_session"),
                );
                metadata.insert("message_index".into(), serde_json::json!(message_index));
                metadata.insert("chunk_index".into(), serde_json::json!(chunk_index));
                metadata.insert(
                    memoria_core::source::SOURCE_POSITION_KEY.into(),
                    serde_json::to_value(memoria_core::source::SourcePosition {
                        batch_id: request_key.clone(),
                        message_index: message_index as u64,
                        chunk_index: chunk_index as u64,
                    })
                    .map_err(invalid_json)?,
                );
                let id = hash_parts(
                    "memoria/aml/memory/v1",
                    &[
                        &request_key,
                        &message_index.to_string(),
                        &chunk_index.to_string(),
                    ],
                );
                memories.push(Memory {
                    memory_id: id[..32].to_owned(),
                    user_id: scope.clone(),
                    author_id: None,
                    subject_id: None,
                    memory_type: MemoryType::Episodic,
                    content: format!("{prefix}{chunk}"),
                    initial_confidence: 0.95,
                    embedding: None,
                    source_event_ids: vec![self.request_id.clone()],
                    superseded_by: None,
                    is_active: true,
                    access_count: 0,
                    session_id: Some(session.clone()),
                    // We observe this source record at ingestion. The message's
                    // historical time is preserved in the prefix and metadata;
                    // it is not the confidence-decay clock for a newly imported record.
                    observed_at: Some(now),
                    created_at: Some(now),
                    updated_at: Some(now),
                    extra_metadata: Some(metadata),
                    trust_tier: TrustTier::T1Verified,
                    retrieval_score: None,
                });
            }
        }
        Ok(PreparedAdd {
            scope,
            request_key,
            payload_hash,
            memories,
        })
    }
}

fn invalid_json(error: serde_json::Error) -> (StatusCode, String) {
    invalid(error.to_string())
}

/// UTF-8-safe overlapping chunks. No source characters are silently truncated.
fn chunks(content: &str) -> Vec<&str> {
    const SIZE: usize = 1000;
    const OVERLAP: usize = 150;
    let offsets: Vec<usize> = content
        .char_indices()
        .map(|(offset, _)| offset)
        .chain(std::iter::once(content.len()))
        .collect();
    let len = offsets.len() - 1;
    let mut out = Vec::new();
    let mut start = 0;
    while start < len {
        let end = (start + SIZE).min(len);
        out.push(&content[offsets[start]..offsets[end]]);
        if end == len {
            break;
        }
        start = end - OVERLAP;
    }
    out
}

async fn add(
    State(state): State<AppState>,
    _auth: AmlAuth,
    Json(req): Json<AddRequest>,
) -> ApiResult<AddResponse> {
    let PreparedAdd {
        scope,
        request_key: key,
        payload_hash: hash,
        memories,
    } = req.prepare()?;
    state
        .service
        .ingest_source_batch(&scope, &key, &hash, memories)
        .await
        .map_err(|error| match error {
            MemoriaError::Validation(ref message)
                if message == "source request key reused with a different payload" =>
            {
                (StatusCode::CONFLICT, message.clone())
            }
            other => api_err_typed(other),
        })?;
    Ok(Json(AddResponse {
        success: true,
        request_id: req.request_id,
        user_id: req.user_id,
        session_id: req.session_id,
    }))
}

async fn search(
    State(state): State<AppState>,
    _auth: AmlAuth,
    Json(req): Json<SearchRequest>,
) -> ApiResult<SearchResponse> {
    validate_text("user_id", &req.user_id)?;
    validate_text("query", &req.query)?;
    if !(1..=1000).contains(&req.top_k) {
        return Err(invalid("top_k must be between 1 and 1000"));
    }
    if let Some(options) = &req.options {
        for option in options {
            validate_text("options[]", option)?;
        }
    }
    // Preserve the original question. Options are accepted but do not bias retrieval.
    let scope = scope_id(&req.user_id);
    let memories = state
        .service
        .retrieve_with_options_on_branch(
            &scope,
            Some("main"),
            &req.query,
            i64::from(req.top_k),
            &RetrieveOptions::default().with_source_context(state.aml_source_context),
        )
        .await
        .map_err(api_err_typed)?;
    let data = memories
        .into_iter()
        .filter(|m| m.user_id == scope && !m.content.trim().is_empty())
        .take(req.top_k as usize)
        .map(|memory| MemoryItem {
            id: memory.memory_id,
            content: memory.content,
            score: memory.retrieval_score.filter(|score| score.is_finite()),
            created_at: memory.created_at,
        })
        .collect();
    Ok(Json(SearchResponse { data }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_records_preserve_roles_time_and_unicode_without_truncation() {
        let content = "记忆🧠".repeat(900);
        let req: AddRequest = serde_json::from_value(serde_json::json!({
            "request_id": "r", "user_id": "eval:long:user", "session_id": "session".repeat(100),
            "messages": [{"role": "assistant", "content": content, "timestamp": 1704067200000_i64}]
        }))
        .unwrap();
        let PreparedAdd {
            scope,
            request_key: key,
            payload_hash: hash,
            memories: rows,
        } = req.prepare().unwrap();
        assert_eq!(scope.len(), 64);
        assert!(rows.len() > 1);
        let slices = chunks(&req.messages[0].content);
        let reconstructed = slices
            .iter()
            .enumerate()
            .map(|(i, slice)| {
                if i == 0 {
                    slice.to_string()
                } else {
                    slice.chars().skip(150).collect()
                }
            })
            .collect::<String>();
        assert_eq!(reconstructed, req.messages[0].content);
        assert!(rows
            .iter()
            .all(|m| m.user_id == scope && m.session_id.as_ref().unwrap().len() == 64));
        assert!(rows[0]
            .content
            .starts_with("[assistant at 2024-01-01T00:00:00+00:00]"));
        assert_eq!(rows[0].observed_at, rows[0].created_at);
        assert!(rows[0].observed_at.unwrap() > timestamp(Some(1704067200000)).unwrap().unwrap());
        assert_eq!(
            rows[0].extra_metadata.as_ref().unwrap()["timestamp"],
            serde_json::json!(1704067200000_i64)
        );
        assert!(rows[0].effective_confidence(None) > 0.94);
        let PreparedAdd {
            request_key: replay_key,
            payload_hash: replay_hash,
            memories: replay_rows,
            ..
        } = req.prepare().unwrap();
        assert_eq!((key, hash), (replay_key, replay_hash));
        assert_eq!(rows[0].memory_id, replay_rows[0].memory_id);
        assert_ne!(scope, scope_id("eval:long:user "));
    }

    #[test]
    fn ids_are_unambiguous_and_payload_changes_are_detected() {
        assert_ne!(
            hash_parts("domain", &["ab", "c"]),
            hash_parts("domain", &["a", "bc"])
        );
        let mut req: AddRequest = serde_json::from_value(serde_json::json!({
            "request_id":"r", "user_id":"u", "session_id":"s", "messages":[{"role":"user","content":"a"}]
        })).unwrap();
        let PreparedAdd {
            request_key: key,
            payload_hash: hash,
            ..
        } = req.prepare().unwrap();
        req.messages[0].content = "b".into();
        let PreparedAdd {
            request_key: changed_key,
            payload_hash: changed_hash,
            ..
        } = req.prepare().unwrap();
        assert_eq!(key, changed_key);
        assert_ne!(hash, changed_hash);
        req.messages[0].timestamp = Some(i64::MAX);
        assert!(req.prepare().is_err());
        req.messages.clear();
        assert!(req.prepare().is_err());
    }
}
