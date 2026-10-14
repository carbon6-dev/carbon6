//! Makes the memex's upgrade test database: a memex database in this
//! version's shape, filled through the API the way a real memex is
//! (docs/os.md §13). Every later upgrade is tested against it.
//!
//! Run when the memex's schema is about to change, to keep the current
//! shape: `cargo test -p c6-memex --test it fixtures -- --ignored`.
//! It writes `crates/c6-memex/fixtures/memex-v<NNNN>.db` (synthetic data only).

use std::time::Duration;

use base64::Engine;
use c6_api::Connection;
use c6_head::HeadConfig;
use c6_testkit::{MockLlm, Reply};
use c6_worker::{ConnState, Identity, Worker, conn, enroll};
use serde_json::{Value, json};
use sqlx::Row;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

async fn eventually<T>(what: &str, mut f: impl AsyncFnMut() -> Option<T>) -> T {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(v) = f().await {
            return v;
        }
        assert!(tokio::time::Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn mail(id: &str, reply_to: Option<&str>, subject: &str, body: &str) -> String {
    let refs = reply_to.map(|r| format!("In-Reply-To: <{r}>\r\nReferences: <{r}>\r\n")).unwrap_or_default();
    format!(
        "From: Ada Lovelace <ada@example.com>\r\nTo: You <you@example.com>\r\nSubject: {subject}\r\nDate: Mon, 12 Oct 2026 09:00:00 +0000\r\nMessage-ID: <{id}>\r\n{refs}\r\n{body}\r\n"
    )
}

#[tokio::test]
#[ignore = "writes the memex upgrade test database; run it when the memex's schema is about to change"]
async fn make_memex_fixture() {
    let mock = MockLlm::start().await;
    let head_dir = tempfile::tempdir().unwrap();
    let head = c6_head::start(HeadConfig {
        data_dir: head_dir.path().to_owned(),
        bind: vec!["127.0.0.1".parse().unwrap()],
        ui_port: 0,
        worker_port: 0,
        auth: false,
        ..Default::default()
    })
    .await
    .unwrap();
    let api = format!("http://127.0.0.1:{}/api/v1", head.ui_addrs[0].port());
    let http = reqwest::Client::new();
    let stop = CancellationToken::new();
    let _conn: Connection = http
        .post(format!("{api}/connections"))
        .json(&json!({"name": "m", "base_url": mock.base_url()}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    let dir = tempfile::tempdir().unwrap();
    let e = c6_head::api::new_enrollment(&head.state, Some("memex")).await.unwrap();
    enroll::enroll(&e.token).await.unwrap().save(dir.path()).unwrap();
    let w = Worker::open(dir.path(), Identity::load(dir.path()).unwrap()).await.unwrap();
    let m = c6_memex::Memex::open(&dir.path().join("memex"), w.link.clone()).await.unwrap();
    m.spawn_jobs();
    w.set_extension(m);
    let (tx, mut rx) = watch::channel(ConnState::Connecting);
    tokio::spawn(conn::run(w.clone(), tx, stop.clone()));
    tokio::time::timeout(Duration::from_secs(10), rx.wait_for(|s| matches!(s, ConnState::Attached { .. }))).await.unwrap().unwrap();

    let send = |path: &str, body: Value| {
        let (http, url) = (http.clone(), format!("{api}/{path}"));
        async move {
            let r = http.post(&url).json(&body).send().await.unwrap();
            let status = r.status();
            let text = r.text().await.unwrap();
            assert!(status.is_success(), "{url}: {status} {text}");
            serde_json::from_str::<Value>(&text).unwrap_or(Value::Null)
        }
    };
    let rpc = |method: &str, params: Value| send("memex/rpc", json!({"method": method, "params": params}));
    let b64 = |s: &str| base64::engine::general_purpose::STANDARD.encode(s);

    // Mail (a thread of two), a document, through ingest.
    let r = rpc(
        "memex.ingest",
        json!({"items": [
            {"type": "message/rfc822", "data": b64(&mail("m1@example.com", None, "Engines", "Shall we meet about the engine?")), "source": {"kind": "mbox", "id": "archive"}, "external_id": "archive#1", "attributes": {"folder": "Work"}},
            {"type": "message/rfc822", "data": b64(&mail("m2@example.com", Some("m1@example.com"), "Re: Engines", "Thursday works.")), "source": {"kind": "mbox", "id": "archive"}, "external_id": "archive#2", "attributes": {"folder": "Work", "read": true}},
            {"type": "text/markdown", "text": "# Notes on the analytical engine\n\nIt weaves algebraic patterns.", "source": {"kind": "folder", "id": "papers"}, "external_id": "engine.md"}
        ]}),
    )
    .await;
    assert!(r.to_string().contains("created"), "{r}");
    // A document by upload, a note, a trail.
    send("memex/upload", json!({"name": "plan.md", "data": b64("# Plan\n\nShip the release on Friday."), "labels": ["client:acme"]})).await;
    send("memex/notes", json!({"text": "Ada prefers Thursday meetings.", "about": ["person:ada"]})).await;
    let search = |q: &str| {
        let (http, url) = (http.clone(), format!("{api}/memex/search?q={q}"));
        async move { http.get(url).send().await.unwrap().json::<Value>().await.unwrap() }
    };
    let found = search("engine").await;
    let item = found.to_string();
    let id = item.split("\"id\":\"").nth(1).and_then(|s| s.split('"').next()).unwrap_or_default().to_owned();
    send("memex/trails", json!({"trail": "engines", "target": id, "comment": "Where it started."})).await;
    // Facts, one contradicted (a dispute).
    let f = rpc(
        "memex.knowledge.record",
        json!({"claim": "CI runs as Argo Workflows in namespace ci", "about": ["ci:argo"], "status": "observed", "volatility": "stable",
               "evidence": {"command": "kubectl get wf -n ci", "excerpt": "build-123 Succeeded"}, "check": {"command": "kubectl get ns ci"}}),
    )
    .await;
    rpc(
        "memex.knowledge.record",
        json!({"claim": "CI runs on GitHub Actions", "about": ["ci:argo"], "status": "observed",
               "evidence": {"source": ".github/workflows/ci.yml"}, "contradicts": f["id"]}),
    )
    .await;
    rpc(
        "memex.knowledge.record",
        json!({"claim": "Staging is reached over WireGuard", "about": ["net:wireguard"], "status": "inferred", "volatility": "slow"}),
    )
    .await;
    // A memory note and a session digest come from the head.
    send("memories", json!({"scope": "client:acme", "text": "Acme deploys on Fridays."})).await;
    mock.push(Reply::Text("Done.".into()));

    eventually("the memory note in the memex", async || {
        let v = search("Fridays").await;
        v.to_string().contains("Acme deploys").then_some(())
    })
    .await;

    stop.cancel();
    let path = dir.path().join("memex").join("memex.db");
    let opts = sqlx::sqlite::SqliteConnectOptions::new().filename(&path);
    let pool = sqlx::sqlite::SqlitePoolOptions::new().max_connections(1).connect_with(opts).await.unwrap();
    let empty = fill_empty_tables(&pool).await;
    let version: i64 = sqlx::query_scalar("SELECT max(version) FROM _sqlx_migrations").fetch_one(&pool).await.unwrap();
    let out = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("fixtures/memex-v{version:04}.db"));
    std::fs::create_dir_all(out.parent().unwrap()).unwrap();
    let _ = std::fs::remove_file(&out);
    sqlx::query("VACUUM INTO ?").bind(out.to_string_lossy().as_ref()).execute(&pool).await.unwrap();
    eprintln!("wrote {}; tables left empty: {empty:?}", out.display());
}

/// Puts one synthetic row into each empty table (foreign keys off), so the
/// fixture has something in every table. Returns the tables it could not
/// fill.
async fn fill_empty_tables(pool: &sqlx::SqlitePool) -> Vec<String> {
    let mut conn = pool.acquire().await.unwrap();
    sqlx::query("PRAGMA foreign_keys = OFF").execute(&mut *conn).await.unwrap();
    let tables = sqlx::query("SELECT name, sql FROM sqlite_master WHERE type = 'table' ORDER BY name").fetch_all(&mut *conn).await.unwrap();
    let virtuals: Vec<String> = tables
        .iter()
        .filter(|r| r.get::<Option<String>, _>("sql").is_some_and(|s| s.to_uppercase().starts_with("CREATE VIRTUAL")))
        .map(|r| r.get("name"))
        .collect();
    let mut left = Vec::new();
    for t in &tables {
        let name: String = t.get("name");
        if name.starts_with("sqlite_")
            || name.starts_with('_')
            || name == "upgrades"
            || virtuals.contains(&name)
            || virtuals.iter().any(|v| name.starts_with(&format!("{v}_")))
        {
            continue;
        }
        let n: i64 =
            sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT count(*) FROM \"{name}\""))).fetch_one(&mut *conn).await.unwrap();
        if n > 0 {
            continue;
        }
        let cols = sqlx::query(sqlx::AssertSqlSafe(format!("PRAGMA table_info(\"{name}\")"))).fetch_all(&mut *conn).await.unwrap();
        let mut names = Vec::new();
        let mut values = Vec::new();
        for c in &cols {
            let (col, ty, notnull, default, pk): (String, String, i64, Option<String>, i64) =
                (c.get("name"), c.get("type"), c.get("notnull"), c.get("dflt_value"), c.get("pk"));
            if default.is_some() || (notnull == 0 && pk == 0) {
                continue;
            }
            let ty = ty.to_uppercase();
            names.push(format!("\"{col}\""));
            values.push(if ty.contains("INT") {
                "1".to_owned()
            } else if ty.contains("REAL") {
                "1.0".to_owned()
            } else if ty.contains("BLOB") {
                "x'00'".to_owned()
            } else {
                format!("'fixture-{col}'")
            });
        }
        let sql = if names.is_empty() {
            format!("INSERT INTO \"{name}\" DEFAULT VALUES")
        } else {
            format!("INSERT INTO \"{name}\" ({}) VALUES ({})", names.join(", "), values.join(", "))
        };
        if let Err(e) = sqlx::query(sqlx::AssertSqlSafe(sql)).execute(&mut *conn).await {
            left.push(format!("{name}: {e}"));
        }
    }
    sqlx::query("PRAGMA foreign_keys = ON").execute(&mut *conn).await.unwrap();
    left
}

