//! The worked examples (docs/os.md §6) are apps made of configuration
//! only: both packages in examples/apps install as they are, ask for their
//! grants, and their triggers, views, series and budgets work.

use c6_api::{AppInstall, AppPlan, Decision};
use c6_head::HeadConfig;
use serde_json::{Value, json};

const FACTORY: &str = include_str!("../../../../examples/apps/service-factory.json");
const CRM: &str = include_str!("../../../../examples/apps/personal-crm.json");

#[tokio::test]
async fn example_apps_install_and_work() {
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
    let base = format!("http://127.0.0.1:{}", head.ui_addrs[0].port());
    let api = format!("{base}/api/v1");
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
    let decisions = || {
        let (http, api) = (http.clone(), api.clone());
        async move { http.get(format!("{api}/decisions")).send().await.unwrap().json::<Vec<Decision>>().await.unwrap() }
    };

    // Each in a realm of its own, which you make first (the packages
    // suggest them; installing never makes one).
    for (pkg, realm) in [(FACTORY, "factory"), (CRM, "crm")] {
        ok(send(M::POST, "realms", json!({"id": realm, "name": realm})).await).await;
        let p: Value = serde_json::from_str(pkg).unwrap();
        let plan: AppPlan = send(M::POST, "apps/plan", p.clone()).await.json().await.unwrap();
        assert!(plan.problems.is_empty(), "{realm}: {:?}", plan.problems);
        let i: AppInstall = serde_json::from_value(ok(send(M::POST, "apps", p).await).await).unwrap();
        assert_eq!(i.realm, realm);
        let r = send(M::POST, &format!("decisions/{}/answer", i.decision.unwrap()), json!({"option": "Install"})).await;
        assert!(r.status().is_success(), "{realm}: {}", r.text().await.unwrap());
    }

    let from_app = |list: Value, app: &str| list.as_array().unwrap().iter().filter(|e| e["from"] == format!("app:{app}")).count();

    // Everything the factory brings is there.
    let count = |path: String| async move { ok(send(M::GET, &path, json!({})).await).await.as_array().map(Vec::len).unwrap_or(0) };
    let cols = ok(send(M::GET, "collections?realm=factory", json!({})).await).await;
    let mine: Vec<&str> = cols.as_array().unwrap().iter().filter(|c| c["system"] == false).map(|c| c["name"].as_str().unwrap()).collect();
    assert_eq!(mine, vec!["candidates", "incidents", "services"]);
    assert_eq!(count("views?realm=factory".into()).await, 7);
    let app = "service-factory";
    assert_eq!(from_app(ok(send(M::GET, "realms/factory/effective/templates", json!({})).await).await, app), 8);
    assert_eq!(from_app(ok(send(M::GET, "realms/factory/effective/skills", json!({})).await).await, app), 4);
    assert_eq!(from_app(ok(send(M::GET, "realms/factory/effective/policies", json!({})).await).await, app), 4);
    let triggers: Vec<c6_api::Automation> = send(M::GET, "triggers", json!({})).await.json().await.unwrap();
    assert_eq!(triggers.len(), 10 + 3);
    // It asked for its grants, and says what is left for you.
    let ds = decisions().await;
    assert!(ds.iter().any(|d| d.title == "Let realm factory read home's knowledge (email)?"), "{ds:?}");
    assert!(ds.iter().any(|d| d.title.contains("Let realm crm call home's operations memex.mail.draft")), "{ds:?}");
    let todo = ds.iter().find(|d| d.title == "Finish setting up service-factory").unwrap();
    assert!(todo.document.contains("GITHUB_TOKEN") && todo.document.contains("Dedicate a worker"), "{}", todo.document);

    // A payment webhook records revenue per service, with no session.
    let payments = triggers.iter().find(|t| t.name == "payments").unwrap();
    let rotated: c6_api::Automation = ok(send(M::POST, &format!("triggers/{}/rotate", payments.id), json!({})).await)
        .await
        .as_object()
        .map(|o| serde_json::from_value(Value::Object(o.clone())).unwrap())
        .unwrap();
    let url = format!("{base}{}", rotated.webhook_url.unwrap());
    assert!(http.post(&url).json(&json!({"amount": 19, "metadata": {"service": "invoicer"}})).send().await.unwrap().status().is_success());
    let rev = ok(send(M::POST, "series/revenue/query?realm=factory", json!({"group_by": "service"})).await).await;
    assert_eq!(rev["groups"], json!([["invoicer", 19.0]]), "{rev}");

    // A strong candidate fires the deep dive (no worker here, so the run
    // fails to start; the wiring is what is tested). Candidates are the
    // factory's: its sessions write them; here you force two in.
    for fields in [
        json!({"name": "csv-sync", "ecosystem": "shopify", "gap": "exports", "stage": "found", "score": 8}),
        json!({"name": "meh", "ecosystem": "shopify", "gap": "x", "stage": "found", "score": 3}),
    ] {
        let r = ok(send(M::POST, "collections/candidates/records?realm=factory&force=true", json!({"fields": fields})).await).await;
        ok(send(M::POST, &format!("decisions/{}/answer", r["decision"]), json!({"option": "Write it"})).await).await;
    }
    let strong = triggers.iter().find(|t| t.name == "strong candidate").unwrap();
    let runs: Vec<c6_api::AutomationRun> = send(M::GET, &format!("triggers/{}/runs", strong.id), json!({})).await.json().await.unwrap();
    assert_eq!(runs.len(), 1, "only the strong one: {runs:?}");
    assert_eq!(runs[0].trigger, "record");

    // The realm's own budget is in force for its sessions, and shows.
    let usage = ok(send(M::GET, "usage?days=1", json!({})).await).await;
    assert!(usage.to_string().contains("factory monthly (realm factory)"), "{usage}");

    // Allowed, the CRM's mail grant lets home's mail events reach its trigger.
    let grant_d = ds.iter().find(|d| d.title == "Let realm crm read home's knowledge (email)?").unwrap();
    ok(send(M::POST, &format!("decisions/{}/answer", grant_d.id), json!({"option": "Allow"})).await).await;
    let readers = c6_head::grants::memex_readers(&head.state, "home", "email").await;
    assert!(readers.iter().any(|(_, to)| to == "crm"), "{readers:?}");

    head.shutdown().await.unwrap();
}
