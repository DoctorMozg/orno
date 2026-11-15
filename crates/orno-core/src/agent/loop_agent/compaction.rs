//! Keeps the conversation history under `max_message_history_bytes` by
//! dropping or summarizing its oldest turns.

use std::fmt::Write as _;
use std::sync::Arc;

use tracing::{info, warn};

use crate::error::AgentError;
use crate::events::Event;
use crate::events::redactor::Keep;
use crate::llm::{LlmRequest, OrnoChatMessage};
use crate::pipeline::OnHistoryOverflow;

use super::LoopAgent;
use super::run::RunState;

/// Opens the message that replaces summarized turns, so the model knows
/// the text is a digest written by orno and not a user request.
const SUMMARY_HEADER: &str = "[orno: summary of earlier turns, removed to keep the \
                              conversation under max_message_history_bytes]";

const SUMMARIZER_SYSTEM: &str = "You compress the earlier part of an AI agent's working \
     conversation so the agent can carry on with its task without it. Keep what the agent \
     will still need: files, paths and URLs it looked at, commands it ran and their outcomes, \
     findings, decisions, errors, and what is left to do. Drop raw tool output that is no \
     longer needed. If the turns open with an earlier summary, merge it into yours. Reply \
     with the summary only, as terse prose or bullet points.";

/// The summarizer needs the task's gist to judge what matters, not a
/// second copy of a prompt that may be a whole diff.
const TASK_EXCERPT_BYTES: usize = 8 * 1024;

/// Rough bytes per token, used only to turn the summary's byte budget
/// into a completion cap.
const BYTES_PER_TOKEN: usize = 4;

impl LoopAgent {
    /// Applies `on_history_overflow` when the history is over the cap.
    pub(super) async fn compact_history(&self, st: &mut RunState<'_>) -> Result<(), AgentError> {
        let Some(cap) = st.req.policy.max_message_history_bytes else {
            return Ok(());
        };
        let bytes_before = st.history.bytes();
        if bytes_before <= cap {
            return Ok(());
        }
        let strategy = st.req.policy.on_history_overflow;
        let evicted = match strategy {
            OnHistoryOverflow::Truncate => st.history.evict_to(cap),
            // Half the cap leaves room for the summary and for several
            // turns before the next summary call.
            OnHistoryOverflow::Summarize => st.history.evict_to(cap / 2),
        };
        if evicted.is_empty() {
            return Ok(());
        }
        if strategy == OnHistoryOverflow::Summarize {
            let summary = self.summarize(st, &evicted, cap / 8).await?;
            st.history.push_front(OrnoChatMessage::User {
                content: format!("{SUMMARY_HEADER}\n\n{summary}"),
            });
        }

        let bytes_after = st.history.bytes();
        match strategy {
            OnHistoryOverflow::Truncate => warn!(
                node.id = %st.node_id,
                cap_bytes = cap,
                bytes_before,
                bytes_after,
                evicted_messages = evicted.len(),
                "message history truncated to stay within cap",
            ),
            OnHistoryOverflow::Summarize => info!(
                node.id = %st.node_id,
                cap_bytes = cap,
                bytes_before,
                bytes_after,
                evicted_messages = evicted.len(),
                "message history summarized to stay within cap",
            ),
        }
        self.config
            .sink
            .record(Event::HistoryCompacted {
                run_id: st.run_id.to_string(),
                node_id: st.node_id.to_string(),
                strategy: strategy.as_str().to_string(),
                evicted_messages: evicted.len() as u64,
                bytes_before: bytes_before as u64,
                bytes_after: bytes_after as u64,
                cap_bytes: cap as u64,
            })
            .await;
        Ok(())
    }

    /// One tool-less call to the agent's own model that condenses
    /// `evicted` into at most `max_bytes` of text.
    async fn summarize(
        &self,
        st: &RunState<'_>,
        evicted: &[OrnoChatMessage],
        max_bytes: usize,
    ) -> Result<String, AgentError> {
        let ceiling = u32::try_from((max_bytes / BYTES_PER_TOKEN).max(1)).unwrap_or(u32::MAX);
        let prompt = summary_prompt(
            &self.config.redactor.redact_bounded(
                &st.req.initial_prompt,
                TASK_EXCERPT_BYTES,
                Keep::Head,
            ),
            evicted,
        );
        let llm_req = LlmRequest {
            provider: Arc::clone(&st.req.provider),
            model: Arc::clone(&st.req.model),
            prompt: Arc::from(prompt.as_str()),
            system: Some(Arc::from(SUMMARIZER_SYSTEM)),
            temperature: None,
            max_tokens: Self::completion_cap(st, Some(ceiling))?,
            messages: Arc::new(Vec::new()),
            tools: Arc::new(Vec::new()),
        };
        let response = self
            .send_llm(
                st,
                llm_req,
                self.excerpt_for_wire(&prompt),
                Some(self.excerpt_for_wire(SUMMARIZER_SYSTEM)),
            )
            .await?;
        Self::charge_usage(st, &response)?;
        Ok(self
            .config
            .redactor
            .redact_bounded(&response.content, max_bytes, Keep::Head))
    }
}

/// The task plus the evicted turns as plain text. Flattening sidesteps
/// providers' rules on how tool calls and results must pair up.
fn summary_prompt(task: &str, evicted: &[OrnoChatMessage]) -> String {
    let mut out = format!(
        "The agent's task:\n<task>\n{task}\n</task>\n\nEarlier turns, oldest first:\n<turns>\n"
    );
    for msg in evicted {
        #[expect(
            unreachable_patterns,
            reason = "OrnoChatMessage is #[non_exhaustive]; the catch-all handles future variants"
        )]
        match msg {
            OrnoChatMessage::User { content } => {
                let _ = writeln!(out, "USER:\n{content}\n");
            },
            OrnoChatMessage::Assistant { content } => {
                let _ = writeln!(out, "ASSISTANT:\n{content}\n");
            },
            OrnoChatMessage::ToolCalls { calls } => {
                for c in calls {
                    let _ = writeln!(
                        out,
                        "TOOL CALL {} {}({})\n",
                        c.call_id, c.fn_name, c.fn_arguments
                    );
                }
            },
            OrnoChatMessage::ToolResult { call_id, content } => {
                let _ = writeln!(out, "TOOL RESULT {call_id}:\n{content}\n");
            },
            _ => {},
        }
    }
    out.push_str("</turns>\n\nWrite the summary of these turns.");
    out
}
