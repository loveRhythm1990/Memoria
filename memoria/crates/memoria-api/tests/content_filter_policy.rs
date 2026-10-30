//! Exercise the real policy middleware and AuthUser admission without a database.
use std::sync::Arc;

use axum::{extract::State, middleware, routing::post, Json, Router};
use memoria_api::{auth::AuthUser, state::CachedApiKeyPrincipal, AppState};
use memoria_service::{content_filter::check_content_sensitivity, MemoryService};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

const MASTER: &str = "synthetic-owner-policy-test-master";
const OWNER: &str = "synthetic-owner";
const TEXT: &str = "The standard bearer carried the flag. Contact person@example.com.";

async fn probe(auth: AuthUser, State(_state): State<AppState>) -> Json<Value> {
    let result = check_content_sensitivity(auth.scope_id(), TEXT);
    Json(json!({
        "blocked": result.blocked,
        "redacted": result.redacted_content.is_some(),
        "other_owner_blocked": check_content_sensitivity("other-owner", TEXT).blocked,
    }))
}

async fn start(enabled: bool) -> (String, reqwest::Client, tokio::task::JoinHandle<()>) {
    // The probe uses no storage operations. A lazy pool avoids DB/network setup.
    let pool = sqlx::mysql::MySqlPoolOptions::new()
        .connect_lazy("mysql://test:test@127.0.0.1:1/policy_test")
        .unwrap();
    let store = Arc::new(memoria_storage::SqlMemoryStore::new(
        pool.clone(),
        4,
        "policy-test".into(),
    ));
    let service = Arc::new(MemoryService::new(store.clone(), None, Some(store)));
    let git = Arc::new(memoria_git::GitForDataService::new(pool, "policy_test"));
    let mut state = AppState::new(service, git, MASTER.into());
    state.owner_content_filter_enabled = enabled;
    state.api_key_cache.insert(
        format!("{:x}", Sha256::digest("synthetic-personal-api-key")),
        CachedApiKeyPrincipal {
            key_id: "test-key".into(),
            user_id: OWNER.into(),
            group_id: None,
            key_prefix: "test".into(),
            scopes: vec!["memory:write".into()],
            expires_at: None,
        },
    );
    let app = Router::new()
        .route("/v1/memories", post(probe))
        .route("/v1/memories/idempotent", post(probe))
        .with_state(state.clone())
        .layer(middleware::from_fn_with_state(
            state,
            memoria_api::auth::owner_content_filter_layer,
        ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .no_proxy()
        .build()
        .unwrap();
    (base, client, task)
}

#[tokio::test]
async fn only_valid_owner_auth_can_use_server_configured_override() {
    for enabled in [true, false] {
        let (base, client, server) = start(enabled).await;
        for endpoint in ["/v1/memories", "/v1/memories/idempotent"] {
            let response: Value = client
                .post(format!("{base}{endpoint}"))
                .header("Authorization", format!("Memoria-Owner {MASTER}"))
                .header("X-User-Id", OWNER)
                .send()
                .await
                .unwrap()
                .error_for_status()
                .unwrap()
                .json()
                .await
                .unwrap();
            assert_eq!(response["blocked"], enabled);
            assert_eq!(response["other_owner_blocked"], true);
        }

        for token in [MASTER, "synthetic-personal-api-key"] {
            let response: Value = client
                .post(format!("{base}/v1/memories?content_filter_enabled=false"))
                .bearer_auth(token)
                .header("X-User-Id", OWNER)
                .header("X-Content-Filter-Enabled", "false")
                .json(&json!({"content_filter_enabled":false}))
                .send()
                .await
                .unwrap()
                .error_for_status()
                .unwrap()
                .json()
                .await
                .unwrap();
            assert_eq!(response["blocked"], true);
        }

        for (token, owner, expected) in [
            ("incorrect-master", Some(OWNER), 401),
            (MASTER, None, 400),
            (MASTER, Some("grp_reserved"), 400),
        ] {
            let mut request = client
                .post(format!("{base}/v1/memories"))
                .header("Authorization", format!("Memoria-Owner {token}"));
            if let Some(owner) = owner {
                request = request.header("X-User-Id", owner);
            }
            assert_eq!(request.send().await.unwrap().status().as_u16(), expected);
        }
        let duplicate_owner = client
            .post(format!("{base}/v1/memories"))
            .header("Authorization", format!("Memoria-Owner {MASTER}"))
            .header("X-User-Id", OWNER)
            .header("X-User-Id", "other-owner")
            .send()
            .await
            .unwrap();
        assert_eq!(duplicate_owner.status(), 400);
        server.abort();
    }
}
