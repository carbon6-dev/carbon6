//! Time limits on a model call: an overall deadline (connecting, waiting
//! for the first token, and the whole answer) and the longest pause between
//! events once the answer streams. Past either, the call fails for now
//! (`LlmError::Network`, retryable): a server that stalled, not an answer.

use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use tokio::time::Instant;

use crate::{EventStream, LlmError, ModelClient, ModelRequest};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Limits {
    /// The whole call, from connecting to its last event.
    pub total: Duration,
    /// The longest pause between events, once the first has come.
    pub idle: Duration,
}

struct Limited {
    inner: Box<dyn ModelClient>,
    limits: Limits,
}

#[async_trait]
impl ModelClient for Limited {
    async fn stream(&self, req: &ModelRequest) -> Result<EventStream, LlmError> {
        let Limits { total, idle } = self.limits;
        let end = Instant::now() + total;
        let stream = match tokio::time::timeout_at(end, self.inner.stream(req)).await {
            Ok(s) => s?,
            Err(_) => return Err(LlmError::Network(format!("no answer within {}s", total.as_secs()))),
        };
        // (stream, an event came, it timed out)
        Ok(futures::stream::unfold((stream, false, false), move |(mut s, started, over)| async move {
            if over {
                return None;
            }
            let until = if started { end.min(Instant::now() + idle) } else { end };
            match tokio::time::timeout_at(until, s.next()).await {
                Ok(Some(ev)) => Some((ev, (s, true, false))),
                Ok(None) => None,
                Err(_) => {
                    let why = if until == end {
                        format!("no full answer within {}s", total.as_secs())
                    } else {
                        format!("the answer stalled for {}s", idle.as_secs())
                    };
                    Some((Err(LlmError::Network(why)), (s, started, true)))
                }
            }
        })
        .boxed())
    }
}

/// The client, its calls held to `limits`.
pub fn limited(client: Box<dyn ModelClient>, limits: Limits) -> Box<dyn ModelClient> {
    Box::new(Limited { inner: client, limits })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Message, ModelEvent};

    /// A server that sends `events`, then (with `stall`) nothing more, after
    /// first waiting `before`.
    struct Slow {
        before: Duration,
        events: Vec<ModelEvent>,
        stall: bool,
    }

    #[async_trait]
    impl ModelClient for Slow {
        async fn stream(&self, _req: &ModelRequest) -> Result<EventStream, LlmError> {
            tokio::time::sleep(self.before).await;
            let events = futures::stream::iter(self.events.clone().into_iter().map(Ok));
            Ok(if self.stall { events.chain(futures::stream::pending()).boxed() } else { events.boxed() })
        }
    }

    fn req() -> ModelRequest {
        ModelRequest {
            model: "m".into(),
            system: String::new(),
            messages: vec![Message::user_text("Sort this.")],
            tools: vec![],
            max_output_tokens: None,
            cache_key: None,
            effort: None,
            web_search: false,
            json: false,
        }
    }

    const LIMITS: Limits = Limits { total: Duration::from_secs(600), idle: Duration::from_secs(120) };

    async fn run(slow: Slow) -> Vec<Result<ModelEvent, String>> {
        let client = limited(Box::new(slow), LIMITS);
        match client.stream(&req()).await {
            Ok(s) => s.map(|e| e.map_err(|e| e.to_string())).collect().await,
            Err(e) => vec![Err(e.to_string())],
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_slow_first_token_is_fine() {
        // A CPU server loading the model: minutes to the first token.
        let events = vec![ModelEvent::TextDelta("ok".into()), ModelEvent::Done { stop_reason: "stop".into() }];
        let out = run(Slow { before: Duration::from_secs(300), events: events.clone(), stall: false }).await;
        assert_eq!(out, events.into_iter().map(Ok).collect::<Vec<_>>());
    }

    #[tokio::test(start_paused = true)]
    async fn a_stream_that_stalls_fails_for_now() {
        let out = run(Slow { before: Duration::ZERO, events: vec![ModelEvent::TextDelta("half".into())], stall: true }).await;
        assert_eq!(out[0], Ok(ModelEvent::TextDelta("half".into())));
        assert_eq!(out[1], Err("network error: the answer stalled for 120s".into()));
        assert_eq!(out.len(), 2, "and ends");
        assert!(LlmError::Network(String::new()).retryable());
    }

    #[tokio::test(start_paused = true)]
    async fn no_answer_in_time_fails_for_now() {
        let out = run(Slow { before: Duration::from_secs(900), events: vec![], stall: false }).await;
        assert_eq!(out, vec![Err("network error: no answer within 600s".into())]);
        // Nothing after the first event, before the deadline.
        let start = Instant::now();
        let out = run(Slow { before: Duration::from_secs(590), events: vec![ModelEvent::TextDelta("a".into())], stall: true }).await;
        assert_eq!(out[1], Err("network error: no full answer within 600s".into()));
        assert_eq!(Instant::now() - start, Duration::from_secs(600));
    }
}
