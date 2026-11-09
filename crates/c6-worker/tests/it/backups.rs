//! Backups to a folder and to a stand-in S3 (docs/backups.md): encrypted
//! to your key, sending only what changed.

use std::path::Path;

use c6_api::{BackupDestination, BackupSettings};
use c6_head::HeadConfig;
use c6_head::backups::{self, crypt};

pub(crate) fn config(dir: &Path) -> HeadConfig {
    HeadConfig {
        data_dir: dir.to_owned(),
        bind: vec!["127.0.0.1".parse().unwrap()],
        ui_port: 0,
        worker_port: 0,
        auth: false,
        ..Default::default()
    }
}

pub(crate) fn folder(name: &str, path: &Path) -> BackupDestination {
    BackupDestination {
        name: name.into(),
        kind: "folder".into(),
        path: path.to_string_lossy().into_owned(),
        endpoint: String::new(),
        bucket: String::new(),
        region: String::new(),
        prefix: String::new(),
        access_key_secret: String::new(),
        secret_key_secret: String::new(),
        keep: 4,
        enabled: true,
    }
}

pub(crate) fn s3(name: &str, endpoint: &str) -> BackupDestination {
    BackupDestination {
        kind: "s3".into(),
        endpoint: endpoint.into(),
        bucket: "bucket".into(),
        prefix: "c6".into(),
        access_key_secret: "S3_KEY".into(),
        secret_key_secret: "S3_SECRET".into(),
        ..folder(name, Path::new("/"))
    }
}

/// Sets up backups to a folder and the stand-in S3; your private key.
pub(crate) async fn set_up(st: &c6_head::HeadState, nas: &Path, s3_endpoint: &str) -> String {
    c6_head::secrets::put_in(st, "root", "S3_KEY", "AKTEST", "").await.unwrap();
    // As Settings → Secrets keeps them: in home.
    c6_head::secrets::put_in(st, "home", "S3_SECRET", "secret", "").await.unwrap();
    let pair = backups::new_key(st).await.unwrap();
    let mut s = backups::settings(st).await;
    s.destinations = vec![folder("nas", nas), s3("s3", s3_endpoint)];
    backups::put_settings(st, s).await.unwrap();
    pair.private
}

#[tokio::test]
async fn blobs_and_keys_go_to_both_destinations_once() {
    let mock = c6_testkit::MockS3::start("AKTEST", "bucket").await;
    let (dir, nas) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let head = c6_head::start(config(dir.path())).await.unwrap();
    let st = head.state.clone();
    let private = set_up(&st, nas.path(), &mock.endpoint()).await;
    let blobs = dir.path().join("knowledge/blobs/ab");
    std::fs::create_dir_all(&blobs).unwrap();
    std::fs::write(blobs.join("ab12"), b"a photo's bytes").unwrap();

    let status = backups::run(&st, false).await.unwrap();
    assert!(status.on);
    for d in &status.destinations {
        assert_eq!(d.last_error, "", "{d:?}");
        assert!(d.last_ok_at.is_some());
    }
    let key = crypt::private_key(&private).unwrap();
    let nas_names: Vec<String> = walk(nas.path());
    for prefix in ["keys/head", "keys/recipient", "files/", "blobs/", "bloblog/"] {
        assert!(nas_names.iter().any(|n| n.starts_with(prefix)), "{prefix} on the NAS: {nas_names:?}");
        assert!(mock.keys().iter().any(|n| n.starts_with(&format!("c6/{prefix}"))), "{prefix} in S3: {:?}", mock.keys());
    }
    // Only your key reads them; the blob comes back as it was.
    let blob = nas_names.iter().find(|n| n.starts_with("blobs/")).unwrap();
    let sealed = std::fs::read(nas.path().join(blob)).unwrap();
    assert!(!sealed.windows(5).any(|w| w == b"photo"), "encrypted");
    assert_eq!(crypt::open(&key, &sealed).unwrap(), b"a photo's bytes");
    let files = nas_names.iter().find(|n| n.starts_with("files/")).unwrap();
    let files: serde_json::Value =
        serde_json::from_slice(&crypt::open(&key, &std::fs::read(nas.path().join(files)).unwrap()).unwrap()).unwrap();
    assert!(files["files"]["master.key"].is_string() && files["files"]["pki/ca.key"].is_string(), "{files}");

    // Nothing new: no blob sent again. A new one: just it, and its log entry.
    let blob_objects = |prefix: &str| mock.keys().iter().filter(|k| k.starts_with(&format!("c6/{prefix}"))).count();
    let nas_objects = |prefix: &str| walk(nas.path()).iter().filter(|k| k.starts_with(prefix)).count();
    backups::run(&st, false).await.unwrap();
    assert_eq!((blob_objects("blobs/"), blob_objects("bloblog/")), (1, 1));
    std::fs::write(blobs.join("ab34"), b"another").unwrap();
    backups::run(&st, false).await.unwrap();
    assert_eq!((blob_objects("blobs/"), blob_objects("bloblog/")), (2, 2));
    assert_eq!((nas_objects("blobs/"), nas_objects("bloblog/")), (2, 2));

    // An unreachable destination says so, and the other carries on.
    mock.set_down(true);
    std::fs::write(blobs.join("ab56"), b"third").unwrap();
    let status = backups::run(&st, false).await.unwrap();
    let s3 = status.destinations.iter().find(|d| d.name == "s3").unwrap();
    assert!(s3.last_error.contains("503"), "{s3:?}");
    assert_eq!(nas_objects("blobs/"), 3);
    mock.set_down(false);
    let status = backups::run(&st, false).await.unwrap();
    assert!(status.destinations.iter().all(|d| d.last_error.is_empty()), "{status:?}");
    assert_eq!(mock.keys().iter().filter(|k| k.starts_with("c6/blobs/")).count(), 3);
    head.shutdown().await.unwrap();
    let _ = BackupSettings::default();
}

