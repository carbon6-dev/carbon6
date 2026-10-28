//! Knowledge in the head (docs/knowledge.md): the knowledge graph is part of
//! the head, not a service beside it. The memex's store (items, things,
//! claims, facts, search and vectors, one database per realm) lives under
//! the head's data folder, and its link calls the head in this process
//! instead of over the fabric.

use std::sync::{Arc, Weak};

use c6_proto::{CredentialGrant, CredentialRequest};
use serde_json::Value;

use crate::HeadState;

/// The head, as the memex's link sees it.
struct Local {
    st: Weak<HeadState>,
}

#[async_trait::async_trait]
impl c6_worker::link::LocalHead for Local {
    async fn call(&self, method: &str, params: Value) -> Result<Value, String> {
        let st = self.st.upgrade().ok_or("the head stopped")?;
        crate::fabric::memex_call(&st, method, params).await.map_err(|e| format!("{e:#}"))
    }

    async fn credential(&self, connection_id: &str) -> CredentialGrant {
        let req = CredentialRequest { request_id: 0, connection_id: connection_id.to_owned(), refresh: false, session_id: String::new() };
        match self.st.upgrade() {
            Some(st) => crate::sessions::credential_for(&st, None, "head", req).await,
            None => CredentialGrant { connection_id: connection_id.to_owned(), error: "the head stopped".into(), ..Default::default() },
        }
    }

    async fn pool(&self, connection_id: &str) -> Option<Arc<c6_llm::pool::Pool>> {
        let st = self.st.upgrade()?;
        crate::pools::get(&st, connection_id).await.unwrap_or_else(|e| {
            tracing::warn!(connection = %connection_id, "a pool: {e:#}");
            None
        })
    }
}

/// The knowledge folder in the head's data folder.
pub fn dir(st: &HeadState) -> std::path::PathBuf {
    st.data_dir.join("knowledge")
}

/// Whether a knowledge folder holds anything (items or things).
async fn holds_knowledge(dir: &std::path::Path) -> bool {
    let db = dir.join("memex.db");
    if !db.exists() {
        return false;
    }
    let opts = sqlx::sqlite::SqliteConnectOptions::new().filename(&db).read_only(true);
    let Ok(pool) = sqlx::sqlite::SqlitePoolOptions::new().max_connections(1).connect_with(opts).await else { return false };
    let n: i64 =
        sqlx::query_scalar("SELECT (SELECT count(*) FROM items) + (SELECT count(*) FROM things)").fetch_one(&pool).await.unwrap_or(0);
    pool.close().await;
    n > 0
}

/// Moves (or copies) a memex's folder in as the head's knowledge
/// (docs/knowledge.md §8). What the head had, if anything, is kept beside
/// it as `knowledge.before-<time>`. Nothing is copied when the folders are
/// on one disk.
pub fn move_in(from: &std::path::Path, data_dir: &std::path::Path, copy: bool) -> anyhow::Result<()> {
    anyhow::ensure!(from.join("memex.db").exists(), "no memex database in {}", from.display());
    let to = data_dir.join("knowledge");
    if to.exists() {
        let aside = data_dir.join(format!("knowledge.before-{}", c6_core::now_ms()));
        std::fs::rename(&to, &aside)?;
    }
    if copy {
        copy_dir(from, &to)?;
        return Ok(());
    }
    std::fs::rename(from, &to).map_err(|e| {
        anyhow::anyhow!("moving {} in: {e} (on another disk, copy it: c6h knowledge import --copy {})", from.display(), from.display())
    })
}

fn copy_dir(from: &std::path::Path, to: &std::path::Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(to)?;
    for e in std::fs::read_dir(from)? {
        let e = e?;
        let (src, dst) = (e.path(), to.join(e.file_name()));
        if e.file_type()?.is_dir() {
            copy_dir(&src, &dst)?;
        } else {
            std::fs::copy(&src, &dst)?;
        }
    }
    Ok(())
}

