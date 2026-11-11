//! What a worker is given (docs/os.md §3.2): credentials only for its own
//! live sessions, as their realms allow; template runs from a session on
//! that session's worker; imports and kit jobs only where the realm is
//! served. The workers here are the head's records (and, for runs, live
//! connections in its registry), not running processes.

use c6_core::WorkerId;
use c6_head::{HeadConfig, HeadState};
use c6_proto::{CredentialGrant, CredentialRequest, head_msg};
use serde_json::{Value, json};

async fn head() -> (c6_head::Head, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let head = c6_head::start(HeadConfig {
        data_dir: dir.path().to_owned(),
        bind: vec!["127.0.0.1".parse().unwrap()],
        ui_port: 0,
        worker_port: 0,
        auth: false,
        ..Default::default()
    })
    .await
    .unwrap();
    (head, dir)
}

/// The head's API, failing the test on an error.
struct Api {
    http: reqwest::Client,
    base: String,
}

impl Api {
    fn of(head: &c6_head::Head) -> Self {
        Self { http: reqwest::Client::new(), base: format!("http://127.0.0.1:{}/api/v1", head.ui_addrs[0].port()) }
    }

    async fn send(&self, method: reqwest::Method, path: &str, body: Value) -> Result<Value, String> {
        let r = self.http.request(method, format!("{}/{path}", self.base)).json(&body).send().await.unwrap();
        let s = r.status();
        let v: Value = r.json().await.unwrap_or(Value::Null);
        if s.is_success() { Ok(v) } else { Err(format!("{s}: {v}")) }
    }

    async fn ok(&self, method: reqwest::Method, path: &str, body: Value) -> Value {
        self.send(method, path, body).await.unwrap_or_else(|e| panic!("{path}: {e}"))
    }
}

/// A worker the head knows, available to `realms` (none: all).
async fn worker(st: &HeadState, name: &str, realms: &[&str]) -> WorkerId {
    let id = WorkerId(uuid::Uuid::now_v7());
    let w = c6_head::store::NewWorker { id, name, hostname: name, os: "linux", arch: "x86_64", version: "0", cert_serial: name };
    st.store.insert_worker(&w, "", 0).await.unwrap();
    st.store.set_worker_role(id, "worker").await.unwrap();
    let realms: Vec<String> = realms.iter().map(|r| r.to_string()).collect();
    c6_head::realms::set_availability(st, "workers", &id.to_string(), &realms).await.unwrap();
    id
}

/// A connection with a key, available to `realms` (none: all).
async fn connection(api: &Api, name: &str, realms: &[&str]) -> String {
    let c = api
        .ok(
            reqwest::Method::POST,
            "connections",
            json!({"name": name, "base_url": "http://127.0.0.1:9/v1", "api_key": format!("key-{name}")}),
        )
        .await;
    let id = c["id"].as_str().unwrap().to_owned();
    api.ok(reqwest::Method::PUT, &format!("connections/{id}/realms"), json!({"realms": realms})).await;
    id
}

/// A Claude Code connection: a session's own, never its subagents'.
async fn claude_code(api: &Api, name: &str) -> String {
    let c = api.ok(reqwest::Method::POST, "connections", json!({"kind": "claude_code", "name": name, "base_url": ""})).await;
    c["id"].as_str().unwrap().to_owned()
}

async fn session(api: &Api, worker: WorkerId, connection: &str, realm: &str) -> String {
    let body = json!({"worker_id": worker.0, "cwd": "", "connection_id": connection, "model": "m", "realm": realm});
    api.ok(reqwest::Method::POST, "sessions", body).await["id"].as_str().unwrap().to_owned()
}

async fn ask(st: &HeadState, worker: WorkerId, session: &str, connection: &str) -> CredentialGrant {
    let req = CredentialRequest { connection_id: connection.into(), session_id: session.into(), ..Default::default() };
    c6_head::sessions::credential(st, worker, req).await
}

