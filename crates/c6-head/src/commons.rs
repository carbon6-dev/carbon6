//! Commons (docs/os.md §3.3, §3.7): knowledge meant for every realm, such
//! as facts about your infrastructure or reference records. Every realm
//! reads it unless you turn that off; nothing arrives there by default. A
//! realm with an append grant to commons proposes items (records of a
//! collection, or facts), and each waits as a decision until you publish
//! or reject it; only then is it in commons, saying where it came from.

use c6_api as api;
use serde_json::{Value, json};

use crate::realms::COMMONS;
use crate::{HeadState, bad_request, forbidden};

fn options() -> Vec<api::DecisionOption> {
    vec![
        api::DecisionOption { label: "Publish".into(), does: "It goes into commons, for every realm that reads it.".into() },
        api::DecisionOption { label: "Reject".into(), does: "Nothing is published.".into() },
    ]
}

/// A record proposed for commons' collection `name` by realm `from`.
pub async fn propose_record(st: &HeadState, from: &str, by: &str, name: &str, rec: &api::RecordIn) -> anyhow::Result<api::Decision> {
    let grant = crate::grants::may_publish(st, from, Some(name))
        .await
        .ok_or_else(|| forbidden(format!("realm {from} may not publish to commons' {name} (it needs an append grant)")))?;
    let c = crate::collections::get(st, COMMONS, name).await?;
    if let Err(e) = crate::schema::validate(&c.schema, &rec.fields) {
        return Err(bad_request(format!("it does not fit commons' {name}: {}", e.join("; "))));
    }
    let d = api::DecisionIn {
        kind: "approve".into(),
        title: format!("Publish to commons' {name}, from realm {from}?"),
        document: format!(
            "Realm **{from}** ({by}) proposes a record for **{name}** in commons, which every realm reads.\n\n```json\n{}\n```",
            serde_json::to_string_pretty(&rec.fields).unwrap_or_default()
        ),
        context: json!({"publish": "record", "from": from, "by": by, "grant": grant, "collection": name,
                        "fields": rec.fields, "links": rec.links, "labels": rec.labels}),
        source: by.to_owned(),
        options: options(),
        ..Default::default()
    };
    crate::decisions::open(st, &d, COMMONS, &format!("publish:{from}")).await
}

/// A fact of realm `from`'s knowledge, proposed for commons.
pub async fn propose_fact(st: &HeadState, from: &str, by: &str, fact_id: &str) -> anyhow::Result<api::Decision> {
    let grant = crate::grants::may_publish(st, from, None)
        .await
        .ok_or_else(|| forbidden(format!("realm {from} may not publish knowledge to commons (it needs an append grant)")))?;
    let found = crate::memex::call_in(st, from, "memex.knowledge.get", json!({"ids": [fact_id]})).await?;
    let fact = found.get(0).cloned().unwrap_or(Value::Null);
    let claim =
        fact["claim"].as_str().filter(|c| !c.is_empty()).ok_or_else(|| bad_request(format!("no fact {fact_id} in realm {from}")))?;
    let about: Vec<&str> = fact["about"].as_array().map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
    let evidence = fact["evidence"].as_array().and_then(|e| e.last()).cloned().unwrap_or(Value::Null);
    let d = api::DecisionIn {
        kind: "approve".into(),
        title: format!("Publish a fact to commons, from realm {from}?"),
        document: format!(
            "Realm **{from}** ({by}) proposes a fact for commons, which every realm reads.\n\n> {claim}\n\nAbout: {}\n\nKnown as: {}{}",
            about.join(", "),
            fact["status"].as_str().unwrap_or("?"),
            match evidence["excerpt"].as_str().or(evidence["source"].as_str()).filter(|s| !s.is_empty()) {
                Some(e) => format!("\n\nEvidence: {e}"),
                None => String::new(),
            }
        ),
        context: json!({"publish": "fact", "from": from, "by": by, "grant": grant, "claim": claim, "about": about,
                        "volatility": fact["volatility"], "conditions": fact["conditions"], "evidence": evidence}),
        source: by.to_owned(),
        options: options(),
        ..Default::default()
    };
    crate::decisions::open(st, &d, COMMONS, &format!("publish:{from}")).await
}

/// Your answer to a proposal: published, it goes into commons as yours,
/// reviewed, carrying where it came from.
pub async fn answer(st: &HeadState, d: &api::Decision, publish: bool) -> anyhow::Result<()> {
    if !publish {
        return Ok(());
    }
    let c = &d.context;
    let from = c["from"].as_str().unwrap_or_default();
    let by = format!("{} published from realm {from}, reviewed by you", c["by"].as_str().unwrap_or("?"));
    match c["publish"].as_str() {
        Some("record") => {
            let mut labels: Vec<String> = serde_json::from_value(c["labels"].clone()).unwrap_or_default();
            labels.push(format!("from:{from}"));
            let rec = api::RecordIn {
                fields: c["fields"].clone(),
                links: serde_json::from_value(c["links"].clone()).ok(),
                labels: Some(labels),
                version: None,
                derived_from: serde_json::from_value(c["derived_from"].clone()).ok(),
            };
            crate::collections::put(st, COMMONS, c["collection"].as_str().unwrap_or_default(), None, &rec, &by).await?;
        }
        Some("fact") => {
            let mut params = json!({"claim": c["claim"], "about": c["about"], "status": "inferred", "made_by": by,
                                    "volatility": c["volatility"].as_str().unwrap_or("stable"), "evidence": c["evidence"]});
            if let Some(cond) = c["conditions"].as_str() {
                params["conditions"] = json!(cond);
            }
            let made = crate::memex::call_in(st, COMMONS, "memex.knowledge.record", params).await?;
            let id = made["id"].as_str().unwrap_or_default();
            crate::memex::call_in(st, COMMONS, "memex.knowledge.set", json!({"id": id, "status": "reviewed", "made_by": "you"})).await?;
        }
        _ => return Err(bad_request("not a proposal for commons")),
    }
    Ok(())
}

/// An agent's call: `commons_publish`, a record or a fact of its realm.
pub async fn from_session(st: &HeadState, session_id: &str, realm: &str, p: &Value) -> anyhow::Result<Value> {
    let by = format!("session:{session_id}");
    let d = if let Some(fact) = p["fact"].as_str().filter(|f| !f.is_empty()) {
        propose_fact(st, realm, &by, fact).await?
    } else {
        let name = p["collection"]
            .as_str()
            .filter(|c| !c.is_empty())
            .ok_or_else(|| bad_request("give a fact's id, or a collection and fields"))?;
        let rec = api::RecordIn {
            fields: p["fields"].clone(),
            links: serde_json::from_value(p["links"].clone()).ok(),
            labels: serde_json::from_value(p["labels"].clone()).ok(),
            version: None,
            derived_from: serde_json::from_value(p["derived_from"].clone()).ok(),
        };
        propose_record(st, realm, &by, name, &rec).await?
    };
    Ok(json!({"decision": d.id}))
}
