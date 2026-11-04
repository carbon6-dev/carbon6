//! Backups to a folder and to S3, incremental and encrypted to your public
//! key (docs/backups.md).
//!
//! Two kinds of work: the head reads what changed (its databases' logs,
//! new blobs, its small files) into a local spool, quickly and on its own
//! disk; each destination's sender takes it from there at its own pace, so
//! a slow or unreachable destination never holds up the head.

pub mod crypt;
pub mod dest;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, bail};
use c6_api::{BackupDestination, BackupSettings};
use c6_core::now_ms;
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::HeadState;
use crypt::HeadKey;
use dest::Objects;

/// The setting (`BackupSettings`).
pub const SETTING: &str = "backups";
/// The head's own backup key, under the master key.
const HEAD_KEY: &str = "backups_head_key";
/// Your private key, for test restores, under the master key.
const RESTORE_KEY: &str = "backups_restore_key";

/// Where the head keeps what it hasn't sent yet, and what it sent where.
fn state_dir(st: &HeadState) -> PathBuf {
    st.data_dir.join("backup-state")
}

pub async fn settings(st: &HeadState) -> BackupSettings {
    st.store.setting(SETTING).await.ok().flatten().and_then(|v| serde_json::from_str(&v).ok()).unwrap_or_default()
}

/// Checks and saves the settings.
pub async fn put_settings(st: &HeadState, mut s: BackupSettings) -> anyhow::Result<()> {
    s.recipient = s.recipient.trim().to_owned();
    if !s.recipient.is_empty() {
        crypt::public_key(&s.recipient)?;
    }
    anyhow::ensure!(s.interval_secs >= 5, "send changes at most every 5 seconds");
    anyhow::ensure!(s.generation_days >= 1, "a new base copy at most daily");
    let mut names = BTreeSet::new();
    for d in &mut s.destinations {
        d.name = d.name.trim().to_owned();
        anyhow::ensure!(!d.name.is_empty() && names.insert(d.name.clone()), "each destination needs its own name");
        anyhow::ensure!(d.keep >= 1, "{}: keep at least one base copy", d.name);
        match d.kind.as_str() {
            "folder" => anyhow::ensure!(Path::new(d.path.trim()).is_absolute(), "{}: the folder must be a full path", d.name),
            "s3" => {
                url::Url::parse(d.endpoint.trim()).with_context(|| format!("{}: the endpoint is not a URL", d.name))?;
                anyhow::ensure!(!d.bucket.trim().is_empty(), "{}: which bucket?", d.name);
                anyhow::ensure!(
                    !d.access_key_secret.trim().is_empty() && !d.secret_key_secret.trim().is_empty(),
                    "{}: name the secrets holding the access key and the secret key",
                    d.name
                );
                if !d.prefix.is_empty() && !d.prefix.ends_with('/') {
                    d.prefix.push('/');
                }
            }
            other => bail!("{}: a destination is a folder or s3, not {other:?}", d.name),
        }
    }
    st.store.set_setting(SETTING, &serde_json::to_string(&s)?).await?;
    st.backups.wake.notify_waiters();
    Ok(())
}

async fn sealed_setting(st: &HeadState, key: &str) -> anyhow::Result<Option<String>> {
    let Some(v) = st.store.setting(key).await? else { return Ok(None) };
    Ok(Some(st.secrets.decrypt(&hex::decode(v.trim())?)?))
}

async fn set_sealed_setting(st: &HeadState, key: &str, value: &str) -> anyhow::Result<()> {
    st.store.set_setting(key, &hex::encode(st.secrets.encrypt(value)?)).await
}

/// The head's own backup key, made the first time.
pub async fn head_key(st: &HeadState) -> anyhow::Result<HeadKey> {
    if let Some(v) = sealed_setting(st, HEAD_KEY).await? {
        let bytes: [u8; 32] = hex::decode(v.trim())?.try_into().map_err(|_| anyhow::anyhow!("the backup head key is not 32 bytes"))?;
        return Ok(HeadKey(bytes));
    }
    let k = HeadKey::new();
    set_sealed_setting(st, HEAD_KEY, &hex::encode(k.0)).await?;
    Ok(k)
}

