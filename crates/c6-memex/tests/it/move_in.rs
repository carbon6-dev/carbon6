//! A memex that ran beside the head (c6m) moves in (docs/knowledge.md §8):
//! its folder becomes the head's knowledge without copying, its worker row
//! is retired, and you are told to remove c6m. A c6m still running keeps
//! its folder, and you are asked to stop it first.

use c6_head::HeadConfig;
use serde_json::{Value, json};

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
    crate::extract_here();
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

/// What the memex synced from Google itself, before Gmail, Calendar and
/// Contacts were apps, moves into the apps when the head starts (docs/
/// knowledge.md §6): installed in home, your client and accounts theirs,
/// the mail and events kept theirs, and each sync where it was.
#[tokio::test]
async fn google_moves_into_its_apps() {
    crate::extract_here();
    c6_llm::install_crypto_provider();
    let data = tempfile::tempdir().unwrap();
    let start = || {
        c6_head::start(HeadConfig {
            data_dir: data.path().to_owned(),
            bind: vec!["127.0.0.1".parse().unwrap()],
            ui_port: 0,
            worker_port: 0,
            auth: false,
            ..Default::default()
        })
    };
    // As it was: an account and a client on the head, mail and an event in
    // the knowledge store, and where each sync was.
    let h = start().await.unwrap();
    let st = &h.state;
    let (refresh, access) = (st.secrets.encrypt("rt").unwrap(), st.secrets.encrypt("at").unwrap());
    let secret: String = st.secrets.encrypt("csecret").unwrap().iter().map(|b| format!("{b:02x}")).collect();
    let m = c6_head::knowledge::get(st).unwrap();
    let mail = c6_api::MemexItemIn {
        kind: "email".into(),
        source: "gmail:acc1".into(),
        external_id: "m1".into(),
        title: "Engines".into(),
        text: "Shall we meet?".into(),
        ..Default::default()
    };
    let item = m.store.put(&mail, "gmail:acc1").await.unwrap();
    sqlx::query("INSERT INTO mail (item_id, account, gmail_id, thread_id, date, blob) VALUES (?, 'acc1', 'm1', 't1', 1, 'x')")
        .bind(&item)
        .execute(&m.store.pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO calendars (account, id, summary) VALUES ('acc1', 'me@example.com', 'Me')")
        .execute(&m.store.pool)
        .await
        .unwrap();
    for (kind, cursor) in [("mail", "h42"), ("cal:me@example.com", "s7"), ("contacts", "p3")] {
        sqlx::query("INSERT INTO sync_state (account, kind, cursor, phase, done, updated_at) VALUES ('acc1', ?, ?, 'live', 10, 1)")
            .bind(kind)
            .bind(cursor)
            .execute(&m.store.pool)
            .await
            .unwrap();
    }
    h.shutdown().await.unwrap();
    let opts = sqlx::sqlite::SqliteConnectOptions::new().filename(data.path().join("c6h.db"));
    let pool = sqlx::sqlite::SqlitePoolOptions::new().max_connections(1).connect_with(opts).await.unwrap();
    sqlx::query("DELETE FROM settings WHERE key = 'google_moved'").execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO google_accounts (id, email, name, refresh_token, access_token, access_expires, scopes, added_at) VALUES ('acc1', 'me@example.com', 'Me', ?, ?, 1, 'all', 1)")
        .bind(refresh)
        .bind(access)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO settings (key, value) VALUES ('google_client', ?)")
        .bind(json!({"client_id": "cid", "secret": secret}).to_string())
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;

    // Started again: moved.
    let h = start().await.unwrap();
    let st = &h.state;
    let apps: Vec<String> = c6_head::google::installed(st).await.unwrap().into_iter().map(|(p, _)| p).collect();
    assert_eq!(apps, ["gmail", "google-calendar", "google-contacts"]);
    let client = c6_head::google::get_client(st).await.unwrap();
    assert!(client.client_id == "cid" && client.has_secret);
    let accounts = c6_head::google::accounts(st).await.unwrap();
    assert_eq!((accounts.len(), accounts[0].id.as_str(), accounts[0].email.as_str()), (1, "acc1", "me@example.com"));
    let m = c6_head::knowledge::get(st).unwrap();
    let (source, ext): (String, String) =
        sqlx::query_as("SELECT source, external_id FROM items WHERE id = ?").bind(&item).fetch_one(&m.store.pool).await.unwrap();
    assert_eq!((source.as_str(), ext.as_str()), ("app:gmail", "acc1:m1"), "the same item, the app's");
    let cal: String = sqlx::query_scalar("SELECT account FROM calendars").fetch_one(&m.store.pool).await.unwrap();
    assert_eq!(cal, "acc1c");
    let status = c6_head::google::status(st).await.unwrap();
    let cursor = |kind: &str| status["sync"].as_array().unwrap().iter().find(|s| s["kind"] == kind).map(|s| s["phase"].clone());
    assert_eq!(cursor("mail"), Some(json!("live")), "{status}");
    assert_eq!(cursor("cal:me@example.com"), Some(json!("live")), "{status}");
    let rec = c6_head::collections::query(st, "home", "gmail_sync", &c6_api::RecordQuery::default()).await.unwrap();
    assert_eq!(rec.records[0].fields["cursor"], "h42", "nothing fetched again");
    let api = format!("http://127.0.0.1:{}/api/v1", h.ui_addrs[0].port());
    let ds: Value = reqwest::get(format!("{api}/decisions")).await.unwrap().json().await.unwrap();
    assert!(ds.to_string().contains("Gmail, Calendar and Contacts are apps now"), "{ds}");
    h.shutdown().await.unwrap();
}