/// Every object's name under `root`.
pub(crate) fn walk(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_owned()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).unwrap().flatten() {
            if e.path().is_dir() {
                stack.push(e.path());
            } else {
                out.push(e.path().strip_prefix(root).unwrap().to_string_lossy().into_owned());
            }
        }
    }
    out.sort();
    out
}

async fn work_titles(api: &str) -> Vec<String> {
    let v: serde_json::Value = reqwest::get(format!("{api}/work?state=all")).await.unwrap().json().await.unwrap();
    let mut t: Vec<String> = v.as_array().unwrap().iter().filter_map(|w| w["title"].as_str().map(str::to_owned)).collect();
    t.sort();
    t
}

async fn add_work(api: &str, title: &str) {
    let r = reqwest::Client::new().post(format!("{api}/work")).json(&serde_json::json!({"title": title})).send().await.unwrap();
    assert!(r.status().is_success(), "{}", r.text().await.unwrap());
}

/// The time after `t`, once the clock has passed it.
async fn after(t: i64) -> i64 {
    while c6_core::now_ms() <= t {
        tokio::task::yield_now().await;
    }
    c6_core::now_ms()
}

#[tokio::test]
async fn databases_come_back_as_of_any_time() {
    let mock = c6_testkit::MockS3::start("AKTEST", "bucket").await;
    let (dir, nas) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let head = c6_head::start(config(dir.path())).await.unwrap();
    let st = head.state.clone();
    let api = format!("http://127.0.0.1:{}/api/v1", head.ui_addrs[0].port());
    let private = set_up(&st, nas.path(), &mock.endpoint()).await;
    add_work(&api, "A").await;

    // The first pass takes base copies of every database at both.
    let status = backups::run(&st, false).await.unwrap();
    for d in &status.destinations {
        assert_eq!(d.last_error, "", "{d:?}");
        assert!(d.generations.iter().any(|g| g.database == "c6h.db" && g.complete), "{d:?}");
        assert!(d.generations.iter().any(|g| g.database == "knowledge/memex.db" && g.complete), "{d:?}");
    }
    add_work(&api, "B").await;
    backups::run(&st, false).await.unwrap();
    let t1 = after(c6_core::now_ms()).await;
    add_work(&api, "C").await;
    let status = backups::run(&st, false).await.unwrap();
    assert!(status.destinations.iter().all(|d| d.behind_ms.is_none() && d.last_error.is_empty()), "{status:?}");

    // Quiet: nothing is sent.
    let (puts, bytes) = mock.puts();
    backups::run(&st, false).await.unwrap();
    backups::run(&st, false).await.unwrap();
    let quiet = mock.puts();
    assert!(quiet.0 - puts <= 2, "at most the head's own housekeeping: {puts} → {}", quiet.0);
    eprintln!("quiet passes sent {} objects, {} bytes", quiet.0 - puts, quiet.1 - bytes);

    // From the NAS as of t1: A and B, not C.
    let nas_dest = c6_head::backups::dest::Folder::new(nas.path());
    let back = tempfile::tempdir().unwrap();
    let r = backups::restore::restore(&nas_dest, &private, Some(t1), back.path()).await.unwrap();
    assert!(r.ok(), "{}", r.text());
    assert!(back.path().join("master.key").is_file() && back.path().join("pki/ca.key").is_file());
    assert_eq!(r.backups_off, 2, "{}", r.text());
    let restored = c6_head::start(config(back.path())).await.unwrap();
    let rapi = format!("http://127.0.0.1:{}/api/v1", restored.ui_addrs[0].port());
    assert_eq!(work_titles(&rapi).await, ["A", "B"]);
    assert!(!backups::status(&restored.state).await.unwrap().on, "the restored head doesn't send to the same places");
    restored.shutdown().await.unwrap();

    // From S3, the newest: all three.
    let s3 = c6_head::backups::dest::S3 {
        endpoint: mock.endpoint(),
        bucket: "bucket".into(),
        region: "us-east-1".into(),
        prefix: "c6/".into(),
        access_key: "AKTEST".into(),
        secret_key: "secret".into(),
        http: reqwest::Client::new(),
    };
    let back2 = tempfile::tempdir().unwrap();
    let r = backups::restore::restore(&s3, &private, None, back2.path()).await.unwrap_or_else(|e| {
        panic!("{e:#}: {:?}", mock.keys().iter().filter(|k| !k.contains("/chunks/") && !k.contains("/seg/")).collect::<Vec<_>>())
    });
    assert!(r.ok(), "{}", r.text());
    let restored = c6_head::start(config(back2.path())).await.unwrap();
    let rapi = format!("http://127.0.0.1:{}/api/v1", restored.ui_addrs[0].port());
    assert_eq!(work_titles(&rapi).await, ["A", "B", "C"]);
    restored.shutdown().await.unwrap();

    // Another key can't, and a restore won't overwrite a data folder.
    let (_, other) = crypt::new_key_pair();
    let back3 = tempfile::tempdir().unwrap();
    assert!(backups::restore::restore(&nas_dest, &other, None, back3.path()).await.is_err());
    assert!(backups::restore::restore(&nas_dest, &private, None, dir.path()).await.is_err());
    // Changed outside the head (another program wrote and checkpointed it):
    // the next pass starts new base copies, which have the change.
    {
        use sqlx::Connection;
        let opts = sqlx::sqlite::SqliteConnectOptions::new().filename(dir.path().join("c6h.db"));
        let mut c = sqlx::SqliteConnection::connect_with(&opts).await.unwrap();
        sqlx::query("INSERT INTO settings (key, value) VALUES ('written_outside', 'yes')").execute(&mut c).await.unwrap();
        sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)").execute(&mut c).await.unwrap();
        c.close().await.unwrap();
    }
    backups::run(&st, false).await.unwrap();
    let status = backups::run(&st, false).await.unwrap();
    assert!(status.destinations.iter().all(|d| d.last_error.is_empty()), "{status:?}");
    let back4 = tempfile::tempdir().unwrap();
    backups::restore::restore(&nas_dest, &private, None, back4.path()).await.unwrap();
    {
        use sqlx::Connection;
        let opts = sqlx::sqlite::SqliteConnectOptions::new().filename(back4.path().join("c6h.db"));
        let mut c = sqlx::SqliteConnection::connect_with(&opts).await.unwrap();
        let v: Option<String> =
            sqlx::query_scalar("SELECT value FROM settings WHERE key = 'written_outside'").fetch_optional(&mut c).await.unwrap();
        assert_eq!(v.as_deref(), Some("yes"), "the outside change is backed up too");
    }
    // Without your key: every object of the newest base copies is there.
    let key = backups::head_key(&st).await.unwrap();
    let v = backups::restore::verify(&nas_dest, &key).await.unwrap();
    assert!(v.contains("all there"), "{v}");
    head.shutdown().await.unwrap();
}

