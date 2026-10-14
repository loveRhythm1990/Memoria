//! Public authenticated idempotency protocol (the normal DB integration lane).
mod support;
use serde_json::{json, Value};

#[tokio::test]
async fn owner_scoped_store_replays_real_receipts_and_rejects_missing_keys() {
    let master = "idempotent-store-test-master";
    let server = support::multi_db::spawn_api_server(
        "idempotent_store",
        4,
        master.into(),
        Some(std::sync::Arc::new(memoria_embedding::MockEmbedder::new(4))),
        None,
        None,
        false,
    )
    .await;
    let owner = format!("idem_api_{}", uuid::Uuid::new_v4().simple());
    let url = format!("{}/v1/memories/idempotent", server.base);
    let body = json!({"content":"[@episode/active] confirmed session", "memory_type":"episodic", "session_id":"session-a", "trust_tier":"T3", "idempotency_key":"same-operation",
        "extra_metadata":{"scene":"qa", "agent":{"b":2,"a":1}}});
    let mut id = None;
    for _ in 0..3 {
        let response = server
            .client
            .post(&url)
            .header("Authorization", format!("Memoria-Owner {master}"))
            .header("X-User-Id", &owner)
            .json(&body)
            .send()
            .await
            .unwrap();
        let status = response.status();
        let response: Value = response.json().await.unwrap();
        assert_eq!(status, 201, "{response}");
        assert_eq!(response["user_id"], owner);
        assert_eq!(response["session_id"], "session-a");
        assert_eq!(response["extra_metadata"], body["extra_metadata"]);
        assert!(response["extra_metadata"]
            .get(memoria_core::STORE_REQUEST_HASH_FIELD)
            .is_none());
        if let Some(ref id) = id {
            assert_eq!(&response["memory_id"], id);
        } else {
            id = Some(response["memory_id"].clone());
        }
    }
    let reordered: Value = serde_json::from_str(r#"{"content":"[@episode/active] confirmed session","memory_type":"episodic","session_id":"session-a","trust_tier":"T3","idempotency_key":"same-operation","extra_metadata":{"agent":{"a":1,"b":2},"scene":"qa"}}"#).unwrap();
    let response: Value = server
        .client
        .post(&url)
        .header("Authorization", format!("Memoria-Owner {master}"))
        .header("X-User-Id", &owner)
        .json(&reordered)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(Some(response["memory_id"].clone()), id);
    let get: Value = server
        .client
        .get(format!(
            "{}/v1/memories/{}",
            server.base,
            response["memory_id"].as_str().unwrap()
        ))
        .header("Authorization", format!("Memoria-Owner {master}"))
        .header("X-User-Id", &owner)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(get["extra_metadata"], body["extra_metadata"]);
    // Ordinary vector dedup must leave exact-key receipts untouched, while
    // remaining effective for ordinary rows. Same embedding/content exercises
    // the metadata-refresh branch that previously overwrote the receipt hash.
    let mut ordinary = body.clone();
    ordinary.as_object_mut().unwrap().remove("idempotency_key");
    ordinary["session_id"] = json!("ordinary-session");
    ordinary["extra_metadata"] = json!({"scene":"ordinary"});
    let ordinary_url = format!("{}/v1/memories", server.base);
    let mut ordinary_id = None;
    for _ in 0..2 {
        let response: Value = server
            .client
            .post(&ordinary_url)
            .header("Authorization", format!("Memoria-Owner {master}"))
            .header("X-User-Id", &owner)
            .json(&ordinary)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_ne!(Some(response["memory_id"].clone()), id);
        assert_eq!(response["session_id"], "ordinary-session");
        if let Some(expected) = ordinary_id.as_ref() {
            assert_eq!(&response["memory_id"], expected);
        }
        ordinary_id = Some(response["memory_id"].clone());
    }
    let replay: Value = server
        .client
        .post(&url)
        .header("Authorization", format!("Memoria-Owner {master}"))
        .header("X-User-Id", &owner)
        .json(&body)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(Some(replay["memory_id"].clone()), id);
    assert_eq!(replay["extra_metadata"], body["extra_metadata"]);
    let mut reserved = body.clone();
    reserved["extra_metadata"][memoria_core::STORE_REQUEST_HASH_FIELD] = json!("forged");
    for endpoint in [url.clone(), format!("{}/v1/memories", server.base)] {
        let mut request = reserved.clone();
        if endpoint == ordinary_url {
            request.as_object_mut().unwrap().remove("idempotency_key");
        }
        let response = server
            .client
            .post(endpoint)
            .header("Authorization", format!("Memoria-Owner {master}"))
            .header("X-User-Id", &owner)
            .json(&request)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 422);
    }
    let response = server
        .client
        .post(format!("{}/v1/memories/query", server.base))
        .header("Authorization", format!("Memoria-Owner {master}"))
        .header("X-User-Id", &owner)
        .json(&json!({"extra_metadata_filter":{(memoria_core::STORE_REQUEST_HASH_FIELD):"hidden"}}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 422);
    let mut changed = body.clone();
    changed["content"] = json!("different payload");
    let response = server
        .client
        .post(&url)
        .header("Authorization", format!("Memoria-Owner {master}"))
        .header("X-User-Id", &owner)
        .json(&changed)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 422);
    let mut missing = body.clone();
    missing.as_object_mut().unwrap().remove("idempotency_key");
    assert_eq!(
        server
            .client
            .post(&url)
            .header("Authorization", format!("Memoria-Owner {master}"))
            .header("X-User-Id", &owner)
            .json(&missing)
            .send()
            .await
            .unwrap()
            .status(),
        422
    );
    assert_eq!(
        server
            .client
            .post(&url)
            .header("X-User-Id", &owner)
            .json(&body)
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    // Batch writes have no atomic replay protocol and must reject keys before
    // inserting even the first row.
    let response = server
        .client
        .post(format!("{}/v1/memories/batch", server.base))
        .header("Authorization", format!("Memoria-Owner {master}"))
        .header("X-User-Id", &owner)
        .json(&json!({"memories":[body.clone()]}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 422);
    let mut reserved_batch = reserved.clone();
    reserved_batch
        .as_object_mut()
        .unwrap()
        .remove("idempotency_key");
    let response = server
        .client
        .post(format!("{}/v1/memories/batch", server.base))
        .header("Authorization", format!("Memoria-Owner {master}"))
        .header("X-User-Id", &owner)
        .json(&json!({"memories":[reserved_batch]}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 422);
    let mut no_metadata = body.clone();
    no_metadata
        .as_object_mut()
        .unwrap()
        .remove("extra_metadata");
    no_metadata["idempotency_key"] = json!("no-metadata-operation");
    let response: Value = server
        .client
        .post(&url)
        .header("Authorization", format!("Memoria-Owner {master}"))
        .header("X-User-Id", &owner)
        .json(&no_metadata)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(response.get("extra_metadata").is_none());
    assert!(response["memory_id"].as_str().is_some());
    let other = format!("idem_other_{}", uuid::Uuid::new_v4().simple());
    let response: Value = server
        .client
        .post(&url)
        .header("Authorization", format!("Memoria-Owner {master}"))
        .header("X-User-Id", &other)
        .json(&body)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(response["user_id"], other);
    assert_ne!(response["memory_id"], id.unwrap());
}
