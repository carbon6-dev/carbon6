//! Claims, reader policies and the shared vocabulary (docs/apps.md §9), on
//! the head's side: who owns a claim (the head stamps it, never the
//! caller), which claims a realm's readers believe by default, and what
//! labels mean. The claims themselves are in the knowledge store.

use c6_api as api;
use serde_json::{Value, json};

use crate::realms::ROOT;
use crate::{HeadState, bad_request};

/// Settings lists: a realm's reader policies, and root's vocabulary.
pub const POLICIES: &str = "claim_policies";
pub const VOCABULARY: &str = "vocabulary";

/// The shared vocabulary carbon6 starts with (memex.md §4.3, and the item
/// kinds): yours to change in Settings.
const DEFAULT_VOCABULARY: &[(&str, &str, &str)] = &[
    ("sent_by", "label", "The item (an email, a message) was written and sent by the person."),
    ("sent_to", "label", "The item was sent to the person."),
    ("cc", "label", "The person was copied on the item."),
    ("about", "label", "The item or thing is about the other thing."),
    ("mentions", "label", "The item mentions the thing, without being about it."),
    ("authored", "label", "The person wrote the item."),
    ("cites", "label", "The item cites the other item."),
    ("part_of", "label", "The thing is a part of the other thing (a team of a company, a page of a document)."),
    ("member_of", "label", "The person belongs to the organization or group."),
    ("works_at", "label", "The person works at the organization."),
    ("knows", "label", "The person knows the other person."),
    ("attended", "label", "The person attended the event."),
    ("located_at", "label", "The thing or item is at the place."),
    ("appears_in", "label", "The person appears in the photo or video."),
    ("decided", "label", "The person or group made the decision."),
    ("committed_to", "label", "The person committed to doing the thing."),
    ("supports", "label", "The item supports the claim or fact."),
    ("contradicts", "label", "The item contradicts the claim or fact."),
    ("same_as", "label", "The two things are the same (a person under two names or addresses)."),
    ("not_same_as", "label", "The two things are not the same, though they may look it."),
    ("email", "kind", "An email message."),
    ("event", "kind", "A calendar event."),
    ("contact", "kind", "An entry in an address book."),
    ("document", "kind", "A document: PDF, Word, Markdown, text, HTML."),
    ("photo", "kind", "A photo."),
    ("video", "kind", "A video."),
    ("note", "kind", "A note written by you or an agent."),
    ("session", "kind", "A carbon6 session's digest."),
    ("record", "kind", "A record of a collection, mirrored for search."),
];

/// Puts the default vocabulary in root once (your edits are kept).
pub async fn seed(pool: &sqlx::SqlitePool) -> anyhow::Result<()> {
    let seeded: Option<String> =
        sqlx::query_scalar("SELECT value FROM settings WHERE key = 'vocabulary_seeded'").fetch_optional(pool).await?;
    if seeded.is_some() {
        return Ok(());
    }
    let entries: Vec<Value> =
        DEFAULT_VOCABULARY.iter().map(|(name, kind, means)| json!({"name": name, "kind": kind, "means": means})).collect();
    crate::lists::write(pool, VOCABULARY, ROOT, &entries, "system").await?;
    sqlx::query("INSERT INTO settings (key, value) VALUES ('vocabulary_seeded', '1')").execute(pool).await?;
    Ok(())
}

pub fn check_policies(p: &[api::ClaimPolicy]) -> anyhow::Result<()> {
    let mut seen = std::collections::HashSet::new();
    for x in p {
        if x.label.trim().is_empty() || x.believe.is_empty() {
            return Err(bad_request("a policy names a label and whose claims to believe, in order"));
        }
        if !seen.insert(x.label.as_str()) {
            return Err(bad_request(format!("two policies for {}", x.label)));
        }
    }
    Ok(())
}

pub fn check_vocabulary(v: &[api::VocabularyEntry]) -> anyhow::Result<()> {
    for e in v {
        if e.name.trim().is_empty() || !matches!(e.kind.as_str(), "label" | "kind") || e.means.trim().is_empty() {
            return Err(bad_request("a vocabulary entry is a name, its kind (label or kind) and what it means"));
        }
    }
    Ok(())
}

