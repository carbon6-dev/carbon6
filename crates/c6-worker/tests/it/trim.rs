//! A subagent's long conversation sheds old tool output before it nears its
//! context window (docs/replay-eval.md): past the threshold, old outputs
//! become stubs naming their call, while the first outputs after its task
//! and the latest stay whole. Your own sessions keep everything.

use c6_api::Session;
use c6_testkit::{Reply, eventually};
use serde_json::{Value, json};

use crate::batch_capture::Env;

/// A command printing about 10,000 characters, marked with its number.
fn dig(n: usize) -> (String, String) {
    ("bash".into(), json!({"command": format!("echo OUTPUT-{n}; seq -f 'row %g of the listing ......' 1 330")}).to_string())
}

/// The request that came after `calls` of the subagent's commands: its
/// tool results, in order, by the number they print.
fn results(req: &Value) -> Vec<String> {
    req["messages"].as_array().unwrap().iter().filter(|m| m["role"] == "tool").map(|m| m["content"].to_string()).collect()
}

#[tokio::test]
async fn a_subagents_old_tool_output_is_trimmed_past_its_threshold() {
    let env = Env::new().await;
    let here = tempfile::tempdir().unwrap();
    // The model reports each request's real size. A 100,000-token window:
    // the subagent trims past half of it, before compaction (70%).
    env.mock.real_usage();
    let small: Value = env.post("connections", json!({"name": "small", "base_url": env.mock.base_url(), "context_window": 100000})).await;
    let calls = 24;
    env.mock.push(Reply::ToolCalls(vec![("task".into(), json!({"description": "dig", "prompt": "DIG-TASK: list rows"}).to_string())]));
    for n in 0..calls {
        env.mock.push(Reply::ToolCalls(vec![dig(n)]));
    }
    env.mock.push(Reply::Text("Dug through it all.".into()));
    env.mock.push(Reply::Text("The subagent finished.".into()));
    let s: Session = serde_json::from_value(
        env.post(
            "sessions",
            json!({"worker_id": env.wid, "cwd": here.path(), "connection_id": small["id"], "model": "m", "prompt": "start digging"}),
        )
        .await,
    )
    .unwrap();
    eventually("the subagent to report", async || {
        env.mock.requests().iter().any(|r| r.to_string().contains("Dug through it all.")).then_some(())
    })
    .await;
    // The subagent's last request: what its conversation had become.
    let reqs = env.mock.requests();
    let last = reqs.iter().rfind(|r| r.to_string().contains("DIG-TASK") && results(r).len() == calls).expect("the last request");
    let outs = results(last);
    let stubbed: Vec<usize> = (0..calls).filter(|&n| outs[n].contains("Earlier output of bash")).collect();
    assert!(!stubbed.is_empty(), "past the threshold, old outputs are trimmed: {}", &outs[6][..80.min(outs[6].len())]);
    assert!(!reqs.iter().any(|r| r.to_string().contains("will be replaced by a summary")), "trimmed before it needed compacting");
    let whole = |n: usize| outs[n].contains(&format!("OUTPUT-{n}")) && outs[n].contains("row 330");
    assert!((0..5).all(whole), "the task's first outputs stay");
    assert!((calls - 8..calls).all(whole), "the latest stay");
    assert!(stubbed.iter().all(|&n| (5..calls - 8).contains(&n)), "only between them: {stubbed:?}");
    assert!(outs[stubbed[0]].contains("echo OUTPUT-"), "a stub names its call: {}", outs[stubbed[0]]);
    // Once trimmed, an output stays trimmed: the conversation's prefix
    // changes only when it is trimmed again.
    let first = reqs.iter().position(|r| results(r).iter().any(|o| o.contains("Earlier output of bash"))).unwrap();
    for r in &reqs[first..] {
        let o = results(r);
        if o.len() > stubbed[0] && r.to_string().contains("DIG-TASK") {
            assert!(o[stubbed[0]].contains("Earlier output of bash"), "trimmed stays trimmed");
        }
    }
    // Your own session isn't trimmed: its history still has whole outputs.
    let parent: Vec<&Value> =
        reqs.iter().filter(|r| r.to_string().contains("The subagent finished") || r.to_string().contains("start digging")).collect();
    assert!(parent.iter().all(|r| !r.to_string().contains("Earlier output of")), "the parent keeps its own");
    let _ = s;
    env.stop.cancel();
}