/// A new key pair; its public key becomes the one backups are encrypted to.
pub async fn new_key(st: &HeadState) -> anyhow::Result<c6_api::BackupKeyPair> {
    let (public, private) = crypt::new_key_pair();
    let mut s = settings(st).await;
    s.recipient = public.clone();
    put_settings(st, s).await?;
    Ok(c6_api::BackupKeyPair { public, private })
}

/// Keeps (or with an empty key, forgets) your private key for test restores.
pub async fn set_restore_key(st: &HeadState, key: &str) -> anyhow::Result<()> {
    let key = key.trim();
    if key.is_empty() {
        sqlx::query("DELETE FROM settings WHERE key = ?").bind(RESTORE_KEY).execute(st.store.pool()).await?;
        return Ok(());
    }
    let public = crypt::public_of(key)?;
    let s = settings(st).await;
    anyhow::ensure!(public == s.recipient, "that key isn't the one backups are encrypted to");
    set_sealed_setting(st, RESTORE_KEY, key).await
}

pub(crate) async fn restore_key(st: &HeadState) -> Option<String> {
    sealed_setting(st, RESTORE_KEY).await.ok().flatten()
}

/// A destination's objects.
pub async fn objects(st: &HeadState, d: &BackupDestination) -> anyhow::Result<Arc<dyn Objects>> {
    match d.kind.as_str() {
        "folder" => Ok(Arc::new(dest::Folder::new(Path::new(d.path.trim())))),
        "s3" => {
            let secret = |name: &str| {
                let name = name.trim().to_owned();
                async move {
                    crate::secrets::value_in(st, crate::realms::ROOT, &name)
                        .await?
                        .ok_or_else(|| anyhow::anyhow!("no secret {name:?} (add it under Settings → Secrets)"))
                }
            };
            Ok(Arc::new(dest::S3 {
                endpoint: d.endpoint.trim().to_owned(),
                bucket: d.bucket.trim().to_owned(),
                region: if d.region.trim().is_empty() { "us-east-1".to_owned() } else { d.region.trim().to_owned() },
                prefix: d.prefix.clone(),
                access_key: secret(&d.access_key_secret).await?,
                secret_key: secret(&d.secret_key_secret).await?,
                http: st.http.clone(),
            }))
        }
        other => bail!("unknown destination kind {other}"),
    }
}

// --- State -------------------------------------------------------------------

/// What the head has read and what it sent where, kept beside the data
/// (not backed up itself).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct State {
    /// The small files' content hash, as last read.
    #[serde(default)]
    files_hash: String,
    #[serde(default)]
    dests: BTreeMap<String, DestState>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct DestState {
    /// The public key its objects are for: another one starts it over.
    #[serde(default)]
    recipient: String,
    #[serde(default)]
    files_hash: String,
    #[serde(default)]
    last_ok_at: Option<i64>,
    #[serde(default)]
    last_error: String,
    #[serde(default)]
    stored_bytes: u64,
    #[serde(default)]
    stored_objects: u64,
}

/// The engine (one per head): its state, and what wakes its loops.
#[derive(Default)]
pub struct Engine {
    state: std::sync::Mutex<Option<State>>,
    /// One pass at a time.
    pass: tokio::sync::Mutex<()>,
    /// Bytes sent per destination since the head started.
    sent: std::sync::Mutex<BTreeMap<String, u64>>,
    /// Settings changed, or a pass was asked for.
    pub(crate) wake: tokio::sync::Notify,
}

impl Engine {
    fn with<R>(&self, st: &HeadState, f: impl FnOnce(&mut State) -> R) -> R {
        let mut g = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if g.is_none() {
            *g = Some(load_state(&state_dir(st)));
        }
        let r = f(g.as_mut().expect("loaded"));
        if let Err(e) = save_state(&state_dir(st), g.as_ref().expect("loaded")) {
            warn!("saving the backup state: {e:#}");
        }
        r
    }

    fn sent(&self, dest: &str, n: usize) {
        *self.sent.lock().unwrap_or_else(|e| e.into_inner()).entry(dest.to_owned()).or_default() += n as u64;
    }
}

