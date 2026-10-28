//! Asking a model (the understanding task's, or the vision task's), once,
//! through one of carbon6's connections.
//!
//! A call that fails for now (the server unreachable, timed out, busy or
//! failing, or no pool member free) is not an answer: the work waits and
//! is asked again, and the connection rests, twice as long after each
//! such failure in a row, so nothing else calls it meanwhile.

use std::time::{Duration, Instant};

use c6_llm::{Accumulator, Block, Message, ModelRequest, Role};
use futures_util::StreamExt;
use serde_json::Value;

use crate::Memex;

/// Room for reasoning: unless effort is none, a model may think (Ollama's
/// qwen3 does by default), and its thinking counts toward the output
/// limit. qwen3.6:35b thought 700 to 3,900 tokens on the memex's prompts.
pub const THINKING_ROOM: u32 = 8_192;
/// How long a connection rests after its first failure for now, and at most.
const REST_FIRST: Duration = Duration::from_secs(10);
const REST_MAX: Duration = Duration::from_secs(300);

/// A connection resting after failures for now: until when, and how many in a row.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Rest {
    until: Instant,
    failures: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Task {
    pub connection_id: String,
    pub model: String,
    /// The reasoning effort asked for (none: no reasoning).
    pub effort: Option<String>,
}

/// The model's text answer to `system` and the user's blocks (text and
/// images); `what` it's for and what it's `about` show in the activity
/// panel.
pub async fn ask(
    m: &Memex,
    task: &Task,
    what: &str,
    about: &str,
    system: &str,
    user: Vec<Block>,
    max_tokens: u32,
) -> anyhow::Result<String> {
    let grant = m.link.credential(&task.connection_id).await?;
    let client = c6_llm::client_for(&grant.kind, &grant.base_url, &grant.bearer, grant.headers.clone())?;
    let origin = c6_llm::activity::Origin {
        what: what.to_owned(),
        about: about.to_owned(),
        connection: task.connection_id.clone(),
        realm: m.realm.clone(),
    };
    let client = c6_llm::activity::track(client, origin);
    let effort = task.effort.clone().filter(|e| !e.is_empty());
    let req = ModelRequest {
        model: task.model.clone(),
        system: system.to_owned(),
        messages: vec![Message { role: Role::User, blocks: user }],
        tools: Vec::new(),
        max_output_tokens: Some(output_limit(max_tokens, effort.as_deref())),
        cache_key: None,
        effort,
        web_search: false,
    };
    let text = match answer(client.as_ref(), &req).await {
        Ok(t) => {
            m.rests.lock().unwrap().remove(&task.connection_id);
            t
        }
        Err(e) => {
            if transient(&e) {
                rest(m, &task.connection_id);
            }
            return Err(e);
        }
    };
    anyhow::ensure!(!text.trim().is_empty(), "{} gave no answer (a thinking model may have spent its output tokens thinking)", task.model);
    Ok(text)
}

async fn answer(client: &dyn c6_llm::ModelClient, req: &ModelRequest) -> anyhow::Result<String> {
    let mut stream = client.stream(req).await?;
    let mut acc = Accumulator::default();
    while let Some(ev) = stream.next().await {
        acc.push(&ev?);
    }
    let msg = acc.finish(true);
    Ok(msg.blocks.iter().filter_map(|b| if let Block::Text(t) = b { Some(t.as_str()) } else { None }).collect())
}

/// The output limit for an answer of up to `answer` tokens: with room to
/// think, unless effort is none.
pub fn output_limit(answer: u32, effort: Option<&str>) -> u32 {
    if effort == Some("none") { answer } else { answer + THINKING_ROOM }
}

/// Whether a model call failed for now only: the server unreachable, timed
/// out, busy (429) or failing (5xx), or no pool member free.
pub fn transient(e: &anyhow::Error) -> bool {
    e.chain().any(|c| c.downcast_ref::<c6_llm::LlmError>().is_some_and(c6_llm::LlmError::retryable))
}

/// Whether work whose model call failed with `e` waits to be asked again
/// (its call goes back to the day's budget) rather than keeping the error.
pub async fn waits(m: &Memex, e: &anyhow::Error) -> anyhow::Result<bool> {
    if !transient(e) {
        return Ok(false);
    }
    crate::understand::refund_call(m).await?;
    Ok(true)
}

fn rest(m: &Memex, connection: &str) {
    let mut rests = m.rests.lock().unwrap();
    let failures = rests.get(connection).map_or(0, |r| r.failures) + 1;
    let wait = REST_FIRST.saturating_mul(1 << (failures - 1).min(10)).min(REST_MAX);
    tracing::warn!(%connection, failures, ?wait, "the model failed for now; its work waits");
    rests.insert(connection.to_owned(), Rest { until: Instant::now() + wait, failures });
}

/// Whether a connection is resting after failing for now.
pub fn resting(m: &Memex, connection: &str) -> bool {
    m.rests.lock().unwrap().get(connection).is_some_and(|r| r.until > Instant::now())
}

/// The first JSON object in a model's answer (it may wrap it in prose or a
/// code fence).
pub fn json_of(text: &str) -> anyhow::Result<Value> {
    let start = text.find('{').ok_or_else(|| anyhow::anyhow!("no JSON in the answer"))?;
    let end = text.rfind('}').ok_or_else(|| anyhow::anyhow!("no JSON in the answer"))?;
    anyhow::ensure!(end > start, "no JSON in the answer");
    Ok(serde_json::from_str(&text[start..=end])?)
}

#[cfg(test)]
mod tests {
    #[test]
    fn room_to_think_unless_effort_is_none() {
        assert_eq!(super::output_limit(400, Some("none")), 400);
        assert_eq!(super::output_limit(400, Some("low")), 400 + super::THINKING_ROOM);
        // Unset: the model's default, which may think.
        assert_eq!(super::output_limit(3000, None), 3000 + super::THINKING_ROOM);
    }

    #[test]
    fn failures_for_now() {
        use c6_llm::LlmError;
        let http = |status| anyhow::Error::from(LlmError::Http { status, body: String::new(), retry_after: None });
        for e in [
            http(429),
            http(503),
            LlmError::Network("connection refused".into()).into(),
            LlmError::Unavailable("every member is busy".into()).into(),
        ] {
            assert!(super::transient(&e.context("sorting")), "transient");
        }
        assert!(!super::transient(&http(400)));
        assert!(!super::transient(&http(404)));
        assert!(!super::transient(&super::json_of("no json here").unwrap_err()));
    }

    #[test]
    fn json_in_prose() {
        let v = super::json_of("Sure! ```json\n{\"summary\": \"ok\", \"topics\": [\"a\"]}\n```").unwrap();
        assert_eq!(v["topics"][0], "a");
        assert!(super::json_of("no json here").is_err());
    }
}
