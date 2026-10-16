//! A memex that ran beside the head (c6m) moves in (docs/knowledge.md §8):
//! its folder becomes the head's knowledge without copying, its worker row
//! is retired, and you are told to remove c6m. A c6m still running keeps
//! its folder, and you are asked to stop it first.

use c6_head::HeadConfig;
use serde_json::Value;

async fn c6m_folder(state: &std::path::Path) {
    let m = c6_memex::Memex::open(&state.join("memex"), std::sync::Arc::new(c6_worker::link::HeadLink::default())).await.unwrap();
    let item = c6_api::MemexItemIn {
        kind: "note".into(),
        source: "test".into(),
        external_id: "n1".into(),
        title: "The analytical engine".into(),
        text: "Notes on the analytical engine.".into(),
        ..Default::default()
    };
    m.store.put(&item, "you").await.unwrap();
    m.store.pool.close().await;
}

async fn head(data: &std::path::Path, c6m: &std::path::Path) -> c6_head::Head {
    c6_head::start(HeadConfig {
        data_dir: data.to_owned(),
        bind: vec!["127.0.0.1".parse().unwrap()],
        ui_port: 0,
        worker_port: 0,
        auth: false,
        memex_dirs: Some(vec![c6m.to_owned()]),
        ..Default::default()
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn a_memex_moves_into_the_head() {
    c6_llm::install_crypto_provider();
    let (data, c6m) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    c6m_folder(c6m.path()).await;

    // c6m still running (it holds its lock): nothing moves; you're asked.
    let lock = std::fs::File::create(c6m.path().join("c6w.lock")).unwrap();
    lock.try_lock().unwrap();
    let h = head(data.path(), c6m.path()).await;
    let api = format!("http://127.0.0.1:{}/api/v1", h.ui_addrs[0].port());
    let ds: Value = reqwest::get(format!("{api}/decisions")).await.unwrap().json().await.unwrap();
    assert!(ds.to_string().contains("Stop c6m so the head can take its knowledge in"), "{ds}");
    assert!(c6m.path().join("memex/memex.db").exists());
    h.shutdown().await.unwrap();
    drop(lock);

    // Stopped: it moves in, nothing copied.
    let h = head(data.path(), c6m.path()).await;
    let api = format!("http://127.0.0.1:{}/api/v1", h.ui_addrs[0].port());
    assert!(!c6m.path().join("memex").exists(), "moved, not copied");
    let found: Value = reqwest::get(format!("{api}/memex/search?q=analytical+engine")).await.unwrap().json().await.unwrap();
    assert!(found.to_string().contains("analytical engine"), "{found}");
    let ds: Value = reqwest::get(format!("{api}/decisions")).await.unwrap().json().await.unwrap();
    assert!(ds.to_string().contains("Remove c6m: the head has its knowledge now"), "{ds}");
    // The empty store an earlier start made is kept aside, not lost.
    assert!(std::fs::read_dir(data.path()).unwrap().flatten().any(|e| e.file_name().to_string_lossy().starts_with("knowledge.before-")));
    h.shutdown().await.unwrap();

    // Once the head has knowledge, another start leaves everything be.
    c6m_folder(c6m.path()).await;
    let h = head(data.path(), c6m.path()).await;
    assert!(c6m.path().join("memex/memex.db").exists(), "the head's own knowledge stays");
    h.shutdown().await.unwrap();
}
