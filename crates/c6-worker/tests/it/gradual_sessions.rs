//! Starting with only a worker and model, then sending the first message.

use c6_api::{ContentBlock, EventKind, Session, SessionDetail, TurnEndReason};
use c6_testkit::Reply;
use serde_json::json;

use crate::batch_capture::Env;

#[tokio::test]
async fn omitting_cwd_runs_in_isolated_worker_scratch() {
    let env = Env::new().await;
    let mut notes = Vec::new();
    for label in ["first", "second"] {
        // Leave cwd out of the JSON entirely, rather than passing an empty string.
        let session: Session = serde_json::from_value(env.post("sessions", json!({"worker_id": env.wid, "model": "m"})).await).unwrap();
        assert_eq!(session.connection_id, env.conn, "the existing default connection is used");

        let prompt = format!("Write the {label} scratch note");
        let reply = format!("Wrote the {label} scratch note.");
        // Both sessions write the same filename, but neither should see it already there.
        let command = format!("test ! -e notes.txt && printf '%s' {label} > notes.txt");
        env.mock.push_about(&prompt, Reply::ToolCalls(vec![("bash".into(), json!({"command": command}).to_string())]));
        env.mock.push_about(&prompt, Reply::Text(reply.clone()));
        env.post(&format!("sessions/{}/messages", session.id), json!({"text": prompt})).await;

        let events = env.ended(&session.id, 1).await;
        assert!(events.iter().any(|e| matches!(e.kind, EventKind::TurnEnded { reason: TurnEndReason::Completed, .. })), "{events:?}");
        assert!(
            events.iter().any(|e| matches!(&e.kind, EventKind::Assistant { blocks, .. }
                if blocks.iter().any(|b| matches!(b, ContentBlock::Text { text, .. } if text == &reply)))),
            "{events:?}"
        );

        let scratch = env.worker_state().join("scratch").join(&session.id).canonicalize().unwrap();
        let detail: SessionDetail = serde_json::from_value(env.get(&format!("sessions/{}", session.id)).await).unwrap();
        assert_eq!(std::path::Path::new(&detail.session.cwd), scratch.as_path());
        notes.push(scratch.join("notes.txt"));
    }

    assert_ne!(notes[0], notes[1]);
    assert_eq!(std::fs::read_to_string(&notes[0]).unwrap(), "first");
    assert_eq!(std::fs::read_to_string(&notes[1]).unwrap(), "second");
    env.stop.cancel();
}
