//! Updating from a signed GitHub release (docs/updates.md): the head finds
//! it, asks, updates itself, and once it runs the new version its workers
//! update and say so.

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Path, State};
use c6_head::HeadConfig;
use c6_testkit::eventually;
use c6_worker::{ConnState, Identity, Worker, conn, enroll};
use ring::signature::{Ed25519KeyPair, KeyPair};
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

const V: &str = "9.9.9";

fn tgz(files: &[(&str, &str)]) -> Vec<u8> {
    let d = tempfile::tempdir().unwrap();
    for (name, body) in files {
        std::fs::write(d.path().join(name), body).unwrap();
    }
    let names: Vec<&str> = files.iter().map(|(n, _)| *n).collect();
    std::process::Command::new("tar").args(["-czf", "-", "-C"]).arg(d.path()).args(names).output().unwrap().stdout
}

/// A fake GitHub: the latest release, and its files.
async fn github(files: HashMap<String, Vec<u8>>) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let files = Arc::new(files);
    let names: Vec<String> = files.keys().cloned().collect();
    let latest = json!({
        "tag_name": format!("v{V}"), "draft": false, "prerelease": false,
        "assets": names.iter().map(|n| json!({"name": n, "browser_download_url": format!("{base}/dl/{n}")})).collect::<Vec<_>>(),
    });
    let app = axum::Router::new()
        .route("/repos/c6/releases/latest", axum::routing::get(move || async move { axum::Json(latest) }))
        .route(
            "/dl/{name}",
            axum::routing::get(|State(f): State<Arc<HashMap<String, Vec<u8>>>>, Path(n): Path<String>| async move {
                f.get(&n).cloned().ok_or(axum::http::StatusCode::NOT_FOUND)
            }),
        )
        .with_state(files);
    tokio::spawn(async move { axum::serve(listener, app).await });
    format!("{base}/repos/c6")
}

