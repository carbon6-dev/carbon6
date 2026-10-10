//! End to end: a real head and worker over loopback mTLS.

use std::path::Path;
use std::time::Duration;

use c6_api::WorkerStatus;
use c6_head::HeadConfig;
use c6_worker::{ConnState, Identity, Worker, conn, enroll};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

fn head_config(data_dir: &Path, worker_port: u16) -> HeadConfig {
    HeadConfig {
        data_dir: data_dir.to_owned(),
        bind: vec!["127.0.0.1".parse().unwrap()],
        ui_port: 0,
        worker_port,
        auth: false,
        ..Default::default()
    }
}

async fn wait_for(rx: &mut watch::Receiver<ConnState>, pred: impl Fn(&ConnState) -> bool) -> ConnState {
    tokio::time::timeout(Duration::from_secs(15), rx.wait_for(|s| pred(s))).await.expect("timed out waiting for state").unwrap().clone()
}

async fn status(head: &c6_head::Head, id: c6_core::WorkerId) -> WorkerStatus {
    let workers = c6_head::api::workers_snapshot(&head.state).await.unwrap();
    workers.iter().find(|w| w.id == id.0).expect("worker listed").status
}

#[tokio::test]
async fn enroll_attach_restart_revoke() {
    let _ = tracing_subscriber::fmt().with_test_writer().try_init();
    let head_dir = tempfile::tempdir().unwrap();
    let worker_dir = tempfile::tempdir().unwrap();

    let head = c6_head::start(head_config(head_dir.path(), 0)).await.unwrap();
    let worker_port = head.worker_addrs[0].port();

    // Enroll with a one-time token.
    let enrollment = c6_head::api::new_enrollment(&head.state, Some("testbox")).await.unwrap();
    let identity = enroll::enroll(&enrollment.token).await.unwrap();
    assert_eq!(identity.meta.worker_name, "testbox");
    identity.save(worker_dir.path()).unwrap();
    let identity = Identity::load(worker_dir.path()).unwrap();
    let id = identity.meta.worker_id;
    let worker_state = worker_dir.path().join("state");

    let reuse = enroll::enroll(&enrollment.token).await;
    assert!(reuse.is_err(), "token must be single-use");

    // Attach.
    let (state_tx, mut state_rx) = watch::channel(ConnState::Connecting);
    let stop = CancellationToken::new();
    let worker = Worker::open(&worker_state, identity).await.unwrap();
    let worker = tokio::spawn(conn::run(worker, state_tx, stop.clone()));
    wait_for(&mut state_rx, |s| matches!(s, ConnState::Attached { .. })).await;
    assert_eq!(status(&head, id).await, WorkerStatus::Online);

    // Head restarts on the same port; the worker comes back by itself.
    head.shutdown().await.unwrap();
    wait_for(&mut state_rx, |s| matches!(s, ConnState::Disconnected { .. })).await;
    let head = c6_head::start(head_config(head_dir.path(), worker_port)).await.unwrap();
    wait_for(&mut state_rx, |s| matches!(s, ConnState::Attached { .. })).await;
    assert_eq!(status(&head, id).await, WorkerStatus::Online);

    // Revocation disconnects the worker and it stops retrying.
    assert!(head.state.store.revoke_worker(id, c6_core::now_ms()).await.unwrap());
    head.state.registry.kick(id);
    let result = tokio::time::timeout(Duration::from_secs(15), worker).await.expect("worker should stop").unwrap();
    assert!(result.is_err(), "revoked worker must stop");
    assert_eq!(status(&head, id).await, WorkerStatus::Revoked);

    head.shutdown().await.unwrap();
}

#[tokio::test]
async fn wrong_ca_fingerprint_is_rejected() {
    let head_dir = tempfile::tempdir().unwrap();
    let head = c6_head::start(head_config(head_dir.path(), 0)).await.unwrap();
    let enrollment = c6_head::api::new_enrollment(&head.state, None).await.unwrap();

    let mut token = c6_core::enrollment::EnrollmentToken::decode(&enrollment.token).unwrap();
    token.ca_sha256 = "00".repeat(32);
    let err = enroll::enroll(&token.encode()).await.err().expect("must fail");
    assert!(format!("{err:#}").contains("pinned"), "{err:#}");

    head.shutdown().await.unwrap();
}
