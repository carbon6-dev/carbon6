//! Apps as instances (docs/apps.md): a package installs in a realm you
//! choose, under a name, as often as you like; what it owns is written
//! only by it (or by you, forced, after a decision); its code is its
//! package's, so nothing edits it, and upgrades and downgrades replace it
//! without merging; its settings fill its code; its sessions act as it and
//! read what its install approved; a fork can replace it; removing it
//! takes what it owns and leaves the realm.

use c6_api::{AppInstall, AppPackage, AppPlan, Connection, Decision, Install, Record, Session};
use c6_head::HeadConfig;
use c6_testkit::eventually;
use c6_testkit::{MockLlm, Reply};
use c6_worker::{ConnState, Identity, Worker, conn, enroll};
use reqwest::StatusCode;
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn apps_install_as_instances() {
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
    let decide = |id: i64, option: &'static str| async move {
        let r = send(M::POST, &format!("decisions/{id}/answer"), json!({"option": option})).await;
        assert!(r.status().is_success(), "{}", r.text().await.unwrap());
    };
    let decision = |id: i64| async move { send(M::GET, &format!("decisions/{id}"), json!({})).await.json::<Decision>().await.unwrap() };
    let conn: Connection =
        serde_json::from_value(ok(send(M::POST, "connections", json!({"name": "m", "base_url": mock.base_url()})).await).await).unwrap();
    // A worker, for the app's sessions.
    let e = c6_head::api::new_enrollment(&head.state, None).await.unwrap();
    enroll::enroll(&e.token).await.unwrap().save(state.path()).unwrap();
    let worker = Worker::open(state.path(), Identity::load(state.path()).unwrap()).await.unwrap();
    let (tx, mut rx) = watch::channel(ConnState::Connecting);
    let stop = CancellationToken::new();
    tokio::spawn(conn::run(worker.clone(), tx, stop.clone()));
    c6_testkit::within("the worker to attach", rx.wait_for(|s| matches!(s, ConnState::Attached { .. }))).await.unwrap();
    let wid = worker.identity.meta.worker_id.0;

    let package = json!({
        "package": "crm", "version": "1.0.0", "description": "Keeps relationships warm",
        "settings": {"type": "object", "properties": {"weeks": {"type": "integer", "minimum": 1, "default": 6}}},
        "reads": {"collections": ["people"]},
        "permissions": {"grants": [{"from_realm": "home", "kind": "memex", "mode": "read", "scope": {"kinds": ["email"]}}]},
        "contents": {
            "collections": [{"name": "contacts", "description": "People", "schema": {"type": "object", "required": ["name"], "properties": {
                "name": {"type": "string"}, "every_days": {"type": "integer"}}}, "keys": ["name"]}],
            "series": [{"name": "touchpoints", "unit": "", "dimensions": ["contact"]}],
            "records": {"contacts": [{"name": "Ada", "every_days": 30}, {"name": "Linus", "every_days": 90}]},
            "views": [{"name": "people", "title": "People", "kind": "table", "source": {"collection": "contacts", "sort": "name"},
                       "actions": [{"label": "Quarterly", "set": {"every_days": 90}}]}],
            "templates": [{"name": "reach-out", "prompt": "Every {{settings.weeks}} weeks: draft a note to {{input}}", "connection": conn.id, "model": "m"}],
            "skills": [{"name": "check-in-voice", "body": "Warm, short, every {{settings.weeks}} weeks."}],
            "triggers": [{"name": "new contact", "trigger": "record", "event_params": {"collection": "contacts", "ops": ["created"]},
                          "action": "notify", "action_params": {"title": "New contact {{$.record.fields.name}}"}}],
            "memory": [{"scope": "", "text": "Contacts are people the user wants to stay in touch with."}]
        }
    });

    // Installing never makes a realm: one that does not exist is a problem.
    let plan: AppPlan = serde_json::from_value(ok(send(M::POST, "apps/plan?realm=work", package.clone()).await).await).unwrap();
    assert!(plan.problems.iter().any(|p| p.contains("there is no realm \"work\": create it first")), "{plan:?}");
    for (id, name) in [("work", "Work"), ("biz", "Second business")] {
        ok(send(M::POST, "realms", json!({"id": id, "name": name})).await).await;
    }
    let plan: AppPlan = serde_json::from_value(ok(send(M::POST, "apps/plan?realm=work", package.clone()).await).await).unwrap();
    assert!(plan.problems.is_empty(), "{plan:?}");
    for want in ["**crm** 1.0.0 (app) in realm **work**", "Reads collections in its realm: people", "Settings you can change later: weeks"]
    {
        assert!(plan.plan.contains(want), "{want} in {}", plan.plan);
    }

    // Installed in work, after your decision.
    let i: AppInstall = serde_json::from_value(ok(send(M::POST, "apps?realm=work", package.clone()).await).await).unwrap();
    assert_eq!((i.realm.as_str(), i.state.as_str()), ("work", "installing"));
    let d = decision(i.decision.unwrap()).await;
    assert_eq!(d.title, "Install the app crm in work?");
    decide(d.id, "Install").await;
    let cols = ok(send(M::GET, "collections?realm=work", json!({})).await).await;
    assert!(cols.as_array().unwrap().iter().any(|c| c["name"] == "contacts" && c["owner"] == "crm"), "{cols}");
    let recs = ok(send(M::GET, "collections/contacts/records?realm=work", json!({})).await).await;
    assert_eq!(recs["total"], 2);
    assert_eq!(recs["records"][0]["created_by"], "app:crm");
    assert!(ok(send(M::GET, "series?realm=work", json!({})).await).await.to_string().contains("\"owner\":\"crm\""));
    // Its code is read from its package: the realm's own lists stay empty,
    // and what the realm uses comes from the app, settings filled in.
    assert_eq!(ok(send(M::GET, "settings/templates?realm=work", json!({})).await).await, json!([]));
    let templates = ok(send(M::GET, "realms/work/effective/templates", json!({})).await).await;
    assert_eq!(templates[0]["from"], "app:crm", "{templates}");
    assert_eq!(templates[0]["entry"]["prompt"], "Every 6 weeks: draft a note to {{input}}");
    assert_eq!(ok(send(M::GET, "views?realm=work", json!({})).await).await[0]["name"], "people");
    let open: Vec<Decision> = send(M::GET, "decisions", json!({})).await.json().await.unwrap();
    assert!(open.iter().any(|d| d.title.contains("Let realm work read home's knowledge") && d.source == "app:crm"), "{open:?}");

    // A second instance of the same package, in another business's realm:
    // its own data.
    let i2: Install = serde_json::from_value(ok(send(M::POST, "installs", json!({"package": "crm", "realm": "biz"})).await).await).unwrap();
    decide(i2.decision.unwrap(), "Install").await;
    let mine = ok(send(M::GET, "collections/contacts/records?realm=biz", json!({})).await).await;
    assert_eq!(mine["total"], 2);
    // And a third, named, in work: its collection clashes.
    let r = send(M::POST, "installs", json!({"package": "crm", "realm": "work", "name": "crm-two"})).await;
    assert!(r.text().await.unwrap().contains("realm work already has a collection contacts"));

    // Only the app writes what it owns.
    let r = send(M::POST, "collections/contacts/records?realm=work", json!({"fields": {"name": "Grace"}})).await;
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    assert!(r.text().await.unwrap().contains("belongs to the app crm"));
    // You can force it, after a decision that says what you risk.
    let r = ok(send(M::POST, "collections/contacts/records?realm=work&force=true", json!({"fields": {"name": "Grace"}})).await).await;
    let d = decision(r["decision"].as_i64().unwrap()).await;
    assert!(d.title.contains("Force: add a record in contacts") && d.document.contains("belongs to the app **crm**"), "{d:?}");
    decide(d.id, "Write it").await;
    let page = ok(send(M::POST, "collections/contacts/query?realm=work", json!({"where": {"name": "Grace"}})).await).await;
    let grace: Record = serde_json::from_value(page["records"][0].clone()).unwrap();
    assert_eq!(grace.created_by, "you forced");
    // The app's view acts as the app.
    let v = ok(send(M::POST, "views/people/act?realm=work", json!({"action": "Quarterly", "record": grace.id})).await).await;
    assert_eq!(v["record"]["fields"]["every_days"], 90);
    assert!(v["record"]["updated_by"].as_str().unwrap().starts_with("app:crm"), "{v}");
    // Its code changes only by upgrading: schemas, its triggers (you may
    // switch them off), its series.
    let r = send(M::PATCH, "collections/contacts?realm=work", json!({"description": "mine"})).await;
    assert!(r.text().await.unwrap().contains("changes only when the app is upgraded"));
    let triggers = ok(send(M::GET, "triggers?realm=work", json!({})).await).await;
    let t = triggers.as_array().unwrap().iter().find(|t| t["name"] == "new contact").unwrap();
    assert_eq!(t["owner"], "crm");
    let tid = t["id"].as_str().unwrap();
    let r = send(M::PATCH, &format!("triggers/{tid}"), json!({"name": "renamed"})).await;
    assert!(r.text().await.unwrap().contains("belongs to the app crm"));
    ok(send(M::PATCH, &format!("triggers/{tid}"), json!({"enabled": false})).await).await;
    assert_eq!(send(M::DELETE, "series/touchpoints?realm=work", json!({})).await.status(), StatusCode::BAD_REQUEST);

    // Settings: checked against the package's schema, and filling its code.
    let installs: Vec<Install> = send(M::GET, "installs", json!({})).await.json().await.unwrap();
    let work = installs.iter().find(|i| i.realm == "work" && i.name == "crm").unwrap().clone();
    assert_eq!(work.settings, json!({"weeks": 6}));
    assert_eq!(work.reads.collections, vec!["people".to_owned()]);
    assert_eq!(send(M::PUT, &format!("installs/{}/settings", work.id), json!({"weeks": 0})).await.status(), StatusCode::BAD_REQUEST);
    ok(send(M::PUT, &format!("installs/{}/settings", work.id), json!({"weeks": 4})).await).await;
    let skills = ok(send(M::GET, "realms/work/effective/skills?instance=crm", json!({})).await).await;
    assert_eq!(skills[0]["entry"]["body"], "Warm, short, every 4 weeks.");

    // Its template runs as it: the session is the app's, with its notes,
    // writes what the app owns, and reads only what the install approved.
    mock.push(Reply::ToolCalls(vec![
        ("records_put".into(), r#"{"collection":"contacts","fields":{"name":"Katherine"}}"#.into()),
        ("records_query".into(), r#"{"collection":"notes"}"#.into()),
    ]));
    mock.push(Reply::Text("done".into()));
    ok(send(
        M::POST,
        "collections?realm=work",
        json!({"name": "notes", "schema": {"type": "object", "properties": {"t": {"type": "string"}}}}),
    )
    .await)
    .await;
    let run = ok(send(M::POST, "templates/reach-out/run", json!({"realm": "work", "input": "Ann"})).await).await;
    let s: Session = serde_json::from_value(run["sessions"][0].clone()).unwrap();
    assert_eq!(s.instance, "crm");
    assert!(s.auto_labels.contains(&"app:crm".to_owned()), "{:?}", s.auto_labels);
    eventually("the app session's calls", async || (mock.requests().len() >= 2).then_some(())).await;
    let reqs = mock.requests();
    assert!(reqs[0].to_string().contains("Every 4 weeks: draft a note to Ann"), "its settings fill its prompt");
    assert!(reqs[0].to_string().contains("Contacts are people the user wants to stay in touch with"), "its memory notes");
    let after = reqs[1]["messages"].to_string();
    assert!(after.contains("the app crm may not read notes"), "{after}");
    let page = ok(send(M::POST, "collections/contacts/query?realm=work", json!({"where": {"name": "Katherine"}})).await).await;
    assert_eq!(page["records"][0]["created_by"], format!("session:{}", s.id));
    // A session of the realm itself can't write the app's collection.
    mock.push(Reply::ToolCalls(vec![("records_put".into(), r#"{"collection":"contacts","fields":{"name":"Mallory"}}"#.into())]));
    mock.push(Reply::Text("done".into()));
    ok(send(
        M::POST,
        "sessions",
        json!({"worker_id": wid, "cwd": "", "connection_id": conn.id, "model": "m", "prompt": "add", "realm": "work"}),
    )
    .await)
    .await;
    eventually("the realm session's call", async || (mock.requests().len() >= 4).then_some(())).await;
    assert!(mock.requests()[3]["messages"].to_string().contains("belongs to the app crm"));

    // Version 2: a new template prompt and a collection. Nothing merges: its
    // code becomes 2.0.0's; settings and data stay.
    let mut v2 = package.clone();
    v2["version"] = json!("2.0.0");
    v2["contents"]["templates"][0]["prompt"] = json!("Every {{settings.weeks}} weeks: write warmly to {{input}}");
    v2["contents"]["collections"]
        .as_array_mut()
        .unwrap()
        .push(json!({"name": "touches", "schema": {"type": "object", "properties": {"at": {"type": "string"}}}}));
    let up: AppInstall = serde_json::from_value(ok(send(M::POST, "apps/work/upgrade", v2.clone()).await).await).unwrap();
    let d = decision(up.decision.unwrap()).await;
    assert!(d.document.contains("Its code becomes the new version's") && d.document.contains("- add collection touches"), "{}", d.document);
    assert!(d.document.contains("- change templates reach-out"), "{}", d.document);
    assert_eq!(d.options.iter().map(|o| o.label.as_str()).collect::<Vec<_>>(), vec!["Upgrade", "Cancel"]);
    decide(d.id, "Upgrade").await;
    let templates = ok(send(M::GET, "realms/work/effective/templates", json!({})).await).await;
    assert_eq!(templates[0]["entry"]["prompt"], "Every 4 weeks: write warmly to {{input}}");
    assert!(ok(send(M::GET, "collections?realm=work", json!({})).await).await.to_string().contains("\"touches\""));
    // A schema the records already there don't fit needs a migration.
    let mut bad = v2.clone();
    bad["version"] = json!("3.0.0");
    bad["contents"]["collections"][0]["schema"]["required"] = json!(["name", "every_days"]);
    let r = send(M::POST, "apps/work/upgrade", bad).await;
    assert!(r.text().await.unwrap().contains("needs a migration"));
    // Back to 1.0.0: a downgrade.
    let installs: Vec<Install> = send(M::GET, "installs", json!({})).await.json().await.unwrap();
    let work = installs.iter().find(|i| i.realm == "work" && i.name == "crm").unwrap().clone();
    let down: Install =
        serde_json::from_value(ok(send(M::POST, &format!("installs/{}/upgrade", work.id), json!({"version": "1.0.0"})).await).await)
            .unwrap();
    let d = decision(down.decision.unwrap()).await;
    assert_eq!(d.options[0].label, "Downgrade");
    decide(d.id, "Downgrade").await;
    let templates = ok(send(M::GET, "realms/work/effective/templates", json!({})).await).await;
    assert_eq!(templates[0]["entry"]["prompt"], "Every 4 weeks: draft a note to {{input}}");
    assert_eq!(ok(send(M::GET, &format!("installs/{}/settings", work.id), json!({})).await).await, json!({"weeks": 4}));

    // A fork: exported, changed, added, and switched to.
    let mut fork: AppPackage = send(M::GET, "apps/export?realm=work&instance=crm&fork=crm-acme", json!({})).await.json().await.unwrap();
    assert_eq!((fork.app.as_str(), fork.forked_from.as_str()), ("crm-acme", "crm"));
    fork.contents.lists.get_mut("templates").unwrap()[0]["prompt"] = json!("Acme style: {{input}}");
    ok(send(M::POST, "packages", json!({"manifest": fork})).await).await;
    let sw: Install =
        serde_json::from_value(ok(send(M::POST, &format!("installs/{}/upgrade", work.id), json!({"package": "crm-acme"})).await).await)
            .unwrap();
    let d = decision(sw.decision.unwrap()).await;
    assert!(d.title.starts_with("Switch crm to crm-acme"), "{}", d.title);
    decide(d.id, "Switch").await;
    let installs: Vec<Install> = send(M::GET, "installs", json!({})).await.json().await.unwrap();
    assert!(installs.iter().any(|i| i.id == work.id && i.package == "crm-acme" && i.name == "crm"), "{installs:?}");
    let templates = ok(send(M::GET, "realms/work/effective/templates", json!({})).await).await;
    assert_eq!(templates[0]["entry"]["prompt"], "Acme style: {{input}}");
    // Not a fork: refused.
    let r = send(M::POST, &format!("installs/{}/upgrade", work.id), json!({"package": "nope"})).await;
    assert_eq!(r.status(), StatusCode::NOT_FOUND);

    // Removing takes what it owns, and leaves the realm and its sessions.
    let rm: Install = serde_json::from_value(ok(send(M::DELETE, &format!("installs/{}", work.id), json!({})).await).await).unwrap();
    let d = decision(rm.decision.unwrap()).await;
    assert!(d.document.contains("collection contacts (4 records)") && d.document.contains("the realm stays"), "{}", d.document);
    decide(d.id, "Remove").await;
    let cols = ok(send(M::GET, "collections?realm=work", json!({})).await).await;
    assert!(!cols.to_string().contains("\"contacts\"") && cols.to_string().contains("\"notes\""), "{cols}");
    assert!(ok(send(M::GET, "realms", json!({})).await).await.to_string().contains("\"work\""));
    assert!(ok(send(M::GET, &format!("sessions/{}", s.id), json!({})).await).await.to_string().contains(&s.id), "its sessions stay");
    assert_eq!(
        ok(send(M::GET, "collections/contacts/records?realm=biz", json!({})).await).await["total"],
        2,
        "the other instance keeps its own"
    );

    stop.cancel();
    head.shutdown().await.unwrap();
}
