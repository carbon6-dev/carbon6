//! Effective context windows and running OpenAI-compatible server limits.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use serde_json::Value;
use tokio::time::Instant;

use crate::{Provider, model_info, pool::Endpoint};

const INTERVAL: Duration = Duration::from_secs(60);
const TIMEOUT: Duration = Duration::from_secs(3);

/// A setting wins; detection can only lower the built-in window.
pub fn effective(built_in: u64, setting: Option<u64>, detected: Option<u64>) -> u64 {
    setting.unwrap_or_else(|| detected.map_or(built_in, |n| n.min(built_in)))
}

pub fn built_in(kind: &str, model: &str) -> u64 {
    model_info(Provider::from_kind(kind).unwrap_or(Provider::OpenAiChat), model).context_window
}

/// A known window, with the built-in ceiling applied to detection. Unknown
/// servers keep the caller's existing behaviour. Cached in this process.
pub async fn known(connection: &str, model: &str, endpoint: &Endpoint) -> Option<u64> {
    if endpoint.context_window > 0 {
        return Some(effective(built_in(&endpoint.kind, model), Some(endpoint.context_window), None));
    }
    if endpoint.kind != "openai_compat" {
        return None;
    }
    static CACHE: OnceLock<Cache> = OnceLock::new();
    let detected = CACHE.get_or_init(Cache::default).detect(connection, model, endpoint).await?;
    Some(effective(built_in(&endpoint.kind, model), None, Some(detected)))
}

#[derive(Default)]
struct Entry {
    checked: Option<Instant>,
    last: Option<u64>,
}

type Cached = Arc<tokio::sync::Mutex<Entry>>;

#[derive(Default)]
struct Cache {
    entries: Mutex<HashMap<(String, String), Cached>>,
}

impl Cache {
    async fn detect(&self, connection: &str, model: &str, endpoint: &Endpoint) -> Option<u64> {
        let entry = self.entries.lock().unwrap().entry((connection.to_owned(), model.to_owned())).or_default().clone();
        let mut entry = entry.lock().await;
        if entry.checked.is_some_and(|at| at.elapsed() < INTERVAL) {
            return entry.last;
        }
        entry.checked = Some(Instant::now());
        if let Some(n) = detect(model, endpoint).await {
            entry.last = Some(n);
        }
        entry.last
    }
}

/// Ollama treats an untagged model as its `latest` tag.
pub(crate) fn same_model(a: &str, b: &str) -> bool {
    a.strip_suffix(":latest").unwrap_or(a) == b.strip_suffix(":latest").unwrap_or(b)
}

async fn get(http: &reqwest::Client, endpoint: &Endpoint, url: String) -> Option<Value> {
    let mut req = http.get(url).timeout(TIMEOUT);
    if !endpoint.bearer.is_empty() {
        req = req.bearer_auth(&endpoint.bearer);
    }
    for (k, v) in &endpoint.headers {
        req = req.header(k, v);
    }
    req.send().await.ok()?.error_for_status().ok()?.json().await.ok()
}

async fn detect(model: &str, endpoint: &Endpoint) -> Option<u64> {
    static HTTP: OnceLock<Option<reqwest::Client>> = OnceLock::new();
    let http = HTTP.get_or_init(|| crate::http::client().ok()).as_ref()?;
    let base = endpoint.base_url.trim_end_matches('/');
    let root = base.strip_suffix("/v1").unwrap_or(base);
    let (models, props, ps) = tokio::join!(
        get(http, endpoint, format!("{base}/models")),
        get(http, endpoint, format!("{root}/props")),
        get(http, endpoint, format!("{root}/api/ps")),
    );
    let vllm = models.as_ref().and_then(|v| {
        v["data"].as_array()?.iter().find(|m| m["id"].as_str().is_some_and(|id| same_model(id, model)))?["max_model_len"].as_u64()
    });
    let llama = props.as_ref().and_then(|v| positive(&v["default_generation_settings"]["n_ctx"]).or_else(|| positive(&v["n_ctx"])));
    let ollama = ps.as_ref().and_then(|v| {
        v["models"]
            .as_array()?
            .iter()
            .find(|m| ["name", "model"].iter().any(|key| m[key].as_str().is_some_and(|id| same_model(id, model))))?["context_length"]
            .as_u64()
    });
    [vllm, llama, ollama].into_iter().flatten().filter(|n| *n > 0).min()
}