#[tokio::test]
async fn credentials_go_only_to_what_a_workers_session_uses() {
    let (head, _dir) = head().await;
    let (st, api) = (&head.state, Api::of(&head));
    use reqwest::Method as M;
    api.ok(M::POST, "realms", json!({"id": "mach5-agents"})).await;
    let (a, b) = (worker(st, "a", &[]).await, worker(st, "b", &["mach5-agents"]).await);
    let home_only = connection(&api, "home-only", &["home"]).await;
    let shared = connection(&api, "shared", &[]).await;
    let mine = session(&api, a, &home_only, "home").await;
    let agents = session(&api, a, &shared, "mach5-agents").await;

    // The normal path: a's session, on its connection.
    let g = ask(st, a, &mine, &home_only).await;
    assert_eq!((g.error.as_str(), g.bearer.as_str()), ("", "key-home-only"));
    // Another worker's session.
    assert_eq!(ask(st, b, &mine, &home_only).await.error, "no such session on this worker");
    // A connection the session doesn't use: one its subagents may run on
    // (any its realm may use, Claude Code too, docs/subagents.md) is given.
    assert_eq!(ask(st, a, &mine, &shared).await.error, "");
    let (cc, cc2) = (claude_code(&api, "cc").await, claude_code(&api, "cc2").await);
    assert_eq!(ask(st, a, &mine, &cc).await.error, "");
    // A mach5-agents session asking for a home-only connection: it can't
    // switch to it, nor have it.
    let switch = json!({"text": "go", "connection_id": home_only, "model": "m"});
    let e = api.send(M::POST, &format!("sessions/{agents}/messages"), switch).await.unwrap_err();
    assert!(e.contains("not available to realm mach5-agents"), "{e}");
    assert!(!ask(st, a, &agents, &home_only).await.error.is_empty());
    // A model switch the worker hasn't reported yet: the new connection
    // (Claude Code to Claude Code over SSH, whose subagents can't run on
    // it, so only the switch gives it).
    let body = json!({"worker_id": a.0, "cwd": "", "connection_id": cc, "model": "m", "realm": "home", "ssh": "dev@box"});
    let on_cc = api.ok(M::POST, "sessions", body).await["id"].as_str().unwrap().to_owned();
    assert!(ask(st, a, &on_cc, &cc2).await.error.contains("does not use"));
    api.ok(M::POST, &format!("sessions/{on_cc}/messages"), json!({"text": "go", "connection_id": cc2, "model": "m"})).await;
    assert_eq!(ask(st, a, &on_cc, &cc2).await.error, "");
    // A connection kept from the session's realm after it started.
    assert_eq!(ask(st, a, &agents, &shared).await.error, "");
    api.ok(M::PUT, &format!("connections/{shared}/realms"), json!({"realms": ["home"]})).await;
    assert!(ask(st, a, &agents, &shared).await.error.contains("not available to realm mach5-agents"));

    // Secrets: name@session only, from the session's realm.
    api.ok(M::PUT, "settings/secrets/DEPLOY_KEY", json!({"value": "dk-1"})).await;
    assert_eq!(ask(st, a, &mine, &format!("secret:DEPLOY_KEY@{mine}")).await.bearer, "dk-1");
    assert!(ask(st, a, "", "secret:DEPLOY_KEY").await.error.contains("name@session"));
    assert_eq!(ask(st, b, &mine, &format!("secret:DEPLOY_KEY@{mine}")).await.error, "no such session on this worker");
    assert!(ask(st, a, &agents, &format!("secret:DEPLOY_KEY@{mine}")).await.error.contains("its own secrets"));

    // An older worker says no session: one of its sessions must use it, and
    // it must serve every realm (one kept to some is as new as this).
    assert_eq!(ask(st, a, "", &home_only).await.bearer, "key-home-only");
    assert!(!ask(st, b, "", &home_only).await.error.is_empty());
    let theirs = connection(&api, "theirs", &[]).await;
    let on_b = session(&api, b, &theirs, "mach5-agents").await;
    assert_eq!(ask(st, b, &on_b, &theirs).await.error, "");
    assert!(ask(st, b, "", &theirs).await.error.contains("says which session asks"));
    // The head's own (the knowledge store, the assistant): as before.
    let req = CredentialRequest { connection_id: home_only.clone(), ..Default::default() };
    assert_eq!(c6_head::sessions::credential_for(st, None, "head", req).await.bearer, "key-home-only");
    // A pool (docs/pools.md) has no credentials of its own.
    let pool =
        api.ok(M::POST, "connections", json!({"kind": "pool", "name": "p", "members": [{"connection_id": shared, "slots": 1}]})).await;
    assert!(ask(st, a, &mine, pool["id"].as_str().unwrap()).await.error.contains("is a pool"));
    head.shutdown().await.unwrap();
}