/// Every saved memex database (each earlier version's shape) upgrades to
/// this version and is served: its mail, documents, notes and facts come
/// back.
#[tokio::test]
async fn memex_fixtures_upgrade_and_serve() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures");
    let mut fixtures: Vec<_> =
        std::fs::read_dir(&dir).unwrap().flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "db")).collect();
    fixtures.sort();
    assert!(!fixtures.is_empty(), "no fixtures in {}", dir.display());
    for fixture in fixtures {
        let name = fixture.file_name().unwrap().to_string_lossy().into_owned();
        let checked = c6_memex::store::check_upgrade(&fixture).await.unwrap_or_else(|e| panic!("{name}: {e:#}"));
        assert!(checked.checks.iter().all(|c| c.ok), "{name}: {}", checked.text());

        let state = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(state.path().join("memex")).unwrap();
        std::fs::copy(&fixture, state.path().join("memex/memex.db")).unwrap();
        let (store, _) =
            c6_memex::store::Store::open_reporting(&state.path().join("memex/memex.db")).await.unwrap_or_else(|e| panic!("{name}: {e:#}"));
        let count = |sql: &'static str| {
            let pool = store.pool.clone();
            async move { sqlx::query_scalar::<_, i64>(sql).fetch_one(&pool).await.unwrap() }
        };
        assert!(count("SELECT count(*) FROM mail").await >= 2, "{name}: mail");
        assert!(count("SELECT count(*) FROM facts").await >= 3, "{name}: facts");
        assert!(count("SELECT count(*) FROM items WHERE kind = 'note'").await >= 1, "{name}: notes");
        assert!(count("SELECT count(*) FROM trail_steps").await >= 1, "{name}: trails");
        assert!(!store.upgrades().await.unwrap().is_empty(), "{name}: upgrade history");
        drop(store);

        // Served: home's knowledge answers; another realm's call is refused
        // (only home keeps knowledge in the memex so far).
        c6_llm::install_crypto_provider();
        let m =
            c6_memex::Memex::open(&state.path().join("memex"), std::sync::Arc::new(c6_worker::link::HeadLink::default())).await.unwrap();
        let found = m.handle_method("memex.search", json!({"query": "engine", "realm": "home"})).await.unwrap();
        assert!(found.to_string().contains("analytical engine"), "{name}: {found}");
        let found = m.handle_method("memex.search", json!({"query": "engine"})).await.unwrap();
        assert!(found.to_string().contains("analytical engine"), "{name}: no realm means home");
        let refused = m.handle_method("memex.search", json!({"query": "engine", "realm": "lab"})).await.unwrap_err();
        assert!(refused.to_string().contains("realm lab keeps no knowledge"), "{name}: {refused}");
    }
}
