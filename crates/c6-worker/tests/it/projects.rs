//! Projects (docs/projects.md): work moves in and out of a project (a work
//! item is in one at most), each work item shows its project, and projects
//! merge and close.

use c6_head::HeadConfig;
use reqwest::{Method as M, StatusCode};
use serde_json::{Value, json};

#[tokio::test]
async fn work_moves_in_and_out_of_projects() {
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
    let send = |method: M, path: String, body: Value| {
        let (http, url) = (http.clone(), format!("{api}/{path}"));
        async move {
            let r = http.request(method, &url).json(&body).send().await.unwrap();
            let s = r.status();
            (s, r.json::<Value>().await.unwrap_or(Value::Null))
        }
    };
    let ok = |(s, v): (StatusCode, Value)| {
        assert!(s.is_success(), "{s}: {v}");
        v
    };
    let get = |path: String| {
        let (http, url) = (http.clone(), format!("{api}/{path}"));
        async move { http.get(&url).send().await.unwrap().json::<Value>().await.unwrap() }
    };
    let work = |title: &str| json!({"title": title});
    let a = ok(send(M::POST, "work".into(), work("Draft the realm guide")).await)["id"].as_str().unwrap().to_owned();
    let b = ok(send(M::POST, "work".into(), work("Invite the team")).await)["id"].as_str().unwrap().to_owned();
    let c = ok(send(M::POST, "work".into(), work("Fix the login page")).await)["id"].as_str().unwrap().to_owned();

    // A project, with work brought in at once.
    let p =
        ok(send(M::POST, "projects".into(), json!({"title": "Realms for the team", "purpose": "The team uses realms daily", "work": [a]}))
            .await);
    let pid = p["id"].as_str().unwrap().to_owned();
    assert_eq!((p["status"].as_str(), p["work"][0]["id"].as_str()), (Some("active"), Some(a.as_str())));
    // Work joins from the project, and from the work item.
    ok(send(M::POST, format!("projects/{pid}/work"), json!({"work": b})).await);
    ok(send(M::PATCH, format!("work/{c}"), json!({"project": pid})).await);
    let p = get(format!("projects/{pid}")).await;
    assert_eq!(p["work"].as_array().unwrap().len(), 3);
    assert_eq!(p["why"], "3 pieces of work open");
    // Each work item shows its project.
    let w = get(format!("work/{a}")).await;
    assert_eq!((w["project"].as_str(), w["project_title"].as_str()), (Some(pid.as_str()), Some("Realms for the team")));
    // A work item is in one project at most: moving it moves it out.
    let q = ok(send(M::POST, "projects".into(), json!({"title": "Login revamp"})).await);
    let qid = q["id"].as_str().unwrap().to_owned();
    ok(send(M::POST, format!("projects/{qid}/work"), json!({"work": c})).await);
    assert_eq!(get(format!("projects/{pid}")).await["work"].as_array().unwrap().len(), 2);
    assert_eq!(get(format!("work/{c}")).await["project_title"], "Login revamp");
    // Out of a project: from the project, or the work item.
    ok(send(M::POST, format!("projects/{pid}/detach"), json!({"work": b})).await);
    assert!(get(format!("work/{b}")).await.get("project").is_none());
    ok(send(M::PATCH, format!("work/{c}"), json!({"project": ""})).await);
    assert_eq!(get(format!("projects/{qid}")).await["work"].as_array().unwrap().len(), 0);
    // A phase the project doesn't have, or a project that isn't there.
    assert_eq!(send(M::POST, format!("projects/{pid}/work"), json!({"work": b, "phase": "nope"})).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(send(M::PATCH, format!("work/{b}"), json!({"project": "nope"})).await.0, StatusCode::BAD_REQUEST);

    // Merge: the other's work comes along; it's gone.
    ok(send(M::POST, format!("projects/{qid}/work"), json!({"work": c})).await);
    let m = ok(send(M::POST, format!("projects/{pid}/merge"), json!({"from": qid})).await);
    assert_eq!(m["work"].as_array().unwrap().len(), 2);
    assert_eq!(get(format!("work/{c}")).await["project"], pid.as_str());
    assert_eq!(send(M::GET, format!("projects/{qid}"), Value::Null).await.0, StatusCode::NOT_FOUND);
    // Closed: out of the list, behind closed=true; its history is kept.
    ok(send(M::PATCH, format!("projects/{pid}"), json!({"status": "done"})).await);
    assert_eq!(get("projects".into()).await.as_array().unwrap().len(), 0);
    let closed = get("projects?closed=true".into()).await;
    assert_eq!((closed[0]["state"].as_str(), closed[0]["closed_at"].is_i64()), (Some("done"), true));
    let h = get(format!("collections/projects/records/{pid}/history")).await;
    assert!(h.as_array().is_some_and(|v| v.len() >= 3), "{h}");
}
