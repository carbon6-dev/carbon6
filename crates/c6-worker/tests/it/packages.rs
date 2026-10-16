//! Packages (docs/packages.md): the local registry keeps versions by
//! digest; installing resolves what a package requires (kits, libraries,
//! apps), asks once, shares installs, and refuses cycles and conflicts;
//! upgrades ask only for what grew; removal waits for what requires it.

use base64::Engine;
use c6_api::{Decision, Install, PackageInfo};
use c6_head::HeadConfig;
use reqwest::StatusCode;
use serde_json::{Value, json};

#[tokio::test]
async fn packages_require_install_and_remove() {
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
    use reqwest::Method as M;
    let send = |method: M, path: &str, body: Value| {
        let (http, url) = (http.clone(), format!("{api}/{path}"));
        async move { http.request(method, &url).json(&body).send().await.unwrap() }
    };
    let ok = async |r: reqwest::Response| -> Value {
        let s = r.status();
        let v: Value = r.json().await.unwrap_or(Value::Null);
        assert!(s.is_success(), "{s}: {v}");
        v
    };
    let b64 = |s: &str| base64::engine::general_purpose::STANDARD.encode(s);
    let add = |manifest: Value, files: Value| async move { send(M::POST, "packages", json!({"manifest": manifest, "files": files})).await };
    let decide = |id: i64, option: &'static str| async move {
        let r = send(M::POST, &format!("decisions/{id}/answer"), json!({"option": option})).await;
        assert!(r.status().is_success(), "{}", r.text().await.unwrap());
    };

    // A kit, built from another kit.
    let base = json!({"package": "py-base", "version": "1.0.0", "kit": {"tools": [{"name": "py", "command": ["python3", "{script}"], "args": {"script": "path"}}]}});
    ok(add(base.clone(), json!({"Dockerfile": b64("FROM debian:bookworm-slim\n")})).await).await;
    let render = json!({"package": "render", "version": "1.0.0", "requires": {"py-base": "^1"},
        "permissions": {"memory_mb": 2048, "timeout_s": 300},
        "kit": {"build": {"files": ["r.sh"]}, "tools": [{"name": "pdf", "command": ["sh", "/opt/r.sh", "{html}"], "args": {"html": "path"}}]}});
    ok(add(render.clone(), json!({"Dockerfile": b64("FROM kit:py-base\nCOPY r.sh /opt/r.sh\n"), "r.sh": b64("echo hi\n")})).await).await;
    // Checked: files, kit references, ranges; the same version with other
    // content is refused, the same content is fine.
    let r = add(json!({"package": "bad", "version": "1.0.0", "kit": {"tools": [{"name": "t", "command": ["x"]}]}}), json!({})).await;
    assert!(r.text().await.unwrap().contains("Dockerfile is not among the package's files"));
    let r = add(
        json!({"package": "bad", "version": "1.0.0", "kit": {"tools": [{"name": "t", "command": ["x"]}]}}),
        json!({"Dockerfile": b64("FROM kit:nope\n")}),
    )
    .await;
    assert!(r.text().await.unwrap().contains("kit:nope, which the package does not require"));
    assert_eq!(
        add(json!({"package": "x", "version": "one", "contents": {"skills": [{"name": "s", "body": "b"}]}}), json!({})).await.status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(add(base.clone(), json!({"Dockerfile": b64("FROM debian:bookworm\n")})).await.status(), StatusCode::BAD_REQUEST);
    ok(add(base, json!({"Dockerfile": b64("FROM debian:bookworm-slim\n")})).await).await;

    // A library, and two apps: one exports a template, the other requires
    // it, the library and the kit.
    ok(add(
        json!({"package": "style", "version": "1.0.0", "contents": {"skills": [{"name": "house-style", "body": "Short sentences."}]}}),
        json!({}),
    )
    .await)
    .await;
    ok(add(
        json!({"package": "decks", "version": "1.0.0", "realm": "decks", "requires": {"render": "^1"},
        "contents": {"templates": [{"name": "deck", "prompt": "Make a deck: {{input}}"}]}, "exports": {"templates": ["deck"]}}),
        json!({}),
    )
    .await)
    .await;
    ok(add(
        json!({"package": "factory", "version": "1.0.0", "realm": "factory", "requires": {"decks": "^1", "style": "*"},
        "permissions": {"secrets": ["GITHUB_TOKEN"]}}),
        json!({}),
    )
    .await)
    .await;

    let reg: Vec<PackageInfo> = send(M::GET, "packages", json!({})).await.json().await.unwrap();
    let roles = |n: &str| reg.iter().find(|p| p.name == n).unwrap().roles.clone();
    assert_eq!(
        (roles("render"), roles("style"), roles("decks")),
        (vec!["kit".to_owned()], vec!["library".to_owned()], vec!["app".to_owned()])
    );

    // The plan: everything, deepest first, with what each asks for. Apps and
    // libraries go in the realm you choose (it must exist), and what they
    // require goes with them.
    let plan = ok(send(M::POST, "installs/plan", json!({"package": "factory"})).await).await;
    assert_eq!(plan["realm"], "home", "the realm it suggests does not exist: home");
    assert!(
        ok(send(M::POST, "installs/plan", json!({"package": "factory", "realm": "nope"})).await).await["problems"]
            .to_string()
            .contains("create it first")
    );
    ok(send(M::POST, "realms", json!({"id": "factory", "name": "Factory"})).await).await;
    let plan = ok(send(M::POST, "installs/plan", json!({"package": "factory"})).await).await;
    let text = plan["plan"].as_str().unwrap();
    for want in [
        "**py-base** 1.0.0 (kit)",
        "**render** 1.0.0 (kit)",
        "**decks** 1.0.0 (app) in realm **factory**",
        "**style** 1.0.0 (library) in realm **factory**",
        "Install **factory**",
        "No network. Up to 1 CPUs and 2.0 GB of memory, 5 minutes a run.",
        "GITHUB_TOKEN",
    ] {
        assert!(text.contains(want), "{want} in {text}");
    }
    assert!(text.find("py-base").unwrap() < text.find("**render**").unwrap(), "requirements first");

    // One decision installs them all.
    let i: Install = serde_json::from_value(ok(send(M::POST, "installs", json!({"package": "factory"})).await).await).unwrap();
    let d: Decision = send(M::GET, &format!("decisions/{}", i.decision.unwrap()), json!({})).await.json().await.unwrap();
    assert_eq!(d.title, "Install the app factory in factory?");
    decide(d.id, "Install").await;
    let all: Vec<Install> = send(M::GET, "installs", json!({})).await.json().await.unwrap();
    let by = |n: &str| all.iter().find(|i| i.package == n).unwrap().clone();
    assert!(all.iter().all(|i| i.state == "installed"), "{all:?}");
    assert_eq!((by("render").realm.as_str(), by("style").realm.as_str(), by("decks").realm.as_str()), ("", "factory", "factory"));
    assert_eq!(by("render").required_by, vec![by("decks").id]);
    assert_eq!(by("py-base").required_by, vec![by("render").id]);
    assert_eq!(by("factory").locks.len(), 2);
    let skills = ok(send(M::GET, "realms/factory/effective/skills", json!({})).await).await;
    assert!(skills.to_string().contains("house-style") && skills.to_string().contains("app:style"), "the library is in factory: {skills}");
    let templates = ok(send(M::GET, "templates", json!({})).await).await;
    assert!(templates.as_array().unwrap().iter().any(|t| t["realm"] == "factory" && t["name"] == "deck"), "{templates}");

    // Conflicts and cycles are refused.
    ok(add(json!({"package": "old-user", "version": "1.0.0", "realm": "olduser", "requires": {"decks": "^2"}}), json!({})).await).await;
    let r = send(M::POST, "installs", json!({"package": "old-user"})).await;
    assert!(r.text().await.unwrap().contains("not in the registry"));
    ok(add(
        json!({"package": "ping", "version": "1.0.0", "requires": {"pong": "*"}, "contents": {"skills": [{"name": "p", "body": "b"}]}}),
        json!({}),
    )
    .await)
    .await;
    ok(add(
        json!({"package": "pong", "version": "1.0.0", "requires": {"ping": "*"}, "contents": {"skills": [{"name": "q", "body": "b"}]}}),
        json!({}),
    )
    .await)
    .await;
    let r = send(M::POST, "installs", json!({"package": "ping", "realm": "factory"})).await;
    assert!(r.text().await.unwrap().contains("a cycle: ping → pong → ping"));

    // Removal waits for what requires it.
    let r = send(M::DELETE, &format!("installs/{}", by("render").id), json!({})).await;
    assert!(r.text().await.unwrap().contains("required by decks in factory"));
    assert_eq!(send(M::DELETE, "packages/render/1.0.0", json!({})).await.status(), StatusCode::BAD_REQUEST);

    // An upgrade asks again only for what grew.
    ok(add(json!({"package": "render", "version": "1.1.0", "requires": {"py-base": "^1"}, "permissions": {"memory_mb": 2048, "timeout_s": 300, "network": "any"},
        "kit": {"build": {"files": ["r.sh"]}, "tools": [{"name": "pdf", "command": ["sh", "/opt/r.sh", "{html}"], "args": {"html": "path"}}]}}),
        json!({"Dockerfile": b64("FROM kit:py-base\nCOPY r.sh /opt/r.sh\n"), "r.sh": b64("echo hi v2\n")})).await).await;
    let up: Install =
        serde_json::from_value(ok(send(M::POST, &format!("installs/{}/upgrade", by("render").id), json!({})).await).await).unwrap();
    let d: Decision = send(M::GET, &format!("decisions/{}", up.decision.unwrap()), json!({})).await.json().await.unwrap();
    assert!(d.document.contains("It asks for more than before: network access"), "{}", d.document);
    decide(d.id, "Upgrade").await;
    assert_eq!(install(&send, by("render").id).await.version, "1.1.0");

    // Removing the library takes its entries out of the realm; then the
    // app and its realm.
    let lib = by("style").id;
    let r = send(M::DELETE, &format!("installs/{lib}"), json!({})).await;
    assert!(r.text().await.unwrap().contains("required by factory"));
    let rm: Install =
        serde_json::from_value(ok(send(M::DELETE, &format!("installs/{}", by("factory").id), json!({})).await).await).unwrap();
    decide(rm.decision.unwrap(), "Remove").await;
    let rm: Install = serde_json::from_value(ok(send(M::DELETE, &format!("installs/{lib}"), json!({})).await).await).unwrap();
    assert_eq!(rm.state, "removing");
    let left: Vec<Install> = send(M::GET, "installs", json!({})).await.json().await.unwrap();
    assert!(left.iter().all(|i| i.package != "factory"), "{left:?}");

    head.shutdown().await.unwrap();
}