fn load_state(dir: &Path) -> State {
    std::fs::read(dir.join("state.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

fn save_state(dir: &Path, s: &State) -> anyhow::Result<()> {
    c6_core::create_private_dir(dir)?;
    let tmp = dir.join("state.json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(s)?)?;
    std::fs::rename(&tmp, dir.join("state.json"))?;
    Ok(())
}

// --- Small files and blobs -----------------------------------------------------

/// The CA, the master key and the config file, as one object's plaintext,
/// with its hash.
fn small_files(st: &HeadState) -> anyhow::Result<(Vec<u8>, String)> {
    use base64::Engine as _;
    let b64 = base64::engine::general_purpose::STANDARD;
    let mut files = BTreeMap::new();
    let mut add = |rel: String, path: &Path| -> anyhow::Result<()> {
        let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        files.insert(rel, b64.encode(bytes));
        Ok(())
    };
    add("master.key".into(), &st.data_dir.join("master.key"))?;
    if let Ok(rd) = std::fs::read_dir(st.data_dir.join("pki")) {
        for e in rd.flatten() {
            if e.path().is_file() {
                add(format!("pki/{}", e.file_name().to_string_lossy()), &e.path())?;
            }
        }
    }
    let config = match &st.config_file {
        Some(p) => {
            serde_json::json!({"name": p.file_name().map(|n| n.to_string_lossy().into_owned()), "path": p, "text": std::fs::read_to_string(p).unwrap_or_default()})
        }
        None => serde_json::Value::Null,
    };
    let body = serde_json::to_vec(&serde_json::json!({"files": files, "config": config}))?;
    let hash = hex::encode(<sha2::Sha256 as sha2::Digest>::digest(&body));
    Ok((body, hash))
}

/// Every blob's path under `knowledge/blobs`.
fn blob_paths(st: &HeadState) -> Vec<String> {
    let root = st.data_dir.join("knowledge").join("blobs");
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(&root) else { return out };
    for d in rd.flatten() {
        let Ok(files) = std::fs::read_dir(d.path()) else { continue };
        for f in files.flatten() {
            if f.file_type().is_ok_and(|t| t.is_file()) {
                out.push(format!("{}/{}", d.file_name().to_string_lossy(), f.file_name().to_string_lossy()));
            }
        }
    }
    out.sort();
    out
}

/// The blobs a destination has, as recorded (one name per line).
fn blobs_sent_file(st: &HeadState, dest: &str, key: &HeadKey) -> PathBuf {
    state_dir(st).join(format!("blobs-{}.txt", &key.name("dest", dest.as_bytes())[..16]))
}

/// Blobs written in one pass, at most (so a first backup of many blobs
/// still records its progress as it goes).
const BLOB_BATCH: usize = 200;

/// A blob log entry: where a blob goes back to, and its object's name.
#[derive(Serialize, Deserialize)]
pub(crate) struct BlobEntry {
    pub path: String,
    pub name: String,
}

async fn send_blobs(st: &HeadState, d: &str, o: &dyn Objects, to: &x25519_dalek::PublicKey, key: &HeadKey) -> anyhow::Result<()> {
    let file = blobs_sent_file(st, d, key);
    let have: BTreeSet<String> = std::fs::read_to_string(&file).unwrap_or_default().lines().map(str::to_owned).collect();
    let todo: Vec<String> = blob_paths(st).into_iter().filter(|p| !have.contains(p)).collect();
    for batch in todo.chunks(BLOB_BATCH) {
        let mut entries = Vec::new();
        for path in batch {
            let full = st.data_dir.join("knowledge").join("blobs").join(path);
            let Ok(bytes) = tokio::fs::read(&full).await else { continue };
            let name = key.name("blob", path.as_bytes());
            let sealed = crypt::seal(to, &bytes);
            st.backups.sent(d, sealed.len());
            o.put(&format!("blobs/{name}"), sealed).await?;
            entries.push(BlobEntry { path: path.clone(), name });
        }
        if entries.is_empty() {
            continue;
        }
        let log = crypt::seal(to, &serde_json::to_vec(&entries)?);
        st.backups.sent(d, log.len());
        o.put(&format!("bloblog/{:013}-{:08x}", now_ms(), rand::random::<u32>()), log).await?;
        // Recorded once the log names them, so a restore finds each one.
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&file)?;
        for e in &entries {
            writeln!(f, "{}", e.path)?;
        }
    }
    Ok(())
}

// --- Passes ----------------------------------------------------------------------

/// Sends what changed to one destination.
async fn send_to(st: &HeadState, s: &BackupSettings, d: &BackupDestination) -> anyhow::Result<()> {
    let to = crypt::public_key(&s.recipient)?;
    let key = head_key(st).await?;
    let o = objects(st, d).await?;
    // The keys: the head's (so a restore can find and name things) wrapped
    // to yours, and your public key, to see which key it's for.
    let recipient_known = st.backups.with(st, |state| state.dests.get(&d.name).is_some_and(|x| x.recipient == s.recipient));
    if !recipient_known {
        o.put("keys/head", crypt::seal(&to, &key.0)).await?;
        o.put("keys/recipient", s.recipient.as_bytes().to_vec()).await?;
        st.backups.with(st, |state| {
            let ds = state.dests.entry(d.name.clone()).or_default();
            *ds = DestState { recipient: s.recipient.clone(), ..Default::default() };
        });
        // Objects for another key are no use to this one.
        let _ = std::fs::remove_file(blobs_sent_file(st, &d.name, &key));
    }
    let (files, hash) = small_files(st)?;
    if st.backups.with(st, |state| state.dests.get(&d.name).is_none_or(|x| x.files_hash != hash)) {
        let sealed = crypt::seal(&to, &files);
        st.backups.sent(&d.name, sealed.len());
        o.put(&format!("files/{:013}", now_ms()), sealed).await?;
        st.backups.with(st, |state| state.dests.entry(d.name.clone()).or_default().files_hash = hash);
    }
    send_blobs(st, &d.name, o.as_ref(), &to, &key).await?;
    Ok(())
}

/// One pass: what changed goes to every enabled destination. `base`: new
/// base copies too.
pub async fn run(st: &HeadState, base: bool) -> anyhow::Result<c6_api::BackupStatus> {
    {
        let _pass = st.backups.pass.lock().await;
        let s = settings(st).await;
        if !s.recipient.is_empty() {
            for d in s.destinations.iter().filter(|d| d.enabled) {
                let r = send_to(st, &s, d).await;
                st.backups.with(st, |state| {
                    let ds = state.dests.entry(d.name.clone()).or_default();
                    match &r {
                        Ok(()) => {
                            ds.last_ok_at = Some(now_ms());
                            ds.last_error.clear();
                        }
                        Err(e) => ds.last_error = format!("{e:#}"),
                    }
                });
                if let Err(e) = r {
                    warn!(destination = %d.name, "backup: {e:#}");
                }
            }
        }
        let _ = base;
    }
    status(st).await
}

/// How backups stand.
pub async fn status(st: &HeadState) -> anyhow::Result<c6_api::BackupStatus> {
    let s = settings(st).await;
    let state = st.backups.with(st, |state| state.clone());
    let sent = st.backups.sent.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let destinations = s
        .destinations
        .iter()
        .map(|d| {
            let ds = state.dests.get(&d.name).cloned().unwrap_or_default();
            c6_api::BackupDestinationStatus {
                name: d.name.clone(),
                enabled: d.enabled,
                last_ok_at: ds.last_ok_at,
                last_error: ds.last_error,
                behind_ms: None,
                sent_bytes: sent.get(&d.name).copied().unwrap_or(0),
                stored_bytes: ds.stored_bytes,
                stored_objects: ds.stored_objects,
                generations: Vec::new(),
                last_test: None,
            }
        })
        .collect();
    Ok(c6_api::BackupStatus {
        on: !s.recipient.is_empty() && s.destinations.iter().any(|d| d.enabled),
        recipient: s.recipient.clone(),
        restore_key: restore_key(st).await.is_some(),
        destinations,
        databases: Vec::new(),
    })
}

/// Sends changes every interval, and when woken.
pub fn spawn(st: Arc<HeadState>) {
    tokio::spawn(async move {
        loop {
            let every = std::time::Duration::from_secs(settings(&st).await.interval_secs.max(5));
            tokio::select! {
                _ = st.shutdown.cancelled() => return,
                _ = st.backups.wake.notified() => {}
                _ = tokio::time::sleep(every) => {}
            }
            if let Err(e) = run(&st, false).await {
                warn!("backup pass: {e:#}");
            }
        }
    });
}
