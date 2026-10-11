//! Turning text into vectors through one of carbon6's connections (an
//! OpenAI-style `/embeddings` endpoint: OpenAI, Ollama, most local
//! servers). Credentials come from the head, as for a worker.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use c6_worker::link::HeadLink;
use serde_json::{Value, json};

#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    pub connection_id: String,
    pub model: String,
}

pub struct Embedder {
    link: Arc<HeadLink>,
    http: reqwest::Client,
    config: Mutex<Option<Config>>,
}

impl Embedder {
    pub fn new(link: Arc<HeadLink>) -> Self {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(120))
            .build()
            .expect("HTTP client");
        Self { link, http, config: Mutex::new(None) }
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
        let grant = self.link.credential(&c.connection_id).await?;
        let url = format!("{}/embeddings", grant.base_url.trim_end_matches('/'));
        let mut req = self.http.post(&url).json(&json!({"model": c.model, "input": texts}));
        if !grant.bearer.is_empty() {
            req = req.bearer_auth(&grant.bearer);
        }
        for (k, v) in &grant.headers {
            req = req.header(k, v);
        }
        let resp = req.send().await?;
        let status = resp.status();
        let body: Value = resp.json().await.unwrap_or(Value::Null);
        anyhow::ensure!(
            status.is_success(),
            "embeddings from {url}: {status}: {}",
            body["error"]["message"].as_str().or(body["error"].as_str()).unwrap_or("")
        );
        let mut data: Vec<&Value> = body["data"].as_array().map(|a| a.iter().collect()).unwrap_or_default();
        data.sort_by_key(|d| d["index"].as_u64().unwrap_or(0));
        anyhow::ensure!(data.len() == texts.len(), "embeddings: asked for {}, got {}", texts.len(), data.len());
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
