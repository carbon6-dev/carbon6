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
    c6_head::secrets::put_in(st, "root", "S3_SECRET", "secret", "").await.unwrap();
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

    // Nothing new: nothing sent.
    let before = mock.puts();
    let n = walk(nas.path()).len();
    backups::run(&st, false).await.unwrap();
    assert_eq!(mock.puts(), before, "nothing changed, nothing sent");
    assert_eq!(walk(nas.path()).len(), n);
    // A new blob: just it, and its log entry.
    std::fs::write(blobs.join("ab34"), b"another").unwrap();
    backups::run(&st, false).await.unwrap();
    assert_eq!(mock.puts().0, before.0 + 2);
    assert_eq!(walk(nas.path()).len(), n + 2);

    // An unreachable destination says so, and the other carries on.
    mock.set_down(true);
    std::fs::write(blobs.join("ab56"), b"third").unwrap();
    let status = backups::run(&st, false).await.unwrap();
    let s3 = status.destinations.iter().find(|d| d.name == "s3").unwrap();
    assert!(s3.last_error.contains("503"), "{s3:?}");
    assert_eq!(walk(nas.path()).len(), n + 4);
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
