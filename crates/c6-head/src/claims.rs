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
];

/// The shared item kinds (docs/knowledge.md §6.2, #6): what each means,
/// the shape of its items' `meta` (every app's items of the kind are
/// checked against it), and its data card. Gmail, IMAP and an import all
/// make the same `email`.
fn default_kinds() -> Vec<Value> {
    let s = |d: &str| json!({"type": "string", "description": d});
    let n = |d: &str| json!({"type": "integer", "description": d});
    let list = |d: &str| json!({"type": "array", "items": {"type": "string"}, "description": d});
    let file = || {
        json!({
            "name": s("Its file name"), "mime": s("Its MIME type"), "size": n("Bytes"),
            "blob": s("The content's blob (its SHA-256), when the store holds it"),
            "path": s("Where it is, in a folder on a worker"), "worker": s("That worker (empty: the head's machine)"),
        })
    };
    let card = |sources: &str, invariants: &[&str], freshness: &str| json!({"stability": "published", "sources": sources, "invariants": invariants, "freshness": freshness});
    let mut photo = file();
    photo.as_object_mut().unwrap().extend(
        json!({
            "width": n("Pixels"), "height": n("Pixels"), "thumb": s("A JPEG thumbnail's blob, if not the store's own"),
            "camera": s("The camera"), "lat": {"type": "number"}, "lon": {"type": "number"},
        })
        .as_object()
        .cloned()
        .unwrap(),
    );
    let mut video = photo.clone();
    video["duration"] = json!({"type": "number", "description": "Seconds"});
    let mut paper = file();
    paper["paper"] = json!({"type": "object", "description": "Its identity: doi, arxiv, authors, year, references (a count)"});
    let kind = |name: &str, means: &str, schema: Value, card: Value| {
        let mut e = json!({"name": name, "kind": "kind", "means": means, "card": card});
        if !schema.is_null() {
            e["schema"] = schema;
        }
        e
    };
    let obj = |props: Value, required: &[&str]| {
        let mut o = json!({"type": "object", "properties": props});
        if !required.is_empty() {
            o["required"] = json!(required);
        }
        o
    };
    vec![
        kind(
            "email",
            "An email message. Its created_at is when it was sent; its text starts with From and To lines.",
            obj(
                json!({
                    "thread_id": s("The conversation it belongs to: the same for every message of a thread"),
                    "from": s("The sender's address"), "to": list("Addresses it was sent to"), "cc": list("Addresses copied"),
                    "account": s("The mailbox it is in"), "attachments": n("How many attachments it has"),
                    "html": s("Its HTML's blob, if it has HTML"),
                }),
                &["thread_id"],
            ),
            card(
                "Mail apps (Gmail, IMAP, an import): one item per message, sent_by, sent_to and cc claims to people.",
                &["Messages of one conversation share thread_id.", "Spam is the kind spam, not email, and is not shown to agents."],
                "As often as its app syncs (Gmail: minutes).",
            ),
        ),
        kind(
            "chat",
            "A chat message (Slack and the like). Its created_at is when it was posted; its text has mentions and channel links as names.",
            obj(
                json!({
                    "service": s("Which chat service: slack"), "workspace": s("The workspace's id"),
                    "workspace_name": s("The workspace's name"), "channel": s("The conversation's id"),
                    "channel_name": s("The conversation's name: #eng, or the people of a DM"),
                    "channel_kind": s("public, private, dm or group"), "ts": s("The message's id in its conversation"),
                    "thread_ts": s("The thread it replies in (its first message's ts)"), "user": s("Who posted it (the service's id)"),
                    "user_name": s("Their name"), "reply_count": n("Replies, for a thread's first message"),
                    "reactions": {"type": "object", "description": "Reaction name to count"},
                    "files": {"type": "array", "description": "Files shared with it: name, mime, size, id"},
                    "edited": {"type": "boolean"}, "deleted": {"type": "boolean", "description": "Gone from the service; kept here"},
                    "from_me": {"type": "boolean", "description": "Posted by you"},
                    "user_email": s("Who posted it, by address (when the service says)"),
                    "mentions_me": {"type": "boolean", "description": "It mentions you"},
                }),
                &["workspace", "channel", "ts"],
            ),
            card(
                "Chat apps (Slack): one item per message, about its channel and the people it's from and mentions.",
                &["A reply's thread_ts is its thread's first message's ts.", "Messages of one conversation share workspace and channel."],
                "As often as its app syncs (Slack: minutes).",
            ),
        ),
        kind(
            "attachment",
            "A file attached to an email.",
            obj(
                json!({"message": s("The email item it came with"), "index": n("Its place among the message's attachments")}),
                &["message"],
            )
            .as_object()
            .cloned()
            .map(|mut o| {
                o["properties"].as_object_mut().unwrap().extend(file().as_object().cloned().unwrap());
                Value::Object(o)
            })
            .unwrap(),
            card("Mail apps, from the messages they keep.", &["An attachment's message is an email item."], "With its message."),
        ),
        kind(
            "event",
            "A calendar event. Its created_at is when it starts.",
            obj(
                json!({
                    "end": n("When it ends (ms since the epoch)"), "all_day": {"type": "boolean"},
                    "location": s("Where"), "calendar": s("The calendar it is in"), "account": s("The account"),
                    "attendees": list("Attendees' addresses"), "status": s("confirmed, tentative or cancelled"),
                }),
                &[],
            ),
            card(
                "Calendar apps (Google Calendar, CalDAV): one item per event or occurrence, attended and invited claims.",
                &["end is at or after the start (created_at)."],
                "As often as its app syncs.",
            ),
        ),
        kind(
            "contact",
            "An entry in an address book.",
            obj(json!({"emails": list("Its addresses"), "phones": list("Its numbers"), "organization": s("Where they work")}), &[]),
            card("Contacts apps (Google Contacts, CardDAV).", &[], "As often as its app syncs."),
        ),
        kind(
            "document",
            "A document: PDF, Word, Markdown, text, HTML.",
            obj(file(), &[]),
            card(
                "Uploads, watched folders and imports; its text as the c6-extract kit read it.",
                &[],
                "When added, and when its file changes.",
            ),
        ),
        kind(
            "paper",
            "A research paper: a document that says what it is and what it cites.",
            obj(paper, &[]),
            card(
                "Documents recognized as papers.",
                &["It is linked to its paper thing (is), its authors and the papers it cites."],
                "When added.",
            ),
        ),
        kind(
            "photo",
            "A photo. Its created_at is when it was taken.",
            obj(photo, &[]),
            card(
                "Watched folders and photo apps; EXIF and a thumbnail as the c6-extract kit read them.",
                &[],
                "When its folder is scanned.",
            ),
        ),
        kind(
            "video",
            "A video. Its created_at is when it was recorded.",
            obj(video, &[]),
            card("Watched folders and photo apps.", &[], "When its folder is scanned."),
        ),
        kind("note", "A note written by you or an agent.", Value::Null, json!({})),
        kind("session", "A carbon6 session's digest.", Value::Null, json!({})),
        kind("record", "A record of a collection, mirrored for search.", Value::Null, json!({})),
    ]
}

