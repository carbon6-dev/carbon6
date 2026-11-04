//! The copies of each database made before its upgrades: only the newest
//! few are kept, as many as the head's setting says (docs/os.md §13).

use std::path::{Path, PathBuf};

use c6_head::HeadConfig;
use serde_json::json;

fn config(dir: &Path) -> HeadConfig {
    HeadConfig {
        data_dir: dir.to_owned(),
        bind: vec!["127.0.0.1".parse().unwrap()],
        ui_port: 0,
        worker_port: 0,
        auth: false,
        ..Default::default()
    }
}

/// `n` backups of database `stem` in `dir/backups`, oldest first.
fn plant(dir: &Path, stem: &str, n: u32) -> Vec<PathBuf> {
    std::fs::create_dir_all(dir.join("backups")).unwrap();
    (1..=n)
        .map(|i| {
            let p = dir.join("backups").join(format!("{stem}-v{i:04}-to-v{:04}-202610{i:02}-120000.db", i + 1));
            std::fs::write(&p, "").unwrap();
            p
        })
        .collect()
}

fn left(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir.join("backups")).unwrap().flatten().map(|e| e.path()).collect();
    v.sort();
    v
}

#[tokio::test]
async fn only_the_newest_backups_of_each_database_are_kept() {
    let dir = tempfile::tempdir().unwrap();
    let realm = dir.path().join("knowledge/realms/r1");
    std::fs::create_dir_all(&realm).unwrap();
    // Left by earlier upgrades: the head's, the knowledge store's and a realm's.
    let head_copies = plant(dir.path(), "c6h", 5);
    let home_copies = plant(&dir.path().join("knowledge"), "memex", 4);
    let realm_copies = plant(&realm, "memex", 2);

    // A start keeps the newest three of each.
    let head = c6_head::start(config(dir.path())).await.unwrap();
    assert_eq!(left(dir.path()), head_copies[2..].to_vec());
    assert_eq!(left(&dir.path().join("knowledge")), home_copies[1..].to_vec());
    assert_eq!(left(&realm), realm_copies);

    // Setting it removes more now, never the last copy.
    let api = format!("http://127.0.0.1:{}/api/v1", head.ui_addrs[0].port());
    let http = reqwest::Client::new();
    let r = http.put(format!("{api}/system/upgrades/backups")).json(&json!({"keep": 0})).send().await.unwrap();
    assert_eq!(r.status(), 400);
    let r = http.put(format!("{api}/system/upgrades/backups")).json(&json!({"keep": 1})).send().await.unwrap();
    assert_eq!(r.status(), 204);
    let got: serde_json::Value = http.get(format!("{api}/system/upgrades/backups")).send().await.unwrap().json().await.unwrap();
    assert_eq!(got, json!({"keep": 1}));
    assert_eq!(left(dir.path()), vec![head_copies[4].clone()]);
    assert_eq!(left(&dir.path().join("knowledge")), vec![home_copies[3].clone()]);
    assert_eq!(left(&realm), vec![realm_copies[1].clone()]);
    head.shutdown().await.unwrap();

    // The next start keeps what the setting says.
    let more = plant(&realm, "memex", 4);
    let head = c6_head::start(config(dir.path())).await.unwrap();
    assert_eq!(left(&realm), vec![more[3].clone()]);
    head.shutdown().await.unwrap();
}
