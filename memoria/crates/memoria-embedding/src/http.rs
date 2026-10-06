use async_trait::async_trait;
use memoria_core::{interfaces::EmbeddingProvider, MemoriaError};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Semaphore;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_RETRIES: u32 = 2;
const DEFAULT_MAX_CONCURRENT: usize = 32;
const DEFAULT_SEMAPHORE_TIMEOUT_SECS: u64 = 5;

/// HTTP-based embedding client — OpenAI-compatible API.
pub struct HttpEmbedder {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
    model: String,
    dimension: usize,
    /// Semaphore to limit concurrent embedding requests
    semaphore: Arc<Semaphore>,
    /// Timeout for acquiring a semaphore permit
    semaphore_timeout: Duration,
}

#[derive(Serialize)]
struct EmbedRequest<'a> {
    input: EmbedInput<'a>,
    model: &'a str,
}

#[derive(Serialize)]
#[serde(untagged)]
enum EmbedInput<'a> {
    Single(&'a str),
    Batch(&'a [String]),
}

#[derive(Deserialize)]
struct EmbedResponse {
    data: Vec<EmbedData>,
}

#[derive(Deserialize)]
struct EmbedData {
    embedding: Vec<f32>,
}

impl HttpEmbedder {
    pub fn new(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<String>,
        dimension: usize,
    ) -> Self {
        let max_concurrent: usize = std::env::var("EMBED_MAX_CONCURRENT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_MAX_CONCURRENT)
            .clamp(1, 256);
        let semaphore_timeout = Duration::from_secs(
            std::env::var("EMBED_SEMAPHORE_TIMEOUT_SECS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(DEFAULT_SEMAPHORE_TIMEOUT_SECS)
                .clamp(1, 120),
        );
        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self {
            client,
            base_url: base_url.into(),
            api_key: api_key.into(),
            model: model.into(),
            dimension,
            semaphore: Arc::new(Semaphore::new(max_concurrent)),
            semaphore_timeout,
        }
    }
}

impl HttpEmbedder {
    /// Send an embedding request with retry on transient errors.
    async fn post_embed(&self, body: &EmbedRequest<'_>) -> Result<EmbedResponse, MemoriaError> {
        let _permit = tokio::time::timeout(self.semaphore_timeout, self.semaphore.acquire())
            .await
            .map_err(|_| {
                MemoriaError::Embedding("embedding concurrency limit timeout".to_string())
            })?
            .map_err(|_| MemoriaError::Embedding("embedding semaphore closed".to_string()))?;
        let url = format!("{}/embeddings", self.base_url.trim_end_matches('/'));
        let mut last_err = String::new();
        for attempt in 0..=MAX_RETRIES {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_millis(200 * (1 << (attempt - 1)))).await;
            }
            let resp = match self
                .client
                .post(&url)
                .bearer_auth(&self.api_key)
                .json(body)
                .send()
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    // Reqwest errors can include URLs. Keep diagnostics independent
                    // of provider credentials, request inputs and upstream bodies.
                    last_err = if e.is_timeout() {
                        "embedding request timed out".into()
                    } else {
                        "embedding request failed".into()
                    };
                    continue;
                }
            };
            if resp.status().is_server_error()
                || resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS
            {
                last_err = format!("HTTP {}", resp.status());
                continue;
            }
            if !resp.status().is_success() {
                let status = resp.status();
                return Err(MemoriaError::Embedding(format!("HTTP {status}")));
            }
            // Read separately from JSON parsing: response-body transport failures
            // (including truncation/timeouts after headers) can be transient.
            // Retrying the embedding computation happens before memory persistence.
            let bytes = match resp.bytes().await {
                Ok(bytes) => bytes,
                Err(_) => {
                    last_err = "embedding response body could not be read".into();
                    continue;
                }
            };
            // A fully received but incompatible response should fail promptly.
            // Serde error text can echo unexpected values, so expose only its class.
            return serde_json::from_slice::<EmbedResponse>(&bytes).map_err(|error| {
                let reason = match error.classify() {
                    serde_json::error::Category::Data => "invalid embedding response schema",
                    _ => "invalid embedding response JSON",
                };
                MemoriaError::Embedding(reason.into())
            });
        }
        Err(MemoriaError::Embedding(format!(
            "failed after {} attempts: {last_err}",
            MAX_RETRIES + 1
        )))
    }
}

