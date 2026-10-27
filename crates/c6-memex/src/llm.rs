//! Asking a model (the understanding task's, or the vision task's), once,
//! through one of carbon6's connections.

use c6_llm::{Accumulator, Block, Message, ModelRequest, Role};
use futures_util::StreamExt;
use serde_json::Value;

use crate::Memex;

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
    let req = ModelRequest {
        model: task.model.clone(),
        system: system.to_owned(),
        messages: vec![Message { role: Role::User, blocks: user }],
        tools: Vec::new(),
        max_output_tokens: Some(max_tokens),
        cache_key: None,
        effort: task.effort.clone().filter(|e| !e.is_empty()),
        web_search: false,
    };
    let mut stream = client.stream(&req).await?;
    let mut acc = Accumulator::default();
    while let Some(ev) = stream.next().await {
        acc.push(&ev?);
    }
    let msg = acc.finish(true);
    let text: String = msg.blocks.iter().filter_map(|b| if let Block::Text(t) = b { Some(t.as_str()) } else { None }).collect();
    anyhow::ensure!(!text.trim().is_empty(), "{} gave no answer (a thinking model may have spent its output tokens thinking)", task.model);
    Ok(text)
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
    fn json_in_prose() {
        let v = super::json_of("Sure! ```json\n{\"summary\": \"ok\", \"topics\": [\"a\"]}\n```").unwrap();
        assert_eq!(v["topics"][0], "a");
        assert!(super::json_of("no json here").is_err());
    }
}