/// A worker older than the head (the head is updated first) says no session
/// when it asks: serving every realm, it still gets what its live sessions
/// use (a session's connection, a model switch's, their secrets as
/// name@session) or their subagents may run on, and nothing else.
#[tokio::test]
async fn an_older_worker_still_gets_what_its_sessions_use() {
    let (head, _dir) = head().await;
    let (st, api) = (&head.state, Api::of(&head));
    use reqwest::Method as M;
    let mac = worker(st, "mac", &[]).await;
    let (main, next, other) =
        (connection(&api, "main", &[]).await, connection(&api, "next", &[]).await, connection(&api, "other", &[]).await);
    let s = session(&api, mac, &main, "home").await;
    let older = async |c: &str| ask(st, mac, "", c).await;
    assert_eq!(older(&main).await.bearer, "key-main");
    // A model switch it hasn't reported yet.
    api.ok(M::POST, &format!("sessions/{s}/messages"), json!({"text": "go", "connection_id": next, "model": "m"})).await;
    assert_eq!(older(&next).await.bearer, "key-next");
    // One its subagents may run on, Claude Code too.
    assert_eq!(older(&other).await.bearer, "key-other");
    assert_eq!(older(&claude_code(&api, "cc").await).await.error, "");
    // Its sessions' secrets, asked for as such workers ask.
    api.ok(M::PUT, "settings/secrets/DEPLOY_KEY", json!({"value": "dk-1"})).await;
    assert_eq!(older(&format!("secret:DEPLOY_KEY@{s}")).await.bearer, "dk-1");
    head.shutdown().await.unwrap();
}

/// What the head sends a worker.
type Inbox = tokio::sync::mpsc::Receiver<Result<c6_proto::HeadMsg, tonic::Status>>;

/// A live connection for a worker in the head's registry: the guard that
/// takes its answers, and what the head sends it.
fn attach<'a>(st: &'a HeadState, id: WorkerId, since: i64, caps: &[&str]) -> (c6_head::registry::ConnGuard<'a>, Inbox) {
    let (tx, rx) = tokio::sync::mpsc::channel(16);
    (st.registry.attach(id, since, tx, caps.iter().map(|c| c.to_string()).collect(), "worker"), rx)
}

async fn worker_of(st: &HeadState, session: &Value) -> String {
    st.store.session(session.as_str().unwrap()).await.unwrap().unwrap().worker_name
}

#[tokio::test]
async fn templates_from_a_session_run_on_its_worker() {
    let (head, _dir) = head().await;
    let (st, api) = (&head.state, Api::of(&head));
    use reqwest::Method as M;
    api.ok(M::POST, "realms", json!({"id": "mach5-agents"})).await;
    let (a, b) = (worker(st, "a", &[]).await, worker(st, "b", &[]).await);
    let (ga, _ra) = attach(st, a, 1, &[]);
    let (gb, _rb) = attach(st, b, 2, &[]);
    let m = connection(&api, "m", &[]).await;
    api.ok(
        M::PUT,
        "settings/templates",
        json!([{"name": "anywhere", "workers": ["*"], "model": "m", "prompt": "x"},
               {"name": "on-b", "workers": ["b"], "model": "m", "prompt": "x"}]),
    )
    .await;
    let caller = session(&api, a, &m, "home").await;
    let start =
        |template: &str| c6_head::fabric::handle(st, a, "session.template_start", json!({"session_id": caller, "template": template}));

    // Every worker, from a session on a: only a.
    let run = start("anywhere").await.unwrap();
    let started = run["sessions"].as_array().unwrap();
    assert_eq!(started.len(), 1, "{run}");
    assert_eq!(worker_of(st, &started[0]).await, "a");
    // Naming b: refused, until you grant it.
    let e = start("on-b").await.unwrap_err().to_string();
    assert!(e.contains("grant_request"), "{e}");
    let g = c6_api::GrantIn {
        from_realm: "home".into(),
        to_realm: "home".into(),
        kind: "workers".into(),
        mode: "start".into(),
        scope: json!({"template": "on-b"}),
        ..Default::default()
    };
    c6_head::grants::create(st, &g, "you", "active").await.unwrap();
    let run = start("on-b").await.unwrap();
    assert_eq!(worker_of(st, &run["sessions"][0]).await, "b");

    // Any run: only workers that serve its realm.
    api.ok(M::PUT, &format!("workers/{}/realms", b.0), json!({"realms": ["mach5-agents"]})).await;
    let run = c6_head::templates::run(st, "anywhere", "", &[], "home").await.unwrap();
    assert_eq!(run.sessions.len(), 1, "{:?}", run.errors);
    assert_eq!(run.sessions[0].worker_id, a.0);
    let e = c6_head::templates::run(st, "on-b", "", &[], "home").await.unwrap_err().to_string();
    assert!(e.contains("not available to realm home"), "{e}");
    drop((ga, gb));
    head.shutdown().await.unwrap();
}

