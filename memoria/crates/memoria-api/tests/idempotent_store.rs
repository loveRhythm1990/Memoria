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
        None,
        None,
        None,
        false,
    )
    .await;
    let owner = format!("idem_api_{}", uuid::Uuid::new_v4().simple());
    let url = format!("{}/v1/memories/idempotent", server.base);
    let body = json!({"content":"[@episode/active] confirmed session", "memory_type":"episodic", "session_id":"session-a", "trust_tier":"T3", "idempotency_key":"same-operation"});
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
        if let Some(ref id) = id {
            assert_eq!(&response["memory_id"], id);
        } else {
            id = Some(response["memory_id"].clone());
        }
    }
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
