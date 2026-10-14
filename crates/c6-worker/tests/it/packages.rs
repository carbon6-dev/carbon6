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
    let add = |manifest: Value, files: Value| {
        async move { send(M::POST, "packages", json!({"manifest": manifest, "files": files})).await }
    };
    let decide = |id: i64, option: &'static str| {
        async move {
            let r = send(M::POST, &format!("decisions/{id}/answer"), json!({"option": option})).await;
            assert!(r.status().is_success(), "{}", r.text().await.unwrap());
        }
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

    // The plan: everything, deepest first, with what each asks for.
    let plan = ok(send(M::POST, "installs/plan", json!({"package": "factory"})).await).await;
    let text = plan["plan"].as_str().unwrap();
    for want in [
        "**py-base** 1.0.0 (kit)",
        "**render** 1.0.0 (kit)",
        "**decks** 1.0.0 (app) into a new realm **decks**",
        "**style** 1.0.0 (library) into realm **factory**",
        "Install **factory**",
        "No network. Up to 1 CPUs and 2.0 GB of memory, 5 minutes a run.",
        "GITHUB_TOKEN",
        "May start decks's templates: deck",
    ] {
        assert!(text.contains(want), "{want} in {text}");
    }
    assert!(text.find("py-base").unwrap() < text.find("**render**").unwrap(), "requirements first");

    // One decision installs them all.
    let i: Install = serde_json::from_value(ok(send(M::POST, "installs", json!({"package": "factory"})).await).await).unwrap();
    let d: Decision = send(M::GET, &format!("decisions/{}", i.decision.unwrap()), json!({})).await.json().await.unwrap();
    assert_eq!(d.title, "Install the app factory?");
    decide(d.id, "Install").await;
    let all: Vec<Install> = send(M::GET, "installs", json!({})).await.json().await.unwrap();
    let by = |n: &str| all.iter().find(|i| i.package == n).unwrap().clone();
    assert!(all.iter().all(|i| i.state == "installed"), "{all:?}");
    assert_eq!((by("render").realm.as_str(), by("style").realm.as_str(), by("decks").realm.as_str()), ("", "factory", "decks"));
    assert_eq!(by("render").required_by, vec![by("decks").id]);
    assert_eq!(by("py-base").required_by, vec![by("render").id]);
    assert_eq!(by("factory").locks.len(), 2);
    let skills = ok(send(M::GET, "settings/skills?realm=factory", json!({})).await).await;
    assert!(skills.to_string().contains("house-style"), "the library joined factory: {skills}");
    let grants = ok(send(M::GET, "grants", json!({})).await).await;
    assert!(
        grants.as_array().unwrap().iter().any(|g| g["from_realm"] == "decks"
            && g["to_realm"] == "factory"
            && g["scope"]["methods"][0] == "template:deck"
            && g["state"] == "active"),
        "{grants}"
    );

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
    assert!(r.text().await.unwrap().contains("required by decks in decks"));
    assert_eq!(send(M::DELETE, "packages/render/1.0.0", json!({})).await.status(), StatusCode::BAD_REQUEST);

    // An upgrade asks again only for what grew.
    ok(add(json!({"package": "render", "version": "1.1.0", "requires": {"py-base": "^1"}, "permissions": {"memory_mb": 2048, "timeout_s": 300, "network": "any"},
        "kit": {"build": {"files": ["r.sh"]}, "tools": [{"name": "pdf", "command": ["sh", "/opt/r.sh", "{html}"], "args": {"html": "path"}}]}}),
        json!({"Dockerfile": b64("FROM kit:py-base\nCOPY r.sh /opt/r.sh\n"), "r.sh": b64("echo hi v2\n")})).await).await;
    let up: Install =
        serde_json::from_value(ok(send(M::POST, &format!("installs/{}/upgrade", by("render").id), json!({})).await).await).unwrap();
    let d: Decision = send(M::GET, &format!("decisions/{}", up.decision.unwrap()), json!({})).await.json().await.unwrap();
    assert!(d.document.contains("It asks for more than before: network access"), "{}", d.document);
    decide(d.id, "Upgrade, keeping my edits").await;
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
