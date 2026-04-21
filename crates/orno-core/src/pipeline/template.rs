//! `MiniJinja` environment for prompt rendering.
//!
//! `auto_escape` is explicitly forced to `None`. Jinja's default
//! extension-heuristic will HTML-escape anything that looks like a template
//! filename — rendering prompts that then break tool-call JSON downstream.

use minijinja::{AutoEscape, Environment, UndefinedBehavior};

use crate::error::PipelineError;

pub struct TemplateEngine {
    env: Environment<'static>,
}

impl TemplateEngine {
    #[must_use]
    pub fn new() -> Self {
        let mut env = Environment::new();
        env.set_auto_escape_callback(|_| AutoEscape::None);
        // ADR 0020: missing `env.FOO` or `secrets.FOO` references must
        // surface as hard render errors, not silent empty strings.
        // Strict undefined applies uniformly across every namespace.
        env.set_undefined_behavior(UndefinedBehavior::Strict);
        Self { env }
    }

    pub fn render(
        &self,
        name: &str,
        source: &str,
        ctx: &serde_json::Value,
    ) -> Result<String, PipelineError> {
        let tmpl =
            self.env
                .template_from_str(source)
                .map_err(|source| PipelineError::Template {
                    name: name.to_string(),
                    source,
                })?;
        tmpl.render(ctx).map_err(|source| PipelineError::Template {
            name: name.to_string(),
            source,
        })
    }
}

impl Default for TemplateEngine {
    fn default() -> Self {
        Self::new()
    }
}