#[tokio::test]
async fn imports_come_only_from_enabled_workers_into_their_realms() {
    let (head, _dir) = head().await;
    let (st, api) = (&head.state, Api::of(&head));
    use reqwest::Method as M;
    api.ok(M::POST, "realms", json!({"id": "work"})).await;
    let (a, b) = (worker(st, "a", &["work"]).await, worker(st, "b", &[]).await);
    api.ok(M::PUT, "settings/session_imports", json!({"workers": ["a"], "rules": [{"folder": "~/work/**", "realm": "work"}]})).await;
    let import = |from: WorkerId, cwd: &str| {
        let id = uuid::Uuid::now_v7().to_string();
        let p = json!({"source": "claude_code", "id": id, "meta": {"cwd": cwd, "home": "/home/me"}, "events": []});
        async move { c6_head::imports::append(st, from, &p).await.map(|_| id).map_err(|e| e.to_string()) }
    };

    let e = import(b, "/home/me/work/app").await.unwrap_err();
    assert!(e.contains("imports are off for b"), "{e}");
    // Home's folder, from a worker kept to work.
    let e = import(a, "/home/me/notes").await.unwrap_err();
    assert!(e.contains("not available to realm home"), "{e}");
    let id = import(a, "/home/me/work/app").await.unwrap();
    assert_eq!(st.store.session(&id).await.unwrap().unwrap().realm, "work");
    head.shutdown().await.unwrap();
}

