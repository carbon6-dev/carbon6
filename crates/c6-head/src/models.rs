//! Provider model lists: one in-flight fetch per connection, ten minutes
//! fresh, then the last successful list while one refresh runs. Never used
//! for testing a connection's credentials.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::http::StatusCode;
use futures::FutureExt;
use futures::future::{BoxFuture, Shared};

use crate::api::ApiResult;

const FRESH: Duration = Duration::from_secs(10 * 60);
const RETRY: Duration = Duration::from_secs(30);
const LIMIT: usize = 128;
type Listed = Result<Vec<String>, (StatusCode, String)>;
type Fetch = Shared<BoxFuture<'static, Listed>>;

#[derive(Clone, Default)]
pub(crate) struct Cache(Arc<Mutex<State>>);

#[derive(Default)]
struct State {
    generation: u64,
    next: u64,
    entries: HashMap<String, Entry>,
}

struct Entry {
    token: u64,
    touched: Instant,
    value: Option<(Instant, Vec<String>)>,
    retry_at: Option<Instant>,
    pending: Option<Fetch>,
}

impl Cache {
    /// Capture before reading the connection row, not after: an edit can
    /// happen between that read and reserving its provider request.
    pub fn generation(&self) -> u64 {
        self.0.lock().unwrap().generation
    }

    /// Clear pools as well as members. A completed pre-edit request may
    /// answer its original caller, but must never refill this cache.
    pub fn clear(&self) {
        let mut state = self.0.lock().unwrap();
        state.generation = state.generation.wrapping_add(1);
        state.entries.clear();
    }

    pub fn get<F, Fut>(&self, id: String, generation: u64, fetch: F) -> Fetch
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = ApiResult<Vec<String>>> + Send + 'static,
    {
        self.get_at(id, generation, Instant::now(), fetch)
    }