#[tokio::test]
async fn the_head_then_its_workers_update_to_a_signed_release() {
    // The release: c6h and c6w for every platform, signed with a test key.
    let doc = Ed25519KeyPair::generate_pkcs8(&ring::rand::SystemRandom::new()).unwrap();
    let key = Ed25519KeyPair::from_pkcs8(doc.as_ref()).unwrap();
    let public: String = key.public_key().as_ref().iter().map(|b| format!("{b:02x}")).collect();
    let mut files: HashMap<String, Vec<u8>> = HashMap::new();
    for arch in ["arm64", "x86_64"] {
        files.insert(format!("c6h-{V}-darwin-{arch}.tar.gz"), tgz(&[("c6h", "new c6h")]));
        files.insert(format!("c6w-{V}-darwin-{arch}.tar.gz"), tgz(&[("c6w", "new c6w")]));
    }
    for arch in ["amd64", "arm64"] {
        files.insert(format!("carbon6-{V}-linux-{arch}.tar.gz"), tgz(&[("c6h", "new c6h"), ("c6w", "new c6w")]));
    }
    let assets: serde_json::Map<String, Value> = files.iter().map(|(n, b)| (n.clone(), json!(c6_core::update::checksum(b)))).collect();
    let manifest = serde_json::to_vec(&json!({"version": V, "date": "2026-10-17", "notes": "- Faster.", "assets": assets})).unwrap();
    files.insert("release.json.sig".into(), key.sign(&manifest).as_ref().to_vec());
    files.insert("release.json".into(), manifest);
    let source = github(files).await;

    // The binaries being updated (fakes, not this test).
    let bin = tempfile::tempdir().unwrap();
    let (c6h, c6w) = (bin.path().join("c6h"), bin.path().join("c6w"));
    std::fs::write(&c6h, "old c6h").unwrap();
    std::fs::write(&c6w, "old c6w").unwrap();

    let (head_dir, state) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let cfg = |runs: &str, (ui_port, worker_port): (u16, u16)| HeadConfig {
        data_dir: head_dir.path().to_owned(),
        bind: vec!["127.0.0.1".parse().unwrap()],
        ui_port,
        worker_port,
        auth: false,
        update_source: source.clone(),
        update_keys: vec![public.clone()],
        update_exe: Some(c6h.clone()),
        update_as_version: Some(runs.to_owned()),
        ..Default::default()
    };
    // Ports it gets back when it restarts (its worker knows them).
    let mut head = c6_head::start(cfg("9.9.8", (c6_testkit::port(), c6_testkit::port()))).await.unwrap();
    let ports = (head.ui_addrs[0].port(), head.worker_addrs[0].port());
    let api = format!("http://127.0.0.1:{}/api/v1", ports.0);
    let http = reqwest::Client::new();
    let get = async |path: &str| -> Value { http.get(format!("{api}/{path}")).send().await.unwrap().json().await.unwrap() };
    let post = async |path: &str, body: Value| -> Value {
        let r = http.post(format!("{api}/{path}")).json(&body).send().await.unwrap();
        let s = r.status();
        let v: Value = r.json().await.unwrap_or(Value::Null);
        assert!(s.is_success(), "{path}: {s}: {v}");
        v
    };

    let e = c6_head::api::new_enrollment(&head.state, Some("laptop")).await.unwrap();
    enroll::enroll(&e.token).await.unwrap().save(state.path()).unwrap();
    let worker = Worker::open(state.path(), Identity::load(state.path()).unwrap()).await.unwrap();
    let overrides = c6_worker::Overrides { update_exe: Some(c6w.clone()), update_keys: Some(vec![public.clone()]), ..Default::default() };
    *worker.overrides.lock().unwrap() = overrides.clone();
    let (tx, mut rx) = watch::channel(ConnState::Connecting);
    let run = tokio::spawn(conn::run(worker.clone(), tx, CancellationToken::new()));
    c6_testkit::within("the worker to attach", rx.wait_for(|s| matches!(s, ConnState::Attached { .. }))).await.unwrap();

    // Found, verified, and asked about (the default: approve).
    let s = post("system/updates/check", json!({})).await;
    assert_eq!(s["check_error"], "", "{s}");
    assert_eq!((s["releases"][0]["version"].as_str(), s["releases"][0]["state"].as_str()), (Some(V), Some("available")), "{s}");
    let decisions = get("decisions").await;
    let d = decisions.as_array().unwrap().iter().find(|d| d["title"] == format!("Update carbon6 to {V}?")).expect("a decision").clone();
    assert!(d["document"].as_str().unwrap().contains("Faster."));
    post(&format!("decisions/{}/answer", d["id"]), json!({"option": "Update now"})).await;

    // The head first: its binary is replaced, and it exits to restart.
    eventually("the head to restart", async || head.state.updates.restart_requested().then_some(())).await;
    head.wait().await.unwrap();
    assert_eq!(std::fs::read_to_string(&c6h).unwrap(), "new c6h");
    assert_eq!(std::fs::read_to_string(bin.path().join("c6h.previous")).unwrap(), "old c6h");
    drop(head);

    // The new version starts: done; then the worker updates.
    let head = crate::restart_head(cfg(V, ports)).await;
    let s = get("system/updates").await;
    assert_eq!(s["machines"][0]["state"], "done", "{s}");
    run.await.unwrap().unwrap();
    assert!(worker.update.restart_requested());
    assert_eq!(std::fs::read_to_string(&c6w).unwrap(), "new c6w");
    let s = get("system/updates").await;
    assert_eq!((s["machines"][1]["state"].as_str(), s["machines"][1]["method"].as_str()), (Some("restarting"), Some("tarball")), "{s}");
    drop(worker);

    // The new worker attaches, and says the update is done.
    let worker = Worker::open(state.path(), Identity::load(state.path()).unwrap()).await.unwrap();
    // The new version, as it says.
    *worker.overrides.lock().unwrap() = c6_worker::Overrides { update_as_version: Some(V.into()), ..overrides };
    let (tx, _rx) = watch::channel(ConnState::Connecting);
    let stop = CancellationToken::new();
    tokio::spawn(conn::run(worker.clone(), tx, stop.clone()));
    let s = eventually("the update to be done", async || {
        let s = get("system/updates").await;
        (s["releases"][0]["state"] == "done").then_some(s)
    })
    .await;
    assert_eq!(s["machines"][1]["state"], "done", "{s}");

    stop.cancel();
    head.shutdown().await.unwrap();
}

/// Release files a fake GitHub serves, by path.
type Files = Arc<std::sync::Mutex<HashMap<String, Vec<u8>>>>;

