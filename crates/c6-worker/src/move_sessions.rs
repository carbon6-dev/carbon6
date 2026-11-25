//! Head-authorized, idempotent lifecycle RPCs for a fenced session move.
//! The target stays inactive until the head commits the ownership CAS.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use c6_proto::{CreateSession, SessionEvent, SessionEventBatch, SessionPlace};
use prost::Message as _;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{Worker, move_files};

#[derive(Clone, Serialize, Deserialize)]
struct Descriptor {
    source: move_files::Source,
    from: SessionPlaceWire,
    #[serde(default)]
    settings_current: bool,
}

impl Descriptor {
    fn apply_legacy_settings(&self, spec: &mut CreateSession, events: &[SessionEvent]) {
        // Only legacy exporters send the raw spec. Replaying the full history
        // over an effective spec could undo newer settings from Restore.
        if !self.settings_current {
            for event in events {
                crate::journal::apply_notice_settings_snapshot(spec, event);
            }
        }
    }
}

// Prost deliberately does not derive serde for the wire protocol.
#[derive(Clone, Serialize, Deserialize)]
struct SessionPlaceWire {
    cwd: String,
    repo_root: String,
    worktree_branch: String,
    repo: String,
    worker_id: String,
}
impl From<SessionPlaceWire> for SessionPlace {
    fn from(p: SessionPlaceWire) -> Self {
        Self { cwd: p.cwd, repo_root: p.repo_root, worktree_branch: p.worktree_branch, repo: p.repo, worker_id: p.worker_id }
    }
}

fn string<'a>(p: &'a Value, key: &str) -> Result<&'a str> {
    p[key].as_str().with_context(|| format!("move lacks {key}"))
}
fn epoch(p: &Value, key: &str) -> Result<u64> {
    p[key].as_u64().filter(|n| *n <= i64::MAX as u64).context("invalid ownership epoch")
}
fn config_dir() -> Option<PathBuf> {
    std::env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from).or_else(|| dirs::home_dir().map(|home| home.join(".claude")))
}