#[tokio::test]
async fn base_copies_send_what_changed_and_old_ones_go() {
    let mock = c6_testkit::MockS3::start("AKTEST", "bucket").await;
    let (dir, nas) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let head = c6_head::start(config(dir.path())).await.unwrap();
    let st = head.state.clone();
    let api = format!("http://127.0.0.1:{}/api/v1", head.ui_addrs[0].port());
    let private = set_up(&st, nas.path(), &mock.endpoint()).await;
    let mut s = backups::settings(&st).await;
    for d in &mut s.destinations {
        d.keep = 2;
    }
    backups::put_settings(&st, s).await.unwrap();
    for i in 0..50 {
        add_work(&api, &format!("work {i}")).await;
    }
    backups::run(&st, false).await.unwrap();
    let chunks = |m: &c6_testkit::MockS3| m.keys().iter().filter(|k| k.starts_with("c6/chunks/")).count();
    let first = chunks(&mock);
    assert!(first > 0);

    // A new base copy of an unchanged database sends no chunk again.
    add_work(&api, "one more").await;
    let (puts, _) = mock.puts();
    backups::run(&st, true).await.unwrap();
    let (after, _) = mock.puts();
    let gens_now = |m: &c6_testkit::MockS3| {
        let mut g: Vec<String> = m.keys().iter().filter(|k| k.ends_with("/manifest")).cloned().collect();
        g.sort();
        g
    };
    eprintln!("a second base copy wrote {} objects ({} chunks in all, {first} at first)", after - puts, chunks(&mock));
    assert!(chunks(&mock) <= first + 2 * gens_now(&mock).len(), "few new chunks: {} then {first}", chunks(&mock));

    // Keep 2: a third base copy drops the first, and chunks nothing uses.
    backups::run(&st, true).await.unwrap();
    let dbs = gens_now(&mock).iter().map(|k| k.split('/').nth(2).unwrap().to_owned()).collect::<std::collections::BTreeSet<_>>();
    for db in &dbs {
        assert_eq!(gens_now(&mock).iter().filter(|k| k.contains(db.as_str())).count(), 2, "{:?}", gens_now(&mock));
    }
    let nas_manifests = walk(nas.path()).into_iter().filter(|k| k.ends_with("/manifest")).count();
    assert_eq!(nas_manifests, 2 * dbs.len());
    let status = backups::status(&st).await.unwrap();
    assert!(status.destinations.iter().all(|d| d.stored_objects > 0 && d.stored_bytes > 0), "{status:?}");

    // Too far behind: S3 starts again from a new base copy, and has it all.
    let mut s = backups::settings(&st).await;
    s.spool_limit_mb = 0;
    backups::put_settings(&st, s).await.unwrap();
    mock.set_down(true);
    add_work(&api, "while S3 was down").await;
    let status = backups::run(&st, false).await.unwrap();
    let s3 = status.destinations.iter().find(|d| d.name == "s3").unwrap();
    assert!(!s3.last_error.is_empty() && s3.behind_ms.is_some(), "{s3:?}");
    let c6h = s3.generations.iter().find(|g| g.database == "c6h.db").unwrap();
    assert!(!c6h.complete, "its changes didn't get there: {s3:?}");
    mock.set_down(false);
    // Back to a real limit: at 0, any change the head writes between two
    // passes (a busy head writes all the time) is "too far behind" again.
    let mut s = backups::settings(&st).await;
    s.spool_limit_mb = c6_api::BackupSettings::default().spool_limit_mb;
    backups::put_settings(&st, s).await.unwrap();
    // Back: its next base copies catch it up (a forced pass, so as not to
    // wait out the retry delay after a base copy failed partway).
    backups::run(&st, true).await.unwrap();
    let status = backups::run(&st, false).await.unwrap();
    let s3 = status.destinations.iter().find(|d| d.name == "s3").unwrap();
    assert_eq!(s3.last_error, "", "{s3:?}");
    assert!(s3.behind_ms.is_none(), "{s3:?}");
    let o = c6_head::backups::dest::S3 {
        endpoint: mock.endpoint(),
        bucket: "bucket".into(),
        region: "us-east-1".into(),
        prefix: "c6/".into(),
        access_key: "AKTEST".into(),
        secret_key: "secret".into(),
        http: reqwest::Client::new(),
    };
    let back = tempfile::tempdir().unwrap();
    let r = backups::restore::restore(&o, &private, None, back.path()).await.unwrap();
    assert!(r.ok(), "{}", r.text());
    let restored = c6_head::start(config(back.path())).await.unwrap();
    let rapi = format!("http://127.0.0.1:{}/api/v1", restored.ui_addrs[0].port());
    assert!(work_titles(&rapi).await.contains(&"while S3 was down".to_owned()));
    restored.shutdown().await.unwrap();
    head.shutdown().await.unwrap();
}

