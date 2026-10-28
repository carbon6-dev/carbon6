//! What a worker is given (docs/os.md §3.2): credentials only for its own
//! live sessions, as their realms allow; template runs from a session on
//! that session's worker; imports and kit jobs only where the realm is
//! served. The workers here are the head's records (and, for runs, live
//! connections in its registry), not running processes.

use c6_core::WorkerId;
use c6_head::{HeadConfig, HeadState};
use c6_proto::{CredentialGrant, CredentialRequest};
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
    // A connection the session doesn't use.
    assert!(ask(st, a, &mine, &shared).await.error.contains("does not use"));
    // A mach5-agents session asking for a home-only connection: it can't
    // switch to it, nor have it.
    let switch = json!({"text": "go", "connection_id": home_only, "model": "m"});
    let e = api.send(M::POST, &format!("sessions/{agents}/messages"), switch).await.unwrap_err();
    assert!(e.contains("not available to realm mach5-agents"), "{e}");
    assert!(!ask(st, a, &agents, &home_only).await.error.is_empty());
    // A model switch the worker hasn't reported yet: the new connection.
    api.ok(M::POST, &format!("sessions/{mine}/messages"), json!({"text": "go", "connection_id": shared, "model": "m"})).await;
    assert_eq!(ask(st, a, &mine, &shared).await.error, "");
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
    head.shutdown().await.unwrap();
}

/// A worker older than the head (the head is updated first) says no session
/// when it asks: serving every realm, it still gets what its live sessions
/// use (a session's connection, a model switch's, their secrets as
/// name@session), and nothing they don't.
#[tokio::test]
async fn an_older_worker_still_gets_what_its_sessions_use() {
    let (head, _dir) = head().await;
    let (st, api) = (&head.state, Api::of(&head));
    use reqwest::Method as M;
    let mac = worker(st, "mac", &[]).await;
    let (main, next, unused) =
        (connection(&api, "main", &[]).await, connection(&api, "next", &[]).await, connection(&api, "unused", &[]).await);
    let s = session(&api, mac, &main, "home").await;
    let older = async |c: &str| ask(st, mac, "", c).await;
    assert_eq!(older(&main).await.bearer, "key-main");
    // A model switch it hasn't reported yet.
    api.ok(M::POST, &format!("sessions/{s}/messages"), json!({"text": "go", "connection_id": next, "model": "m"})).await;
    assert_eq!(older(&next).await.bearer, "key-next");
    assert!(older(&unused).await.error.contains("no session on this worker may use"));
    // Its sessions' secrets, asked for as such workers ask.
    api.ok(M::PUT, "settings/secrets/DEPLOY_KEY", json!({"value": "dk-1"})).await;
    assert_eq!(older(&format!("secret:DEPLOY_KEY@{s}")).await.bearer, "dk-1");
    head.shutdown().await.unwrap();
}
