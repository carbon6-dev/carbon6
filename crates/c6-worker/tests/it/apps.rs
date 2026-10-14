//! Apps (docs/os.md §5): a package installs into a realm of its own after
//! your decision, asks for its grants and tells you its secrets; it
//! exports back; an upgrade keeps your edits and lets you settle
//! conflicts; removing deletes the realm and everything in it.

use c6_api::{AppInstall, AppPackage, AppPlan, Decision};
use c6_head::HeadConfig;
use c6_testkit::MockLlm;
use reqwest::StatusCode;
use serde_json::{Value, json};

#[tokio::test]
async fn apps_install_export_upgrade_remove() {
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
    let conn = ok(send(M::POST, "connections", json!({"name": "m", "base_url": mock.base_url()})).await).await;
    let decide = |id: i64, option: &'static str| async move {
        let r = send(M::POST, &format!("decisions/{id}/answer"), json!({"option": option})).await;
        assert!(r.status().is_success(), "{}", r.text().await.unwrap());
    };

    let package = json!({
        "app": "crm", "version": "1.0.0", "description": "Keeps relationships warm",
        "needs": {"secrets": ["CRM_KEY"], "workers": "any", "grants": [{"from_realm": "home", "kind": "memex", "mode": "read", "scope": {"kinds": ["email"]}}]},
        "contents": {
            "collections": [{"name": "contacts", "description": "People", "schema": {"type": "object", "required": ["name"], "properties": {
                "name": {"type": "string"}, "every_days": {"type": "integer"}}}, "keys": ["name"]}],
            "series": [{"name": "touchpoints", "unit": "", "dimensions": ["contact"]}],
            "records": {"contacts": [{"name": "Ada", "every_days": 30}, {"name": "Linus", "every_days": 90}]},
            "views": [{"name": "people", "title": "People", "kind": "table", "source": {"collection": "contacts", "sort": "name"}}],
            "templates": [{"name": "reach-out", "prompt": "Draft a note to {{input}}", "connection": conn["id"], "model": "m"}],
            "triggers": [{"name": "new contact", "trigger": "record", "event_params": {"collection": "contacts", "ops": ["created"]},
                          "action": "notify", "action_params": {"title": "New contact {{$.record.fields.name}}"}}],
            "memory": [{"scope": "", "text": "Contacts are people the user wants to stay in touch with."}]
        }
    });

    // Checked first.
    let bad: AppPlan =
        send(M::POST, "apps/plan", json!({"app": "x", "contents": {"channels": [{"name": "c"}]}})).await.json().await.unwrap();
    assert!(bad.problems.iter().any(|p| p.contains("channels")), "{bad:?}");
    assert_eq!(send(M::POST, "apps", json!({"app": "home"})).await.status(), StatusCode::BAD_REQUEST);
    let plan: AppPlan = send(M::POST, "apps/plan", package.clone()).await.json().await.unwrap();
    assert!(plan.problems.is_empty(), "{plan:?}");
    assert!(plan.plan.contains("1 collections") && plan.plan.contains("CRM_KEY") && plan.plan.contains("grant"), "{}", plan.plan);

    // Installing waits for you; cancelling leaves nothing.
    let other = json!({"app": "scratch", "contents": {}});
    let i: AppInstall = ok(send(M::POST, "apps", other).await)
        .await
        .as_object()
        .map(|o| serde_json::from_value(Value::Object(o.clone())).unwrap())
        .unwrap();
    decide(i.decision.unwrap(), "Cancel").await;
    let realms = ok(send(M::GET, "realms", json!({})).await).await;
    assert!(!realms.to_string().contains("scratch"));
    assert_eq!(ok(send(M::GET, "apps", json!({})).await).await, json!([]));

    let i: AppInstall = serde_json::from_value(ok(send(M::POST, "apps", package.clone()).await).await).unwrap();
    assert_eq!((i.realm.as_str(), i.state.as_str()), ("crm", "installing"));
    let d: Decision = send(M::GET, &format!("decisions/{}", i.decision.unwrap()), json!({})).await.json().await.unwrap();
    assert!(d.title.contains("Install the app crm") && d.document.contains("contacts"), "{d:?}");
    decide(d.id, "Install").await;

    // Everything is there, in realm crm.
    let apps: Vec<AppInstall> = send(M::GET, "apps", json!({})).await.json().await.unwrap();
    assert_eq!((apps[0].state.as_str(), apps[0].version.as_str()), ("installed", "1.0.0"));
    let recs = ok(send(M::GET, "collections/contacts/records?realm=crm", json!({})).await).await;
    assert_eq!(recs["total"], 2);
    let views = ok(send(M::GET, "views?realm=crm", json!({})).await).await;
    assert_eq!(views[0]["name"], "people");
    assert!(ok(send(M::GET, "series?realm=crm", json!({})).await).await.to_string().contains("touchpoints"));
    let realm = ok(send(M::GET, "realms", json!({})).await).await;
    assert!(realm.as_array().unwrap().iter().any(|r| r["id"] == "crm" && r["app"] == "crm"), "{realm}");
    let open: Vec<Decision> = send(M::GET, "decisions", json!({})).await.json().await.unwrap();
    assert!(open.iter().any(|d| d.title == "Finish setting up crm" && d.document.contains("CRM_KEY")), "{open:?}");
    assert!(open.iter().any(|d| d.title.contains("Let realm crm read home's knowledge") && d.source == "app:crm"), "{open:?}");

    // Export: the same configuration back (with data: the records too).
    let exp: AppPackage = send(M::GET, "apps/export?realm=crm", json!({})).await.json().await.unwrap();
    assert_eq!(exp.contents.collections[0].name, "contacts");
    assert_eq!(exp.contents.lists["views"][0]["name"], "people");
    assert_eq!(exp.contents.triggers[0].name.as_deref(), Some("new contact"));
    assert!(exp.contents.records.is_empty());
    assert!(exp.permissions.grants.iter().any(|g| g.from_realm == "home"));
    let with_data: AppPackage = send(M::GET, "apps/export?realm=crm&data=true", json!({})).await.json().await.unwrap();
    assert_eq!(with_data.contents.records["contacts"].len(), 2);

    // You edit the view; version 2 changes the template, the same view, and
    // adds a collection.
    ok(send(
        M::PUT,
        "settings/views?realm=crm",
        json!([{"name": "people", "title": "My people", "kind": "table", "source": {"collection": "contacts"}}]),
    )
    .await)
    .await;
    let mut v2 = package.clone();
    v2["version"] = json!("2.0.0");
    v2["contents"]["templates"][0]["prompt"] = json!("Draft a warm note to {{input}}");
    v2["contents"]["views"][0]["title"] = json!("Everyone");
    v2["contents"]["collections"]
        .as_array_mut()
        .unwrap()
        .push(json!({"name": "notes", "schema": {"type": "object", "properties": {"text": {"type": "string"}}}}));
    let up: AppInstall = serde_json::from_value(ok(send(M::POST, "apps/crm/upgrade", v2.clone()).await).await).unwrap();
    let d: Decision = send(M::GET, &format!("decisions/{}", up.decision.unwrap()), json!({})).await.json().await.unwrap();
    assert!(d.document.contains("update templates reach-out") && d.document.contains("update collection notes"), "{}", d.document);
    assert!(d.document.contains("both changed") && d.document.contains("views people"), "{}", d.document);
    assert_eq!(
        d.options.iter().map(|o| o.label.as_str()).collect::<Vec<_>>(),
        vec!["Upgrade, keeping my edits", "Upgrade, taking the new version", "Cancel"]
    );
    decide(d.id, "Upgrade, keeping my edits").await;
    let views = ok(send(M::GET, "views?realm=crm", json!({})).await).await;
    assert_eq!(views[0]["title"], "My people", "yours kept");
    let templates = ok(send(M::GET, "settings/templates?realm=crm", json!({})).await).await;
    assert_eq!(templates[0]["prompt"], "Draft a warm note to {{input}}");
    // Every realm's templates, each with its realm; one is run in its app's
    // realm without naming it.
    let all = ok(send(M::GET, "templates", json!({})).await).await;
    assert!(all.as_array().unwrap().iter().any(|t| t["realm"] == "crm" && t["name"] == "reach-out"), "{all}");
    assert_eq!(send(M::POST, "templates/nope/run", json!({})).await.status(), 404);
    assert_eq!(send(M::POST, "templates/reach-out/run", json!({"realm": "home"})).await.status(), 404, "not home's");
    let r = send(M::POST, "templates/reach-out/run", json!({"input": "Ann"})).await;
    assert_ne!(r.status(), 404, "found in crm: {}", r.text().await.unwrap());
    assert!(ok(send(M::GET, "collections?realm=crm", json!({})).await).await.to_string().contains("\"notes\""));
    let apps: Vec<AppInstall> = send(M::GET, "apps", json!({})).await.json().await.unwrap();
    assert_eq!(apps[0].version, "2.0.0");

    // Removing deletes the realm and everything in it, once you decide.
    let rm: AppInstall = serde_json::from_value(ok(send(M::DELETE, "apps/crm", json!({})).await).await).unwrap();
    assert_eq!(rm.state, "removing");
    assert!(ok(send(M::GET, "realms", json!({})).await).await.to_string().contains("\"crm\""), "not before you decide");
    decide(rm.decision.unwrap(), "Remove").await;
    assert!(!ok(send(M::GET, "realms", json!({})).await).await.to_string().contains("\"crm\""));
    assert_eq!(ok(send(M::GET, "apps", json!({})).await).await, json!([]));
    assert_eq!(send(M::GET, "collections?realm=crm", json!({})).await.status(), StatusCode::BAD_REQUEST);
    let grants = ok(send(M::GET, "grants", json!({})).await).await;
    assert!(grants.as_array().unwrap().iter().all(|g| g["state"] == "revoked"), "{grants}");

    head.shutdown().await.unwrap();
}
