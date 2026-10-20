//! Makes the upgrade test database: a head database in this version's
//! shape, filled through the API the way a real setup is (docs/os.md §13).
//! Every later upgrade is tested against the databases this made.
//!
//! Run when the head's schema is about to change, to keep the current
//! shape: `cargo test -p c6-worker --test it fixtures -- --ignored`.
//! It writes `crates/c6-head/fixtures/c6h-v<NNNN>.db` (synthetic data only).

use std::time::Duration;

use c6_api::{Connection, Event, EventKind, Session};
use c6_head::HeadConfig;
use c6_testkit::eventually;
use c6_testkit::{MockLlm, Reply};
use c6_worker::{ConnState, Identity, Worker, conn, enroll};
use serde_json::{Value, json};
use sqlx::Row;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

#[tokio::test]
#[ignore = "writes the upgrade test database; run it when the head's schema is about to change"]
async fn make_head_fixture() {
    let mock = MockLlm::start().await;
    let (head_dir, state) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
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
    let e = c6_head::api::new_enrollment(&head.state, Some("fixture-worker")).await.unwrap();
    enroll::enroll(&e.token).await.unwrap().save(state.path()).unwrap();
    let worker = Worker::open(state.path(), Identity::load(state.path()).unwrap()).await.unwrap();
    let (tx, mut rx) = watch::channel(ConnState::Connecting);
    let stop = CancellationToken::new();
    tokio::spawn(conn::run(worker.clone(), tx, stop.clone()));
    c6_testkit::within("the worker to attach", rx.wait_for(|s| matches!(s, ConnState::Attached { .. }))).await.unwrap();
    let wid = worker.identity.meta.worker_id.0;

    let send = |method: reqwest::Method, path: &str, body: Value| {
        let (http, url) = (http.clone(), format!("{api}/{path}"));
        async move {
            let r = http.request(method, &url).json(&body).send().await.unwrap();
            let status = r.status();
            let text = r.text().await.unwrap();
            assert!(status.is_success(), "{url}: {status} {text}");
            serde_json::from_str::<Value>(&text).unwrap_or(Value::Null)
        }
    };
    let put = |path: &str, body: Value| send(reqwest::Method::PUT, path, body);
    let post = |path: &str, body: Value| send(reqwest::Method::POST, path, body);

    let conn: Connection = serde_json::from_value(post("connections", json!({"name": "m", "base_url": mock.base_url()})).await).unwrap();

    // Every setting, with a realistic entry.
    put("settings/skills", json!([{"name": "release-notes", "description": "How to write release notes", "body": "Group by user-facing change.", "applies_to": ["repo:github.com/acme/app"]}])).await;
    put(
        "settings/agents",
        json!([{"name": "finder", "description": "Finds things in the code", "tools": "Read, Grep", "prompt": "Search, never edit."}]),
    )
    .await;
    put("settings/commands", json!([{"name": "review", "description": "Review the diff", "body": "Review the current diff for bugs."}]))
        .await;
    put("settings/hooks", json!([{"name": "fmt", "event": "PostToolUse", "matcher": "Write|Edit", "command": "true", "enabled": true}]))
        .await;
    put("settings/mcp-servers", json!([{"name": "fs", "transport": "stdio", "command": "true", "args": [], "applies_to": ["uses-fs"]}]))
        .await;
    put("settings/templates", json!([
        {"name": "triage", "worker": wid.to_string(), "connection": conn.id, "model": "m", "prompt": "Triage this alert: {{input}}", "labels": ["ops"]},
        {"name": "check", "worker": wid.to_string(), "connection": conn.id, "model": "m", "prompt": "check things"}
    ]))
    .await;
    put(
        "settings/workflows",
        json!([{"name": "check-then-fix", "description": "Check, ask me, then fix", "steps": [
            {"id": "check", "kind": "run", "template": "check"},
            {"id": "ok", "kind": "approve", "title": "Fix it?", "message": "{{steps.check.output}}"},
            {"id": "fix", "kind": "run", "template": "triage", "input": "{{steps.check.output}}"}
        ]}]),
    )
    .await;
    put(
        "settings/policies",
        json!([
            {"name": "Ask before push", "tools": "Bash", "pattern": "git\\s+push", "action": "ask"},
            {"name": "Ask on ops", "applies_to": ["ops"], "tools": "Bash", "action": "ask"}
        ]),
    )
    .await;
    put("settings/routes", json!([{"name": "ops on m", "applies_to": ["ops"], "connection": conn.id, "model": "m"}])).await;
    put("settings/channels", json!([{"name": "hook", "kind": "webhook", "url": "http://127.0.0.1:9/hook", "enabled": false}])).await;
    put("settings/prices", json!([{"model": "m", "input": 3, "output": 15}])).await;
    put("settings/budgets", json!([{"name": "monthly", "period": "month", "limit": 50, "action": "notify"}])).await;
    put(
        "settings/hosts",
        json!([{"name": "web1", "target": "deploy@web1.internal", "worker": "fixture-worker", "labels": ["env:prod"], "check_every": 5.0}]),
    )
    .await;
    put("settings/memex_scopes", json!([{"name": "mail triage", "applies_to": ["mail-triage"], "kinds": ["email"]}])).await;
    put("settings/memex_folders", json!([{"path": "/tmp/fixture-papers", "labels": ["research"]}])).await;
    put("settings/public_url", json!({"url": "https://c6.example.net"})).await;
    put("settings/assistant", json!({"model": {"connection": conn.id, "model": "m"}})).await;
    put("settings/secrets/DEPLOY_TOKEN", json!({"value": "not-a-real-token", "description": "deploys", "applies_to": ["ops"]})).await;

    // Memory notes, action items, an automation, an API token.
    post("memories", json!({"scope": "host:web1", "text": "web1 runs nginx behind the load balancer."})).await;
    post("memories", json!({"scope": "", "text": "Prefer small commits."})).await;
    post("decisions", json!({"kind": "confirm", "title": "Is CI on Argo?", "detail": "Assumed yes.", "context": {"things": ["ci:argo"]}, "priority": "high"})).await;
    post("decisions", json!({"kind": "question", "title": "Which region for staging?", "group": "staging-region"})).await;
    post("automations", json!({"name": "nightly check", "template": "check", "trigger": "schedule", "cron": "0 3 * * *"})).await;
    post("automations", json!({"name": "alerts", "template": "triage", "trigger": "webhook"})).await;
    post("auth/tokens", json!({"name": "fixture script"})).await;

    // A collection with records, one changed (history).
    post("collections", json!({"name": "candidates", "description": "Gaps worth a look", "keys": ["name"], "schema": {"type": "object", "required": ["name", "stage"], "properties": {
        "name": {"type": "string"}, "stage": {"enum": ["found", "approved", "build"]}, "score": {"type": "number"}, "tags": {"type": "array", "items": {"type": "string"}}
    }}}))
    .await;
    let rec = post(
        "collections/candidates/records",
        json!({"fields": {"name": "csv sync", "stage": "found", "score": 7}, "links": ["ecosystem:shopify"]}),
    )
    .await;
    post("collections/candidates/records", json!({"fields": {"name": "pdf invoices", "stage": "approved", "tags": ["stripe"]}})).await;
    put(
        &format!("collections/candidates/records/{}", rec["id"].as_str().unwrap()),
        json!({"fields": {"name": "csv sync", "stage": "build", "score": 8}, "version": 1}),
    )
    .await;

    // Views of home's candidates.
    put(
        "settings/views",
        json!([{"name": "candidates", "title": "Candidates", "kind": "board", "group_by": "stage", "source": {"collection": "candidates"},
                "actions": [{"label": "Approve", "set": {"stage": "approved"}}]}]),
    )
    .await;

    // A realm of its own, granted a read of home's candidates.
    post("realms", json!({"id": "lab", "name": "Lab", "description": "experiments"})).await;
    post(
        "grants",
        json!({"from_realm": "home", "to_realm": "lab", "kind": "collection", "mode": "read",
               "scope": {"collection": "candidates", "where": {"stage": "build"}}, "note": "fixture"}),
    )
    .await;

    // An app, installed (the decision answered).
    let app = post(
        "apps",
        json!({"app": "notes", "version": "1.0.0", "description": "Quick notes", "contents": {
            "collections": [{"name": "notes", "schema": {"type": "object", "required": ["text"], "properties": {"text": {"type": "string"}}}}],
            "records": {"notes": [{"text": "first"}]},
            "views": [{"name": "notes", "kind": "table", "source": {"collection": "notes"}}]}}),
    )
    .await;
    post(&format!("decisions/{}/answer", app["decision"]), json!({"option": "Install"})).await;

    // A finished session with a tool call (events, usage), labels.
    mock.push(Reply::ToolCalls(vec![("bash".into(), r#"{"command":"echo hello"}"#.into())]));
    mock.push(Reply::Text("Said hello.".into()));
    let s: Session = serde_json::from_value(
        post("sessions", json!({"worker_id": wid, "cwd": "", "connection_id": conn.id, "model": "m", "prompt": "say hello", "labels": ["client:acme"], "title": "Hello"})).await,
    )
    .unwrap();
    let done = |id: String| {
        let (http, api) = (http.clone(), api.clone());
        async move {
            let mut n = 0;
            eventually("a finished turn", async || {
                let evs: Vec<Event> = http.get(format!("{api}/sessions/{id}/events")).send().await.ok()?.json().await.ok()?;
                n += 1;
                if n % 100 == 0 {
                    eprintln!(
                        "{:#?}",
                        evs.iter().map(|e| format!("{:?}", e.kind).chars().take(300).collect::<String>()).collect::<Vec<_>>()
                    );
                }
                evs.iter().any(|e| matches!(e.kind, EventKind::TurnEnded { .. })).then_some(())
            })
            .await
        }
    };
    done(s.id.clone()).await;

    // An assistant conversation (it runs in the head).
    mock.push(Reply::Text("You have one worker.".into()));
    let a: Session = serde_json::from_value(post("assistant", json!({"prompt": "what workers do I have?"})).await).unwrap();
    done(a.id.clone()).await;

    // A session waiting for approval (an "ask" policy), left waiting.
    mock.push(Reply::ToolCalls(vec![("bash".into(), r#"{"command":"git push origin main"}"#.into())]));
    let waiting: Session = serde_json::from_value(
        post(
            "sessions",
            json!({"worker_id": wid, "cwd": "", "connection_id": conn.id, "model": "m", "prompt": "push it", "title": "Push"}),
        )
        .await,
    )
    .unwrap();
    eventually("an approval request", async || {
        let evs: Vec<Event> = http.get(format!("{api}/sessions/{}/events", waiting.id)).send().await.ok()?.json().await.ok()?;
        evs.iter().any(|e| matches!(&e.kind, EventKind::Approval { state, .. } if state == "requested")).then_some(())
    })
    .await;

    // A workflow run waiting at its approve step.
    mock.push(Reply::Text("RESULT: disk at 91%".into()));
    let run = post("workflows/check-then-fix/run", json!({"input": "web1"})).await;
    let run_id = run["id"].as_str().map(str::to_owned).unwrap_or_default();
    eventually("the workflow waiting for approval", async || {
        let r: Value = http.get(format!("{api}/workflows/runs/{run_id}")).send().await.ok()?.json().await.ok()?;
        r["steps"].as_array()?.iter().any(|s| s["id"] == "ok" && s["status"] == "waiting").then_some(())
    })
    .await;

    // Stop, then fill whatever the API did not reach.
    stop.cancel();
    let opts = sqlx::sqlite::SqliteConnectOptions::new().filename(head_dir.path().join("c6h.db"));
    let pool = sqlx::sqlite::SqlitePoolOptions::new().max_connections(1).connect_with(opts).await.unwrap();
    let empty = fill_empty_tables(&pool).await;
    let version: i64 = sqlx::query_scalar("SELECT max(version) FROM _sqlx_migrations").fetch_one(&pool).await.unwrap();
    let out = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("../c6-head/fixtures/c6h-v{version:04}.db"));
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

/// Every saved head database (each earlier version's shape) upgrades to
/// this version and is served: its sessions, settings, action items and
/// automations come back through the API.
#[tokio::test]
async fn head_fixtures_upgrade_and_serve() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../c6-head/fixtures");
    let mut fixtures: Vec<_> =
        std::fs::read_dir(&dir).unwrap().flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "db")).collect();
    fixtures.sort();
    assert!(!fixtures.is_empty(), "no fixtures in {}", dir.display());
    for fixture in fixtures {
        let name = fixture.file_name().unwrap().to_string_lossy().into_owned();
        // A try on a copy first, as `c6h upgrade --check` does.
        let checked = c6_head::store::check_upgrade(&fixture).await.unwrap_or_else(|e| panic!("{name}: {e:#}"));
        assert!(checked.checks.iter().all(|c| c.ok), "{name}: {}", checked.text());

        let data = tempfile::tempdir().unwrap();
        std::fs::copy(&fixture, data.path().join("c6h.db")).unwrap();
        let head = c6_head::start(HeadConfig {
            data_dir: data.path().to_owned(),
            bind: vec!["127.0.0.1".parse().unwrap()],
            ui_port: 0,
            worker_port: 0,
            auth: false,
            ..Default::default()
        })
        .await
        .unwrap_or_else(|e| panic!("{name}: {e:#}"));
        let api = format!("http://127.0.0.1:{}/api/v1", head.ui_addrs[0].port());
        let get = |path: &str| {
            let url = format!("{api}/{path}");
            async move {
                let r = reqwest::get(&url).await.unwrap();
                assert!(r.status().is_success(), "{url}: {}", r.status());
                r.json::<Value>().await.unwrap()
            }
        };
        let sessions = get("sessions").await;
        assert!(sessions.as_array().is_some_and(|s| s.len() >= 2), "{name}: {sessions}");
        for setting in [
            "skills",
            "agents",
            "commands",
            "hooks",
            "mcp-servers",
            "templates",
            "workflows",
            "policies",
            "routes",
            "channels",
            "prices",
            "budgets",
            "hosts",
            "memex_scopes",
            "memex_folders",
        ] {
            let v = get(&format!("settings/{setting}")).await;
            assert!(v.as_array().is_some_and(|a| !a.is_empty()), "{name}: settings/{setting} is empty: {v}");
        }
        assert_eq!(get("settings/public_url").await["url"], "https://c6.example.net", "{name}");
        assert!(get("decisions?state=open").await.as_array().is_some_and(|t| t.len() >= 2), "{name}");
        assert!(get("automations").await.as_array().is_some_and(|a| a.len() >= 2), "{name}");
        assert!(get("memories").await.as_array().is_some_and(|m| m.len() >= 2), "{name}");
        assert!(get("workflows/runs").await.to_string().contains("check-then-fix"), "{name}");
        // Realms: root and home; the assistant's conversations in root,
        // everything else in home.
        let realms = get("realms").await;
        let ids: Vec<&str> = realms.as_array().unwrap().iter().filter_map(|r| r["id"].as_str()).collect();
        assert_eq!(&ids[..2], ["root", "home"], "{name}: {realms}");
        // Commons (from version 34 on), read by home.
        {
            assert!(ids.contains(&"commons"), "{name}: {realms}");
            assert_eq!(realms[1]["reads_commons"], true, "{name}: {realms}");
        }
        // Views (from version 37 on).
        if name.as_str() >= "c6h-v0037.db" {
            let v = get("views").await;
            assert!(v.as_array().unwrap().iter().any(|x| x["name"] == "candidates"), "{name}: {v}");
        }
        // Apps (from version 38 on), as instances (version 42): the app owns
        // its collection, and its code is read from its package, so the
        // copies it once wrote into its realm's lists are gone while what
        // the realm uses is unchanged.
        if name.as_str() >= "c6h-v0038.db" {
            let a = get("apps").await;
            assert!(a.as_array().unwrap().iter().any(|x| x["app"] == "notes" && x["state"] == "installed"), "{name}: {a}");
            // Before instances an app got a realm of its own; since, it goes in home.
            let realm = if name.as_str() >= "c6h-v0042.db" { "home" } else { "notes" };
            let cols = get(&format!("collections?realm={realm}")).await;
            assert!(cols.as_array().unwrap().iter().any(|c| c["name"] == "notes" && c["owner"] == "notes"), "{name}: {cols}");
            if realm == "notes" {
                assert_eq!(get("settings/views?realm=notes").await, json!([]), "{name}");
            }
            let views = get(&format!("realms/{realm}/effective/views")).await;
            assert!(views.as_array().unwrap().iter().any(|v| v["entry"]["name"] == "notes" && v["from"] == "app:notes"), "{name}: {views}");
            let installs = get("installs").await;
            assert!(installs.as_array().unwrap().iter().any(|i| i["name"] == "notes" && i["realm"] == realm), "{name}: {installs}");
            // An app in the realm it was given is offered a move; moved to
            // home, its records come along, with their history.
            if realm == "notes" {
                let ds = get("decisions?state=open").await;
                let d = ds
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|d| d["title"] == "Move notes out of its own realm?")
                    .unwrap_or_else(|| panic!("{name}: {ds}"));
                let r = reqwest::Client::new()
                    .post(format!("{api}/decisions/{}/answer", d["id"]))
                    .json(&json!({"option": "Move it", "values": {"realm": "home"}}))
                    .send()
                    .await
                    .unwrap();
                assert!(r.status().is_success(), "{name}: {}", r.text().await.unwrap());
                let recs = get("collections/notes/records?realm=home").await;
                assert_eq!(recs["records"][0]["fields"]["text"], "first", "{name}: {recs}");
                assert!(recs["records"][0]["derived_from"].to_string().contains("record:notes/notes/"), "{name}: {recs}");
                let installs = get("installs").await;
                assert!(installs.as_array().unwrap().iter().any(|i| i["name"] == "notes" && i["realm"] == "home"), "{name}: {installs}");
                assert!(!installs.as_array().unwrap().iter().any(|i| i["realm"] == "notes"), "{name}: {installs}");
            }
        }
        // Grants (from version 33 on).
        if name.as_str() >= "c6h-v0033.db" {
            let g = get("grants").await;
            assert!(g.as_array().unwrap().iter().any(|g| g["to_realm"] == "lab" && g["state"] == "active"), "{name}: {g}");
        }
        let all = sessions.as_array().unwrap();
        assert!(all.iter().all(|s| s["realm"] == "home" || s["realm"] == "root"), "{name}: {sessions}");
        assert!(all.iter().any(|s| s["realm"] == "root"), "{name}: the assistant's conversation is in root");
        // Collections (from version 29 on): the records and their history.
        if name.as_str() >= "c6h-v0029.db" {
            let page = reqwest::Client::new()
                .post(format!("{api}/collections/candidates/query"))
                .json(&json!({"sort": "name"}))
                .send()
                .await
                .unwrap();
            let page: Value = page.json().await.unwrap();
            assert_eq!(page["total"], 2, "{name}: {page}");
            let id = page["records"][0]["id"].as_str().unwrap();
            let h = get(&format!("collections/candidates/records/{id}/history")).await;
            assert_eq!(h.as_array().map(Vec::len), Some(2), "{name}: {h}");
        }
        // What already waited (a held tool call, a workflow's approve
        // step) has its decision once the head is up.
        let deadline = tokio::time::Instant::now() + c6_testkit::wait::deadline();
        loop {
            let ds = get("decisions").await;
            let kinds: Vec<String> = ds
                .as_array()
                .unwrap()
                .iter()
                .filter(|d| d["waiting"] == true)
                .map(|d| d["source"].as_str().unwrap_or("").to_owned())
                .collect();
            if kinds.iter().any(|k| k.starts_with("session:")) && kinds.iter().any(|k| k.starts_with("workflow:")) {
                break;
            }
            assert!(tokio::time::Instant::now() < deadline, "{name}: no decisions for what waits: {ds}");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        // Its upgrade history includes the fixture's own creation.
        assert!(get("system/upgrades").await.as_array().is_some_and(|u| !u.is_empty()), "{name}");
        head.shutdown_token().cancel();
    }
}
