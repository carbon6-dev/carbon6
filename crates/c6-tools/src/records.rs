//! Collections (docs/os.md §4.1): typed tables of records in the session's
//! realm, for anything an app or agent keeps. Records always match their
//! collection's schema; updates give the version they read.

use async_trait::async_trait;
use c6_llm::ToolSpec;
use serde_json::{Value, json};

use crate::{Concurrency, Tool, ToolCtx, ToolOutput};

async fn call(ctx: &ToolCtx, method: &str, params: Value) -> ToolOutput {
    let Some(head) = ctx.session.head() else { return ToolOutput::err("carbon6's head is not reachable from here") };
    match head.call(method, params).await {
        Ok(v) => ToolOutput::limited(serde_json::to_string_pretty(&v).unwrap_or_default(), false),
        Err(e) => ToolOutput::err(e),
    }
}

macro_rules! records_tool {
    ($name:ident, $plain:literal, $claude:literal, $conc:expr, $desc:expr, $params:expr, |$ctx:ident, $input:ident| $body:block) => {
        pub struct $name {
            claude: bool,
        }

        impl $name {
            pub fn new(claude: bool) -> Self {
                Self { claude }
            }
        }

        #[async_trait]
        impl Tool for $name {
            fn spec(&self) -> ToolSpec {
                ToolSpec { name: if self.claude { $claude } else { $plain }.into(), description: $desc.into(), parameters: $params }
            }

            fn concurrency(&self) -> Concurrency {
                $conc
            }

            async fn call(&self, $ctx: &ToolCtx, $input: Value) -> ToolOutput $body
        }
    };
}

records_tool!(
    RecordsQuery,
    "records_query",
    "RecordsQuery",
    Concurrency::Shared,
    "Finds records in a collection (a typed table in your realm). Without a collection: lists the collections, with their \
     descriptions and schemas. A query filters by field (where: {field: value} or {field: {$gt: 3}}; also $ne, $in, $nin, $gte, \
     $lt, $lte, $contains), by words (text), sorts (sort: field or -field), pages (limit, offset), or counts per value (group_by).",
    json!({"type": "object", "properties": {
        "collection": {"type": "string"},
        "realm": {"type": "string", "description": "Another realm's collection, through a grant it gave yours"},
        "query": {"type": "object", "properties": {
            "where": {"type": "object"}, "text": {"type": "string"}, "sort": {"type": "string"}, "group_by": {"type": "string"},
            "limit": {"type": "integer"}, "offset": {"type": "integer"}
        }}
    }, "additionalProperties": false}),
    |ctx, input| {
        let collection = input["collection"].as_str().unwrap_or_default();
        if collection.is_empty() {
            return call(ctx, "session.collections", json!({})).await;
        }
        call(
            ctx,
            "session.records_query",
            json!({"collection": collection, "realm": input.get("realm"), "query": input.get("query").cloned().unwrap_or(json!({}))}),
        )
        .await
    }
);

records_tool!(
    RecordsGet,
    "records_get",
    "RecordsGet",
    Concurrency::Shared,
    "A record by its id, with its version (give the version back when you update it).",
    json!({"type": "object", "properties": {"collection": {"type": "string"},
        "realm": {"type": "string", "description": "Another realm's collection, through a grant it gave yours"}, "id": {"type": "string"}}, "required": ["collection", "id"], "additionalProperties": false}),
    |ctx, input| { call(ctx, "session.records_get", input).await }
);

