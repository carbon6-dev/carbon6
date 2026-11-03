//! Time limits on a model call: an overall deadline (connecting, waiting
//! for the first token, and the whole answer) and the longest pause between
//! events once the answer streams. Past either, the call fails for now
//! (`limited`: retryable; `bounded`: stop without retrying): a server that
//! stalled, not an answer. These limits do not cover tool execution.

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
    retryable: bool,
}

#[async_trait]
impl ModelClient for Limited {
    async fn stream(&self, req: &ModelRequest) -> Result<EventStream, LlmError> {
        let Limits { total, idle } = self.limits;
        let retryable = self.retryable;
        let failure = move |phase: &str, why: String| {
            if retryable { LlmError::Network(why) } else { LlmError::Deadline(format!("{phase}: {why}")) }
        };
        let end = Instant::now() + total;
        tracing::debug!(model = %req.model, cache_key = ?req.cache_key, phase = "opening_response", "bounded model call");
        let stream = match tokio::time::timeout_at(end, self.inner.stream(req)).await {
            Ok(s) => s?,
            Err(_) => {
                let why = format!("no answer within {}s", total.as_secs());
                tracing::warn!(model = %req.model, cache_key = ?req.cache_key, phase = "opening_response", %why, "model deadline exceeded");
                return Err(failure("opening response", why));
            }
        };
        let model = req.model.clone();
        let cache_key = req.cache_key.clone();
        tracing::debug!(%model, ?cache_key, phase = "waiting_first_event", "model response opened");
        // (stream, an event came, it timed out)
        Ok(futures::stream::unfold((stream, false, false), move |(mut s, started, over)| {
            let model = model.clone();
            let cache_key = cache_key.clone();
            async move {
                if over {
                    return None;
                }
                let until = if started { end.min(Instant::now() + idle) } else { end };
                match tokio::time::timeout_at(until, s.next()).await {
                    Ok(Some(ev)) => {
                        if !started {
                            tracing::debug!(%model, ?cache_key, phase = "streaming", "first model event");
                        }
                        Some((ev, (s, true, false)))
                    }
                    Ok(None) => None,
                    Err(_) => {
                        let why = if until == end {
                            format!("no full answer within {}s", total.as_secs())
                        } else {
                            format!("the answer stalled for {}s", idle.as_secs())
                        };
                        let phase = if started { "streaming" } else { "waiting_first_event" };
                        tracing::warn!(%model, ?cache_key, phase, %why, "model deadline exceeded");
                        Some((Err(failure(phase, why)), (s, started, true)))
                    }
                }
            }
        })
        .boxed())
    }
}

/// The client, its calls held to `limits`.
pub fn limited(client: Box<dyn ModelClient>, limits: Limits) -> Box<dyn ModelClient> {
    Box::new(Limited { inner: client, limits, retryable: true })
}

/// A caller's budget, including connection setup and any inner retries.
/// Unlike `limited`, expiry is not retryable: an interactive turn should
/// stop with an explicit error, not spend another budget on the same call.
/// Only model calls are bounded, never tool execution or approval waits.
pub fn bounded(client: Box<dyn ModelClient>, limits: Limits) -> Box<dyn ModelClient> {
    Box::new(Limited { inner: client, limits, retryable: false })
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

    #[tokio::test(start_paused = true)]
    async fn bounded_deadlines_are_not_retryable_and_identify_the_phase() {
        for (before, events, phase, elapsed) in [
            (900, vec![], "opening response", 600),
            (0, vec![], "waiting_first_event", 600),
            (0, vec![ModelEvent::TextDelta("partial".into())], "streaming", 120),
        ] {
            let client = bounded(Box::new(Slow { before: Duration::from_secs(before), events, stall: true }), LIMITS);
            let start = Instant::now();
            let err = match client.stream(&req()).await {
                Err(e) => e,
                Ok(mut s) => loop {
                    if let Err(e) = s.next().await.expect("deadline must terminate the stream") {
                        assert!(s.next().await.is_none());
                        break e;
                    }
                },
            };
            assert!(matches!(err, LlmError::Deadline(_)));
            assert!(!err.retryable());
            assert!(err.to_string().contains(phase), "{err}");
            assert_eq!(Instant::now() - start, Duration::from_secs(elapsed));
        }
    }

    struct Endless;

    #[async_trait]
    impl ModelClient for Endless {
        async fn stream(&self, _req: &ModelRequest) -> Result<EventStream, LlmError> {
            Ok(futures::stream::unfold((), |()| async {
                tokio::time::sleep(Duration::from_secs(60)).await;
                Some((Ok(ModelEvent::TextDelta("still going".into())), ()))
            })
            .boxed())
        }
    }

    #[tokio::test(start_paused = true)]
    async fn ongoing_events_do_not_extend_the_total_budget() {
        let client = bounded(Box::new(Endless), LIMITS);
        let start = Instant::now();
        let mut stream = client.stream(&req()).await.unwrap();
        loop {
            if let Err(e) = stream.next().await.unwrap() {
                assert!(matches!(e, LlmError::Deadline(_)));
                assert!(e.to_string().contains("no full answer within 600s"));
                break;
            }
        }
        assert_eq!(Instant::now() - start, Duration::from_secs(600));
        assert!(stream.next().await.is_none());
    }
}
