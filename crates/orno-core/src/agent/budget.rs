//! Token and tool-call accounting shared down a subagent tree.
//!
//! Each loop owns one [`BudgetScope`] linked to its parent's. Spend is
//! charged to the scope and every ancestor, and every limit up the chain
//! is checked, so a grandchild cannot outspend the root. Iterations are
//! deliberately not shared: each loop keeps its own `max_iterations`.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use crate::events::BudgetKind;

/// Counters and limits for one loop plus the chain of its ancestors.
/// A limit of `0` means unbounded, matching `AgentPolicy`.
#[derive(Debug)]
pub struct BudgetScope {
    max_tokens: u64,
    max_tool_calls: u32,
    tokens: AtomicU64,
    tool_calls: AtomicU32,
    parent: Option<Arc<BudgetScope>>,
}

impl BudgetScope {
    /// Open a scope under `parent` (or a root scope when `None`).
    #[must_use]
    pub fn new(
        max_tokens: u64,
        max_tool_calls: u32,
        parent: Option<Arc<BudgetScope>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            max_tokens,
            max_tool_calls,
            tokens: AtomicU64::new(0),
            tool_calls: AtomicU32::new(0),
            parent,
        })
    }

    /// Tokens spent by this loop and its descendants.
    #[must_use]
    pub fn tokens_used(&self) -> u64 {
        self.tokens.load(Ordering::Relaxed)
    }

    /// Smallest token allowance left anywhere on the chain, `None` when
    /// no scope on it is bounded.
    #[must_use]
    pub fn tokens_remaining(&self) -> Option<u64> {
        let own = (self.max_tokens > 0).then(|| self.max_tokens.saturating_sub(self.tokens_used()));
        let inherited = self.parent.as_deref().and_then(Self::tokens_remaining);
        match (own, inherited) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    /// Charge `delta` tokens to this scope and all ancestors.
    ///
    /// Errors: [`BudgetKind::Tokens`] when any scope on the chain is over its limit.
    pub fn charge_tokens(&self, delta: u64) -> Result<(), BudgetKind> {
        let mut over = false;
        for scope in self.chain() {
            // Saturating so a huge delta cannot wrap and re-enable a spent budget.
            let _ = scope
                .tokens
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |old| {
                    Some(old.saturating_add(delta))
                });
            over |= scope.max_tokens > 0 && scope.tokens_used() > scope.max_tokens;
        }
        if over {
            Err(BudgetKind::Tokens)
        } else {
            Ok(())
        }
    }

    /// Count one tool call against this scope and all ancestors.
    ///
    /// Errors: [`BudgetKind::ToolCalls`] when any scope on the chain is over its limit.
    pub fn charge_tool_call(&self) -> Result<(), BudgetKind> {
        let mut over = false;
        for scope in self.chain() {
            let used = scope
                .tool_calls
                .fetch_add(1, Ordering::Relaxed)
                .saturating_add(1);
            over |= scope.max_tool_calls > 0 && used > scope.max_tool_calls;
        }
        if over {
            Err(BudgetKind::ToolCalls)
        } else {
            Ok(())
        }
    }

    fn chain(&self) -> impl Iterator<Item = &BudgetScope> {
        std::iter::successors(Some(self), |s| s.parent.as_deref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descendant_spend_reaches_every_ancestor() {
        let root = BudgetScope::new(100, 0, None);
        let child = BudgetScope::new(1_000, 0, Some(Arc::clone(&root)));
        let grandchild = BudgetScope::new(1_000, 0, Some(Arc::clone(&child)));

        assert!(grandchild.charge_tokens(60).is_ok());
        assert_eq!(root.tokens_used(), 60);
        assert_eq!(child.tokens_used(), 60);
        assert_eq!(grandchild.tokens_remaining(), Some(40));
        assert!(matches!(
            grandchild.charge_tokens(50),
            Err(BudgetKind::Tokens)
        ));
    }

    #[test]
    fn tool_calls_are_counted_transitively() {
        let root = BudgetScope::new(0, 2, None);
        let child = BudgetScope::new(0, 0, Some(Arc::clone(&root)));

        assert!(root.charge_tool_call().is_ok());
        assert!(child.charge_tool_call().is_ok());
        assert!(matches!(
            child.charge_tool_call(),
            Err(BudgetKind::ToolCalls)
        ));
    }

    #[test]
    fn unbounded_chain_has_no_remaining_limit() {
        let root = BudgetScope::new(0, 0, None);
        assert_eq!(root.tokens_remaining(), None);
        assert!(root.charge_tokens(u64::MAX).is_ok());
        assert!(root.charge_tokens(1).is_ok());
        assert_eq!(root.tokens_used(), u64::MAX);
    }
}
