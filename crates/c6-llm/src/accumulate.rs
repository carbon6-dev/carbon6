//! Builds the final assistant message from streamed events.

use std::collections::BTreeMap;

use crate::{Block, Message, ModelEvent, Role, Usage};

#[derive(Default)]
pub struct Accumulator {
    blocks: Vec<Block>,
    calls: BTreeMap<u32, (String, String, String)>,
    pub usage: Usage,
    pub stop_reason: Option<String>,
}

impl Accumulator {
    pub fn push(&mut self, ev: &ModelEvent) {
        match ev {
            ModelEvent::TextDelta(t) => match self.blocks.last_mut() {
                Some(Block::Text(s)) => s.push_str(t),
                _ => self.blocks.push(Block::Text(t.clone())),
            },
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
            ModelEvent::Usage(u) => self.usage = *u,
            ModelEvent::Done { stop_reason } => self.stop_reason = Some(stop_reason.clone()),
        }
    }

    /// Whether anything visible was produced (used to decide whether a
    /// partial response is worth keeping after an interrupt).
    pub fn has_content(&self) -> bool {
        !self.blocks.is_empty() || !self.calls.is_empty()
    }

    /// The assistant message. With `complete == false` (interrupted), tool
    /// calls are dropped since their arguments may be cut off.
    pub fn finish(self, complete: bool) -> Message {
        let mut blocks: Vec<Block> =
            self.blocks.into_iter().filter(|b| !matches!(b, Block::Text(t) | Block::Thinking(t) if t.is_empty())).collect();
        if complete {
            for (_, (id, name, args)) in self.calls {
                let input_json = if args.trim().is_empty() { "{}".to_owned() } else { args };
                blocks.push(Block::ToolUse { id, name, input_json });
            }
        }
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
}