async fn install<F, Fut>(send: &F, id: i64) -> Install
where
    F: Fn(reqwest::Method, &str, Value) -> Fut,
    Fut: std::future::Future<Output = reqwest::Response>,
{
    let all: Vec<Install> = send(reqwest::Method::GET, "installs", json!({})).await.json().await.unwrap();
    all.into_iter().find(|i| i.id == id).unwrap()
}

/// A kit built from a kit, run by an agent in the sandbox: no network, a
/// read-only root, not root, the session's folder at /work. Needs Docker or
/// Podman with debian:bookworm-slim already pulled; skipped otherwise.
#[tokio::test]
async fn kits_build_and_run_sandboxed() {
    use c6_testkit::Reply;
    use std::time::Duration;

    let Some(rt) = c6_tools::kits::runtime().await else {
        eprintln!("no container runtime; skipped");
        return;
    };
    let has_base =
        tokio::process::Command::new(&rt).args(["image", "inspect", "debian:bookworm-slim"]).output().await.unwrap().status.success();
    if !has_base {
        eprintln!("debian:bookworm-slim is not pulled; skipped");
        return;
    }
    let work = tempfile::tempdir().unwrap();
    let w = with_worker().await;
    let (mock, wid) = (&w.mock, w.wid);
    let api = w.api.clone();
    let http = reqwest::Client::new();
    use reqwest::Method as M;
    let send = |method: M, path: &str, body: Value| {
        let (http, url) = (http.clone(), format!("{api}/{path}"));
        async move { http.request(method, &url).json(&body).send().await.unwrap() }
    };
    let ok = async |r: reqwest::Response| -> Value {
        let s = r.status();
        let v: Value = r.json().await.unwrap_or(Value::Null);
        assert!(s.is_success(), "{s}: {v}");
        v
    };
    let b64 = |s: &str| base64::engine::general_purpose::STANDARD.encode(s);
    let conn = w.conn.clone();

    // A base kit, a kit built from it, and a library in the home realm that
    // requires the kit.
    let nonce = format!("{:x}", c6_core::now_ms());
    let base = format!("c6test-base-{nonce}");
    let probe = format!("c6test-probe-{nonce}");
    ok(send(
        M::POST,
        "packages",
        json!({"manifest": {"package": base, "version": "1.0.0", "kit": {"tools": []}},
        "files": {"Dockerfile": b64("FROM debian:bookworm-slim\nRUN echo base > /opt/base.txt\n")}}),
    )
    .await)
    .await;
    let script = "echo \"routes: $(tail -n +2 /proc/net/route | wc -l)\"; echo \"uid: $(id -u)\"; cat /opt/base.txt; \
                  touch /etc/x 2>/dev/null && echo root-writable || echo root-read-only; echo made > \"$1\"; echo \"tmp: $(touch /tmp/t && echo ok)\"\n";
    ok(send(M::POST, "packages", json!({"manifest": {"package": probe, "version": "1.0.0", "requires": {&base: "^1"},
        "kit": {"build": {"files": ["probe.sh"]}, "tools": [{"name": "probe", "description": "Looks around.", "command": ["sh", "/opt/probe.sh", "{out}"], "args": {"out": "path"}}]}},
        "files": {"Dockerfile": b64(&format!("FROM kit:{base}\nCOPY probe.sh /opt/probe.sh\n")), "probe.sh": b64(script)}})).await).await;
    ok(send(
        M::POST,
        "packages",
        json!({"manifest": {"package": "probe-lib", "version": "1.0.0", "requires": {&probe: "*"},
        "contents": {"skills": [{"name": "probing", "body": "Use kit_run."}]}}, "files": {}}),
    )
    .await)
    .await;
    let i: Install =
        serde_json::from_value(ok(send(M::POST, "installs", json!({"package": "probe-lib", "realm": "home"})).await).await).unwrap();
    let r = send(M::POST, &format!("decisions/{}/answer", i.decision.unwrap()), json!({"option": "Install"})).await;
    assert!(r.status().is_success(), "{}", r.text().await.unwrap());

    mock.push(Reply::ToolCalls(vec![("kit_run".into(), json!({}).to_string())]));
    mock.push(Reply::ToolCalls(vec![(
        "kit_run".into(),
        json!({"kit": probe, "tool": "probe", "args": {"out": "out/made.txt"}}).to_string(),
    )]));
    mock.push(Reply::ToolCalls(vec![(
        "kit_run".into(),
        json!({"kit": probe, "tool": "probe", "args": {"out": "../escape.txt"}}).to_string(),
    )]));
    mock.push(Reply::ToolCalls(vec![("kit_shell".into(), json!({"kit": probe, "command": "id"}).to_string())]));
    mock.push(Reply::Text("Done.".into()));
    std::fs::create_dir(work.path().join("out")).unwrap();
    ok(send(M::POST, "sessions", json!({"worker_id": wid, "cwd": work.path(), "connection_id": conn.id, "model": "m", "prompt": "probe"}))
        .await)
    .await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(600);
    let seen = loop {
        let reqs = mock.requests();
        if reqs.len() >= 5 {
            break reqs.last().unwrap()["messages"].to_string();
        }
        assert!(tokio::time::Instant::now() < deadline, "timed out; {} requests", reqs.len());
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    assert!(!seen.contains(&format!("\\\"kit\\\": \\\"{base}")), "build-only kits are not listed: {seen}");
    for want in [
        "Looks around.",
        "routes: 0",
        "uid: ",
        "base",
        "root-read-only",
        "tmp: ok",
        "outside the session's folder",
        "does not allow a shell",
    ] {
        assert!(seen.contains(want), "{want} in {seen}");
    }
    assert!(!seen.contains("uid: 0\\"), "not root: {seen}");
    assert_eq!(std::fs::read_to_string(work.path().join("out/made.txt")).unwrap().trim(), "made");

    // Clean up the test images.
    let tags = tokio::process::Command::new(&rt).args(["image", "ls", "--format", "{{.Repository}}:{{.Tag}}"]).output().await.unwrap();
    for t in String::from_utf8_lossy(&tags.stdout).lines().filter(|t| t.contains(&nonce)) {
        let _ = tokio::process::Command::new(&rt).args(["rmi", t]).output().await;
    }
    w.stop().await;
}

/// A head with a connected worker and a mock model.
struct WithWorker {
    head: c6_head::Head,
    api: String,
    wid: uuid::Uuid,
    conn: c6_api::Connection,
    mock: c6_testkit::MockLlm,
    stop: tokio_util::sync::CancellationToken,
    _dirs: (tempfile::TempDir, tempfile::TempDir),
}

impl WithWorker {
    async fn stop(self) {
        self.stop.cancel();
        self.head.shutdown().await.unwrap();
    }
}

async fn with_worker() -> WithWorker {
    use c6_worker::{ConnState, Identity, Worker, conn, enroll};
    let mock = c6_testkit::MockLlm::start().await;
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
    let e = c6_head::api::new_enrollment(&head.state, None).await.unwrap();
    enroll::enroll(&e.token).await.unwrap().save(state.path()).unwrap();
    let worker = Worker::open(state.path(), Identity::load(state.path()).unwrap()).await.unwrap();
    let (tx, mut rx) = tokio::sync::watch::channel(ConnState::Connecting);
    let stop = tokio_util::sync::CancellationToken::new();
    tokio::spawn(conn::run(worker.clone(), tx, stop.clone()));
    tokio::time::timeout(std::time::Duration::from_secs(10), rx.wait_for(|s| matches!(s, ConnState::Attached { .. })))
        .await
        .unwrap()
        .unwrap();
    let conn: c6_api::Connection = reqwest::Client::new()
        .post(format!("{api}/connections"))
        .json(&json!({"name": "m", "base_url": mock.base_url()}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    WithWorker { head, api, wid: worker.identity.meta.worker_id.0, conn, mock, stop, _dirs: (head_dir, state) }
}

/// Another realm's templates start through a grant naming them; nothing
/// else of that realm (docs/apps.md §7). An app's own requirements are
/// installed beside it, in its realm.
#[tokio::test]
async fn template_start_through_an_export() {
    use c6_testkit::Reply;
    let w = with_worker().await;
    let http = reqwest::Client::new();
    let api = w.api.clone();
    use reqwest::Method as M;
    let send = |method: M, path: &str, body: Value| {
        let (http, url) = (http.clone(), format!("{api}/{path}"));
        async move { http.request(method, &url).json(&body).send().await.unwrap() }
    };
    let ok = async |r: reqwest::Response| -> Value {
        let s = r.status();
        let v: Value = r.json().await.unwrap_or(Value::Null);
        assert!(s.is_success(), "{s}: {v}");
        v
    };
    let templates = json!([{"name": "make", "prompt": "Make: {{input}}"}, {"name": "secret", "prompt": "Secret: {{input}}"}]);
    ok(send(
        M::POST,
        "packages",
        json!({"manifest": {"package": "maker", "version": "1.0.0", "realm": "maker",
        "contents": {"templates": templates}, "exports": {"templates": ["make"]}}, "files": {}}),
    )
    .await)
    .await;
    ok(send(
        M::POST,
        "packages",
        json!({"manifest": {"package": "user", "version": "1.0.0", "realm": "user", "requires": {"maker": "^1"},
        "contents": {"skills": [{"name": "s", "body": "b"}]}}, "files": {}}),
    )
    .await)
    .await;
    for realm in ["maker", "user"] {
        ok(send(M::POST, "realms", json!({"id": realm})).await).await;
    }
    for package in ["maker", "user"] {
        let i: Install = serde_json::from_value(ok(send(M::POST, "installs", json!({"package": package})).await).await).unwrap();
        let r = send(M::POST, &format!("decisions/{}/answer", i.decision.unwrap()), json!({"option": "Install"})).await;
        assert!(r.status().is_success(), "{}", r.text().await.unwrap());
    }
    let all: Vec<Install> = send(M::GET, "installs", json!({})).await.json().await.unwrap();
    assert!(all.iter().any(|i| i.package == "maker" && i.realm == "user"), "a requirement goes beside what requires it: {all:?}");
    ok(send(
        M::POST,
        "grants",
        json!({"from_realm": "maker", "to_realm": "user", "kind": "operations", "mode": "call", "scope": {"methods": ["template:make"]}}),
    )
    .await)
    .await;

    w.mock
        .push(Reply::ToolCalls(vec![("template_start".into(), json!({"realm": "maker", "template": "secret", "input": "x"}).to_string())]));
    w.mock.push(Reply::ToolCalls(vec![(
        "template_start".into(),
        json!({"realm": "maker", "template": "make", "input": "a chair"}).to_string(),
    )]));
    w.mock.push(Reply::Text("Started.".into()));
    w.mock.push(Reply::Text("Made.".into()));
    let s: c6_api::Session = serde_json::from_value(
        ok(send(
            M::POST,
            "sessions",
            json!({"worker_id": w.wid, "cwd": "", "connection_id": w.conn.id, "model": "m", "prompt": "go", "realm": "user"}),
        )
        .await)
        .await,
    )
    .unwrap();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
    let started = loop {
        let all: Vec<c6_api::Session> = send(M::GET, "sessions", json!({})).await.json().await.unwrap();
        if let Some(x) = all.into_iter().find(|x| x.labels.contains(&format!("started-by:{}", s.id))) {
            break x;
        }
        assert!(tokio::time::Instant::now() < deadline, "no started session");
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    };
    assert_eq!(started.realm, "maker");
    let seen = loop {
        let seen = w.mock.requests().iter().map(|r| r["messages"].to_string()).collect::<Vec<_>>().join("\n");
        if seen.contains("Make: a chair") || tokio::time::Instant::now() >= deadline {
            break seen;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    };
    assert!(seen.contains("may not start maker's template \\\"secret\\\""), "{seen}");
    assert!(seen.contains("Make: a chair"), "{seen}");
    w.stop().await;
}

/// The decks-render kit from examples/packages, installed with the decks
/// app and run by an agent: slides, PDF and PowerPoint. Its first build
/// downloads Debian packages, Chromium and fonts, so it runs only with
/// C6_TEST_DECKS_RENDER=1.
#[tokio::test]
async fn decks_render_kit_renders_a_deck() {
    use c6_testkit::Reply;
    if std::env::var("C6_TEST_DECKS_RENDER").is_err() || c6_tools::kits::runtime().await.is_none() {
        eprintln!("set C6_TEST_DECKS_RENDER=1 (and have Docker or Podman) to run");
        return;
    }
    let w = with_worker().await;
    let http = reqwest::Client::new();
    let api = w.api.clone();
    let ok = async |r: reqwest::Response| -> Value {
        let s = r.status();
        let v: Value = r.json().await.unwrap_or(Value::Null);
        assert!(s.is_success(), "{s}: {v}");
        v
    };
    let kit: Value = serde_json::from_str(include_str!("../../../../examples/packages/decks-render.json")).unwrap();
    let decks: Value = serde_json::from_str(include_str!("../../../../examples/apps/decks.json")).unwrap();
    ok(http.post(format!("{api}/packages")).json(&kit).send().await.unwrap()).await;
    let i: c6_api::AppInstall =
        serde_json::from_value(ok(http.post(format!("{api}/apps")).json(&decks).send().await.unwrap()).await).unwrap();
    ok(http.post(format!("{api}/decisions/{}/answer", i.decision.unwrap())).json(&json!({"option": "Install"})).send().await.unwrap())
        .await;

    // A deck made the way deck-html says: its base CSS and a theme.
    let skill =
        decks["contents"]["skills"].as_array().unwrap().iter().find(|s| s["name"] == "deck-html").unwrap()["body"].as_str().unwrap();
    let base = skill.split("```css\n").nth(1).unwrap().split("```").next().unwrap();
    let work = tempfile::tempdir().unwrap();
    std::fs::write(
        work.path().join("deck.html"),
        format!(
            "<!doctype html><meta charset=utf-8><style>{base}:root{{--bg:#fff;--fg:#111;--accent:#c00;--muted:#666;--font-body:Inter,sans-serif;--font-display:P052,serif;--display-weight:700}}</style>\
             <section class='slide title'><h1>One</h1></section><section class='slide'><h2>Two</h2><ul class=points><li>é ✓</li></ul></section>"
        ),
    )
    .unwrap();
    for (tool, out) in [("slides", "slides"), ("pdf", "deck.pdf"), ("pptx", "deck.pptx")] {
        w.mock.push(Reply::ToolCalls(vec![(
            "kit_run".into(),
            json!({"kit": "decks-render", "tool": tool, "args": {"html": "deck.html", "out": out}}).to_string(),
        )]));
    }
    w.mock.push(Reply::Text("Rendered.".into()));
    ok(http
        .post(format!("{api}/sessions"))
        .json(&json!({"worker_id": w.wid, "cwd": work.path(), "connection_id": w.conn.id, "model": "m", "prompt": "render", "realm": "decks"}))
        .send()
        .await
        .unwrap())
    .await;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(1800);
    while w.mock.requests().len() < 4 {
        assert!(tokio::time::Instant::now() < deadline, "timed out");
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    let seen = w.mock.requests().last().unwrap()["messages"].to_string();
    for want in ["2 slides: /work/slides/slide-01.png", "printed /work/deck.pdf", "2 slides in /work/deck.pptx"] {
        assert!(seen.contains(want), "{want} in {seen}");
    }
    let png = std::fs::read(work.path().join("slides/slide-02.png")).unwrap();
    assert_eq!(&png[1..4], b"PNG");
    assert!(std::fs::read(work.path().join("deck.pdf")).unwrap().starts_with(b"%PDF"));
    assert!(std::fs::read(work.path().join("deck.pptx")).unwrap().starts_with(b"PK"));
    w.stop().await;
}