#[tokio::test]
async fn test_restores_say_whether_a_backup_would_come_back() {
    let mock = c6_testkit::MockS3::start("AKTEST", "bucket").await;
    let (dir, nas) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let head = c6_head::start(config(dir.path())).await.unwrap();
    let st = head.state.clone();
    let api = format!("http://127.0.0.1:{}/api/v1", head.ui_addrs[0].port());
    let private = set_up(&st, nas.path(), &mock.endpoint()).await;
    add_work(&api, "A").await;
    backups::run(&st, false).await.unwrap();

    // Without your key: a check that everything is there.
    let status = backups::test_now(&st, None).await.unwrap();
    for d in &status.destinations {
        let t = d.last_test.as_ref().unwrap();
        assert!(t.ok && !t.decrypted, "{t:?}");
    }
    // With it: a full restore, every database checked.
    assert!(backups::set_restore_key(&st, &crypt::new_key_pair().1).await.is_err(), "only the key backups are for");
    backups::set_restore_key(&st, &private).await.unwrap();
    let status = backups::test_now(&st, Some("nas")).await.unwrap();
    let t = status.destinations.iter().find(|d| d.name == "nas").unwrap().last_test.clone().unwrap();
    assert!(t.ok && t.decrypted && t.detail.contains("integrity ok"), "{t:?}");

    // A damaged object fails it.
    let chunk = walk(nas.path()).into_iter().find(|n| n.starts_with("chunks/")).unwrap();
    let mut b = std::fs::read(nas.path().join(&chunk)).unwrap();
    let last = b.len() - 1;
    b[last] ^= 1;
    std::fs::write(nas.path().join(&chunk), b).unwrap();
    let status = backups::test_now(&st, Some("nas")).await.unwrap();
    let t = status.destinations.iter().find(|d| d.name == "nas").unwrap().last_test.clone().unwrap();
    assert!(!t.ok && t.detail.contains("changed"), "{t:?}");

    // Through the API, as the Settings page uses it.
    let http = reqwest::Client::new();
    let st_api: serde_json::Value = http.get(format!("{api}/system/backups")).send().await.unwrap().json().await.unwrap();
    assert_eq!(st_api["on"], true, "{st_api}");
    let mut set: serde_json::Value = http.get(format!("{api}/system/backups/settings")).send().await.unwrap().json().await.unwrap();
    assert_eq!(set["destinations"].as_array().unwrap().len(), 2);
    set["destinations"][0]["kind"] = "ftp".into();
    let r = http.put(format!("{api}/system/backups/settings")).json(&set).send().await.unwrap();
    assert_eq!(r.status(), 400, "{}", r.text().await.unwrap());
    let r: serde_json::Value =
        http.post(format!("{api}/system/backups/run")).json(&serde_json::json!({})).send().await.unwrap().json().await.unwrap();
    assert!(r["destinations"].as_array().unwrap().iter().any(|d| d["name"] == "s3" && d["last_error"] == ""), "{r}");
    head.shutdown().await.unwrap();
}