/// The worker a kit job went to: its RPC, answered as a finished run.
async fn ran_on(workers: &mut [(&str, &c6_head::registry::ConnGuard<'_>, &mut Inbox)]) -> String {
    c6_testkit::eventually("a kit job", async || {
        for (name, guard, rx) in workers.iter_mut() {
            if let Ok(Ok(c6_proto::HeadMsg { msg: Some(head_msg::Msg::RpcRequest(r)) })) = rx.try_recv() {
                assert_eq!(r.method, "kit.job");
                guard.rpc_response(c6_proto::RpcResponse { id: r.id, result_json: br#"{"exit": 0}"#.to_vec(), error: String::new() });
                return Some(name.to_string());
            }
        }
        None
    })
    .await
}

#[tokio::test]
async fn kit_jobs_run_only_where_their_realm_is_served() {
    let (head, _dir) = head().await;
    let st = &head.state;
    let api = Api::of(&head);
    api.ok(reqwest::Method::POST, "realms", json!({"id": "mach5-agents"})).await;
    let (vm, mac) = (worker(st, "vm", &["mach5-agents"]).await, worker(st, "mac", &[]).await);
    // The VM connected first.
    let (gvm, mut rvm) = attach(st, vm, 1, &["kits"]);
    let (gmac, mut rmac) = attach(st, mac, 2, &["kits"]);
    let job = |realm: &str, worker: Option<WorkerId>| c6_head::kitjobs::Job {
        spec: json!({"name": "sync", "tools": [{"name": "run", "command": ["true"]}]}),
        tool: "run".into(),
        realm: realm.into(),
        worker,
        ..Default::default()
    };
    let run = |j| {
        let st = head.state.clone();
        tokio::spawn(async move { c6_head::kitjobs::run(&st, j).await.map(|o| o.exit).map_err(|e| e.to_string()) })
    };

    // Home's job: never the VM, though it connected first.
    let r = run(job("home", None));
    assert_eq!(ran_on(&mut [("vm", &gvm, &mut rvm), ("mac", &gmac, &mut rmac)]).await, "mac");
    assert_eq!(r.await.unwrap(), Ok(Some(0)));
    // mach5-agents's: the worker kept to it first.
    let r = run(job("mach5-agents", None));
    assert_eq!(ran_on(&mut [("vm", &gvm, &mut rvm), ("mac", &gmac, &mut rmac)]).await, "vm");
    r.await.unwrap().unwrap();
    // Asked for by name, it must serve the realm too.
    let e = run(job("home", Some(vm))).await.unwrap().unwrap_err();
    assert!(e.contains("not available to realm home"), "{e}");
    // None that serves it: it fails, and goes nowhere else.
    drop(gmac);
    let e = run(job("home", None)).await.unwrap().unwrap_err();
    assert!(e.contains("no worker available to realm home"), "{e}");
    assert!(rvm.try_recv().is_err());
    drop(gvm);
    head.shutdown().await.unwrap();
}

/// A realm's knowledge scopes reach its sessions' memex calls, whichever
/// realm the call comes from: a worker's call carries none, and a message
/// sent from another realm's page carries that one.
#[tokio::test]
async fn a_realms_knowledge_scopes_reach_its_sessions() {
    let (head, _dir) = head().await;
    let (st, api) = (&head.state, Api::of(&head));
    use reqwest::Method as M;
    api.ok(M::POST, "realms", json!({"id": "work"})).await;
    let w = worker(st, "w", &[]).await;
    let c = connection(&api, "c", &[]).await;
    let (theirs, mine) = (session(&api, w, &c, "work").await, session(&api, w, &c, "home").await);
    let scope = json!([{"name": "Chat for all", "applies_to": [], "enabled": true, "kinds": ["chat"]}]);
    c6_head::lists::write(st.store.pool(), c6_head::memex::SCOPES, "work", scope.as_array().unwrap(), "you").await.unwrap();
    let scope_of = async |id: &str| {
        let row = st.store.session(id).await.unwrap().unwrap();
        c6_head::fabric::session_scope(st, &row).await.unwrap()
    };
    assert!(scope_of(&theirs).await.contains(&"chat".to_owned()), "its realm's scope, from a worker's call");
    let from_home = c6_head::store::SETTINGS_REALM.scope("home".into(), scope_of(&theirs)).await;
    assert!(from_home.contains(&"chat".to_owned()), "and from another realm's page");
    assert!(!scope_of(&mine).await.contains(&"chat".to_owned()), "not another realm's");
    head.shutdown().await.unwrap();
}

/// An approved app kit, without building an image or running a container.
async fn secret_kit(api: &Api) {
    use base64::Engine;
    use reqwest::Method as M;
    api.ok(
        M::POST,
        "packages",
        json!({"manifest": {"package": "scoped-kit", "version": "1.0.0",
            "permissions": {"secrets": ["KIT_CREDENTIAL"]},
            "kit": {"tools": [{"name": "run", "command": ["true"]}]}},
            "files": {"Dockerfile": base64::engine::general_purpose::STANDARD.encode("FROM scratch\n")}}),
    )
    .await;
    api.ok(
        M::POST,
        "packages",
        json!({"manifest": {"package": "scoped-app", "version": "1.0.0", "requires": {"scoped-kit": "^1"},
            "operations": [{"name": "run", "kit": {"kit": "scoped-kit", "tool": "run"}}]}}),
    )
    .await;
    let install = api.ok(M::POST, "installs", json!({"package": "scoped-app"})).await;
    api.ok(M::POST, &format!("decisions/{}/answer", install["decision"]), json!({"option": "Install"})).await;
}

async fn run_secret_kit(st: &HeadState) -> anyhow::Result<Value> {
    c6_head::kitjobs::for_instance(st, "home", "scoped-app", "scoped-kit", "run", Default::default(), Default::default(), "test", &[], None)
        .await
}

#[tokio::test]
async fn kit_jobs_refuse_worker_scoped_secrets_on_the_head() {
    let (head, _dir) = head().await;
    let st = &head.state;
    secret_kit(&Api::of(&head)).await;
    let value = uuid::Uuid::now_v7().to_string();
    st.store.put_secret("root", "KIT_CREDENTIAL", Some(st.secrets.encrypt(&value).unwrap()), "", &["worker:allowed".into()]).await.unwrap();
    let called = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let seen = called.clone();
    let delivered = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let received = delivered.clone();
    c6_head::kitjobs::run_locally_in(
        &st.data_dir,
        "scoped-kit",
        std::sync::Arc::new(move |run| {
            seen.store(true, std::sync::atomic::Ordering::SeqCst);
            received.store(run.env.iter().any(|(name, _)| name == "KIT_CREDENTIAL"), std::sync::atomic::Ordering::SeqCst);
            Box::pin(async { Ok(c6_head::kitjobs::Output { exit: Some(0), ..Default::default() }) })
        }),
    );
    let result = run_secret_kit(st).await;
    assert!(!delivered.load(std::sync::atomic::Ordering::SeqCst), "a worker-scoped credential reached the head's kit runner");
    assert!(result.is_err(), "a worker-scoped secret must not reach a kit on the head");
    assert!(result.unwrap_err().to_string().contains("not for this worker"));
    assert!(!called.load(std::sync::atomic::Ordering::SeqCst), "refuse before invoking the kit");
    st.store.put_secret("root", "KIT_CREDENTIAL", None, "", &[]).await.unwrap();
    assert!(run_secret_kit(st).await.is_ok(), "unscoped secrets still work in a local kit");
    assert!(called.load(std::sync::atomic::Ordering::SeqCst));
    assert!(delivered.load(std::sync::atomic::Ordering::SeqCst));
    head.shutdown().await.unwrap();
}

#[tokio::test]
async fn kit_jobs_apply_secret_scopes_to_the_selected_worker() {
    let (head, _dir) = head().await;
    let st = &head.state;
    secret_kit(&Api::of(&head)).await;
    let (allowed, other) = (worker(st, "allowed", &[]).await, worker(st, "other", &[]).await);
    // The default target is the oldest connected worker, not the scoped one.
    let (guard, mut inbox) = attach(st, other, 1, &["kits"]);
    let (allowed_guard, mut allowed_inbox) = attach(st, allowed, 2, &["kits"]);
    let value = uuid::Uuid::now_v7().to_string();
    for (realm, scopes, permitted) in [
        ("root", vec!["worker:allowed"], false),
        ("root", vec!["worker:allowed", "worker:other"], true),
        ("home", vec!["worker:allowed"], false),
        ("home", vec!["worker:other"], true),
        ("home", vec!["app:scoped-app"], true),
        ("home", vec![], true),
    ] {
        let scopes = scopes.into_iter().map(str::to_owned).collect::<Vec<_>>();
        st.store.put_secret(realm, "KIT_CREDENTIAL", Some(st.secrets.encrypt(&value).unwrap()), "", &scopes).await.unwrap();
        let run = run_secret_kit(st);
        tokio::pin!(run);
        let result = c6_testkit::within("the kit's scope check or RPC", async {
            tokio::select! {
                result = &mut run => result,
                message = inbox.recv() => {
                    let Some(head_msg::Msg::RpcRequest(request)) = message.unwrap().unwrap().msg else {
                        panic!("expected a kit RPC");
                    };
                    assert_eq!(request.method, "kit.job");
                    let params: Value = serde_json::from_slice(&request.params_json).unwrap();
                    // Never print the environment or credential, even on a failure.
                    assert!(params["env"].as_array().unwrap().iter().any(|e| e[0] == "KIT_CREDENTIAL" && e[1].as_str() == Some(&value)));
                    guard.rpc_response(c6_proto::RpcResponse {
                        id: request.id, result_json: br#"{"exit": 0}"#.to_vec(), error: String::new(),
                    });
                    run.await
                }
            }
        })
        .await;
        assert_eq!(result.is_ok(), permitted, "kit delivery must obey worker scopes");
        if !permitted {
            assert!(result.unwrap_err().to_string().contains("not for this worker"));
        }
        assert!(allowed_inbox.try_recv().is_err(), "a refused job must not be moved to another worker");
    }
    drop((guard, allowed_guard));
    head.shutdown().await.unwrap();
}
