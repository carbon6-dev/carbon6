//! How dev work starts (docs/dev-starts.md): a work item's template picks a
//! worker from a labelled pool and makes a worktree as it says; the work's
//! next start goes back there; closing the work asks, and Remove takes the
//! worktree and branch away and runs the WorkEnded hook.

use std::path::Path;

use c6_api::starts::StartPlan;
use c6_api::work::WorkItem;
use c6_api::{Connection, Session, SessionDetail, SessionStatus, Worker as ApiWorker};
use c6_head::HeadConfig;
use c6_testkit::{MockLlm, Reply, eventually};
use c6_worker::{ConnState, Identity, Worker, conn, enroll};
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

const GIT: &str = "git -c user.name=t -c user.email=t@t";

fn sh(dir: &Path, script: &str) -> String {
    let out = std::process::Command::new("sh").arg("-c").arg(script).current_dir(dir).output().unwrap();
    assert!(out.status.success(), "{script}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

async fn worker(head: &c6_head::Head, name: &str, state: &Path, stop: &CancellationToken) -> std::sync::Arc<Worker> {
    let e = c6_head::api::new_enrollment(&head.state, Some(name)).await.unwrap();
    enroll::enroll(&e.token).await.unwrap().save(state).unwrap();
    let w = Worker::open(state, Identity::load(state).unwrap()).await.unwrap();
    let (tx, mut rx) = watch::channel(ConnState::Connecting);
    tokio::spawn(conn::run(w.clone(), tx, stop.clone()));
    c6_testkit::within("the worker to attach", rx.wait_for(|s| matches!(s, ConnState::Attached { .. }))).await.unwrap();
    w
}

#[tokio::test]
async fn work_starts_from_its_template_goes_back_to_its_place_and_cleans_up_when_it_ends() {
    let mock = MockLlm::start().await;
    let (head_dir, s1, s2, tmp) =
        (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
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
    let stop = CancellationToken::new();
    let _w1 = worker(&head, "dev-01", s1.path(), &stop).await;
    let _w2 = worker(&head, "dev-02", s2.path(), &stop).await;
    let send = async |r: reqwest::RequestBuilder| -> Value {
        let r = r.send().await.unwrap();
        let status = r.status();
        let text = r.text().await.unwrap();
        assert!(status.is_success(), "{status}: {text}");
        serde_json::from_str(&text).unwrap_or(Value::Null)
    };

    // Both workers labelled the same: one pool.
    let workers: Vec<ApiWorker> = serde_json::from_value(send(http.get(format!("{api}/workers"))).await).unwrap();
    for w in &workers {
        send(http.put(format!("{api}/workers/{}/labels", w.id)).json(&json!({"labels": ["dev-vm"]}))).await;
    }
    let r = http.put(format!("{api}/workers/{}/labels", workers[0].id)).json(&json!({"labels": ["bad label"]})).send().await.unwrap();
    assert_eq!(r.status(), reqwest::StatusCode::BAD_REQUEST);
    let workers: Vec<ApiWorker> = serde_json::from_value(send(http.get(format!("{api}/workers"))).await).unwrap();
    assert!(workers.iter().all(|w| w.labels == ["dev-vm"]), "{workers:?}");

    // A repository with a remote whose main line has moved on.
    let (origin, clone) = (tmp.path().join("origin"), tmp.path().join("clone"));
    std::fs::create_dir_all(&origin).unwrap();
    sh(&origin, &format!("git init -q -b main && echo one > a.txt && git add . && {GIT} commit -qm one"));
    sh(tmp.path(), &format!("git clone -q {} clone", origin.display()));
    sh(&origin, &format!("echo two > b.txt && git add . && {GIT} commit -qm two"));

    let conn: Connection = serde_json::from_value(
        send(http.post(format!("{api}/connections")).json(&json!({"name": "m", "base_url": mock.base_url()}))).await,
    )
    .unwrap();
    // No agent writes the work's notes (they would ask the mock model too).
    send(http.put(format!("{api}/settings/work")).json(&json!({"guess": false, "notes": false}))).await;
    let hook_out = tmp.path().join("ended.txt");
    send(http.put(format!("{api}/settings/hooks")).json(&json!([{"event": "WorkEnded", "command":
        format!("echo \"$C6_BRANCH|$C6_REMOVED\" > {}", hook_out.display())}])))
    .await;
    // A bad branch pattern is refused.
    let mut template = json!({
        "name": "dev", "prompt": "Work on it.", "cwd": clone.display().to_string(), "connection": "m", "model": "m",
        "worktree": true, "pick": true, "worker_labels": ["dev-vm"],
        "worktree_options": {"branch": "me/{{name}}..x", "base": "origin/main", "fetch": true, "setup": ["echo set up in $C6_BRANCH"], "cleanup": "ask"},
    });
    let r = http.put(format!("{api}/settings/templates")).json(&json!([template])).send().await.unwrap();
    assert_eq!(r.status(), reqwest::StatusCode::BAD_REQUEST);
    template["worktree_options"]["branch"] = json!("me/{{name}}");
    send(http.put(format!("{api}/settings/templates")).json(&json!([template]))).await;

    // The work item starts with it.
    let w: WorkItem =
        serde_json::from_value(send(http.post(format!("{api}/work")).json(&json!({"title": "Kafka ingest: retries"}))).await).unwrap();
    let r = http.patch(format!("{api}/work/{}", w.id)).json(&json!({"start_with": "nope"})).send().await.unwrap();
    assert_eq!(r.status(), reqwest::StatusCode::BAD_REQUEST);
    send(http.patch(format!("{api}/work/{}", w.id)).json(&json!({"start_with": "dev"}))).await;
    let plan: StartPlan = serde_json::from_value(send(http.get(format!("{api}/starts?work={}", w.id))).await).unwrap();
    assert_eq!(plan.template, "dev");
    assert_eq!(plan.options.branch, "me/kafka_ingest_retries");
    assert!(plan.worktree && !plan.reuse && !plan.offline);
    assert!(plan.why.contains("the least busy of label dev-vm"), "{}", plan.why);
    assert!(plan.labels.contains(&"template:dev".to_owned()));

    let start = async |plan: &StartPlan| -> Session {
        mock.push(Reply::Text("done".into()));
        let s: Session = serde_json::from_value(
            send(http.post(format!("{api}/sessions")).json(&json!({
                "worker_id": plan.worker_id, "cwd": plan.cwd, "connection_id": conn.id, "model": "m", "prompt": plan.prompt, "title": "Kafka ingest",
                "labels": plan.labels, "worktree": plan.worktree, "worktree_options": plan.worktree.then_some(&plan.options), "work": w.id,
            })))
            .await,
        )
        .unwrap();
        eventually("the session to finish its turn", async || {
            let d: SessionDetail = http.get(format!("{api}/sessions/{}", s.id)).send().await.ok()?.json().await.ok()?;
            (d.session.status == SessionStatus::Idle).then_some(())
        })
        .await;
        s
    };
    let first = start(&plan).await;
    let item = async || serde_json::from_value::<WorkItem>(send(http.get(format!("{api}/work/{}", w.id))).await).unwrap();
    let place = eventually("the work's place", async || item().await.fields.place).await;
    assert_eq!(place.branch, "me/kafka_ingest_retries");
    assert_eq!(place.cleanup, "ask");
    assert_eq!(place.session, first.id);
    assert_eq!(Some(place.worker_id.clone()), plan.worker_id.map(|i| i.to_string()));
    let top = Path::new(&place.path);
    assert!(top.join("b.txt").exists(), "branched from the fetched origin/main");
    assert_eq!(sh(top, "git branch --show-current"), "me/kafka_ingest_retries");
    // The setup's output is in the transcript.
    let events = send(http.get(format!("{api}/sessions/{}/events", first.id))).await.to_string();
    assert!(events.contains("set up in me/kafka_ingest_retries"), "{events}");

    // The next start goes back there: same worker, same worktree.
    let again: StartPlan = serde_json::from_value(send(http.get(format!("{api}/starts?work={}", w.id))).await).unwrap();
    assert!(again.reuse && !again.worktree, "{again:?}");
    assert_eq!((again.worker_id, again.cwd.as_str()), (plan.worker_id, place.path.as_str()));
    assert!(again.why.contains("where this work's worktree is"), "{}", again.why);
    let second = start(&again).await;
    let d: SessionDetail = serde_json::from_value(send(http.get(format!("{api}/sessions/{}", second.id))).await).unwrap();
    assert_eq!(d.session.cwd, place.path);
    assert_eq!(sh(&clone, "git worktree list | wc -l"), "2", "no new worktree");
    assert_eq!(item().await.fields.place.unwrap().session, first.id);
    // Another worker: a fresh worktree of the same branch there.
    let other = workers.iter().find(|x| Some(x.id) != plan.worker_id).unwrap();
    let moved: StartPlan =
        serde_json::from_value(send(http.get(format!("{api}/starts?work={}&worker={}", w.id, other.name))).await).unwrap();
    assert!(moved.worktree && !moved.reuse && moved.options.branch == place.branch && moved.worker_id == Some(other.id), "{moved:?}");

    // Closing the work asks; Remove takes it away (nothing would be lost).
    send(http.patch(format!("{api}/work/{}", w.id)).json(&json!({"status": "done"}))).await;
    let decision = eventually("the decision", async || {
        let all: Vec<Value> = http.get(format!("{api}/decisions")).send().await.ok()?.json().await.ok()?;
        all.into_iter().find(|d| d["title"].as_str().is_some_and(|t| t.starts_with("Remove the worktree")))
    })
    .await;
    send(http.post(format!("{api}/decisions/{}/answer", decision["id"])).json(&json!({"option": "Remove"}))).await;
    eventually("the worktree to go", async || (!top.exists()).then_some(())).await;
    assert_eq!(sh(&clone, "git branch --list me/kafka_ingest_retries"), "");
    assert_eq!(std::fs::read_to_string(&hook_out).unwrap().trim(), "me/kafka_ingest_retries|1");
    eventually("the place to be forgotten", async || item().await.fields.place.is_none().then_some(())).await;

    // A session in no work, started by the template set to automatic:
    // archiving it removes its worktree at once.
    template["worktree_options"]["cleanup"] = json!("auto");
    template["worktree_options"]["branch"] = json!("me/{{id}}");
    send(http.put(format!("{api}/settings/templates")).json(&json!([template]))).await;
    mock.push(Reply::Text("done".into()));
    let run: c6_api::TemplateRun =
        serde_json::from_value(send(http.post(format!("{api}/templates/dev/run")).json(&json!({"input": ""}))).await).unwrap();
    let s = run.sessions[0].clone();
    eventually("the run's session", async || {
        let d: SessionDetail = http.get(format!("{api}/sessions/{}", s.id)).send().await.ok()?.json().await.ok()?;
        (d.session.status == SessionStatus::Idle).then_some(())
    })
    .await;
    let made = std::path::PathBuf::from(sh(&clone, "git worktree list --porcelain | grep '^worktree ' | tail -1 | cut -c10-"));
    assert!(made.exists() && sh(&made, "git branch --show-current").starts_with("me/"));
    send(http.patch(format!("{api}/sessions/{}", s.id)).json(&json!({"archived": true}))).await;
    eventually("the archived session's worktree to go", async || (!made.exists()).then_some(())).await;

    // A setup command that fails stops the start, with what it said.
    template["worktree_options"]["setup"] = json!(["echo cannot >&2; exit 3"]);
    send(http.put(format!("{api}/settings/templates")).json(&json!([template]))).await;
    let run: c6_api::TemplateRun =
        serde_json::from_value(send(http.post(format!("{api}/templates/dev/run")).json(&json!({"input": ""}))).await).unwrap();
    let s = run.sessions[0].clone();
    let d: SessionDetail = eventually("the start to fail", async || {
        let d: SessionDetail = http.get(format!("{api}/sessions/{}", s.id)).send().await.ok()?.json().await.ok()?;
        (d.session.status == SessionStatus::Failed).then_some(d)
    })
    .await;
    let error = d.session.error.unwrap_or_default();
    assert!(error.contains("setup `echo cannot >&2; exit 3` failed (exit 3)") && error.contains("cannot"), "{error}");
    assert_eq!(sh(&clone, "git worktree list | wc -l"), "1", "the failed worktree is taken away");

    stop.cancel();
    head.shutdown().await.unwrap();
}
