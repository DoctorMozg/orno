//! Conversation history with a running byte total and unit-wise eviction.

use std::sync::Arc;

use crate::llm::OrnoChatMessage;

/// Approximate byte cost of one message. Content strings dominate; the
/// fixed per-variant overhead is ignored to keep the estimate cheap.
fn estimated_message_bytes(msg: &OrnoChatMessage) -> usize {
    #[expect(
        unreachable_patterns,
        reason = "OrnoChatMessage is #[non_exhaustive]; the catch-all handles future variants"
    )]
    match msg {
        OrnoChatMessage::User { content } | OrnoChatMessage::Assistant { content } => content.len(),
        OrnoChatMessage::ToolCalls { calls } => calls
            .iter()
            .map(|c| c.fn_name.len() + c.fn_arguments.to_string().len())
            .sum(),
        OrnoChatMessage::ToolResult { call_id, content } => call_id.len() + content.len(),
        _ => 0,
    }
}

/// Messages sent with each LLM request. Behind an `Arc` so a request
/// clones a pointer, not the history.
pub(super) struct History {
    messages: Arc<Vec<OrnoChatMessage>>,
    bytes: usize,
}

impl History {
    pub(super) fn new() -> Self {
        Self {
            messages: Arc::new(Vec::new()),
            bytes: 0,
        }
    }

    pub(super) fn snapshot(&self) -> Arc<Vec<OrnoChatMessage>> {
        Arc::clone(&self.messages)
    }

    pub(super) fn push(&mut self, msg: OrnoChatMessage) {
        self.bytes = self.bytes.saturating_add(estimated_message_bytes(&msg));
        // INVARIANT: no transport retains an Arc clone across an await, so
        // the refcount is 1 here; otherwise make_mut deep-clones the whole
        // history every iteration (O(n^2)). See the LlmTransport contract.
        Arc::make_mut(&mut self.messages).push(msg);
    }

    pub(super) fn bytes(&self) -> usize {
        self.bytes
    }

    /// Remove and return the oldest units until the estimate fits
    /// `target`. A unit is a `ToolCalls` message with all of its
    /// `ToolResult`s, or any other message alone, because providers reject
    /// a result whose call is gone. The newest unit always stays so the
    /// model sees the latest turn.
    pub(super) fn evict_to(&mut self, target: usize) -> Vec<OrnoChatMessage> {
        let msgs = &self.messages;
        let mut evict = 0usize;
        let mut remaining = self.bytes;
        while remaining > target {
            let unit_len = unit_len(&msgs[evict..]);
            if evict + unit_len >= msgs.len() {
                break;
            }
            let unit_bytes: usize = msgs[evict..evict + unit_len]
                .iter()
                .map(estimated_message_bytes)
                .sum();
            evict += unit_len;
            remaining = remaining.saturating_sub(unit_bytes);
        }
        if evict == 0 {
            return Vec::new();
        }
        self.bytes = remaining;
        Arc::make_mut(&mut self.messages).drain(..evict).collect()
    }

    /// Put `msg` before every other message.
    pub(super) fn push_front(&mut self, msg: OrnoChatMessage) {
        self.bytes = self.bytes.saturating_add(estimated_message_bytes(&msg));
        Arc::make_mut(&mut self.messages).insert(0, msg);
    }
}

/// Length of the unit starting at `msgs[0]`.
fn unit_len(msgs: &[OrnoChatMessage]) -> usize {
    if !matches!(msgs[0], OrnoChatMessage::ToolCalls { .. }) {
        return 1;
    }
    1 + msgs[1..]
        .iter()
        .take_while(|m| matches!(m, OrnoChatMessage::ToolResult { .. }))
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::OrnoChatToolCall;
    use rstest::rstest;

    fn calls(ids: &[&str]) -> OrnoChatMessage {
        OrnoChatMessage::ToolCalls {
            calls: ids
                .iter()
                .map(|id| OrnoChatToolCall {
                    call_id: (*id).into(),
                    fn_name: "T".into(),
                    fn_arguments: serde_json::json!({}),
                })
                .collect(),
        }
    }

    fn result(id: &str, len: usize) -> OrnoChatMessage {
        OrnoChatMessage::ToolResult {
            call_id: id.into(),
            content: "x".repeat(len),
        }
    }

    fn turn(history: &mut History, ids: &[&str], result_len: usize) {
        history.push(calls(ids));
        for id in ids {
            history.push(result(id, result_len));
        }
    }

    #[rstest]
    #[case::tight_cap_keeps_only_newest_unit(1, 4)]
    #[case::cap_fits_last_two_turns(700, 8)]
    #[case::cap_fits_everything(10_000, 12)]
    fn eviction_removes_whole_turns(#[case] cap: usize, #[case] expected_len: usize) {
        let mut history = History::new();
        turn(&mut history, &["a1", "a2", "a3"], 100);
        turn(&mut history, &["b1", "b2", "b3"], 100);
        turn(&mut history, &["c1", "c2", "c3"], 100);
        history.evict_to(cap);

        let msgs = history.snapshot();
        assert_eq!(msgs.len(), expected_len);
        assert!(
            matches!(msgs.first(), Some(OrnoChatMessage::ToolCalls { .. })),
            "history must not start with a ToolResult: {msgs:?}",
        );
        let results = msgs
            .iter()
            .filter(|m| matches!(m, OrnoChatMessage::ToolResult { .. }))
            .count();
        assert_eq!(
            results,
            expected_len / 4 * 3,
            "every kept call keeps its results"
        );
    }

    #[test]
    fn running_total_matches_a_fresh_sum_after_eviction() {
        let mut history = History::new();
        turn(&mut history, &["a1", "a2"], 100);
        turn(&mut history, &["b1", "b2"], 100);
        history.evict_to(250);

        let fresh: usize = history.snapshot().iter().map(estimated_message_bytes).sum();
        assert_eq!(history.bytes, fresh);
    }
}
