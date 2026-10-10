//! Builds the final assistant message from streamed events.

use std::collections::BTreeMap;

use crate::{Block, Message, ModelEvent, Role, Usage};

#[derive(Default)]
pub struct Accumulator {
    /// Delta-built blocks (Chat Completions).
    blocks: Vec<Block>,
    calls: BTreeMap<u32, (String, String, String)>,
    /// Provider-finished blocks by response position (Anthropic, Responses).
    finished: BTreeMap<u32, Block>,
    /// Text streamed since the last finished block, kept if interrupted.
    partial_text: String,
    pub usage: Usage,
    pub stop_reason: Option<String>,
}

impl Accumulator {
    pub fn push(&mut self, ev: &ModelEvent) {
        match ev {
            ModelEvent::TextDelta(t) => {
                self.partial_text.push_str(t);
                match self.blocks.last_mut() {
                    Some(Block::Text(s)) => s.push_str(t),
                    _ => self.blocks.push(Block::Text(t.clone())),
                }
            }
            ModelEvent::ThinkingDelta(t) => match self.blocks.last_mut() {
                Some(Block::Thinking(s)) => s.push_str(t),
                _ => self.blocks.push(Block::Thinking(t.clone())),
            },
            ModelEvent::ToolCallStart { index, id, name } => {
                self.calls.insert(*index, (id.clone(), name.clone(), String::new()));
            }
            ModelEvent::ToolCallDelta { index, args } => {
                if let Some(c) = self.calls.get_mut(index) {
                    c.2.push_str(args);
                }
            }
            ModelEvent::Block { index, block } => {
                if matches!(block, Block::Text(_)) {
                    self.partial_text.clear();
                }
                self.finished.insert(*index, block.clone());
            }
            ModelEvent::Usage(u) => self.usage = *u,
            ModelEvent::Done { stop_reason } => self.stop_reason = Some(stop_reason.clone()),
        }
    }

    /// Whether anything visible was produced (used to decide whether a
    /// partial response is worth keeping after an interrupt).
    pub fn has_content(&self) -> bool {
        !self.blocks.is_empty() || !self.calls.is_empty() || !self.finished.is_empty()
    }

    /// The assistant message. With `complete == false` (interrupted or
    /// failed), only text is kept: tool calls may be cut off, and partial
    /// reasoning has no signature to replay.
    pub fn finish(self, complete: bool) -> Message {
        let blocks = if !self.finished.is_empty() {
            if complete {
                self.finished.into_values().collect()
            } else {
                let mut text: Vec<Block> = self.finished.into_values().filter(|b| matches!(b, Block::Text(_))).collect();
                if !self.partial_text.is_empty() {
                    text.push(Block::Text(self.partial_text));
                }
                text
            }
        } else {
            let mut blocks: Vec<Block> =
                self.blocks.into_iter().filter(|b| !matches!(b, Block::Text(t) | Block::Thinking(t) if t.is_empty())).collect();
            if complete {
                for (_, (id, name, args)) in self.calls {
                    let input_json = if args.trim().is_empty() { "{}".to_owned() } else { args };
                    blocks.push(Block::ToolUse { id, name, input_json });
                }
            }
            blocks
        };
        Message { role: Role::Assistant, blocks }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merges_deltas_and_orders_calls() {
        let mut a = Accumulator::default();
        for ev in [
            ModelEvent::ThinkingDelta("hm".into()),
            ModelEvent::TextDelta("Hel".into()),
            ModelEvent::TextDelta("lo".into()),
            ModelEvent::ToolCallStart { index: 1, id: "b".into(), name: "glob".into() },
            ModelEvent::ToolCallStart { index: 0, id: "a".into(), name: "read".into() },
            ModelEvent::ToolCallDelta { index: 0, args: "{\"path\":".into() },
            ModelEvent::ToolCallDelta { index: 0, args: "\"x\"}".into() },
            ModelEvent::Done { stop_reason: "tool_calls".into() },
        ] {
            a.push(&ev);
        }
        let msg = a.finish(true);
        assert_eq!(
            msg.blocks,
            vec![
                Block::Thinking("hm".into()),
                Block::Text("Hello".into()),
                Block::ToolUse { id: "a".into(), name: "read".into(), input_json: "{\"path\":\"x\"}".into() },
                Block::ToolUse { id: "b".into(), name: "glob".into(), input_json: "{}".into() },
            ]
        );
    }

    #[test]
    fn finished_blocks_keep_provider_order_and_empty_signed_thinking() {
        let mut a = Accumulator::default();
        a.push(&ModelEvent::Block { index: 1, block: Block::Text("hi".into()) });
        a.push(&ModelEvent::Block { index: 0, block: Block::SignedThinking { text: String::new(), signature: "sig".into() } });
        a.push(&ModelEvent::TextDelta("partial".into()));
        let interrupted = a;
        let mut again = Accumulator::default();
        again.push(&ModelEvent::Block { index: 0, block: Block::SignedThinking { text: String::new(), signature: "sig".into() } });
        again.push(&ModelEvent::Block { index: 1, block: Block::Text("hi".into()) });
        assert_eq!(again.finish(true).blocks[0], Block::SignedThinking { text: String::new(), signature: "sig".into() });
        assert_eq!(interrupted.finish(false).blocks, vec![Block::Text("hi".into()), Block::Text("partial".into())]);
    }
}
