//! OTLP traces with OpenTelemetry GenAI semantic conventions, built from
//! session events when a turn ends.
//!
//! Every model call and tool call is already in the event log with its
//! timing and token usage, so spans are derived on the head instead of
//! being emitted by workers. This also covers turns that ran while a worker
//! was offline: they are exported when the events sync.

use std::collections::HashMap;
use std::time::Duration;

use c6_proto::{SessionEvent, TurnEndReason, session_event};
use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use opentelemetry_proto::tonic::common::v1::{AnyValue, ArrayValue, InstrumentationScope, KeyValue, any_value};
use opentelemetry_proto::tonic::resource::v1::Resource;
use opentelemetry_proto::tonic::trace::v1::{ResourceSpans, ScopeSpans, Span, Status, span, status};
use prost::Message;
use sha2::{Digest, Sha256};
use tracing::warn;

use crate::store::SessionRow;

pub struct Otlp {
    http: reqwest::Client,
    traces_url: String,
    headers: HashMap<String, String>,
}

impl Otlp {
    /// From config, else the standard `OTEL_EXPORTER_OTLP_ENDPOINT`.
    pub fn from_config(endpoint: Option<&str>, headers: HashMap<String, String>) -> Option<Self> {
        let endpoint =
            endpoint.map(str::to_owned).or_else(|| std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT").ok()).filter(|e| !e.is_empty())?;
        let http = reqwest::Client::builder().timeout(Duration::from_secs(10)).build().ok()?;
        Some(Self { http, traces_url: format!("{}/v1/traces", endpoint.trim_end_matches('/')), headers })
    }

    pub async fn export(&self, req: ExportTraceServiceRequest) {
        let mut r = self.http.post(&self.traces_url).header("content-type", "application/x-protobuf").body(req.encode_to_vec());
        for (k, v) in &self.headers {
            r = r.header(k, v);
        }
        match r.send().await {
            Ok(resp) if resp.status().is_success() => {}
            Ok(resp) => warn!(status = %resp.status(), "OTLP export rejected"),
            Err(e) => warn!("OTLP export failed: {e}"),
        }
    }
}

fn kv(k: &str, v: any_value::Value) -> KeyValue {
    KeyValue { key: k.to_owned(), value: Some(AnyValue { value: Some(v) }), ..Default::default() }
}

fn s(k: &str, v: impl Into<String>) -> KeyValue {
    kv(k, any_value::Value::StringValue(v.into()))
}

fn i(k: &str, v: u64) -> KeyValue {
    kv(k, any_value::Value::IntValue(v as i64))
}

fn id_bytes(parts: &[&str], n: usize) -> Vec<u8> {
    let mut h = Sha256::new();
    for p in parts {
        h.update(p.as_bytes());
        h.update([0]);
    }
    h.finalize()[..n].to_vec()
}

fn nanos(ms: i64) -> u64 {
    (ms.max(0) as u64) * 1_000_000
}

/// Spans for one turn: `events` runs from its `TurnStarted` to its `TurnEnded`.
pub fn turn_spans(session: &SessionRow, provider: &str, worker_name: &str, events: &[SessionEvent]) -> Option<ExportTraceServiceRequest> {
    let (Some(first), Some(last)) = (events.first(), events.last()) else { return None };
    let Some(session_event::Kind::TurnStarted(t)) = &first.kind else { return None };
    let Some(session_event::Kind::TurnEnded(end)) = &last.kind else { return None };
    let turn = t.turn.to_string();
    // Deterministic ids: one trace per session, stable span ids per event.
    let trace_id = id_bytes(&[&session.id], 16);
    let agent_span_id = id_bytes(&[&session.id, "turn", &turn], 8);
    let reason = TurnEndReason::try_from(end.reason).unwrap_or(TurnEndReason::Unspecified);
    let common =
        |name: String, span_id: Vec<u8>, parent: Vec<u8>, start: i64, end: i64, attrs: Vec<KeyValue>, error: Option<String>| Span {
            trace_id: trace_id.clone(),
            span_id,
            parent_span_id: parent,
            name,
            kind: span::SpanKind::Internal as i32,
            start_time_unix_nano: nanos(start),
            end_time_unix_nano: nanos(end.max(start)),
            attributes: attrs,
            status: Some(match error {
                Some(m) => Status { code: status::StatusCode::Error as i32, message: m },
                None => Status { code: status::StatusCode::Ok as i32, message: String::new() },
            }),
            ..Default::default()
        };

    let mut spans = vec![common(
        "invoke_agent carbon6".into(),
        agent_span_id.clone(),
        Vec::new(),
        first.ts_ms,
        last.ts_ms,
        vec![
            s("gen_ai.operation.name", "invoke_agent"),
            s("gen_ai.agent.name", "carbon6"),
            s("gen_ai.conversation.id", &session.id),
            s("gen_ai.provider.name", provider),
            s("gen_ai.request.model", &session.model),
            i("carbon6.turn", t.turn),
            s("carbon6.turn.end_reason", format!("{reason:?}")),
            s("carbon6.worker.name", worker_name),
            s("carbon6.session.title", &session.title),
        ],
        matches!(reason, TurnEndReason::Error).then(|| "turn ended with an error".to_owned()),
    )];

    let mut prev_ts = first.ts_ms;
    for (n, ev) in events.iter().enumerate() {
        match &ev.kind {
            Some(session_event::Kind::Assistant(a)) => {
                // The request started when the previous event was recorded.
                let usage = events[n + 1..].iter().take(2).find_map(|e| match &e.kind {
                    Some(session_event::Kind::Usage(u)) => Some(u.clone()),
                    _ => None,
                });
                let mut attrs = vec![
                    s("gen_ai.operation.name", "chat"),
                    s("gen_ai.provider.name", provider),
                    s("gen_ai.request.model", &session.model),
                    s("gen_ai.conversation.id", &session.id),
                    kv(
                        "gen_ai.response.finish_reasons",
                        any_value::Value::ArrayValue(ArrayValue {
                            values: vec![AnyValue { value: Some(any_value::Value::StringValue(a.stop_reason.clone())) }],
                        }),
                    ),
                ];
                if let Some(u) = usage {
                    if !u.model.is_empty() {
                        attrs.push(s("gen_ai.response.model", &u.model));
                    }
                    attrs.push(i("gen_ai.usage.input_tokens", u.input_tokens + u.cache_read_tokens + u.cache_write_tokens));
                    attrs.push(i("gen_ai.usage.output_tokens", u.output_tokens));
                    attrs.push(i("gen_ai.usage.cache_read.input_tokens", u.cache_read_tokens));
                    attrs.push(i("gen_ai.usage.cache_creation.input_tokens", u.cache_write_tokens));
                    if u.reasoning_tokens > 0 {
                        attrs.push(i("gen_ai.usage.reasoning.output_tokens", u.reasoning_tokens));
                    }
                }
                let id = id_bytes(&[&session.id, "chat", &ev.seq.to_string()], 8);
                spans.push(common(format!("chat {}", session.model), id, agent_span_id.clone(), prev_ts, ev.ts_ms, attrs, None));
            }
            Some(session_event::Kind::ToolResult(r)) => {
                let tool = if r.tool.is_empty() { "tool" } else { &r.tool };
                let attrs = vec![
                    s("gen_ai.operation.name", "execute_tool"),
                    s("gen_ai.tool.name", tool),
                    s("gen_ai.tool.call.id", &r.call_id),
                    s("gen_ai.conversation.id", &session.id),
                ];
                let id = id_bytes(&[&session.id, "tool", &ev.seq.to_string()], 8);
                let start = ev.ts_ms - r.duration_ms as i64;
                spans.push(common(
                    format!("execute_tool {tool}"),
                    id,
                    agent_span_id.clone(),
                    start,
                    ev.ts_ms,
                    attrs,
                    r.is_error.then(|| "tool error".to_owned()),
                ));
            }
            Some(session_event::Kind::Error(e)) => {
                if let Some(first) = spans.first_mut() {
                    first.events.push(span::Event {
                        time_unix_nano: nanos(ev.ts_ms),
                        name: "error".into(),
                        attributes: vec![s("message", &e.message)],
                        ..Default::default()
                    });
                }
            }
            _ => {}
        }
        prev_ts = ev.ts_ms;
    }

    Some(ExportTraceServiceRequest {
        resource_spans: vec![ResourceSpans {
            resource: Some(Resource {
                attributes: vec![s("service.name", "carbon6"), s("service.version", c6_core::VERSION)],
                ..Default::default()
            }),
            scope_spans: vec![ScopeSpans {
                scope: Some(InstrumentationScope { name: "carbon6".into(), version: c6_core::VERSION.into(), ..Default::default() }),
                spans,
                ..Default::default()
            }],
            ..Default::default()
        }],
    })
}