/// A memex that ran beside the head on this machine moves in when the head
/// first starts with knowledge of its own (docs/knowledge.md §8): its
/// folder becomes the head's knowledge, nothing copied, and `c6m` is done.
/// A `c6m` still running keeps its folder; you are asked to stop it.
pub async fn adopt_c6m(st: &HeadState, state_dirs: &[std::path::PathBuf]) -> anyhow::Result<()> {
    let target = dir(st);
    if state_dirs.is_empty() || holds_knowledge(&target).await {
        return Ok(());
    }
    for state in state_dirs {
        let src = state.join("memex");
        if !holds_knowledge(&src).await {
            continue;
        }
        let lock = std::fs::OpenOptions::new().read(true).write(true).open(state.join("c6w.lock"));
        if let Ok(f) = &lock
            && f.try_lock().is_err()
        {
            tracing::warn!(folder = %src.display(), "c6m is running: its knowledge moves into the head once it stops");
            let d = c6_api::DecisionIn {
                kind: "follow_up".into(),
                title: "Stop c6m so the head can take its knowledge in".into(),
                document: format!(
                    "The memex is part of the head now. Its knowledge in `{}` moves in (nothing is copied) the next time the head starts \
                     with `c6m` stopped:\n\n- `brew services stop c6m` (or `sudo systemctl disable --now c6m`), then restart the head.",
                    src.display()
                ),
                source: "knowledge".into(),
                ..Default::default()
            };
            crate::decisions::open(st, &d, crate::realms::ROOT, "knowledge:c6m-running").await.ok();
            return Ok(());
        }
        drop(lock);
        move_in(&src, &st.data_dir, false)?;
        tracing::info!(from = %src.display(), "the memex's knowledge moved into the head");
        sqlx::query("UPDATE workers SET revoked_at = ? WHERE role = 'memex' AND revoked_at IS NULL")
            .bind(c6_core::now_ms())
            .execute(&st.store.pool)
            .await?;
        let d = c6_api::DecisionIn {
            kind: "follow_up".into(),
            title: "Remove c6m: the head has its knowledge now".into(),
            document: format!(
                "The knowledge in `{}` moved into the head (`{}`); nothing was copied. `c6m` has nothing left to do:\n\n\
                 - Homebrew: `brew services stop c6m && brew uninstall c6m`\n- Ubuntu: `sudo apt remove c6m`\n\n\
                 Its folder `{}` keeps only its old identity; delete it when you like.",
                src.display(),
                target.display(),
                state.display()
            ),
            source: "knowledge".into(),
            ..Default::default()
        };
        crate::decisions::open(st, &d, crate::realms::ROOT, "").await.ok();
        return Ok(());
    }
    Ok(())
}

/// `c6h import-knowledge`: a memex folder (a `c6m`'s `memex/`, from this
/// machine or copied from another) becomes the head's knowledge. Run it
/// with the head stopped.
pub async fn import(cfg: &crate::HeadConfig, from: &std::path::Path, copy: bool) -> anyhow::Result<String> {
    move_in(from, &cfg.data_dir, copy)?;
    let store = crate::store::Store::open(&cfg.data_dir.join("c6h.db")).await?;
    sqlx::query("UPDATE workers SET revoked_at = ? WHERE role = 'memex' AND revoked_at IS NULL")
        .bind(c6_core::now_ms())
        .execute(&store.pool)
        .await?;
    Ok(format!(
        "{} {} into {}; start the head, and remove c6m (brew uninstall c6m, or apt remove c6m)",
        if copy { "copied" } else { "moved" },
        from.display(),
        cfg.data_dir.join("knowledge").display()
    ))
}

/// Opens the knowledge store and starts its work (the head always has it).
pub async fn open(st: &Arc<HeadState>, memex_dirs: &[std::path::PathBuf]) -> anyhow::Result<()> {
    if let Err(e) = adopt_c6m(st, memex_dirs).await {
        tracing::warn!("taking in the memex's knowledge: {e:#}");
    }
    let link = Arc::new(c6_worker::link::HeadLink::local(Arc::new(Local { st: Arc::downgrade(st) })));
    let m = c6_memex::Memex::open(&dir(st), link).await?;
    m.set_extractor(Arc::new(crate::extract::KitExtractor { st: Arc::downgrade(st) }));
    m.spawn_jobs();
    let _ = st.memex.set(m);
    Ok(())
}

/// The knowledge store, once open.
pub fn get(st: &HeadState) -> Option<Arc<c6_memex::Memex>> {
    st.memex.get().cloned()
}
