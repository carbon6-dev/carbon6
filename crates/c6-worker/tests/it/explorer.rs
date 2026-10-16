//! The realm explorer (docs/explorer.md): a realm's inventory, its recent
//! activity, lists per section, and objects with what is connected to them;
//! a realm's token sees only its own realm.

use c6_api::{ExObject, ExRef, RealmSummary};
use c6_head::HeadConfig;
use reqwest::StatusCode;
use serde_json::{Value, json};

#[tokio::test]
async fn explorer_shows_a_realm_and_its_connections() {
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

    // A small realm: deals that, when won, write an invoice.
    ok(send(M::POST, "realms", json!({"id": "crm", "name": "CRM"})).await).await;
    let schema = |props: Value| json!({"type": "object", "properties": props});
    ok(send(M::POST, "collections?realm=crm", json!({"name": "deals", "schema": schema(json!({"name": {"type": "string"}, "stage": {"type": "string", "enum": ["new", "won"]}}))})).await).await;
    ok(send(M::POST, "collections?realm=crm", json!({"name": "invoices", "schema": schema(json!({"client": {"type": "string"}}))})).await)
        .await;
    ok(send(
        M::PUT,
        "settings/views?realm=crm",
        json!([{"name": "pipeline", "kind": "board", "group_by": "stage", "source": {"collection": "deals"}}]),
    )
    .await)
    .await;
    let wins = ok(send(
        M::POST,
        "triggers?realm=crm",
        json!({"name": "invoice wins", "trigger": "record", "event_params": {"collection": "deals", "ops": ["updated"]},
               "filter": {"$.record.fields.stage": "won"}, "action": "write",
               "action_params": {"collection": "invoices", "fields": {"client": "$.record.fields.name"}, "links": ["$.record.id"]}}),
    )
    .await)
    .await;
    let deal = ok(send(
        M::POST,
        "collections/deals/records?realm=crm",
        json!({"fields": {"name": "Acme", "stage": "new"}, "links": ["client:acme"]}),
    )
    .await)
    .await;
    let did = deal["id"].as_str().unwrap().to_owned();
    ok(send(
        M::PUT,
        &format!("collections/deals/records/{did}?realm=crm"),
        json!({"fields": {"name": "Acme", "stage": "won"}, "version": 1}),
    )
    .await)
    .await;
    // A decision about the deal (as a trigger or agent would open it).
    ok(send(M::POST, "decisions?realm=crm", json!({"title": "Discount for Acme?", "context": {"collection": "deals", "id": did}})).await)
        .await;

    // The inventory.
    let s: RealmSummary = send(M::GET, "realms/crm/summary", json!({})).await.json().await.unwrap();
    let e = |section: &str| s.entries.iter().find(|e| e.section == section).cloned();
    assert_eq!(e("collection:deals").unwrap().count, 1);
    assert_eq!(e("collection:invoices").unwrap().count, 1);
    assert_eq!(e("triggers").unwrap().count, 1);
    assert_eq!(e("decisions:open").unwrap().count, 1);
    assert_eq!(e("list:views").unwrap().count, 1);
    assert!(e("list:prompts").is_some_and(|p| p.count == 0 && p.inherited > 0), "root's prompts are inherited");
    assert!(s.knowledge, "the head keeps knowledge (docs/knowledge.md)");

    // Activity: the deal's changes, the trigger's run, its invoice, the decision.
    let act: Vec<ExRef> = send(M::GET, "realms/crm/activity", json!({})).await.json().await.unwrap();
    let subs: Vec<String> = act.iter().map(|a| format!("{}: {} ({})", a.kind, a.title, a.sub)).collect();
    for want in [
        "record: deals: Acme (updated by you)",
        "record: invoices: Acme (created by trigger:invoice-wins)",
        "trigger: Trigger invoice wins (record run, ok)",
        "decision: Discount for Acme? (opened)",
    ] {
        assert!(subs.iter().any(|s| s == want), "{want} in {subs:#?}");
    }

    // Lists.
    let deals: Vec<ExRef> = send(M::GET, "realms/crm/list?section=collection:deals", json!({})).await.json().await.unwrap();
    assert_eq!((deals[0].title.as_str(), deals[0].id.clone()), ("Acme", format!("deals/{did}")));
    let views: Vec<ExRef> = send(M::GET, "realms/crm/list?section=list:views", json!({})).await.json().await.unwrap();
    assert_eq!(views[0].title, "pipeline");

    // The deal, and what is connected to it.
    let o: ExObject = send(M::GET, &format!("realms/crm/object?kind=record&id=deals/{did}"), json!({})).await.json().await.unwrap();
    assert_eq!(o.title, "Acme");
    let g = |o: &ExObject, label: &str| {
        o.connected.iter().find(|g| g.label == label).map(|g| g.items.iter().map(|i| i.title.clone()).collect::<Vec<_>>())
    };
    assert_eq!(g(&o, "Things it links to"), Some(vec!["client:acme".to_owned()]));
    assert_eq!(g(&o, "Records linking here"), Some(vec!["Acme".to_owned()]), "the invoice links back");
    assert_eq!(g(&o, "Decisions about it"), Some(vec!["Discount for Acme?".to_owned()]));
    assert_eq!(g(&o, "Triggers watching its collection"), Some(vec!["invoice wins".to_owned()]));
    assert!(o.body.contains("v2 update by you"), "{}", o.body);
    // The collection: its records, views and triggers.
    let c: ExObject = send(M::GET, "realms/crm/object?kind=collection&id=deals", json!({})).await.json().await.unwrap();
    assert_eq!(g(&c, "Views showing it"), Some(vec!["pipeline".to_owned()]));
    assert!(c.fields.iter().any(|(k, v)| k == "stage" && v.contains("one of new, won")), "{:?}", c.fields);
    // The invoice was written by the trigger; the trigger involves both collections.
    let inv: Vec<ExRef> = send(M::GET, "realms/crm/list?section=collection:invoices", json!({})).await.json().await.unwrap();
    let io: ExObject = send(M::GET, &format!("realms/crm/object?kind=record&id={}", inv[0].id), json!({})).await.json().await.unwrap();
    assert_eq!(g(&io, "Written by"), Some(vec!["invoice wins".to_owned()]));
    let t: ExObject =
        send(M::GET, &format!("realms/crm/object?kind=trigger&id={}", wins["id"].as_str().unwrap()), json!({})).await.json().await.unwrap();
    assert_eq!(g(&t, "Involves"), Some(vec!["deals".to_owned(), "invoices".to_owned()]));

    // Search: across records, decisions, triggers and settings.
    let found: Vec<c6_api::ExGroup> = send(M::GET, "realms/crm/search?q=acme", json!({})).await.json().await.unwrap();
    let labels: Vec<&str> = found.iter().map(|g| g.label.as_str()).collect();
    assert!(labels.contains(&"Records") && labels.contains(&"Decisions"), "{found:?}");
    assert_eq!(found.iter().find(|g| g.label == "Records").unwrap().items.len(), 2, "the deal and its invoice");
    let found: Vec<c6_api::ExGroup> = send(M::GET, "realms/crm/search?q=invoice", json!({})).await.json().await.unwrap();
    assert!(found.iter().any(|g| g.label == "Triggers" && g.items[0].title == "invoice wins"), "{found:?}");
    let found: Vec<c6_api::ExGroup> = send(M::GET, "realms/crm/search?q=pipeline", json!({})).await.json().await.unwrap();
    assert!(found.iter().any(|g| g.label == "Configuration" && g.items[0].title == "pipeline"), "{found:?}");

    // Two hops: the deal → the invoice (linking back) → … and the trigger →
    // the invoices collection.
    let n = ok(send(M::GET, &format!("realms/crm/neighborhood?kind=record&id=deals/{did}"), json!({})).await).await;
    let nodes = n["nodes"].as_array().unwrap();
    assert_eq!(nodes[0]["ref"]["title"], "Acme");
    assert!(nodes.iter().any(|x| x["depth"] == 1 && x["ref"]["kind"] == "trigger"), "{n}");
    assert!(nodes.iter().any(|x| x["depth"] == 2 && x["ref"]["kind"] == "collection" && x["ref"]["id"] == "invoices"), "{n}");
    let ids: Vec<(String, String)> =
        nodes.iter().map(|x| (x["ref"]["kind"].as_str().unwrap().to_owned(), x["ref"]["id"].as_str().unwrap().to_owned())).collect();
    let mut unique = ids.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), ids.len(), "each object once: {ids:?}");
    assert!(n["edges"].as_array().unwrap().iter().all(|e| e["label"].is_string()));

    // The map; and a realm's token sees only its own realm.
    let map = ok(send(M::GET, "explorer/map", json!({})).await).await;
    assert!(
        map["realms"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["realm"]["id"] == "crm" && r["counts"]["records"] == 2 && r["counts"]["collections"] == 2),
        "{map}"
    );
    let tok: Value = send(M::POST, "auth/tokens", json!({"name": "c", "realm": "crm"})).await.json().await.unwrap();
    let tok = tok["token"].as_str().unwrap();
    let mine = http.get(format!("{api}/realms/crm/summary")).bearer_auth(tok).send().await.unwrap();
    assert!(mine.status().is_success());
    let other = http.get(format!("{api}/realms/home/summary")).bearer_auth(tok).send().await.unwrap();
    assert_eq!(other.status(), StatusCode::FORBIDDEN);

    head.shutdown().await.unwrap();
}
