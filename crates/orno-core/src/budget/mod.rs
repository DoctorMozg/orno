//! Budget enforcement seam — preflight heuristic plus runtime tally.
//! Skeleton stubs only; the real implementation arrives with the LLM
//! transport and token-counting feature.

use async_trait::async_trait;

use crate::error::LlmError;
use crate::llm::LlmRequest;

/// Contract for preflight and post-hoc budget checks.
#[async_trait]
pub trait BudgetEnforcer: Send + Sync {
    /// Preflight gate — fail fast with a heuristic character estimate.
    async fn preflight(&self, req: &LlmRequest) -> Result<(), LlmError>;

    /// Runtime tally from provider-reported usage.
    async fn record_usage(&self, prompt_tokens: u32, completion_tokens: u32);
}

/// Always-allow stub enforcer used in the skeleton and in tests.
#[derive(Debug, Default, Clone)]
pub struct NoopEnforcer;

#[async_trait]
impl BudgetEnforcer for NoopEnforcer {
    async fn preflight(&self, _req: &LlmRequest) -> Result<(), LlmError> {
        Ok(())
    }

    async fn record_usage(&self, _prompt_tokens: u32, _completion_tokens: u32) {}
}