records_tool!(
    RecordsPut,
    "records_put",
    "RecordsPut",
    Concurrency::Exclusive,
    "Adds a record to a collection (no id) or replaces one (its id, and the version you read). fields is the whole record and \
     must match the collection's schema exactly: no fields it does not name, every required one, the right types. links names \
     things and items it is about (e.g. client:acme); labels are labels; derived_from names the evidence it came from \
     (item:<id> for a memex item, record:<collection>/<id> for a record), so others can check it. An app's collection is \
     written only by the app: use app_call for its operations.",
    json!({"type": "object", "properties": {
        "collection": {"type": "string"},
        "realm": {"type": "string", "description": "Another realm's collection, through a grant it gave yours"},
        "id": {"type": "string"},
        "fields": {"type": "object"},
        "version": {"type": "integer"},
        "links": {"type": "array", "items": {"type": "string"}},
        "labels": {"type": "array", "items": {"type": "string"}},
        "derived_from": {"type": "array", "items": {"type": "string"}}
    }, "required": ["collection", "fields"], "additionalProperties": false}),
    |ctx, input| { call(ctx, "session.records_put", input).await }
);

records_tool!(
    RecordsDelete,
    "records_delete",
    "RecordsDelete",
    Concurrency::Exclusive,
    "Deletes a record (its history is kept).",
    json!({"type": "object", "properties": {"collection": {"type": "string"},
        "realm": {"type": "string", "description": "Another realm's collection, through a grant it gave yours"}, "id": {"type": "string"}}, "required": ["collection", "id"], "additionalProperties": false}),
    |ctx, input| { call(ctx, "session.records_delete", input).await }
);

records_tool!(
    SeriesRecord,
    "series_record",
    "SeriesRecord",
    Concurrency::Shared,
    "Adds a point to a series (numbers over time in your realm, such as revenue or a service's uptime): value, and dims for \
     the series' dimensions (e.g. {\"service\": \"invoicer\"}); ts in ms, default now.",
    json!({"type": "object", "properties": {
        "series": {"type": "string"},
        "value": {"type": "number"},
        "dims": {"type": "object"},
        "ts": {"type": "integer"}
    }, "required": ["series", "value"], "additionalProperties": false}),
    |ctx, input| { call(ctx, "session.series_record", input).await }
);

records_tool!(
    SeriesQuery,
    "series_query",
    "SeriesQuery",
    Concurrency::Shared,
    "Summarizes a series over a window: agg (sum, avg, min, max, last, count), minutes (default a day) or from/to (ms), where \
     (dimension values), group_by (a dimension), bucket (hour or day). Without a series: lists your realm's series, including \
     model_spend, model_tokens and model_calls.",
    json!({"type": "object", "properties": {
        "series": {"type": "string"},
        "query": {"type": "object", "properties": {
            "agg": {"type": "string"}, "minutes": {"type": "integer"}, "from": {"type": "integer"}, "to": {"type": "integer"},
            "where": {"type": "object"}, "group_by": {"type": "string"}, "bucket": {"type": "string"}
        }}
    }, "additionalProperties": false}),
    |ctx, input| { call(ctx, "session.series_query", input).await }
);

records_tool!(
    Work,
    "work",
    "Work",
    Concurrency::Shared,
    "The user's work this session belongs to (docs/work.md): what they mean to finish, what's attached, its to-dos, and where it \
     stands. action get (default) shows it. action attach adds something you made or found that belongs to it, so the user sees it \
     with the work: ref (a URL, such as a pull request or CI run, or doc:<id> for a carbon6 document), title, status (running, waiting, done or failed, as you \
     last saw it) and detail (one line). Attach again to update its status. With to: \"project\", what you made goes on \
     the work's project instead (a doc:<id> with a role: plan, spec, notes or review; or a link), so it outlives this work; get shows \
     the project, its purpose, where it stands and its documents. action todo adds a to-do the user can start as a session \
     later: text (one line) and prompt (what that session is told; make it stand alone). action todo_done ticks one off when it is \
     done: id and evidence (one line).",
    json!({"type": "object", "properties": {
        "action": {"type": "string", "enum": ["get", "attach", "todo", "todo_done"]},
        "ref": {"type": "string"},
        "title": {"type": "string"},
        "status": {"type": "string", "enum": ["running", "waiting", "done", "failed"]},
        "detail": {"type": "string"},
        "text": {"type": "string"},
        "prompt": {"type": "string"},
        "id": {"type": "string"},
        "evidence": {"type": "string"},
        "to": {"type": "string", "enum": ["work", "project"]},
        "role": {"type": "string", "enum": ["plan", "spec", "notes", "review"]}
    }, "additionalProperties": false}),
    |ctx, input| { call(ctx, "session.work", input).await }
);

