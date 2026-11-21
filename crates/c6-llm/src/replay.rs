//! Provider-private alias history is replayable only to its known source account.

use std::collections::{HashMap, VecDeque, hash_map::Entry};
use std::sync::{Arc, Mutex};

use futures::StreamExt;

use crate::{Block, EventStream, ModelEvent, ModelRequest, Role};

const MAX_BLOCKS: usize = 4096;
const MAX_BYTES: usize = 16 * 1024 * 1024;

type Scope = (String, String); // actual caller session, alias ID
type Source = (String, String); // provider kind, real connection ID

#[derive(PartialEq, Eq, Hash)]
struct Key {
    scope: Arc<Scope>,
    block: Vec<u8>,
}

impl Key {
    fn new(scope: &Arc<Scope>, block: &Block) -> Option<Self> {
        // Exact content, not an ID supplied by the provider or a lossy hash.
        Some(Self { scope: scope.clone(), block: serde_json::to_vec(block).ok()? })
    }
}

#[derive(Default)]
struct Known {
    blocks: HashMap<Arc<Key>, Option<Arc<Source>>>,
    order: VecDeque<Arc<Key>>,
    bytes: usize,
}

/// In-memory provenance. Stale entries are evicted in insertion order;
/// history loaded on a new worker and evicted blocks are both untrusted.
#[derive(Default)]
pub struct AliasHistory(Mutex<Known>);

impl AliasHistory {
    /// Filters a request without changing the saved conversation. A missing
    /// caller key or actual account means no private blocks can be trusted.
    pub fn prepare(self: &Arc<Self>, req: &mut ModelRequest, alias: &str, kind: &str, account: &str) -> AliasReplay {
        let scope = req.cache_key.as_deref().filter(|key| !key.is_empty()).map(|key| Arc::new((key.to_owned(), alias.to_owned())));
        let source = Arc::new((kind.to_owned(), account.to_owned()));
        let replay = AliasReplay { history: self.clone(), scope: scope.filter(|_| !account.is_empty()), source };
        let known = self.0.lock().unwrap();
        req.messages.retain_mut(|m| {
            m.blocks.retain(|block| {
                !private(block)
                    || m.role == Role::Assistant
                        && replay
                            .scope
                            .as_ref()
                            .and_then(|scope| Key::new(scope, block))
                            .is_some_and(|key| known.blocks.get(&key).and_then(Option::as_ref) == Some(&replay.source))
            });
            !m.blocks.is_empty()
        });
        replay
    }
}

pub struct AliasReplay {
    history: Arc<AliasHistory>,
    scope: Option<Arc<Scope>>,
    source: Arc<Source>,
}

impl AliasReplay {
    /// Remembers complete private blocks emitted by this exact request.
    pub fn track(self, stream: EventStream) -> EventStream {
        stream
            .inspect(move |event| {
                if let Ok(ModelEvent::Block { block, .. }) = event
                    && private(block)
                    && let Some(scope) = &self.scope
                    && let Some(key) = Key::new(scope, block)
                {
                    self.remember(key);
                }
            })
            .boxed()
    }

    fn remember(&self, key: Key) {
        let len = key.block.len();
        if len > MAX_BYTES {
            return;
        }
        let mut known = self.history.0.lock().unwrap();
        let key = Arc::new(key);
        match known.blocks.entry(key.clone()) {
            Entry::Occupied(mut entry) => {
                // If two sources emitted identical blocks, we cannot tell
                // them apart during replay. Do not attribute either copy.
                if entry.get().as_ref() != Some(&self.source) {
                    entry.insert(None);
                }
                return;
            }
            Entry::Vacant(entry) => entry.insert(Some(self.source.clone())),
        };
        known.bytes += len;
        known.order.push_back(key);
        while known.order.len() > MAX_BLOCKS || known.bytes > MAX_BYTES {
            let oldest = known.order.pop_front().unwrap();
            known.blocks.remove(&oldest);
            known.bytes -= oldest.block.len();
        }
    }
}

fn private(block: &Block) -> bool {
    // Unsigned Thinking is display-only and isn't sent by a provider.
    matches!(block, Block::Reasoning { .. } | Block::SignedThinking { .. } | Block::RedactedThinking(_) | Block::Opaque { .. })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Message;

    #[tokio::test]
    async fn alias_private_replay_requires_exact_caller_alias_kind_and_account() {
        let history = Arc::new(AliasHistory::default());
        let private = [
            Block::SignedThinking { text: String::new(), signature: "signed".into() },
            Block::RedactedThinking("redacted".into()),
            Block::Opaque { provider: "anthropic".into(), json: "{}".into(), label: "search".into() },
        ];
        let request = ModelRequest {
            model: "m".into(),
            system: String::new(),
            messages: vec![Message { role: Role::Assistant, blocks: private.to_vec() }, Message::user_text("keep me")],
            tools: Vec::new(),
            max_output_tokens: None,
            cache_key: Some("child".into()),
            effort: None,
            web_search: false,
            json: false,
        };
        let replay = history.prepare(&mut request.clone(), "alias", "anthropic", "a");
        let stream = futures::stream::iter(private.map(|block| Ok(ModelEvent::Block { index: 0, block }))).boxed();
        assert_eq!(replay.track(stream).count().await, 3);
        for (caller, alias, kind, account, keep) in [
            (Some("child"), "alias", "anthropic", "a", true),
            (Some("root"), "alias", "anthropic", "a", false),
            (Some("child"), "other", "anthropic", "a", false),
            (Some("child"), "alias", "chatgpt", "a", false),
            (Some("child"), "alias", "anthropic", "b", false),
            (Some("child"), "alias", "anthropic", "", false),
            (None, "alias", "anthropic", "a", false),
        ] {
            let mut req = request.clone();
            req.cache_key = caller.map(str::to_owned);
            history.prepare(&mut req, alias, kind, account);
            assert_eq!(req.messages, if keep { request.messages.as_slice() } else { &request.messages[1..] });
        }
    }
}