/// The default vocabulary's version: a new one adds the kinds that came
/// later (3: chat) and the fields their schemas gained (4: chat's from_me,
/// mentions_me and user_email).
const SEEDED: &str = "4";

/// Puts the default vocabulary in root (your edits are kept): once, and
/// then the item kinds' schemas and cards, kinds that came later, and
/// fields a kind's schema gained (an item with them is refused without).
pub async fn seed(pool: &sqlx::SqlitePool) -> anyhow::Result<()> {
    let seeded: Option<String> =
        sqlx::query_scalar("SELECT value FROM settings WHERE key = 'vocabulary_seeded'").fetch_optional(pool).await?;
    if seeded.as_deref() == Some(SEEDED) {
        return Ok(());
    }
    let mut entries: Vec<Value> = match seeded {
        None => DEFAULT_VOCABULARY.iter().map(|(name, kind, means)| json!({"name": name, "kind": kind, "means": means})).collect(),
        Some(_) => crate::lists::read(pool, VOCABULARY, ROOT).await?,
    };
    for k in default_kinds() {
        match entries.iter_mut().find(|e| e["name"] == k["name"] && e["kind"] == "kind") {
            Some(e) => {
                for f in ["schema", "card"] {
                    if e.get(f).is_none_or(Value::is_null) && !k[f].is_null() {
                        e[f] = k[f].clone();
                    }
                }
                if let (Some(have), Some(new)) = (e["schema"]["properties"].as_object_mut(), k["schema"]["properties"].as_object()) {
                    for (name, field) in new {
                        have.entry(name.clone()).or_insert_with(|| field.clone());
                    }
                }
            }
            None => entries.push(k),
        }
    }
    crate::lists::write(pool, VOCABULARY, ROOT, &entries, "system").await?;
    sqlx::query("INSERT INTO settings (key, value) VALUES ('vocabulary_seeded', ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value")
        .bind(SEEDED)
        .execute(pool)
        .await?;
    Ok(())
}

/// An item kind's schema in the shared vocabulary, if it has one.
pub async fn kind_schema(st: &HeadState, kind: &str) -> Option<Value> {
    crate::lists::read(&st.store.pool, VOCABULARY, ROOT)
        .await
        .ok()?
        .into_iter()
        .find(|e| e["kind"] == "kind" && e["name"] == kind)
        .map(|e| e["schema"].clone())
        .filter(|s| !s.is_null())
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
        if !e.schema.is_null() {
            if e.kind != "kind" {
                return Err(bad_request(format!("{}: only item kinds have a schema", e.name)));
            }
            crate::schema::check_schema(&e.schema).map_err(|m| bad_request(format!("{}: {m}", e.name)))?;
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
