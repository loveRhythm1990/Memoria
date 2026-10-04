//! Real HTTP + MatrixOne contract checks. Uses isolated shared/user databases.
use memoria_api::{routes::aml::scope_id, AppState};
use memoria_test_utils::MultiDbTestContext;
use reqwest::{Client, StatusCode};
use serde_json::{json, Value};

struct Server {
    base: String,
    handle: tokio::task::JoinHandle<()>,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

async fn serve(state: AppState) -> Server {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let handle = tokio::spawn(async move {
        axum::serve(listener, memoria_api::build_router(state))
            .await
            .unwrap();
    });
    Server { base, handle }
}

async fn fixture() -> MultiDbTestContext {
    let url = std::env::var("AML_TEST_DATABASE_URL")
        .expect("set AML_TEST_DATABASE_URL for the isolated MatrixOne test fixture");
    MultiDbTestContext::new(&url, "aml_adapter", 8, None, None).await
}

async fn post(client: &Client, server: &Server, path: &str, payload: &Value) -> reqwest::Response {
    client
        .post(format!("{}{path}", server.base))
        .bearer_auth("aml-test-key")
        .json(payload)
        .send()
        .await
        .unwrap()
}

async fn assert_success(response: reqwest::Response) -> Value {
    let status = response.status();
    let body = response.text().await.unwrap();
    assert_eq!(status, StatusCode::OK, "{body}");
    serde_json::from_str(&body).unwrap()
}

#[tokio::test]
#[ignore = "requires AML_TEST_DATABASE_URL pointing to MatrixOne"]
async fn aml_http_contract_isolation_retries_and_validation() {
    let context = fixture().await;
    let mut state = AppState::new(context.service(), context.git(), "normal-master-key".into());
    state.aml_api_key = None;
    let disabled = serve(state.clone()).await;
    let client = Client::builder().no_proxy().build().unwrap();
    assert_eq!(
        client
            .post(format!("{}/aml/add", disabled.base))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    state.aml_api_key = Some("aml-test-key".into());
    let server = serve(state.clone()).await;
    assert_eq!(
        client
            .get(format!("{}/health", server.base))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    let run_id = uuid::Uuid::new_v4().simple().to_string();
    let user = format!("eval:{run_id}:locomo:conversation-zero").repeat(3);
    let other_user = format!("another-sample-{run_id}");
    let cjk_user = format!("cjk-sample-{run_id}");
    let payload = json!({"request_id":"r-1", "user_id":user, "session_id":"session-zero".repeat(10),
    "messages":[
        {"role":"user", "content":"Cedar Observatory has a violet telescope.", "timestamp":1704067200000_i64},
        {"role":"assistant", "content":"Cedar Observatory opens on Thursday."}
    ]});
    for key in [None, Some("normal-master-key"), Some("wrong-key")] {
        let mut request = client
            .post(format!("{}/aml/add", server.base))
            .json(&payload);
        if let Some(key) = key {
            request = request.bearer_auth(key);
        }
        assert_eq!(
            request.send().await.unwrap().status(),
            StatusCode::UNAUTHORIZED
        );
    }
    let added = assert_success(post(&client, &server, "/aml/add", &payload).await).await;
    assert_eq!(
        added,
        json!({"success":true, "request_id":payload["request_id"], "user_id":user, "session_id":payload["session_id"]})
    );
    let query = json!({"user_id":user, "query":"Cedar Observatory", "top_k":100, "options":["violet", "orange"]});
    let first = assert_success(post(&client, &server, "/aml/search", &query).await).await;
    let evidence = first["data"].as_array().unwrap();
    assert_eq!(
        evidence.len(),
        2,
        "Search must see both source records immediately: {first}"
    );
    assert!(evidence
        .iter()
        .any(|m| m["content"].as_str().unwrap().contains("violet telescope")));
    assert!(evidence
        .iter()
        .any(|m| m["content"].as_str().unwrap().contains("2024-01-01")));
    assert!(evidence
        .iter()
        .all(|m| !m["id"].as_str().unwrap().is_empty()));
    let mut limited = query.clone();
    limited["top_k"] = json!(1);
    assert_eq!(
        assert_success(post(&client, &server, "/aml/search", &limited).await).await["data"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_success(post(&client, &server, "/aml/add", &payload).await).await;
    let store = context.user_store(&scope_id(&user)).await;
    let table = store.t("mem_memories");
    let count: i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {table}"))
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(count, 2);
    // Reconstruct the HTTP app: idempotency lives in the database, not app state.
    let restarted = serve(state).await;
    assert_success(post(&client, &restarted, "/aml/add", &payload).await).await;
    let mut changed = payload.clone();
    changed["messages"][0]["content"] = json!("changed");
    assert_eq!(
        post(&client, &server, "/aml/add", &changed).await.status(),
        StatusCode::CONFLICT
    );
    let mut other_query = query.clone();
    other_query["user_id"] = json!(other_user);
    assert_eq!(
        assert_success(post(&client, &server, "/aml/search", &other_query).await).await,
        json!({"data":[]})
    );
    let mut other_add = payload.clone();
    other_add["user_id"] = json!(other_user);
    other_add["messages"] =
        json!([{"role":"user", "content":"Cedar Observatory has an orange telescope."}]);
    assert_success(post(&client, &server, "/aml/add", &other_add).await).await;
    let other = assert_success(post(&client, &server, "/aml/search", &other_query).await).await;
    assert_eq!(other["data"].as_array().unwrap().len(), 1);
    assert!(!other["data"][0]["content"]
        .as_str()
        .unwrap()
        .contains("violet"));
    let mut bad = payload.clone();
    bad["messages"] = json!([]);
    assert_eq!(
        post(&client, &server, "/aml/add", &bad).await.status(),
        StatusCode::UNPROCESSABLE_ENTITY
    );
    bad = payload.clone();
    bad["messages"][0]["content"] = json!([{"type":"image_url"}]);
    assert_eq!(
        post(&client, &server, "/aml/add", &bad).await.status(),
        StatusCode::UNPROCESSABLE_ENTITY
    );
    let mut bad_query = query.clone();
    bad_query["top_k"] = json!(0);
    assert_eq!(
        post(&client, &server, "/aml/search", &bad_query)
            .await
            .status(),
        StatusCode::UNPROCESSABLE_ENTITY
    );
    bad_query = query.clone();
    bad_query.as_object_mut().unwrap().remove("top_k");
    assert_eq!(
        post(&client, &server, "/aml/search", &bad_query)
            .await
            .status(),
        StatusCode::UNPROCESSABLE_ENTITY
    );
    // Same user can retrieve across source sessions; session_id is not a Search filter.
    let mut session_add = payload.clone();
    session_add["session_id"] = json!("another-session");
    session_add["messages"] =
        json!([{ "role":"user", "content":"Cedar Observatory displays an amber telescope." }]);
    assert_success(post(&client, &server, "/aml/add", &session_add).await).await;
    let across_sessions = assert_success(post(&client, &server, "/aml/search", &query).await).await;
    assert_eq!(across_sessions["data"].as_array().unwrap().len(), 3);
    let cjk = json!({"request_id":"cjk", "user_id":cjk_user, "session_id":"s",
        "messages":[{"role":"user", "content":"王强养了一只猫叫豆豆，今年三岁，住在杭州西湖区。"}]});
    assert_success(post(&client, &server, "/aml/add", &cjk).await).await;
    let cjk_query = json!({"query":"豆豆", "user_id":cjk_user, "top_k":100});
    let cjk_result = assert_success(post(&client, &server, "/aml/search", &cjk_query).await).await;
    assert!(cjk_result["data"]
        .as_array()
        .unwrap()
        .iter()
        .any(|row| row["content"].as_str().unwrap().contains("三岁")));
    let oversized = json!({"request_id":"large", "user_id":"large", "session_id":"s",
        "messages":[{"role":"user", "content":"x".repeat(2 * 1024 * 1024)}]});
    assert_eq!(
        post(&client, &server, "/aml/add", &oversized)
            .await
            .status(),
        StatusCode::PAYLOAD_TOO_LARGE
    );
    // Concurrent first writes, not merely replays of an already completed Add.
    let mut concurrent = payload.clone();
    concurrent["request_id"] = json!("r-concurrent");
    let (a, b) = tokio::join!(
        post(&client, &server, "/aml/add", &concurrent),
        post(&client, &server, "/aml/add", &concurrent)
    );
    for response in [a, b] {
        let status = response.status();
        assert!(
            status == StatusCode::OK || status.is_server_error(),
            "unexpected retry status {status}"
        );
    }
    assert_success(post(&client, &server, "/aml/add", &concurrent).await).await;
    let count: i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {table}"))
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(
        count, 5,
        "concurrent retry must not duplicate source records"
    );
    drop(restarted);
    drop(server);
    drop(disabled);
}

#[tokio::test]
#[ignore = "requires AML_TEST_DATABASE_URL pointing to MatrixOne"]
async fn source_batch_rolls_back_receipt_and_partial_rows() {
    let context = fixture().await;
    let scope = scope_id(&format!("rollback-{}", uuid::Uuid::new_v4().simple()));
    let store = context.user_store(&scope).await;
    let base: memoria_core::Memory = serde_json::from_value(json!({
        "memory_id":"seed", "user_id":scope, "author_id":null, "subject_id":null,
        "memory_type":"episodic", "content":"seed source", "initial_confidence":0.95,
        "embedding":null, "source_event_ids":[], "superseded_by":null, "is_active":true,
        "access_count":0, "session_id":null, "observed_at":null, "created_at":null,
        "updated_at":null, "extra_metadata":null, "trust_tier":"T1", "retrieval_score":null
    }))
    .unwrap();
    let mut duplicate = base.clone();
    duplicate.content = "second row with duplicate id".into();
    assert!(store
        .insert_source_batch(&scope, "request", "hash", &[base.clone(), duplicate])
        .await
        .is_err());
    assert!(!store
        .source_batch_committed(&scope, "request", "hash")
        .await
        .unwrap());
    let table = store.t("mem_memories");
    let count: i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {table}"))
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(count, 0);
    assert!(store
        .insert_source_batch(&scope, "request", "hash", &[base])
        .await
        .unwrap());
    assert!(!store
        .insert_source_batch(&scope, "request", "hash", &[])
        .await
        .unwrap());
    assert!(store
        .source_batch_committed(&scope, "request", "different")
        .await
        .is_err());
}

struct TestEmbedder {
    fail: std::sync::atomic::AtomicBool,
    batches: std::sync::atomic::AtomicUsize,
}

#[async_trait::async_trait]
impl memoria_core::interfaces::EmbeddingProvider for TestEmbedder {
    async fn embed(&self, _text: &str) -> Result<Vec<f32>, memoria_core::MemoriaError> {
        Ok(vec![1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0])
    }
    async fn embed_batch(
        &self,
        texts: &[String],
    ) -> Result<Vec<Vec<f32>>, memoria_core::MemoriaError> {
        self.batches
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if self.fail.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(memoria_core::MemoriaError::Embedding(
                "simulated provider outage".into(),
            ));
        }
        Ok(vec![
            vec![1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0];
            texts.len()
        ])
    }
    fn dimension(&self) -> usize {
        8
    }
}

#[tokio::test]
#[ignore = "requires AML_TEST_DATABASE_URL pointing to MatrixOne"]
async fn embedding_failure_is_retryable_and_source_events_remain_distinct() {
    use std::sync::{atomic::Ordering, Arc};
    let url = std::env::var("AML_TEST_DATABASE_URL").unwrap();
    let embedder = Arc::new(TestEmbedder {
        fail: std::sync::atomic::AtomicBool::new(true),
        batches: std::sync::atomic::AtomicUsize::new(0),
    });
    let context =
        MultiDbTestContext::new(&url, "aml_vectors", 8, Some(embedder.clone()), None).await;
    let mut state = AppState::new(context.service(), context.git(), "master".into());
    state.aml_api_key = Some("aml-test-key".into());
    let server = serve(state).await;
    let client = Client::builder().no_proxy().build().unwrap();
    let user = format!("vector-{}", uuid::Uuid::new_v4().simple());
    let payload = json!({"request_id":"r", "user_id":user, "session_id":"s",
        "messages":[{"role":"user", "content":"Cedar Observatory opens on Thursday."},
                    {"role":"assistant", "content":"Cedar Observatory opens on Friday."}]});
    assert_eq!(
        post(&client, &server, "/aml/add", &payload).await.status(),
        StatusCode::INTERNAL_SERVER_ERROR
    );
    let scope = scope_id(&user);
    let store = context.user_store(&scope).await;
    let table = store.t("mem_memories");
    let count: i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {table}"))
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(count, 0, "embedding failure cannot commit a partial batch");
    embedder.fail.store(false, Ordering::SeqCst);
    assert_success(post(&client, &server, "/aml/add", &payload).await).await;
    assert_success(post(&client, &server, "/aml/add", &payload).await).await;
    assert_eq!(
        embedder.batches.load(Ordering::SeqCst),
        2,
        "completed replay must not re-embed"
    );
    let query = json!({"query":"Cedar Observatory", "user_id":user, "top_k":100});
    let results = assert_success(post(&client, &server, "/aml/search", &query).await).await;
    assert_eq!(
        results["data"].as_array().unwrap().len(),
        2,
        "even identical vectors must retain distinct source events"
    );
    let vectors: i64 = sqlx::query_scalar(&format!(
        "SELECT COUNT(*) FROM {table} WHERE embedding IS NOT NULL"
    ))
    .fetch_one(store.pool())
    .await
    .unwrap();
    assert_eq!(vectors, 2);
    let mut private = payload.clone();
    private["request_id"] = json!("redaction");
    private["messages"] =
        json!([{"role":"user", "content":"Cedar Observatory contact is jane@example.com"}]);
    assert_success(post(&client, &server, "/aml/add", &private).await).await;
    let redacted: String = sqlx::query_scalar(&format!(
        "SELECT content FROM {table} WHERE content LIKE '%contact%'"
    ))
    .fetch_one(store.pool())
    .await
    .unwrap();
    assert!(redacted.contains("[email]"));
    assert!(!redacted.contains("jane@example.com"));
    let mut blocked = payload.clone();
    blocked["request_id"] = json!("blocked");
    blocked["messages"] = json!([{"role":"user", "content":"safe source"}, {"role":"user", "content":"password: secret-value"}]);
    assert_eq!(
        post(&client, &server, "/aml/add", &blocked).await.status(),
        StatusCode::FORBIDDEN
    );
    let count: i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {table}"))
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(count, 3, "blocked batch cannot insert its earlier safe row");
}
