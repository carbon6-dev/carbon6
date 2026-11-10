//! A fixture worker whose actors stop before its private held records go away.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use c6_testkit::within;
use c6_worker::{ConnState, Identity, Worker, conn};
use nix::sys::signal::killpg;
use nix::unistd::Pid;
use tokio::sync::{oneshot, watch};
use tokio_util::sync::CancellationToken;

pub struct HeldWorker {
    pub id: String,
    state: Option<PathBuf>,
    stop: Option<oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl HeldWorker {
    pub async fn start(state: &Path) -> Self {
        let (stop, mut stopped) = oneshot::channel();
        let (ready, attached) = oneshot::channel();
        // Arm cleanup BEFORE opening a journal or starting any worker task:
        // a panic/cancellation during startup must still join and clean up.
        let mut fixture = Self { id: String::new(), state: Some(state.to_owned()), stop: Some(stop), thread: None };
        let state = state.to_owned();
        fixture.thread = Some(std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            rt.block_on(async {
                let worker = Worker::open(&state, Identity::load(&state).unwrap()).await.unwrap();
                let id = worker.identity.meta.worker_id.to_string();
                let (tx, mut rx) = watch::channel(ConnState::Connecting);
                tokio::spawn(conn::run(worker, tx, CancellationToken::new()));
                tokio::select! {
                    _ = &mut stopped => return,
                    attached = within("the fixture worker to attach", rx.wait_for(|s| matches!(s, ConnState::Attached { .. }))) => {
                        attached.unwrap();
                    }
                }
                let _ = ready.send(id);
                let _ = stopped.await;
            });
            // Drop this entire runtime, not just its connection: all actors
            // and detached turn tasks are gone before held cleanup starts.
        }));
        fixture.id = within("the fixture worker's ready signal", attached).await.unwrap();
        fixture
    }

    pub fn stop(mut self) {
        self.finish().expect("fixture worker must stop and leave no held process groups");
    }

    fn finish(&mut self) -> anyhow::Result<()> {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        let joined = self.thread.take().map(|thread| thread.join());
        // Even a worker-thread panic must not skip cleanup. A fresh thread
        // avoids nesting a runtime in the test's async context (also in Drop).
        let cleaned = self.state.take().map(|state| {
            std::thread::spawn(move || tokio::runtime::Builder::new_current_thread().enable_all().build()?.block_on(clean_held(&state)))
                .join()
        });
        if let Some(result) = cleaned {
            result.map_err(|_| anyhow::anyhow!("held fixture cleanup thread panicked"))??;
        }
        anyhow::ensure!(joined.is_none_or(|result| result.is_ok()), "fixture worker thread panicked");
        Ok(())
    }
}

impl Drop for HeldWorker {
    fn drop(&mut self) {
        if let Err(error) = self.finish() {
            // Never double-panic while unwinding the assertion being tested.
            eprintln!("held fixture cleanup failed: {error:#}");
        }
    }
}

async fn clean_held(state: &Path) -> anyhow::Result<()> {
    let held = state.join("claude/held");
    let entries = match std::fs::read_dir(&held) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    let mut records = Vec::new();
    let mut sessions = HashSet::new();
    for entry in entries {
        let path = entry?.path();
        let meta: serde_json::Value = serde_json::from_slice(&std::fs::read(path.join("meta.json"))?)?;
        let session = meta["session"].as_str().ok_or_else(|| anyhow::anyhow!("fixture held session is missing"))?.to_owned();
        let pg = i32::try_from(meta["pgid"].as_i64().ok_or_else(|| anyhow::anyhow!("fixture held PGID is missing"))?)?;
        anyhow::ensure!(pg > 0, "fixture held PGID must be positive");
        sessions.insert(session);
        records.push((path, Pid::from_raw(pg)));
    }
    let journal = c6_worker::journal::Journal::open(&state.join("journal.db")).await?;
    for session in sessions {
        // Production helper revalidates ownership before signaling and
        // requires whole-group ESRCH; saved fixture PIDs alone never suffice.
        c6_worker::claude_code::stop_held_for_rewind(&held, &session, &journal).await?;
        anyhow::ensure!(journal.claude_processes(&session).await?.is_empty(), "fixture retains a tracked Claude group");
    }
    for (path, pg) in records {
        anyhow::ensure!(killpg(pg, None) == Err(nix::errno::Errno::ESRCH), "fixture Claude group {pg} is still present");
        anyhow::ensure!(!path.exists(), "fixture held metadata survived cleanup: {}", path.display());
    }
    Ok(())
}
