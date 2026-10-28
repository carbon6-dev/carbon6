//! Turning text into vectors through one of carbon6's connections (an
//! OpenAI-style `/embeddings` endpoint: OpenAI, Ollama, most local
//! servers), or a pool of them. Credentials come from the head, as for a
//! worker.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use c6_llm::LlmError;
use c6_llm::pool::Endpoint;
use c6_worker::link::HeadLink;
use serde_json::{Value, json};

#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    pub connection_id: String,
    pub model: String,
}

pub struct Embedder {
    link: Arc<HeadLink>,
    /// The realm it embeds for (a pool's members may be for some only).
    realm: String,
    http: reqwest::Client,
    config: Mutex<Option<Config>>,
}

impl Embedder {
    pub fn new(link: Arc<HeadLink>, realm: &str) -> Self {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(120))
            .build()
            .expect("HTTP client");
        Self { link, realm: realm.to_owned(), http, config: Mutex::new(None) }
    }

    pub fn set(&self, c: Option<Config>) {
        *self.config.lock().unwrap() = c;
    }

    pub fn config(&self) -> Option<Config> {
        self.config.lock().unwrap().clone()
    }

    /// The space vectors are kept in: the model's name ("" without one).
    pub fn space(&self) -> String {
        self.config().map(|c| c.model).unwrap_or_default()
    }

    /// Unit-length vectors for `texts`, in order.
    pub async fn embed(&self, texts: &[String]) -> anyhow::Result<Vec<Vec<f32>>> {
        let c = self.config().ok_or_else(|| anyhow::anyhow!("no embedding model is set (Settings → Memex)"))?;
        if let Some(pool) = self.link.pool(&c.connection_id).await {
            return Ok(pool.run(&c.model, &self.realm, |e| self.post(e, &c.model, texts)).await?);
        }
        let g = self.link.credential(&c.connection_id).await?;
        Ok(self.post(Endpoint { kind: g.kind, base_url: g.base_url, bearer: g.bearer, headers: g.headers }, &c.model, texts).await?)
    }

    async fn post(&self, e: Endpoint, model: &str, texts: &[String]) -> Result<Vec<Vec<f32>>, LlmError> {
        let url = format!("{}/embeddings", e.base_url.trim_end_matches('/'));
        let mut req = self.http.post(&url).json(&json!({"model": model, "input": texts}));
        if !e.bearer.is_empty() {
            req = req.bearer_auth(&e.bearer);
        }
        for (k, v) in &e.headers {
            req = req.header(k, v);
        }
        let resp = req.send().await.map_err(|e| LlmError::Network(format!("embeddings from {url}: {e}")))?;
        let status = resp.status();
        let body: Value = resp.json().await.unwrap_or(Value::Null);
        if !status.is_success() {
            let why = body["error"]["message"].as_str().or(body["error"].as_str()).unwrap_or("");
            return Err(LlmError::Http { status: status.as_u16(), body: format!("embeddings from {url}: {why}"), retry_after: None });
        }
        let mut data: Vec<&Value> = body["data"].as_array().map(|a| a.iter().collect()).unwrap_or_default();
        data.sort_by_key(|d| d["index"].as_u64().unwrap_or(0));
        if data.len() != texts.len() {
            return Err(LlmError::Protocol(format!("embeddings: asked for {}, got {}", texts.len(), data.len())));
        }
        Ok(data
            .iter()
            .map(|d| {
                let v: Vec<f32> =
                    d["embedding"].as_array().map(|a| a.iter().map(|x| x.as_f64().unwrap_or(0.0) as f32).collect()).unwrap_or_default();
                unit(v)
            })
            .collect())
    }
}

fn unit(mut v: Vec<f32>) -> Vec<f32> {
    let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if n > 0.0 {
        v.iter_mut().for_each(|x| *x /= n);
    }
    v
}

/// Cosine similarity of unit vectors.
pub fn similarity(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}
