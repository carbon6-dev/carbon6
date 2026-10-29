//! Files an app keeps for showing (docs/conversations.md §2.2): a chat's
//! pictures, kept as files (not items) and served by their source's id.

use c6_head::HeadConfig;
use serde_json::json;

#[tokio::test]
async fn a_chat_picture_is_kept_and_shown() {
    use base64::Engine;
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
    let m = c6_head::knowledge::get(&head.state).expect("the head's knowledge store");
    let png = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 1, 2, 3];
    let picture = json!({"type": "image/png", "source": {"kind": "app"}, "external_id": "T1/file/F1",
                         "data": base64::engine::general_purpose::STANDARD.encode(png), "attributes": {"name": "shot.png"}});
    let r = c6_memex::ingest::ingest(&m, json!({"as": "app:slack", "wait": false, "items": [picture.clone()]})).await.unwrap();
    assert_eq!(r["results"][0]["status"], "kept", "{r}");
    // Not an item: not searched, not a photo.
    assert_eq!(m.store.id_of("app:slack", "T1/file/F1").await.unwrap(), None);
    // Only apps keep them.
    let r = c6_memex::ingest::ingest(&m, json!({"wait": false, "items": [picture]})).await.unwrap();
    assert_eq!(r["results"][0]["status"], "rejected", "{r}");

    let http = reqwest::Client::new();
    let resp = http.get(format!("{api}/memex/file?external_id=T1/file/F1")).send().await.unwrap();
    assert!(resp.status().is_success(), "{}", resp.status());
    assert_eq!(resp.headers()["content-type"], "image/png");
    assert!(resp.headers()["content-disposition"].to_str().unwrap().starts_with("inline"));
    assert_eq!(&resp.bytes().await.unwrap()[..], &png[..]);
    let missing = http.get(format!("{api}/memex/file?external_id=T1/file/F2")).send().await.unwrap();
    assert!(!missing.status().is_success());
    head.shutdown().await.unwrap();
}
