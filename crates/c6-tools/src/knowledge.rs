//! Environment knowledge and action items (docs/follow-through.md §6–7):
//! agents record what they learn about the systems around the code (with
//! how they know it), re-check facts they rely on, and raise questions for
//! the user instead of stopping to ask.

use async_trait::async_trait;
use c6_llm::ToolSpec;
use serde_json::{Value, json};

use crate::{Concurrency, Tool, ToolCtx, ToolOutput};

async fn call(ctx: &ToolCtx, method: &str, params: Value) -> Result<Value, String> {
    let head = ctx.session.head().ok_or("carbon6's head is not reachable from here")?;
    head.call(method, params).await
}

macro_rules! simple_tool {
    ($name:ident, $plain:literal, $claude:literal, $desc:expr, $params:expr, $conc:expr, |$ctx:ident, $input:ident| $body:block) => {
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

fn result(r: Result<Value, String>, ok: impl FnOnce(&Value) -> String) -> ToolOutput {
    match r {
        Ok(v) => ToolOutput::ok(ok(&v)),
        Err(e) => ToolOutput::err(e),
    }
}

simple_tool!(
    KnowledgeRecord,
    "knowledge_record",
    "KnowledgeRecord",
    "Records something you figured out about the environment around this code (CI, clusters, networks, hosts, services, how to do \
     things there) so later sessions start with it. Record everything you learn, honestly labelled: status observed (you saw it: give \
     evidence, the command and an excerpt of its output, or the file you read) or inferred (you concluded it: name the facts it rests on \
     in supports). Volatility: stable (structural; true until someone redesigns something) or slow (addresses, versions, which machine \
     reaches what; re-checked after 30 days). Current states (\"CI is red now\") are not facts. Give a check command that proves it when \
     you can, where it applies (labels such as repo:…, conditions) and negative: true for \"this does not apply to …\". The same claim \
     again adds evidence. If it contradicts a fact you were given, pass contradicts with that fact's id (the user settles it later); if it \
     replaces an outdated one, pass replaces.",
    json!({"type": "object", "properties": {
        "claim": {"type": "string", "description": "One plain statement, e.g. CI runs as Argo Workflows in namespace ci, labelled git-sha=<commit>"},
        "about": {"type": "array", "items": {"type": "string"}, "description": "The systems it is about, as keys kind:name, e.g. [\"ci:argo@internal\", \"repo:github.com/you/app\"]"},
        "status": {"type": "string", "enum": ["observed", "inferred"]},
        "volatility": {"type": "string", "enum": ["stable", "slow"]},
        "evidence": {"type": "object", "properties": {
            "command": {"type": "string"}, "excerpt": {"type": "string"}, "source": {"type": "string", "description": "a file or page you read"}, "where": {"type": "string", "description": "the machine"}
        }},
        "check": {"type": "object", "properties": {"command": {"type": "string"}, "on": {"type": "string", "enum": ["session", "worker", "head"]}, "worker": {"type": "string"}}},
        "labels": {"type": "array", "items": {"type": "string"}, "description": "Applies only to sessions with these labels"},
        "conditions": {"type": "string", "description": "And only when, e.g. for release builds"},
        "negative": {"type": "boolean"},
        "supports": {"type": "array", "items": {"type": "string"}, "description": "Fact ids an inference rests on"},
        "replaces": {"type": "string"},
        "contradicts": {"type": "string"}
    }, "required": ["claim", "about"], "additionalProperties": false}),
    Concurrency::Shared,
    |ctx, input| {
        result(call(ctx, "memex.knowledge.record", input).await, |v| {
            let how = if v["merged"] == true { "added evidence to" } else { "recorded" };
            let note = v["note"].as_str().filter(|n| !n.is_empty()).map(|n| format!(" ({n})")).unwrap_or_default();
            format!("{how} fact {} ({}){note}", v["id"].as_str().unwrap_or(""), v["status"].as_str().unwrap_or(""))
        })
    }
);

simple_tool!(
    KnowledgeVerify,
    "knowledge_verify",
    "KnowledgeVerify",
    "Reports that you re-checked a fact (by its id), with the evidence: holds true marks it verified (and when); false marks it \
     disputed and raises an action item for the user. Re-check facts that are due or only observed/inferred before relying on them for \
     something that matters.",
    json!({"type": "object", "properties": {
        "id": {"type": "string"},
        "holds": {"type": "boolean"},
        "evidence": {"type": "object", "properties": {"command": {"type": "string"}, "excerpt": {"type": "string"}, "where": {"type": "string"}}},
        "note": {"type": "string"}
    }, "required": ["id", "holds"], "additionalProperties": false}),
    Concurrency::Shared,
    |ctx, input| {
        result(call(ctx, "memex.knowledge.verify", input).await, |v| {
            format!("fact {} is now {}", v["id"].as_str().unwrap_or(""), v["status"].as_str().unwrap_or(""))
        })
    }
);

simple_tool!(
    KnowledgeList,
    "knowledge_list",
    "KnowledgeList",
    "Lists environment knowledge: facts about systems (by key, e.g. ci:argo or repo:github.com/you/app), with status, where they \
     apply, their check and evidence. Filter by status or due (re-check due). Use memex_search to find facts by words.",
    json!({"type": "object", "properties": {
        "about": {"type": "array", "items": {"type": "string"}},
        "status": {"type": "string"},
        "due": {"type": "boolean"}
    }, "additionalProperties": false}),
    Concurrency::Shared,
    |ctx, input| {
        result(call(ctx, "memex.knowledge.list", input).await, |v| {
            let facts = v.as_array().cloned().unwrap_or_default();
            if facts.is_empty() {
                return "No facts.".into();
            }
            facts
                .iter()
                .map(|f| {
                    format!(
                        "[fact {}] ({}, {}{}) {}{} — about {}{}{}",
                        f["id"].as_str().unwrap_or(""),
                        f["status"].as_str().unwrap_or(""),
                        f["volatility"].as_str().unwrap_or(""),
                        if f["due"] == true { ", due" } else { "" },
                        if f["negative"] == true { "NOT: " } else { "" },
                        f["claim"].as_str().unwrap_or(""),
                        f["about"]
                            .as_array()
                            .map(|a| a.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(", "))
                            .unwrap_or_default(),
                        f["check"]["command"].as_str().map(|c| format!(" · check: `{c}`")).unwrap_or_default(),
                        f["labels"]
                            .as_array()
                            .filter(|a| !a.is_empty())
                            .map(|a| format!(" · only {}", a.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(",")))
                            .unwrap_or_default(),
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        })
    }
);

simple_tool!(
    KnowledgeSet,
    "knowledge_set",
    "KnowledgeSet",
    "Records the user's decision about a fact: reviewed (they confirmed it), retired (no longer true), a corrected claim (replaces it, \
     reviewed), or where it applies (labels, conditions). Only for what the user decided in this conversation.",
    json!({"type": "object", "properties": {
        "id": {"type": "string"},
        "status": {"type": "string", "enum": ["reviewed", "retired", "disputed"]},
        "claim": {"type": "string"},
        "labels": {"type": "array", "items": {"type": "string"}},
        "conditions": {"type": "string"}
    }, "required": ["id"], "additionalProperties": false}),
    Concurrency::Shared,
    |ctx, input| {
        result(call(ctx, "memex.knowledge.set", input).await, |v| {
            format!("fact {} is now {}", v["id"].as_str().unwrap_or(""), v["status"].as_str().unwrap_or(""))
        })
    }
);

simple_tool!(
    TodoAdd,
    "todo_add",
    "TodoAdd",
    "Raises an action item for the user to settle when they want to, instead of stopping to ask: a question you could not answer, \
     something to confirm, a credential they need to provide, a follow-up. Carry on with your best assumption and say it in detail. \
     Give context (fact ids, systems, links) so they can settle it later; the same group again counts as one item.",
    json!({"type": "object", "properties": {
        "kind": {"type": "string", "enum": ["confirm", "provide", "question", "follow_up"]},
        "title": {"type": "string"},
        "detail": {"type": "string", "description": "What you assumed meanwhile, and why it matters"},
        "context": {"type": "object", "description": "e.g. {\"facts\": [ids], \"things\": [keys], \"worker\": \"build-1\", \"links\": [urls]}"},
        "priority": {"type": "string", "enum": ["high", "normal", "low"]},
        "group": {"type": "string", "description": "Items with the same group are one item"}
    }, "required": ["title"], "additionalProperties": false}),
    Concurrency::Shared,
    |ctx, input| {
        result(call(ctx, "session.todo", input).await, |v| format!("action item #{} raised (seen {} time(s))", v["id"], v["count"]))
    }
);

simple_tool!(
    DecisionRequest,
    "decision_request",
    "DecisionRequest",
    "Asks the user to decide something, without stopping: carry on with what you can meanwhile, and the answer arrives later in \
     this conversation as a message (\"Your question was answered (decision #N …)\"). Give the case in document: what you need, \
     why, the evidence, and what you do meanwhile. Offer options when there are a few clear answers (each with what it does), or a \
     form for values to fill in. With a deadline (in minutes), give the default answer taken if nobody answers by then.",
    json!({"type": "object", "properties": {
        "title": {"type": "string", "description": "The question, in a line"},
        "document": {"type": "string", "description": "The case (Markdown)"},
        "options": {"type": "array", "items": {"type": "object", "properties": {
            "label": {"type": "string"}, "does": {"type": "string", "description": "What happens if chosen"}
        }, "required": ["label"]}},
        "form": {"type": "array", "items": {"type": "object", "properties": {
            "name": {"type": "string"}, "label": {"type": "string"}, "kind": {"type": "string", "enum": ["text", "long", "choice"]},
            "choices": {"type": "array", "items": {"type": "string"}}, "required": {"type": "boolean"}
        }, "required": ["name"]}},
        "priority": {"type": "string", "enum": ["high", "normal", "low"]},
        "deadline_minutes": {"type": "integer", "description": "Answer needed within this many minutes"},
        "default_answer": {"type": "string", "description": "The option taken at the deadline"},
        "group": {"type": "string", "description": "Shown together with others of the same group"},
        "context": {"type": "object"}
    }, "required": ["title", "document"], "additionalProperties": false}),
    Concurrency::Shared,
    |ctx, input| {
        result(call(ctx, "session.decision", input).await, |v| {
            format!("decision #{} asked; carry on, and its answer will arrive as a message", v["id"])
        })
    }
);

simple_tool!(
    GrantRequest,
    "grant_request",
    "GrantRequest",
    "Asks the user to let this realm use another realm's data, without stopping: the request comes to them as a decision, and \
     its answer arrives as a message. kind collection with mode read (its records; scope {collection, where?}), append (add \
     records) or snapshot (a copy once); kind memex with mode read (its knowledge; scope {kinds?}); kind operations with mode \
     call (scope {methods: [\"memex.mail.draft\", …]}). Once allowed, give realm on records_* calls to reach the collection; \
     memex searches include the granted knowledge, and granted operations run in the realm that owns them.",
    json!({"type": "object", "properties": {
        "from_realm": {"type": "string", "description": "The realm whose data you need"},
        "kind": {"type": "string", "enum": ["collection", "memex", "operations"]},
        "mode": {"type": "string", "enum": ["read", "append", "snapshot", "call"]},
        "scope": {"type": "object"},
        "why": {"type": "string", "description": "What you need it for, for the user"}
    }, "required": ["from_realm", "kind", "mode", "scope", "why"], "additionalProperties": false}),
    Concurrency::Shared,
    |ctx, input| {
        result(call(ctx, "session.grant_request", input).await, |v| {
            format!(
                "grant {} requested (decision #{}); carry on, and the answer will arrive as a message",
                v["grant"].as_str().unwrap_or(""),
                v["decision"]
            )
        })
    }
);

simple_tool!(
    TodoResolve,
    "todo_resolve",
    "TodoResolve",
    "Marks an action item settled, with a one-line resolution (when the user and you settled it in this conversation).",
    json!({"type": "object", "properties": {"id": {"type": "integer"}, "resolution": {"type": "string"}}, "required": ["id", "resolution"], "additionalProperties": false}),
    Concurrency::Shared,
    |ctx, input| {
        result(call(ctx, "session.todo_resolve", input).await, |v| {
            format!("action item #{} is {}", v["id"], v["state"].as_str().unwrap_or(""))
        })
    }
);
