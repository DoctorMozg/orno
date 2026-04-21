//! Kind-keyed executor registry. The scheduler resolves `NodeRequest` to
//! an executor through here.

use std::collections::HashMap;
use std::sync::Arc;

use super::NodeExecutor;

#[derive(Default)]
pub struct NodeRegistry {
    map: HashMap<String, Arc<dyn NodeExecutor>>,
}

impl NodeRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, kind: impl Into<String>, executor: Arc<dyn NodeExecutor>) {
        self.map.insert(kind.into(), executor);
    }

    #[must_use]
    pub fn get(&self, kind: &str) -> Option<Arc<dyn NodeExecutor>> {
        self.map.get(kind).cloned()
    }
}