#[async_trait]
impl EmbeddingProvider for HttpEmbedder {
    async fn embed(&self, text: &str) -> Result<Vec<f32>, MemoriaError> {
        let data = self
            .post_embed(&EmbedRequest {
                input: EmbedInput::Single(text),
                model: &self.model,
            })
            .await?;
        data.data
            .into_iter()
            .next()
            .map(|d| d.embedding)
            .ok_or_else(|| MemoriaError::Embedding("Empty embedding response".into()))
    }

    async fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, MemoriaError> {
        if texts.is_empty() {
            return Ok(vec![]);
        }
        if texts.len() == 1 {
            return Ok(vec![self.embed(&texts[0]).await?]);
        }
        let data = self
            .post_embed(&EmbedRequest {
                input: EmbedInput::Batch(texts),
                model: &self.model,
            })
            .await?;
        Ok(data.data.into_iter().map(|d| d.embedding).collect())
    }

    fn dimension(&self) -> usize {
        self.dimension
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::task::JoinHandle;

    const VECTOR: &str = r#"{"data":[{"embedding":[0.1,0.2,0.3]}]}"#;
    const BATCH: &str = r#"{"data":[{"embedding":[0.1,0.2,0.3]},{"embedding":[0.4,0.5,0.6]}]}"#;

    #[derive(Clone, Copy)]
    struct Reply {
        status: &'static str,
        body: &'static str,
        extra_length: usize,
    }

    impl Reply {
        fn ok(body: &'static str) -> Self {
            Self {
                status: "200 OK",
                body,
                extra_length: 0,
            }
        }

        fn truncated() -> Self {
            Self {
                extra_length: 100,
                ..Self::ok("{\"data\":[")
            }
        }
    }

    struct Server {
        url: String,
        calls: Arc<AtomicUsize>,
        task: JoinHandle<()>,
    }

    impl Server {
        async fn start(replies: Vec<Reply>) -> Self {
            assert!(!replies.is_empty());
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}/v1", listener.local_addr().unwrap());
            let calls = Arc::new(AtomicUsize::new(0));
            let count = calls.clone();
            let task = tokio::spawn(async move {
                loop {
                    let (mut socket, _) = listener.accept().await.unwrap();
                    // Consume the entire request before closing the connection;
                    // otherwise unread input can cause an unrelated TCP reset.
                    tokio::time::timeout(Duration::from_secs(5), async {
                        let mut request = Vec::new();
                        let mut buffer = [0u8; 4096];
                        loop {
                            let n = socket.read(&mut buffer).await.unwrap();
                            assert!(n > 0);
                            request.extend_from_slice(&buffer[..n]);
                            assert!(request.len() < 65_536);
                            if let Some(end) = request.windows(4).position(|s| s == b"\r\n\r\n") {
                                let headers = String::from_utf8_lossy(&request[..end]);
                                let length: usize = headers.lines().find_map(|line| {
                                    let (name, value) = line.split_once(':')?;
                                    name.eq_ignore_ascii_case("content-length")
                                        .then(|| value.trim().parse().unwrap())
                                }).unwrap_or(0);
                                if request.len() >= end + 4 + length {
                                    break;
                                }
                            }
                        }
                        let index = count.fetch_add(1, Ordering::SeqCst);
                        let reply = replies[index.min(replies.len() - 1)];
                        let response = format!(
                            "HTTP/1.1 {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            reply.status, reply.body.len() + reply.extra_length, reply.body,
                        );
                        socket.write_all(response.as_bytes()).await.unwrap();
                        socket.shutdown().await.unwrap();
                    }).await.unwrap();
                }
            });
            Self { url, calls, task }
        }

        fn embedder(&self) -> HttpEmbedder {
            HttpEmbedder::new(&self.url, "synthetic-key", "synthetic-model", 3)
        }

        fn requests(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    impl Drop for Server {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    #[tokio::test]
    async fn truncated_response_retries_before_returning_single_embedding() {
        let server = Server::start(vec![Reply::truncated(), Reply::ok(VECTOR)]).await;
        let result = server.embedder().embed("original fact").await.unwrap();
        assert_eq!(result, vec![0.1, 0.2, 0.3]);
        assert_eq!(server.requests(), 2);
    }

    #[tokio::test]
    async fn truncated_response_retries_before_returning_batch_embeddings() {
        let server = Server::start(vec![Reply::truncated(), Reply::ok(BATCH)]).await;
        let result = server
            .embedder()
            .embed_batch(&["first".into(), "second".into()])
            .await
            .unwrap();
        assert_eq!(result, vec![vec![0.1, 0.2, 0.3], vec![0.4, 0.5, 0.6]]);
        assert_eq!(server.requests(), 2);
    }

    #[tokio::test]
    async fn persistent_body_failure_exhausts_the_existing_retry_budget() {
        let server = Server::start(vec![Reply::truncated()]).await;
        let result = server.embedder().embed("original fact").await.unwrap_err();
        assert_eq!(server.requests(), (MAX_RETRIES + 1) as usize);
        assert!(result.to_string().contains("failed after 3 attempts"));
        assert!(result
            .to_string()
            .contains("embedding response body could not be read"));
    }

    #[tokio::test]
    async fn received_invalid_json_is_not_retried() {
        let server = Server::start(vec![
            Reply::ok("not JSON synthetic-secret"),
            Reply::ok(VECTOR),
        ])
        .await;
        let error = server.embedder().embed("original fact").await.unwrap_err();
        assert_eq!(server.requests(), 1);
        assert!(error
            .to_string()
            .contains("invalid embedding response JSON"));
        assert!(!error.to_string().contains("synthetic-secret"));
    }

    #[tokio::test]
    async fn received_invalid_schema_is_not_retried_or_echoed() {
        let invalid = r#"{"data":[{"embedding":["synthetic-secret"]}]}"#;
        let server = Server::start(vec![Reply::ok(invalid), Reply::ok(VECTOR)]).await;
        let error = server.embedder().embed("original fact").await.unwrap_err();
        assert_eq!(server.requests(), 1);
        assert!(error
            .to_string()
            .contains("invalid embedding response schema"));
        assert!(!error.to_string().contains("synthetic-secret"));
    }

    #[tokio::test]
    async fn authentication_failures_are_not_retried_or_echoed() {
        let server = Server::start(vec![
            Reply {
                status: "401 Unauthorized",
                body: "synthetic-secret",
                extra_length: 0,
            },
            Reply::ok(VECTOR),
        ])
        .await;
        let error = server.embedder().embed("original fact").await.unwrap_err();
        assert_eq!(server.requests(), 1);
        assert!(error.to_string().contains("HTTP 401"));
        assert!(!error.to_string().contains("synthetic-secret"));
    }

    #[tokio::test]
    async fn server_errors_and_rate_limits_still_retry() {
        for status in ["503 Service Unavailable", "429 Too Many Requests"] {
            let server = Server::start(vec![
                Reply {
                    status,
                    body: "retry later",
                    extra_length: 0,
                },
                Reply::ok(VECTOR),
            ])
            .await;
            assert_eq!(
                server.embedder().embed("original fact").await.unwrap(),
                vec![0.1, 0.2, 0.3]
            );
            assert_eq!(server.requests(), 2);
        }
    }

    #[tokio::test]
    async fn valid_response_is_not_retried() {
        let server = Server::start(vec![Reply::ok(VECTOR)]).await;
        assert_eq!(
            server.embedder().embed("original fact").await.unwrap(),
            vec![0.1, 0.2, 0.3]
        );
        assert_eq!(server.requests(), 1);
    }
}