fn positive(v: &Value) -> Option<u64> {
    v.as_u64().filter(|n| *n > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    use axum::{
        Json, Router,
        http::{HeaderMap, StatusCode, Uri},
        response::IntoResponse,
    };
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Server {
        endpoint: Endpoint,
        body: Arc<Mutex<Value>>,
        calls: Arc<AtomicUsize>,
    }

    async fn server(path: &'static str, value: Value) -> Server {
        let body = Arc::new(Mutex::new(value));
        let calls = Arc::new(AtomicUsize::new(0));
        let (response, count) = (body.clone(), calls.clone());
        let app = Router::new().fallback(move |uri: Uri, headers: HeaderMap| {
            let (body, calls) = (response.clone(), count.clone());
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                assert_eq!(headers["authorization"], "Bearer key");
                assert_eq!(headers["x-window"], "sent");
                if uri.path() == path { Json(body.lock().unwrap().clone()).into_response() } else { StatusCode::NOT_FOUND.into_response() }
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}/v1/", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Server {
            endpoint: Endpoint {
                kind: "openai_compat".into(),
                base_url,
                bearer: "key".into(),
                headers: HashMap::from([("x-window".into(), "sent".into())]),
                ..Default::default()
            },
            body,
            calls,
        }
    }

    #[tokio::test]
    async fn vllm_reports_the_requested_models_window() {
        let s = server(
            "/v1/models",
            json!({"data": [
                {"id": "other", "max_model_len": 1024},
                {"id": "qwen", "max_model_len": 32768}
            ]}),
        )
        .await;
        assert_eq!(Cache::default().detect("c", "qwen", &s.endpoint).await, Some(32_768));
        assert_eq!(s.calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn llama_cpp_uses_the_per_slot_window_else_top_level() {
        let s = server("/props", json!({"default_generation_settings": {"n_ctx": 8192}, "n_ctx": 65536})).await;
        assert_eq!(Cache::default().detect("c", "qwen", &s.endpoint).await, Some(8_192));
        *s.body.lock().unwrap() = json!({"n_ctx": 16384});
        assert_eq!(Cache::default().detect("c", "qwen", &s.endpoint).await, Some(16_384));
    }

    #[tokio::test]
    async fn ollama_matches_latest_on_name_or_model() {
        let s = server(
            "/api/ps",
            json!({"models": [
                {"name": "other", "context_length": 1024},
                {"name": "qwen:latest", "context_length": 16384}
            ]}),
        )
        .await;
        assert_eq!(Cache::default().detect("c", "qwen", &s.endpoint).await, Some(16_384));
        *s.body.lock().unwrap() = json!({"models": [{"model": "qwen", "context_length": 8192}]});
        assert_eq!(Cache::default().detect("c", "qwen:latest", &s.endpoint).await, Some(8_192));
    }

    #[tokio::test]
    async fn missing_or_unexpected_metadata_is_unknown() {
        for body in [Value::Null, json!({"models": "garbage"}), json!({"models": [{"name": "qwen", "context_length": 0}]})] {
            let s = server("/api/ps", body).await;
            assert_eq!(Cache::default().detect("c", "qwen", &s.endpoint).await, None);
        }
        let s = server("/unused", json!({})).await;
        assert_eq!(Cache::default().detect("c", "qwen", &s.endpoint).await, None);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = Endpoint { base_url: format!("http://{}/v1", listener.local_addr().unwrap()), ..s.endpoint };
        tokio::spawn(async move { axum::serve(listener, Router::new().fallback(|| async { "not json" })).await.unwrap() });
        assert_eq!(Cache::default().detect("c", "qwen", &endpoint).await, None);
    }

    #[tokio::test]
    async fn cache_keeps_the_last_window_when_ollama_unloads() {
        let s = server("/api/ps", json!({"models": [{"name": "qwen", "context_length": 16384}]})).await;
        let cache = Cache::default();
        assert_eq!(cache.detect("c", "qwen", &s.endpoint).await, Some(16_384));
        *s.body.lock().unwrap() = json!({"models": []});
        assert_eq!(cache.detect("c", "qwen", &s.endpoint).await, Some(16_384));
        assert_eq!(s.calls.load(Ordering::SeqCst), 3);
        // Make the entry due, without a wall-clock wait.
        let entry = cache.entries.lock().unwrap()[&("c".into(), "qwen".into())].clone();
        entry.lock().await.checked = Some(Instant::now() - INTERVAL);
        assert_eq!(cache.detect("c", "qwen", &s.endpoint).await, Some(16_384));
        assert_eq!(s.calls.load(Ordering::SeqCst), 6);
        assert_eq!(cache.detect("other", "qwen", &s.endpoint).await, None);
        assert_eq!(cache.detect("c", "other", &s.endpoint).await, None);
        assert_eq!(s.calls.load(Ordering::SeqCst), 12);
        *s.body.lock().unwrap() = json!({"models": [{"name": "qwen", "context_length": 8192}]});
        entry.lock().await.checked = Some(Instant::now() - INTERVAL);
        assert_eq!(cache.detect("c", "qwen", &s.endpoint).await, Some(8_192));
    }

    #[tokio::test]
    async fn only_compat_is_detected_and_settings_skip_detection() {
        let mut s = server("/api/ps", json!({"models": [{"name": "qwen", "context_length": 16384}]})).await;
        s.endpoint.kind = "openai".into();
        assert_eq!(known(&s.endpoint.base_url, "qwen", &s.endpoint).await, None);
        s.endpoint.context_window = 256_000;
        assert_eq!(known(&s.endpoint.base_url, "qwen", &s.endpoint).await, Some(256_000));
        s.endpoint.kind = "openai_compat".into();
        assert_eq!(known(&s.endpoint.base_url, "qwen", &s.endpoint).await, Some(256_000));
        assert_eq!(s.calls.load(Ordering::SeqCst), 0);
        s.endpoint.context_window = 0;
        assert_eq!(known(&s.endpoint.base_url, "qwen", &s.endpoint).await, Some(16_384));
    }

    #[test]
    fn setting_wins_and_detection_only_lowers() {
        assert_eq!(effective(128_000, Some(256_000), Some(16_384)), 256_000);
        assert_eq!(effective(128_000, Some(8_192), Some(16_384)), 8_192);
        assert_eq!(effective(128_000, None, Some(16_384)), 16_384);
        assert_eq!(effective(128_000, None, Some(262_144)), 128_000);
        assert_eq!(effective(128_000, None, None), 128_000);
    }
}