pub(crate) async fn handle(worker: &Arc<Worker>, method: &str, p: Value) -> Result<Value> {
    let id = Uuid::parse_str(string(&p, "move_id")?)?;
    let sid = string(&p, "session_id")?.to_owned();
    Uuid::parse_str(&sid)?;
    let source_epoch = epoch(&p, "source_epoch")?;
    let target_epoch = epoch(&p, "target_epoch")?;
    ensure!(target_epoch > source_epoch, "invalid move generation");
    let source_id = string(&p, "source_worker_id")?;
    let target_id = string(&p, "target_worker_id")?;
    let local = worker.identity.meta.worker_id.to_string();
    ensure!(source_id != target_id, "a worker move requires distinct workers");
    let source_side = matches!(method, "move.inspect" | "move.freeze" | "move.export" | "move.read" | "move.retire")
        || (method == "move.abort" && p["side"] == "source");
    ensure!(local == if source_side { source_id } else { target_id }, "move addressed to the wrong worker");
    let state = worker.state_dir.clone();
    let gate = worker.move_gates.lock().unwrap().entry(sid.clone()).or_default().clone();
    let _lifecycle = gate.lock().await;
    match method {
        "move.inspect" => {
            worker.journal.check_ownership(&sid, source_epoch).await?;
            let (spec, events, started) = worker.sessions.move_snapshot(&sid).await?;
            ensure!(
                spec.delegation.is_none() && spec.depth == 0 && started.ssh_target.is_empty(),
                "only local top-level sessions may move"
            );
            ensure!(
                spec.mcp_servers_json.trim().is_empty() || spec.mcp_servers_json.trim() == "[]",
                "sessions with MCP servers cannot move"
            );
            let native = if started.tool_profile == crate::claude_code::PROFILE {
                let conversation = crate::session::journal_claude_conversation(&worker.journal, &sid, &spec, &events).await?;
                Some(Uuid::parse_str(&conversation.id)?)
            } else {
                None
            };
            let cwd = PathBuf::from(&started.cwd);
            let repo = !started.repo.is_empty() || !started.repo_root.is_empty();
            let source = tokio::task::spawn_blocking(move || move_files::describe(&cwd, repo, native)).await??;
            Ok(serde_json::to_value(Descriptor {
                source,
                settings_current: true,
                from: SessionPlaceWire {
                    cwd: started.cwd,
                    repo_root: started.repo_root,
                    worktree_branch: started.worktree_branch,
                    repo: started.repo,
                    worker_id: local,
                },
            })?)
        }
        "move.preflight" => {
            let descriptor: Descriptor = serde_json::from_value(p["descriptor"].clone())?;
            if descriptor.source.native_resume.is_some() {
                crate::session::SessionManager::check_native_move_runtime()
                    .await
                    .context("Worker: destination cannot resume native Claude sessions")?;
            }
            // Only recorded folders are searched, not the user's home tree.
            let mut known = worker.journal.known_move_folders(&local).await?;
            if let Some(cwd) = p["request"]["previous_cwd"].as_str() {
                known.push(PathBuf::from(cwd));
            }
            if let Some(hints) = p["request"]["known_folders"].as_array() {
                known.extend(hints.iter().filter_map(Value::as_str).map(crate::expand_home));
            }
            let clone = p["request"]["clone"].as_bool().unwrap_or(false);
            let request = p["request"].clone();
            let plan = tokio::task::spawn_blocking(move || {
                if request["cwd"].is_string() {
                    move_files::preflight_selected(&descriptor.source, &state, id, &request).map_err(|error| {
                        if error.to_string().contains("does not match")
                            && let Some(repo) = &descriptor.source.repo
                            && let Some(candidate) = move_files::repository_folders(&state, &known, Some(&repo.remote)).first()
                        {
                            return anyhow::anyhow!(
                                "{error}; choose the matching checkout {} or Clone here in a new folder",
                                candidate.display()
                            );
                        }
                        error
                    })
                } else {
                    // Back is a fresh snapshot, not an in-place restore. Its
                    // historical cwd is nevertheless the unambiguous seed to
                    // prefer when a worker retains several matching checkouts.
                    if let Some(previous) = request["previous_cwd"].as_str()
                        && let Ok(plan) =
                            move_files::preflight_with_clone(&descriptor.source, &[crate::expand_home(previous)], &state, false)
                    {
                        return Ok(plan);
                    }
                    let known = move_files::repository_folders(&state, &known, descriptor.source.repo.as_ref().map(|r| r.remote.as_str()));
                    move_files::preflight_with_clone(&descriptor.source, &known, &state, clone)
                }
            })
            .await??;
            Ok(serde_json::to_value(plan)?)
        }
        "move.freeze" => {
            worker.sessions.freeze_for_move(&sid, source_epoch, &id.to_string()).await?;
            let (_, events, _) = worker.sessions.move_snapshot(&sid).await?;
            Ok(json!({"last_seq": events.last().context("source has no events")?.seq}))
        }
        "move.export" => {
            let (owned, blocked, move_id) = worker.journal.ownership(&sid).await?;
            ensure!(owned == source_epoch && blocked && move_id == id.to_string(), "source has not released this move");
            let descriptor: Descriptor = serde_json::from_value(p["descriptor"].clone())?;
            let (spec, events, started) = worker.sessions.move_snapshot(&sid).await?;
            ensure!(events.last().map(|e| e.seq) == p["last_seq"].as_u64(), "source watermark changed");
            if let Some(native) = descriptor.source.native_resume {
                let selected = crate::session::journal_claude_conversation(&worker.journal, &sid, &spec, &events).await?;
                ensure!(selected.id == native.to_string(), "native conversation changed before source release");
            }
            let spec = spec.encode_to_vec();
            let events = SessionEventBatch { session_id: sid.clone(), events, ownership_epoch: source_epoch }.encode_to_vec();
            let cwd = PathBuf::from(started.cwd);
            let config = config_dir();
            let manifest = tokio::task::spawn_blocking(move || {
                move_files::prepare_export(
                    &state,
                    id,
                    &sid,
                    &cwd,
                    &descriptor.source,
                    config.as_deref(),
                    &[("spec.pb", spec.as_slice()), ("events.pb", events.as_slice())],
                )
            })
            .await??;
            Ok(serde_json::to_value(manifest)?)
        }
        "move.read" => {
            let offset = p["offset"].as_u64().context("missing chunk offset")?;
            let bytes = tokio::task::spawn_blocking(move || move_files::read_chunk(&state, id, offset)).await??;
            ensure!(p["length"].as_u64() == Some(bytes.len() as u64), "chunk length differs from manifest");
            Ok(json!({"data": STANDARD.encode(bytes)}))
        }
        "move.write" => {
            let manifest: move_files::Manifest = serde_json::from_value(p["manifest"].clone())?;
            ensure!(manifest.move_id == id && manifest.session_id == sid, "artifact manifest belongs to another move");
            let plan: move_files::Target = serde_json::from_value(p["plan"].clone())?;
            let offset = p["offset"].as_u64().context("missing chunk offset")?;
            let encoded = string(&p, "data")?;
            ensure!(encoded.len() <= move_files::CHUNK_BYTES.div_ceil(3) * 4, "transfer chunk exceeds limit");
            let data = STANDARD.decode(encoded)?;
            tokio::task::spawn_blocking(move || {
                move_files::begin_import(&state, &manifest, &plan)?;
                move_files::write_chunk(&state, id, offset, &data)
            })
            .await??;
            Ok(json!({}))
        }
        "move.prepare" => {
            let descriptor: Descriptor = serde_json::from_value(p["descriptor"].clone())?;
            // Empty artifacts are legal too: initialize their staging without
            // depending on a first data chunk.
            let manifest: move_files::Manifest = serde_json::from_value(p["manifest"].clone())?;
            let copied_paths: Vec<String> = manifest.copied_files().map(str::to_owned).collect();
            let plan: move_files::Target = serde_json::from_value(p["plan"].clone())?;
            let config = config_dir();
            let prepared = tokio::task::spawn_blocking(move || {
                move_files::begin_import(&state, &manifest, &plan)?;
                move_files::finalize_import(&state, id, config.as_deref())
            })
            .await??;
            let mut spec = CreateSession::decode(tokio::fs::read(prepared.metadata_dir.join("spec.pb")).await?.as_slice())?;
            spec.labels.retain(|label| !label.starts_with("worker:"));
            spec.labels.push(format!("worker:{}", worker.identity.meta.worker_name));
            let batch = SessionEventBatch::decode(tokio::fs::read(prepared.metadata_dir.join("events.pb")).await?.as_slice())?;
            ensure!(
                batch.session_id == sid
                    && batch.ownership_epoch == source_epoch
                    && batch.events.last().map(|e| e.seq) == p["last_seq"].as_u64(),
                "imported history differs from the released source"
            );
            let mut events = batch.events;
            descriptor.apply_legacy_settings(&mut spec, &events);
            // Persist the exact event before importing its incarnation. A
            // replay cannot change its timestamp, notice or runtime prompt.
            let receipt = prepared.metadata_dir.join("place.pb");
            let event = if tokio::fs::try_exists(&receipt).await? {
                if descriptor.source.native_resume.is_some() {
                    crate::session::SessionManager::check_native_move_runtime().await?;
                }
                SessionEvent::decode(tokio::fs::read(&receipt).await?.as_slice())?
            } else {
                let to = SessionPlace {
                    cwd: prepared.cwd.display().to_string(),
                    repo: descriptor.from.repo.clone(),
                    repo_root: prepared.repo_root.as_ref().map(|p| p.display().to_string()).unwrap_or_default(),
                    worktree_branch: prepared.worktree_branch.clone(),
                    worker_id: local,
                };
                let event = worker
                    .sessions
                    .prepare_move_event(
                        &sid,
                        &spec,
                        &events,
                        c6_proto::PlaceChanged {
                            command_id: id.to_string(),
                            from: Some(descriptor.from.into()),
                            to: Some(to),
                            notice: p["request"]["notice_template"].as_str().unwrap_or("Moved from {{from}} to {{to}}.").to_owned(),
                            ownership_epoch: target_epoch,
                            native_resume_id: prepared.native_resume.map(|id| id.to_string()).unwrap_or_default(),
                            copied_paths,
                            uncommitted_files: prepared.uncommitted_files,
                            ..Default::default()
                        },
                    )
                    .await?;
                let data = event.encode_to_vec();
                let path = receipt.clone();
                tokio::task::spawn_blocking(move || -> Result<()> {
                    use std::io::Write as _;
                    let temp = path.with_extension("building");
                    let mut f = std::fs::File::create(&temp)?;
                    f.write_all(&data)?;
                    f.sync_all()?;
                    std::fs::rename(temp, &path)?;
                    std::fs::File::open(path.parent().context("missing receipt parent")?)?.sync_all()?;
                    Ok(())
                })
                .await??;
                event
            };
            events.push(event.clone());
            spec.cwd = prepared.cwd.display().to_string();
            spec.worktree = false;
            spec.worktree_spec = None;
            worker.sessions.import_move(&sid, &spec, &events, target_epoch, &id.to_string()).await?;
            Ok(json!({"event": STANDARD.encode(event.encode_to_vec())}))
        }
        "move.activate" => {
            // The only caller is the authenticated head after its CAS. The
            // local durable fence makes a repeated activation harmless.
            tokio::task::spawn_blocking(move || move_files::commit_import(&state, id)).await??;
            worker.journal.activate_ownership(&sid, target_epoch, &id.to_string()).await?;
            worker.journal.check_ownership(&sid, target_epoch).await?;
            worker.sessions.activate_move(&sid).await?;
            Ok(json!({}))
        }
        "move.abort" if source_side => {
            worker.journal.abort_source_move(&sid, source_epoch, &id.to_string()).await?;
            // The head may have recorded freeze intent without the source
            // ever receiving it. A restart can trim the fully synced session
            // in that window. Acknowledge the durable abort so the head can
            // finish rollback and restore the missing cache on demand.
            if worker.journal.has_session(&sid).await? {
                worker.sessions.activate_move(&sid).await?;
            }
            Ok(json!({}))
        }
        "move.abort" => {
            // A target fence is deliberately retained: stale commands and old
            // actors must not gain authority after an aborted preparation.
            worker.journal.abort_target_move(&sid, target_epoch, &id.to_string()).await?;
            tokio::task::spawn_blocking(move || move_files::abort_import(&state, id)).await??;
            Ok(json!({}))
        }
        "move.retire" => {
            tokio::task::spawn_blocking(move || move_files::finish_export(&state, id)).await??;
            Ok(json!({}))
        }
        _ => anyhow::bail!("unknown session move operation"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn legacy_descriptor() -> Value {
        json!({
            "source": {"repo": null, "native_resume": null},
            "from": {"cwd": "/source", "repo_root": "", "worktree_branch": "", "repo": "", "worker_id": "source"},
        })
    }

    fn historical_settings() -> [SessionEvent; 1] {
        [SessionEvent {
            seq: 2,
            kind: Some(c6_proto::session_event::Kind::Notice(c6_proto::Notice {
                title: "Notice settings refreshed".into(),
                quiet: true,
                message: json!({"notice_quiet_seconds": 6, "max_subagent_depth": 7}).to_string(),
                ..Default::default()
            })),
            ..Default::default()
        }]
    }

    #[test]
    fn legacy_move_folds_settings_from_history() {
        let descriptor: Descriptor = serde_json::from_value(legacy_descriptor()).unwrap();
        assert!(!descriptor.settings_current);
        let mut spec = CreateSession::default();
        descriptor.apply_legacy_settings(&mut spec, &historical_settings());
        assert_eq!((spec.notice_quiet_seconds, spec.max_subagent_depth), (6, 7));
    }

    #[test]
    fn current_move_preserves_effective_settings_over_history() {
        let mut wire = legacy_descriptor();
        wire["settings_current"] = json!(true);
        let descriptor: Descriptor = serde_json::from_value(wire).unwrap();
        let effective = CreateSession { notice_quiet_seconds: 19, max_subagent_depth: 2, ..Default::default() };
        let mut spec = effective.clone();
        descriptor.apply_legacy_settings(&mut spec, &historical_settings());
        assert_eq!(spec, effective);
    }
}