/// The edge channel (docs/updates.md): a machine on it is offered the
/// rolling `edge` pre-release (each green commit on main) when it is newer
/// than what it runs; a machine on stable never is.
#[tokio::test]
async fn the_edge_channel_offers_builds_of_main() {
    let doc = Ed25519KeyPair::generate_pkcs8(&ring::rand::SystemRandom::new()).unwrap();
    let key = Ed25519KeyPair::from_pkcs8(doc.as_ref()).unwrap();
    let public: String = key.public_key().as_ref().iter().map(|b| format!("{b:02x}")).collect();
    let edge = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let signed = |version: &str| {
        let m = serde_json::to_vec(&json!({"version": version, "notes": "- Faster tiles.", "assets": {}})).unwrap();
        let sig = key.sign(&m).as_ref().to_vec();
        (m, sig)
    };
    // A fake GitHub: the latest release is 9.9.8; `edge` is whatever we put there.
    let files: Files = Arc::default();
    let (stable_m, stable_s) = signed("9.9.8");
    files.lock().unwrap().insert("stable/release.json".into(), stable_m);
    files.lock().unwrap().insert("stable/release.json.sig".into(), stable_s);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let (b1, b2, e2) = (base.clone(), base.clone(), edge.clone());
    let app = axum::Router::new()
        .route(
            "/repos/c6/releases/latest",
            axum::routing::get(move || {
                let b = b1.clone();
                async move {
                    axum::Json(json!({"tag_name": "v9.9.8", "draft": false, "prerelease": false, "assets": [
                        {"name": "release.json", "browser_download_url": format!("{b}/dl/stable/release.json")},
                        {"name": "release.json.sig", "browser_download_url": format!("{b}/dl/stable/release.json.sig")}]}))
                }
            }),
        )
        .route(
            "/repos/c6/releases/tags/edge",
            axum::routing::get(move || {
                let (b, e) = (b2.clone(), e2.lock().unwrap().clone());
                async move {
                    if e.is_empty() {
                        return Err(axum::http::StatusCode::NOT_FOUND);
                    }
                    Ok(axum::Json(json!({"tag_name": "edge", "draft": false, "prerelease": true, "assets": [
                        {"name": "release.json", "browser_download_url": format!("{b}/dl/{e}/release.json")},
                        {"name": "release.json.sig", "browser_download_url": format!("{b}/dl/{e}/release.json.sig")}]})))
                }
            }),
        )
        .route(
            "/dl/{dir}/{name}",
            axum::routing::get(|State(f): State<Files>, Path((d, n)): Path<(String, String)>| async move {
                f.lock().unwrap().get(&format!("{d}/{n}")).cloned().ok_or(axum::http::StatusCode::NOT_FOUND)
            }),
        )
        .with_state(files.clone());
    tokio::spawn(async move { axum::serve(listener, app).await });
    let publish_edge = |v: &str| {
        let (m, s) = signed(v);
        let dir = v.replace('+', "_");
        files.lock().unwrap().insert(format!("{dir}/release.json"), m);
        files.lock().unwrap().insert(format!("{dir}/release.json.sig"), s);
        *edge.lock().unwrap() = dir;
    };

    let head_dir = tempfile::tempdir().unwrap();
    let head = c6_head::start(HeadConfig {
        data_dir: head_dir.path().to_owned(),
        bind: vec!["127.0.0.1".parse().unwrap()],
        ui_port: 0,
        worker_port: 0,
        auth: false,
        update_source: format!("{base}/repos/c6"),
        update_keys: vec![public],
        update_as_version: Some("9.9.8".into()),
        ..Default::default()
    })
    .await
    .unwrap();
    let api = format!("http://127.0.0.1:{}/api/v1", head.ui_addrs[0].port());
    let http = reqwest::Client::new();
    let check = async || -> Value { http.post(format!("{api}/system/updates/check")).send().await.unwrap().json().await.unwrap() };
    let versions =
        |s: &Value| s["releases"].as_array().unwrap().iter().map(|r| r["version"].as_str().unwrap().to_owned()).collect::<Vec<_>>();

    // On stable (the default), an edge build of main is never offered.
    publish_edge("9.9.8+git20261023120000.aaaaaaa");
    let s = check().await;
    assert_eq!(s["check_error"], "", "{s}");
    assert!(versions(&s).is_empty(), "{s}");

    // On edge, it is: newer than 9.9.8, and asked about.
    let r = http.put(format!("{api}/settings/updates")).json(&json!({"channel": "edge"})).send().await.unwrap();
    assert!(r.status().is_success());
    let s = check().await;
    assert_eq!(s["check_error"], "", "{s}");
    assert_eq!(versions(&s), vec!["9.9.8+git20261023120000.aaaaaaa"], "{s}");
    let decisions: Vec<Value> = http.get(format!("{api}/decisions")).send().await.unwrap().json().await.unwrap();
    assert!(decisions.iter().any(|d| d["title"] == "Update carbon6 to 9.9.8+git20261023120000.aaaaaaa?"), "{decisions:?}");

    // A later commit's build replaces it (its question goes, the new one asks).
    publish_edge("9.9.8+git20261023130000.bbbbbbb");
    let s = check().await;
    let rel = |v: &str| s["releases"].as_array().unwrap().iter().find(|r| r["version"] == v).cloned().unwrap();
    assert_eq!(rel("9.9.8+git20261023130000.bbbbbbb")["state"], "available", "{s}");
    assert_eq!(rel("9.9.8+git20261023120000.aaaaaaa")["state"], "skipped", "{s}");

    head.shutdown().await.unwrap();
}
