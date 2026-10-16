//! Documents and papers in the memex: uploads and watched folders are read
//! (PDF, Markdown), papers are recognized with their authors and citations
//! (a citation graph), originals can be downloaded, and trails link items
//! and things with comments, from the API and from agents.

use std::time::Duration;

use base64::Engine;
use c6_api::{Connection, Session};
use c6_head::HeadConfig;
use c6_testkit::{MockLlm, Reply};
use c6_worker::{ConnState, Identity, Worker, conn, enroll};
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

async fn eventually<T>(what: &str, mut f: impl AsyncFnMut() -> Option<T>) -> T {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(v) = f().await {
            return v;
        }
        assert!(tokio::time::Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn node(head: &c6_head::Head, _memex: bool, stop: &CancellationToken) -> (std::sync::Arc<Worker>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let e = c6_head::api::new_enrollment(&head.state, None).await.unwrap();
    enroll::enroll(&e.token).await.unwrap().save(dir.path()).unwrap();
    let w = Worker::open(dir.path(), Identity::load(dir.path()).unwrap()).await.unwrap();
    let (tx, mut rx) = watch::channel(ConnState::Connecting);
    tokio::spawn(conn::run(w.clone(), tx, stop.clone()));
    tokio::time::timeout(Duration::from_secs(10), rx.wait_for(|s| matches!(s, ConnState::Attached { .. }))).await.unwrap().unwrap();
    (w, dir)
}

/// A one-page PDF with these lines of text (Helvetica, one per line).
fn pdf(lines: &[&str]) -> Vec<u8> {
    let mut content = String::from("BT /F1 10 Tf 50 780 Td 12 TL\n");
    for l in lines {
        let esc = l.replace('\\', "\\\\").replace('(', "\\(").replace(')', "\\)");
        content.push_str(&format!("({esc}) Tj T*\n"));
    }
    content.push_str("ET");
    let objects = [
        "<< /Type /Catalog /Pages 2 0 R >>".to_owned(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_owned(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 4 0 R /Resources << /Font << /F1 5 0 R >> >> >>".to_owned(),
        format!("<< /Length {} >>\nstream\n{content}\nendstream", content.len()),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>".to_owned(),
    ];
    let mut out = b"%PDF-1.4\n".to_vec();
    let mut offsets = Vec::new();
    for (i, o) in objects.iter().enumerate() {
        offsets.push(out.len());
        out.extend(format!("{} 0 obj\n{o}\nendobj\n", i + 1).as_bytes());
    }
    let xref = out.len();
    out.extend(format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes());
    for o in offsets {
        out.extend(format!("{o:010} 00000 n \n").as_bytes());
    }
    out.extend(format!("trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n", objects.len() + 1).as_bytes());
    out
}

const PAPER: [&str; 16] = [
    "Attention Is All You Need",
    "Ashish Vaswani, Noam Shazeer and Niki Parmar",
    "arXiv:1706.03762",
    "Abstract",
    "The dominant sequence transduction models are based on complex recurrent networks.",
    "We propose the Transformer, based solely on attention mechanisms.",
    "",
    "1 Introduction",
    "Recurrent neural networks have long been the state of the art in sequence modeling.",
    "",
    "References",
    "",
    "[1] Jimmy Lei Ba, Jamie Ryan Kiros, and Geoffrey E Hinton. Layer normalization. arXiv preprint arXiv:1607.06450, 2016.",
    "[2] Dzmitry Bahdanau, Kyunghyun Cho, and Yoshua Bengio. Neural machine translation by jointly learning to align and translate. 2014.",
    "[3] K. He, X. Zhang. Deep residual learning for image recognition. In CVPR, 2016. doi:10.1109/CVPR.2016.90",
    "",
];

#[tokio::test]
async fn documents_papers_and_trails() {
    crate::extract_here();
    let mock = MockLlm::start().await;
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
    let http = reqwest::Client::new();
    let stop = CancellationToken::new();
    let conn: Connection = http
        .post(format!("{api}/connections"))
        .json(&json!({"name": "m", "base_url": mock.base_url()}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    // A watched folder with a note and a second paper that cites the first.
    let folder = tempfile::tempdir().unwrap();
    std::fs::write(folder.path().join("plan.md"), "# Reading plan\n\nRead the transformer paper, then layer normalization.").unwrap();
    std::fs::create_dir(folder.path().join("papers")).unwrap();
    let citing = pdf(&[
        "Reformer: The Efficient Transformer",
        "Nikita Kitaev, Lukasz Kaiser",
        "Abstract",
        "Large Transformer models routinely achieve state-of-the-art results.",
        "",
        "References",
        "",
        "[1] Ashish Vaswani, Noam Shazeer. Attention is all you need. arXiv:1706.03762, 2017.",
        "[2] Jimmy Lei Ba, Jamie Ryan Kiros. Layer normalization. arXiv:1607.06450, 2016.",
    ]);
    std::fs::write(folder.path().join("papers/reformer.pdf"), &citing).unwrap();
    std::fs::write(folder.path().join(".hidden.md"), "secret").unwrap();
    let put = |path: &str, v: Value| {
        let (http, url) = (http.clone(), format!("{api}/settings/{path}"));
        async move { http.put(url).json(&v).send().await.unwrap().status() }
    };
    assert_eq!(put("memex_folders", json!([{"path": "relative/path"}])).await, reqwest::StatusCode::BAD_REQUEST);
    assert!(put("memex_folders", json!([{"path": folder.path().display().to_string(), "labels": ["research"]}])).await.is_success());

    let (worker, _wd) = node(&head, false, &stop).await;
    let status = || {
        let (http, url) = (http.clone(), format!("{api}/memex/status"));
        async move { http.get(url).send().await.unwrap().json::<c6_api::MemexStatus>().await.unwrap() }
    };
    eventually("the folder read", async || {
        let s = status().await;
        (s.items.get("document") == Some(&1) && s.items.get("paper") == Some(&1)).then_some(())
    })
    .await;

    // Upload the transformer paper.
    let bytes = pdf(&PAPER);
    let up: Value = http
        .post(format!("{api}/memex/upload"))
        .json(&json!({"name": "attention.pdf", "data": base64::engine::general_purpose::STANDARD.encode(&bytes), "labels": ["research"]}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!((up["kind"].as_str(), up["title"].as_str()), (Some("paper"), Some("Attention Is All You Need")), "{up}");
    let paper_id = up["id"].as_str().unwrap().to_owned();
    // The same file through memex.ingest is that item; a new document is
    // created, then updated from the same source and id.
    let ingest = |items: Value| {
        let (http, url) = (http.clone(), format!("{api}/memex/rpc"));
        async move {
            let r: Value = http
                .post(url)
                .json(&json!({"method": "memex.ingest", "params": {"items": items}}))
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            r["results"].as_array().unwrap().clone()
        }
    };
    let src = json!({"kind": "notes-app", "id": "laptop"});
    let r = ingest(json!([{"type": "application/pdf", "data": base64::engine::general_purpose::STANDARD.encode(&bytes), "source": src, "external_id": "a.pdf"}])).await;
    assert_eq!((r[0]["status"].as_str(), r[0]["id"].as_str()), (Some("duplicate"), Some(paper_id.as_str())), "{r:?}");
    let note =
        |text: &str| json!([{"type": "text/markdown", "text": text, "source": src, "external_id": "n1", "attributes": {"name": "Ideas"}}]);
    let r = ingest(note("# Ideas\n\nA cache for embeddings.")).await;
    assert_eq!((r[0]["status"].as_str(), r[0]["kind"].as_str()), (Some("created"), Some("document")), "{r:?}");
    let doc_id = r[0]["id"].as_str().unwrap().to_owned();
    let r = ingest(note("# Ideas\n\nA cache for embeddings, keyed by model.")).await;
    assert_eq!((r[0]["status"].as_str(), r[0]["id"].as_str()), (Some("updated"), Some(doc_id.as_str())), "{r:?}");
    let r =
        ingest(json!([{"type": "text/markdown", "text": "x", "source": src, "external_id": "n2", "attributes": {"created": "yesterday"}}]))
            .await;
    assert_eq!(r[0]["error"], "created is an integer", "{r:?}");
    assert!(http.delete(format!("{api}/memex/items/{doc_id}")).send().await.unwrap().status().is_success());

    let original = http.get(format!("{api}/memex/items/{paper_id}/content")).send().await.unwrap();
    assert_eq!(original.headers()["content-type"], "application/pdf");
    assert_eq!(original.bytes().await.unwrap().as_ref(), bytes.as_slice(), "the original, byte for byte");

    let get = |id: &str| {
        let (http, url) = (http.clone(), format!("{api}/memex/get?id={id}"));
        async move { http.get(url).send().await.unwrap().json::<Value>().await.unwrap() }
    };
    let item = get(&paper_id).await;
    assert_eq!(item["item"]["meta"]["paper"]["arxiv"], "1706.03762");
    assert_eq!(item["item"]["meta"]["paper"]["references"], 3);
    assert!(item["item"]["text"].as_str().unwrap().contains("based solely on attention"), "{item}");
    // The citation graph: layer normalization is cited by both papers; the
    // transformer paper (a thing) is the uploaded one and cited by Reformer.
    // The folder's paper is counted before its links are written.
    let ln = eventually("both papers citing layer normalization", async || {
        let ln = get("paper:arxiv:1607.06450").await;
        (ln["thing"]["items"].as_array().map_or(0, Vec::len) == 2).then_some(ln)
    })
    .await;
    assert_eq!(ln["thing"]["thing"]["name"], "Layer normalization");
    let attention = get("paper:arxiv:1706.03762").await;
    let titles: Vec<&str> = attention["thing"]["items"].as_array().unwrap().iter().map(|h| h["title"].as_str().unwrap()).collect();
    assert!(titles.contains(&"Attention Is All You Need") && titles.contains(&"Reformer: The Efficient Transformer"), "{titles:?}");
    let author = get("person:name:noam-shazeer").await;
    assert_eq!(author["thing"]["thing"]["kind"], "person");
    let hits: Vec<c6_api::MemexHit> =
        http.get(format!("{api}/memex/search?q=reading%20plan&kinds=document")).send().await.unwrap().json().await.unwrap();
    assert_eq!(hits.len(), 1, "the hidden file is skipped: {hits:?}");

    // Trails: you start one; an agent adds a step.
    let r = http
        .post(format!("{api}/memex/trails"))
        .json(&json!({"trail": "Transformers", "target": paper_id, "comment": "Where it started"}))
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success(), "{}", r.text().await.unwrap());
    let bad =
        http.post(format!("{api}/memex/trails")).json(&json!({"trail": "Transformers", "target": "nothing-here"})).send().await.unwrap();
    assert_eq!(bad.status(), reqwest::StatusCode::BAD_REQUEST);
    mock.push(Reply::ToolCalls(vec![(
        "memex_trail".into(),
        r#"{"trail": "Transformers", "target": "paper:arxiv:1607.06450", "comment": "Normalization the transformer relies on"}"#.into(),
    )]));
    mock.push(Reply::ToolCalls(vec![("memex_trail".into(), r#"{"trail": "Transformers"}"#.into())]));
    mock.push(Reply::Text("Added.".into()));
    let s: Session = http
        .post(format!("{api}/sessions"))
        .json(&json!({"worker_id": worker.identity.meta.worker_id.0, "cwd": "", "connection_id": conn.id, "model": "m", "prompt": "add layer norm to the trail"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    eventually("the agent's turn", async || (mock.requests().len() >= 3).then_some(())).await;
    let shown = mock.requests()[2]["messages"].to_string();
    assert!(shown.contains("1. [") && shown.contains("Where it started") && shown.contains("Layer normalization"), "{shown}");
    let trails: Vec<c6_api::MemexTrail> = http.get(format!("{api}/memex/trails")).send().await.unwrap().json().await.unwrap();
    assert_eq!((trails.len(), trails[0].step_count), (1, 2));
    let t: c6_api::MemexTrail = http.get(format!("{api}/memex/trails/{}", trails[0].id)).send().await.unwrap().json().await.unwrap();
    assert_eq!(t.steps[1].made_by, format!("session:{}", s.id));
    assert_eq!((t.steps[0].kind.as_str(), t.steps[1].title.as_str()), ("paper", "Layer normalization"));

    // A file removed from the folder leaves the memex at the next scan.
    std::fs::remove_file(folder.path().join("plan.md")).unwrap();
    http.post(format!("{api}/memex/scan")).send().await.unwrap();
    assert_eq!(status().await.items.get("document"), None);

    stop.cancel();
    head.shutdown().await.unwrap();
}