    fn get_at<F, Fut>(&self, id: String, generation: u64, now: Instant, fetch: F) -> Fetch
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = ApiResult<Vec<String>>> + Send + 'static,
    {
        let mut state = self.0.lock().unwrap();
        if generation != state.generation {
            return async move { fetch().await.map_err(|e| (e.0, e.1)) }.boxed().shared();
        }
        let mut cached = None;
        if let Some(entry) = state.entries.get_mut(&id) {
            entry.touched = now;
            cached = entry.value.as_ref().map(|(_, models)| models.clone());
            if let Some((at, models)) = &entry.value
                && (now.saturating_duration_since(*at) < FRESH || entry.retry_at.is_some_and(|at| now < at))
            {
                return futures::future::ready(Ok(models.clone())).boxed().shared();
            }
            if let Some(pending) = &entry.pending {
                return match cached {
                    Some(models) => futures::future::ready(Ok(models)).boxed().shared(),
                    None => pending.clone(),
                };
            }
        } else if state.entries.len() >= LIMIT
            && let Some(oldest) = state.entries.iter().min_by_key(|(_, e)| e.touched).map(|(id, _)| id.clone())
        {
            state.entries.remove(&oldest);
        }
        state.next = state.next.wrapping_add(1);
        let token = state.next;
        let weak = Arc::downgrade(&self.0);
        let key = id.clone();
        let pending = async move {
            let result = fetch().await.map_err(|e| (e.0, e.1));
            if let Some(cache) = weak.upgrade() {
                let mut state = cache.lock().unwrap();
                if generation == state.generation
                    && let Some(entry) = state.entries.get_mut(&key)
                    && entry.token == token
                {
                    entry.pending = None;
                    match &result {
                        Ok(models) => {
                            entry.value = Some((Instant::now(), models.clone()));
                            entry.retry_at = None;
                        }
                        Err(_) if entry.value.is_some() => entry.retry_at = Some(Instant::now() + RETRY),
                        Err(_) => {
                            state.entries.remove(&key);
                        }
                    }
                }
            }
            result
        }
        .boxed()
        .shared();
        let entry = state.entries.entry(id).or_insert(Entry { token, touched: now, value: None, retry_at: None, pending: None });
        entry.token = token;
        entry.pending = Some(pending.clone());
        // Refresh survives a cancelled HTTP request; consumers only share
        // the future, never hold the map lock during network work.
        tokio::spawn(pending.clone());
        match cached {
            Some(models) => futures::future::ready(Ok(models)).boxed().shared(),
            None => pending,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::ApiErr;
    use c6_testkit::{eventually, within};
    use tokio::sync::oneshot;

    fn models(name: &str) -> Vec<String> {
        vec![name.into()]
    }

    #[tokio::test]
    async fn cold_reads_share_a_fetch_and_fresh_reads_do_not_fetch() {
        let cache = Cache::default();
        let generation = cache.generation();
        let (release, gate) = oneshot::channel();
        let first = cache.get("c".into(), generation, || async {
            gate.await.unwrap();
            Ok(models("one"))
        });
        let second = cache.get("c".into(), generation, || async { panic!("duplicate provider request") });
        release.send(()).unwrap();
        assert_eq!(within("first model list", first).await.unwrap(), models("one"));
        assert_eq!(within("shared model list", second).await.unwrap(), models("one"));
        assert_eq!(cache.get("c".into(), generation, || async { panic!("fresh list fetched again") }).await.unwrap(), models("one"));
    }

    #[tokio::test]
    async fn stale_list_is_immediate_while_one_refresh_replaces_it() {
        let cache = Cache::default();
        let generation = cache.generation();
        cache.get("c".into(), generation, || async { Ok(models("old")) }).await.unwrap();
        let later = Instant::now() + FRESH;
        let (release, gate) = oneshot::channel();
        let stale = cache.get_at("c".into(), generation, later, || async {
            gate.await.unwrap();
            Ok(models("new"))
        });
        assert_eq!(within("stale response without waiting for provider", stale).await.unwrap(), models("old"));
        assert_eq!(cache.get_at("c".into(), generation, later, || async { panic!("duplicate refresh") }).await.unwrap(), models("old"));
        release.send(()).unwrap();
        eventually("model refresh committed", async || cache.0.lock().unwrap().entries["c"].pending.is_none().then_some(())).await;
        assert_eq!(cache.get("c".into(), generation, || async { panic!("refreshed list fetched again") }).await.unwrap(), models("new"));
    }

    #[tokio::test]
    async fn edits_and_deletes_cannot_be_undone_by_an_old_in_flight_fetch() {
        let cache = Cache::default();
        let before = cache.generation();
        let (release, gate) = oneshot::channel();
        let old = cache.get("c".into(), before, || async {
            gate.await.unwrap();
            Ok(models("old credentials"))
        });
        cache.clear();
        let after = cache.generation();
        cache.get("c".into(), after, || async { Ok(models("new credentials")) }).await.unwrap();
        release.send(()).unwrap();
        within("pre-edit request finishes", old).await.unwrap();
        assert_eq!(cache.get("c".into(), after, || async { panic!("new list lost") }).await.unwrap(), models("new credentials"));

        // A row read before an edit can reach the cache after clear().
        cache.clear();
        cache.get("c".into(), after, || async { Ok(models("deleted row")) }).await.unwrap();
        assert!(cache.0.lock().unwrap().entries.is_empty(), "an old row must not repopulate a deleted connection");
    }

    #[tokio::test]
    async fn failures_retry_without_poisoning_or_losing_the_last_success() {
        let cache = Cache::default();
        let generation = cache.generation();
        let error = || async { Err(ApiErr(StatusCode::BAD_GATEWAY, "offline".into())) };
        assert!(cache.get("c".into(), generation, error).await.is_err());
        assert!(cache.0.lock().unwrap().entries.is_empty());
        cache.get("c".into(), generation, || async { Ok(models("last good")) }).await.unwrap();
        assert_eq!(cache.get_at("c".into(), generation, Instant::now() + FRESH, error).await.unwrap(), models("last good"));
        eventually("failed refresh completed", async || cache.0.lock().unwrap().entries["c"].pending.is_none().then_some(())).await;
        assert_eq!(cache.0.lock().unwrap().entries["c"].value.as_ref().unwrap().1, models("last good"));
    }

    #[tokio::test]
    async fn bounded_eviction_cannot_be_undone_by_a_late_fetch() {
        let cache = Cache::default();
        let generation = cache.generation();
        let (release, gate) = oneshot::channel();
        let old = cache.get_at("old".into(), generation, Instant::now(), || async {
            gate.await.unwrap();
            Ok(models("evicted"))
        });
        let later = Instant::now() + Duration::from_secs(1);
        for i in 0..LIMIT {
            cache.get_at(i.to_string(), generation, later, || async { Ok(models("cached")) }).await.unwrap();
        }
        assert_eq!(cache.0.lock().unwrap().entries.len(), LIMIT);
        assert!(!cache.0.lock().unwrap().entries.contains_key("old"));
        release.send(()).unwrap();
        within("evicted provider request finishes", old).await.unwrap();
        assert_eq!(cache.0.lock().unwrap().entries.len(), LIMIT);
        assert!(!cache.0.lock().unwrap().entries.contains_key("old"));
    }

    #[tokio::test]
    async fn http_lists_share_cache_edits_clear_it_and_connection_checks_bypass_it() {
        use axum::{Json, Router, routing::get};
        use serde_json::{Value, json};
        use std::sync::atomic::{AtomicUsize, Ordering};

        let calls = Arc::new(AtomicUsize::new(0));
        let seen = calls.clone();
        let provider = Router::new().route(
            "/v1/models",
            get(move || {
                let n = seen.fetch_add(1, Ordering::SeqCst) + 1;
                async move { Json(json!({"data": [{"id": format!("model-{n}")}]})) }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let provider_url = format!("http://{}/v1", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, provider).await.unwrap() });
        let dir = tempfile::tempdir().unwrap();
        let head = crate::start(crate::HeadConfig {
            data_dir: dir.path().to_owned(),
            bind: vec!["127.0.0.1".parse().unwrap()],
            ui_port: 0,
            worker_port: 0,
            auth: false,
            ..Default::default()
        })
        .await
        .unwrap();
        let base = format!("http://{}", head.ui_addrs[0]);
        let http = reqwest::Client::new();
        let c: Value = http
            .post(format!("{base}/api/v1/connections"))
            .json(&json!({"name": "Models", "kind": "openai_compat", "base_url": provider_url, "api_key": "test"}))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        let id = c["id"].as_str().unwrap();
        let endpoint = format!("{base}/api/v1/connections/{id}/models");
        for _ in 0..2 {
            let list: Vec<String> = http.get(&endpoint).send().await.unwrap().error_for_status().unwrap().json().await.unwrap();
            assert_eq!(list, models("model-1"));
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        http.patch(format!("{base}/api/v1/connections/{id}"))
            .json(&json!({"name": "Renamed"}))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
        let list: Vec<String> = http.get(&endpoint).send().await.unwrap().error_for_status().unwrap().json().await.unwrap();
        assert_eq!(list, models("model-2"));
        let checked: Vec<String> =
            http.get(format!("{endpoint}?fresh=true")).send().await.unwrap().error_for_status().unwrap().json().await.unwrap();
        assert_eq!(checked, models("model-3"), "the saved connection's Test button must bypass the cache");
        let checked: Vec<String> = http
            .post(format!("{base}/api/v1/connections/test"))
            .json(&json!({"connection_id": id, "base_url": provider_url}))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(checked, models("model-4"), "Test must check the provider, not a cached success");
        let list: Vec<String> = http.get(&endpoint).send().await.unwrap().error_for_status().unwrap().json().await.unwrap();
        assert_eq!(list, models("model-2"), "connection checks must not populate the read cache either");
        let pool: Value = http
            .post(format!("{base}/api/v1/connections"))
            .json(&json!({"name": "Pool", "kind": "pool", "members": [{"connection_id": id, "slots": 1}]}))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        let pool_id = pool["id"].as_str().unwrap();
        let pooled = format!("{base}/api/v1/connections/{pool_id}/models");
        let list: Vec<String> = http.get(&pooled).send().await.unwrap().error_for_status().unwrap().json().await.unwrap();
        assert_eq!(list, models("model-5"));
        let list: Vec<String> = http.get(&endpoint).send().await.unwrap().error_for_status().unwrap().json().await.unwrap();
        assert_eq!(list, models("model-5"), "pools and direct lists share their member's provider request");
        let checked: Vec<String> =
            http.get(format!("{pooled}?fresh=true")).send().await.unwrap().error_for_status().unwrap().json().await.unwrap();
        assert_eq!(checked, models("model-6"), "pool Test must bypass its members' caches too");
        let list: Vec<String> = http.get(&endpoint).send().await.unwrap().error_for_status().unwrap().json().await.unwrap();
        assert_eq!(list, models("model-5"), "pool checks must not populate their members' caches");
        http.patch(format!("{base}/api/v1/connections/{id}"))
            .json(&json!({"name": "Member edited"}))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
        let list: Vec<String> = http.get(&pooled).send().await.unwrap().error_for_status().unwrap().json().await.unwrap();
        assert_eq!(list, models("model-7"), "editing a member clears the pool's derived list");
        http.delete(format!("{base}/api/v1/connections/{pool_id}")).send().await.unwrap().error_for_status().unwrap();
        http.delete(format!("{base}/api/v1/connections/{id}")).send().await.unwrap().error_for_status().unwrap();
        assert_eq!(http.get(&endpoint).send().await.unwrap().status(), StatusCode::NOT_FOUND);
        assert_eq!(calls.load(Ordering::SeqCst), 7);
        head.shutdown().await.unwrap();
        server.abort();
    }
}