/// The reader policies a realm's readers use: its own (and what its apps
/// bring), over root's.
pub async fn policies(st: &HeadState, realm: &str) -> Vec<api::ClaimPolicy> {
    crate::lists::effective(&st.store.pool, POLICIES, realm)
        .await
        .unwrap_or_default()
        .into_iter()
        .filter_map(|v| serde_json::from_value(v).ok())
        .collect()
}

/// What labels mean in a realm: the shared vocabulary, and the labels its
/// apps declare (with the shared label each maps to).
pub async fn vocabulary(st: &HeadState, realm: &str) -> Vec<(String, api::VocabularyEntry)> {
    let mut out: Vec<(String, api::VocabularyEntry)> = crate::lists::read(&st.store.pool, VOCABULARY, ROOT)
        .await
        .unwrap_or_default()
        .into_iter()
        .filter_map(|v| serde_json::from_value(v).ok())
        .map(|e| (String::new(), e))
        .collect();
    for i in crate::instances::list(&st.store.pool, Some(realm)).await.unwrap_or_default() {
        out.extend(i.package.vocabulary.iter().cloned().map(|e| (format!("app:{}", i.name), e)));
    }
    out
}

/// The owner of what a caller asserts: you, an app instance, or the realm
/// (its own agents and tokens).
pub async fn owner_of_session(st: &HeadState, session_id: &str) -> String {
    match crate::instances::of_session(&st.store.pool, session_id).await.ok().flatten() {
        Some(i) => format!("app:{}", i.name),
        None => "realm".into(),
    }
}

/// A claims call to the knowledge store in `realm`, as `owner` (`made_by`:
/// exactly who). The head stamps both; listing applies the realm's
/// policies and says what each label means; a label filter finds the
/// labels mapped to it too.
pub async fn call(st: &HeadState, realm: &str, method: &str, mut params: Value, owner: &str, made_by: &str) -> anyhow::Result<Value> {
    params["owner"] = json!(owner);
    params["made_by"] = json!(made_by);
    if method != "memex.claims.list" {
        if !matches!(method, "memex.claims.add" | "memex.claims.retract" | "memex.things.create") {
            return Err(bad_request(format!("{method} is not a claims call")));
        }
        return crate::memex::call_in(st, realm, method, params).await;
    }
    // Reading: every claim the caller asked for, then the realm's policies,
    // with owners' labels judged as the shared labels they map to.
    let all = params["policy"].as_str() == Some("all");
    params["owner"] = params.get("filter_owner").cloned().unwrap_or(json!(""));
    params["policy"] = json!("all");
    let policies = policies(st, realm).await;
    let vocab = vocabulary(st, realm).await;
    let shared = |owner: &str, label: &str| -> Option<String> {
        vocab.iter().find(|(o, e)| o == owner && e.name == label && !e.maps_to.is_empty()).map(|(_, e)| e.maps_to.clone())
    };
    let wanted = params["label"].as_str().unwrap_or("").to_owned();
    let mut claims: Vec<api::Claim> = if wanted.is_empty() {
        serde_json::from_value(crate::memex::call_in(st, realm, method, params).await?)?
    } else {
        // The label, and every owner's label mapped to it.
        let mut labels: Vec<String> = vec![wanted.clone()];
        labels.extend(vocab.iter().filter(|(_, e)| e.maps_to == wanted).map(|(_, e)| e.name.clone()));
        labels.dedup();
        let mut out = Vec::new();
        for l in labels {
            let mut p = params.clone();
            p["label"] = json!(l);
            out.extend(serde_json::from_value::<Vec<api::Claim>>(crate::memex::call_in(st, realm, method, p).await?)?);
        }
        out
    };
    let own_labels: Vec<String> = claims.iter().map(|c| c.label.clone()).collect();
    for c in &mut claims {
        if let Some(s) = shared(&c.owner, &c.label) {
            c.label = s;
        }
    }
    c6_memex::claims::apply(&mut claims, &policies);
    for (c, own) in claims.iter_mut().zip(own_labels) {
        c.label = own;
        let entry = vocab
            .iter()
            .find(|(o, e)| *o == c.owner && e.name == c.label)
            .or_else(|| vocab.iter().find(|(o, e)| o.is_empty() && e.name == c.label));
        if let Some((_, e)) = entry {
            c.means = if e.maps_to.is_empty() { e.means.clone() } else { format!("{} (as {})", e.means, e.maps_to) };
        }
    }
    if !all {
        claims.retain(|c| c.believed);
    }
    Ok(json!(claims))
}
