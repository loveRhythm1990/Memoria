//! Regression coverage for deduplicated observe against the production SQL store.
//! Uses the same isolated MatrixOne databases as the other API integration tests.
use memoria_core::MemoryType;
use serde_json::{json, Value};

mod support;

const MASTER_KEY: &str = "observe-dedup-test-master";

#[tokio::test]
async fn inactive_exclusions_survive_same_turn_correction_and_forget_in_sql() {
    for corrected in [true, false] {
        let user = format!("observe_user_{}", uuid::Uuid::new_v4().simple());
        let mut items = vec![
            json!({"content": "User likes rainy days"}),
            json!({"content": "用户喝无糖咖啡"}),
        ];
        if corrected {
            items.push(json!({"content": "User likes sunny days"}));
        }
        let (llm, _shutdown) =
            memoria_test_utils::spawn_fake_llm(vec![("already_saved_memories", json!(items))])
                .await;
        let server = support::multi_db::spawn_api_server(
            "observe_inactive",
            4,
            MASTER_KEY.into(),
            None,
            Some(llm),
            None,
            false,
        )
        .await;
        let service = server.service();
        let saved = service
            .store_memory(
                &user,
                "User likes rainy days",
                MemoryType::Profile,
                None,
                None,
                None,
                None,
                None,
                Some("subject".into()),
            )
            .await
            .unwrap();
        let mut excluded = vec![saved.memory_id.clone()];
        if corrected {
            let updated = service
                .correct_on_branch(
                    &user,
                    Some("main"),
                    &saved.memory_id,
                    "User likes sunny days",
                )
                .await
                .unwrap();
            excluded.push(updated.memory_id);
        } else {
            service
                .purge_on_branch(&user, Some("main"), &saved.memory_id)
                .await
                .unwrap();
        }
        assert!(service
            .get_for_user_on_branch(&user, Some("main"), &saved.memory_id)
            .await
            .unwrap()
            .is_none());
        let store = server.user_store(&user).await;
        let table = store.table_for_branch(&user, Some("main")).await.unwrap();
        assert_eq!(
            store
                .observe_exclusion_content_from(&table, &user, Some("subject"), &saved.memory_id,)
                .await
                .unwrap()
                .as_deref(),
            Some("User likes rainy days")
        );
        for (user, subject) in [("other-user", "subject"), (&user, "other-subject")] {
            assert!(store
                .observe_exclusion_content_from(&table, user, Some(subject), &saved.memory_id,)
                .await
                .unwrap()
                .is_none());
        }
        let unscoped = service
            .store_memory(
                &user,
                "Default-subject preference",
                MemoryType::Profile,
                None,
                None,
                None,
                None,
                None,
                None,
            )
            .await
            .unwrap();
        service
            .purge_on_branch(&user, Some("main"), &unscoped.memory_id)
            .await
            .unwrap();
        assert_eq!(
            store
                .observe_exclusion_content_from(&table, &user, None, &unscoped.memory_id,)
                .await
                .unwrap()
                .as_deref(),
            Some("Default-subject preference")
        );
        assert!(store
            .observe_exclusion_content_from(&table, &user, Some("subject"), &unscoped.memory_id,)
            .await
            .unwrap()
            .is_none());
        // A physically purged/stale hint must not poison the other exclusions.
        excluded.push("missing-id".into());
        let response = server
            .client
            .post(format!("{}/v1/observe/deduplicated", server.base))
            .bearer_auth(MASTER_KEY)
            .header("X-User-Id", &user)
            .json(&json!({
                "messages": [{"role": "user", "content": "Also remember my coffee preference"}],
                "subject_id": "subject", "branch": "main", "exclude_memory_ids": excluded,
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(response.headers()["x-memoria-observe-deduplicated"], "1");
        let body: Value = response.json().await.unwrap();
        assert_eq!(body["memories"].as_array().unwrap().len(), 1);
        assert_eq!(body["memories"][0]["content"], "用户喝无糖咖啡");
        assert!(service
            .get_for_user_on_branch(&user, Some("main"), &saved.memory_id)
            .await
            .unwrap()
            .is_none());
        assert_eq!(
            service.list_active(&user, 20).await.unwrap().len(),
            if corrected { 2 } else { 1 }
        );
    }
}

#[tokio::test]
async fn extraction_errors_certify_no_writes_and_business_404_is_marked() {
    let user = format!("observe_user_{}", uuid::Uuid::new_v4().simple());
    let (llm, _shutdown) =
        memoria_test_utils::spawn_fake_llm(vec![("new fact", json!({"invalid": "not an array"}))])
            .await;
    let server = support::multi_db::spawn_api_server(
        "observe_errors",
        4,
        MASTER_KEY.into(),
        None,
        Some(llm),
        None,
        false,
    )
    .await;
    let payload = json!({
        "messages": [{"role": "user", "content": "new fact"}],
        "branch": "main", "exclude_memory_ids": ["physically-purged-id"],
    });
    let response = server
        .client
        .post(format!("{}/v1/observe/deduplicated", server.base))
        .bearer_auth(MASTER_KEY)
        .header("X-User-Id", &user)
        .json(&payload)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 503);
    assert_eq!(response.headers()["x-memoria-observe-deduplicated"], "1");
    assert_eq!(
        response.headers()["x-memoria-observe-error"],
        "extraction_unavailable"
    );
    assert!(server
        .service()
        .list_active(&user, 20)
        .await
        .unwrap()
        .is_empty());

    let mut missing_branch = payload;
    missing_branch["branch"] = json!("deleted-branch");
    let response = server
        .client
        .post(format!("{}/v1/observe/deduplicated", server.base))
        .bearer_auth(MASTER_KEY)
        .header("X-User-Id", &user)
        .json(&missing_branch)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 404);
    assert_eq!(response.headers()["x-memoria-observe-deduplicated"], "1");
    assert!(response.headers().get("x-memoria-observe-error").is_none());

    // The old route retains its original 500 error mapping for business errors.
    let response = server
        .client
        .post(format!("{}/v1/observe", server.base))
        .bearer_auth(MASTER_KEY)
        .header("X-User-Id", &user)
        .json(&missing_branch)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 500);
    assert!(response
        .headers()
        .get("x-memoria-observe-deduplicated")
        .is_none());
    let response = server
        .client
        .post(format!("{}/v1/observe", server.base))
        .bearer_auth(MASTER_KEY)
        .header("X-User-Id", &user)
        .json(&json!({"messages": [{"role": "user", "content": "new fact"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["memories"][0]["content"], "new fact");
}

#[tokio::test]
async fn deduplicated_route_requires_nonempty_ids_and_read_scope() {
    let user = format!("observe_user_{}", uuid::Uuid::new_v4().simple());
    let server = support::multi_db::spawn_api_server(
        "observe_scope",
        4,
        MASTER_KEY.into(),
        None,
        None,
        None,
        false,
    )
    .await;
    let response = server
        .client
        .post(format!("{}/v1/observe/deduplicated", server.base))
        .bearer_auth(MASTER_KEY)
        .header("X-User-Id", &user)
        .json(&json!({"messages": [], "exclude_memory_ids": []}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 422);
    let response = server
        .client
        .post(format!("{}/auth/keys", server.base))
        .bearer_auth(MASTER_KEY)
        .json(&json!({"user_id": &user, "name": "write-only", "scopes": ["identity:read", "memory:read", "memory:write"]}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 201);
    let key: Value = response.json().await.unwrap();
    // Current key creation requires write => read. Model an older/manually
    // provisioned key to exercise the handler's independent read-scope guard.
    sqlx::query("UPDATE mem_api_keys SET scopes = 'identity:read,memory:write' WHERE key_id = ?")
        .bind(key["key_id"].as_str().unwrap())
        .execute(&server.shared_pool())
        .await
        .unwrap();
    let response = server
        .client
        .post(format!("{}/v1/observe/deduplicated", server.base))
        .bearer_auth(key["raw_key"].as_str().unwrap())
        .json(&json!({"messages": [], "exclude_memory_ids": ["some-id"]}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 403);
    assert_eq!(response.headers()["x-memoria-observe-deduplicated"], "1");
    assert!(response.text().await.unwrap().contains("memory:read"));
}