records_tool!(
    Doc,
    "doc",
    "Doc",
    Concurrency::Shared,
    "carbon6 documents (docs and decks) that you and the user write together and keep (docs/docs.md); they open in a panel the \
     user watches. Markdown in and out; non-text parts are <?c6 block k?> tokens with their values in blocks {k: {...}}. Blocks have \
     stable ids. action guide: how to write one (read it before your first document). action create: kind (doc, deck), title, \
     markdown (a deck's slides separated by ---), blocks; start with a pending block per section ({type: pending, intent}), then fill \
     each with one edit. action read: id, view (outline, block with block=<id>, slide, search with q, since with rev, markdown). \
     action edit: id, ops [{op: insert|replace|delete|move|table|set|add_tab|delete_tab, ...}]; replace a whole block (markdown) or \
     delete one with ifRev and ifHash from your read; replace words with find and with. A refused guard means someone else changed it: \
     read again. action list: documents of your work first (q, kind). action history and restore (id, rev, block). The documents \
     you made and your work's you edit freely; others need the user's OK once.",
    json!({"type": "object", "properties": {
        "action": {"type": "string", "enum": ["guide", "list", "create", "read", "edit", "history", "restore"]},
        "id": {"type": "string", "description": "The document (its id, or doc:<id>)"},
        "kind": {"type": "string", "enum": ["doc", "deck"]},
        "title": {"type": "string"},
        "markdown": {"type": "string"},
        "blocks": {"type": "object", "description": "Values of the <?c6 block k?> tokens: {k: {type: pending, intent}}, chips"},
        "theme": {"type": "string"},
        "labels": {"type": "array", "items": {"type": "string"}},
        "view": {"type": "string", "enum": ["outline", "block", "slide", "search", "since", "markdown", "full"]},
        "block": {"type": "string", "description": "A block or slide id"},
        "q": {"type": "string"},
        "rev": {"type": "integer"},
        "tab": {"type": "string"},
        "ops": {"type": "array", "description": "For edit: operations, applied all or none"},
        "answering": {"type": "string"}
    }, "required": ["action"], "additionalProperties": false})
    .tap_ops(),
    |ctx, input| { call(ctx, "session.doc", input).await }
);

/// The `doc` tool's schema, with what each of `ops` takes (too deep for
/// one `json!`).
trait TapOps {
    fn tap_ops(self) -> Value;
}

impl TapOps for Value {
    fn tap_ops(mut self) -> Value {
        self["properties"]["ops"]["items"] = json!({"type": "object", "properties": {
            "op": {"type": "string", "enum": ["insert", "replace", "delete", "move", "table", "set", "add_tab", "delete_tab"]},
            "id": {"type": "string", "description": "The block or slide it acts on (not the document's id)"},
            "markdown": {"type": "string"},
            "blocks": {"type": "object"},
            "ifRev": {"type": "integer", "description": "The document version you read (whole-block replace and delete)"},
            "ifHash": {"type": "string", "description": "The block's hash when you read it"},
            "find": {"type": "string"}, "with": {"type": "string"}, "nth": {"type": "integer"},
            "after": {"type": "string"}, "before": {"type": "string"}, "section": {"type": "string"},
            "slide": {"type": "string"}, "region": {"type": "string"}, "tab": {"type": "string"},
            "cell": {"type": "array", "items": {"type": "integer"}}, "text": {"type": "string"},
            "layout": {"type": "string"}, "notes": {"type": "string"}, "hidden": {"type": "boolean"},
            "action": {"type": "string"}, "at": {"type": "integer"}, "cells": {"type": "array", "items": {"type": "string"}},
            "title": {"type": "string"}, "theme": {"type": "string"}, "tab_title": {"type": "string"}
        }, "required": ["op"]});
        self
    }
}
