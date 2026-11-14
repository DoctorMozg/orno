//! Semantic checks behind [`super::load::validate`]. Every function appends
//! to a shared error list so one run reports every problem at once.

use std::collections::BTreeSet;
use std::net::IpAddr;
use std::path::Component;

use serde_json::Value;

use crate::error::PipelineError;
use crate::tool::builtin_effect;
use crate::tool::net_guard::{is_blocked_ip, literal_ip};

use super::graph::DepGraph;
use super::schema::{AgentConfig, McpServerConfig, Node, NodeKind, Pipeline};
use super::template::{TemplateEngine, looks_templated};

/// Check an MCP server declaration after its templates were rendered.
/// Messages never quote the url, because a rendered url can carry a secret.
///
/// Errors: `PipelineError::Validation` naming the server and the problem.
pub fn check_rendered_mcp_server(
    name: &str,
    server: &McpServerConfig,
) -> Result<(), PipelineError> {
    mcp_server_error(name, server, true).map_or(Ok(()), |msg| Err(PipelineError::Validation(msg)))
}

/// Stdio needs a non-empty argv; HTTP needs an http(s) url that is
/// loopback when plain `http`, and not a blocked literal IP over https.
/// `check_url` is false for a url that is still a template.
fn mcp_server_error(name: &str, server: &McpServerConfig, check_url: bool) -> Option<String> {
    let prefix = format!("mcp_servers.`{name}`");
    match server {
        McpServerConfig::Stdio(stdio) => {
            if stdio.call_timeout_secs == Some(0) {
                return Some(format!(
                    "{prefix}: `call_timeout_secs` must be greater than zero"
                ));
            }
            stdio
                .command
                .is_empty()
                .then(|| format!("{prefix}: stdio transport requires a non-empty `command:` argv"))
        },
        McpServerConfig::Http(http) => {
            if http.call_timeout_secs == Some(0) {
                return Some(format!(
                    "{prefix}: `call_timeout_secs` must be greater than zero"
                ));
            }
            if !check_url {
                return None;
            }
            http_url_error(&http.url).map(|msg| format!("{prefix}: {msg}"))
        },
    }
}

fn http_url_error(url: &str) -> Option<String> {
    let parsed = match reqwest::Url::parse(url) {
        Ok(parsed) => parsed,
        Err(e) => return Some(format!("invalid url: {e}")),
    };
    let scheme = parsed.scheme();
    if scheme != "https" && scheme != "http" {
        return Some(format!(
            "url scheme `{scheme}` not permitted; use https or http://localhost"
        ));
    }
    let host = parsed.host_str().unwrap_or("");
    let host_ip = literal_ip(&parsed);
    // Plain http is only safe over loopback, so credentials in
    // `Authorization` headers never travel in clear.
    if scheme == "http" {
        let is_loopback = match host_ip {
            Some(IpAddr::V4(v4)) => v4.is_loopback(),
            Some(IpAddr::V6(v6)) => v6.is_loopback(),
            None => host == "localhost" || host == "localhost.",
        };
        return (!is_loopback).then(|| {
            "`http://` is only allowed for loopback hosts; use `https://` for remote servers"
                .to_string()
        });
    }
    // Loopback is in the blocked set too, but over https a literal loopback
    // IP is just as wrong a target as the other private ranges.
    host_ip.filter(|&ip| is_blocked_ip(ip)).map(|ip| {
        format!(
            "url host `{ip}` is in a blocked range \
             (loopback / private / link-local / CGNAT / documentation / metadata)"
        )
    })
}

pub(super) fn validate_mcp_servers(pipeline: &Pipeline, errors: &mut Vec<String>) {
    for (name, server) in &pipeline.mcp_servers {
        // A templated url is only checkable once it is rendered at spawn.
        let url_is_template =
            matches!(server, McpServerConfig::Http(http) if looks_templated(&http.url));
        errors.extend(mcp_server_error(name, server, !url_is_template));
    }

    // `mcp.<server>.<tool>` names are flattened with underscores on the
    // wire, so `foo` and `foo_bar` could not be routed back to one server.
    let names: Vec<&str> = pipeline.mcp_servers.keys().map(String::as_str).collect();
    for (i, a) in names.iter().enumerate() {
        for b in &names[i + 1..] {
            if a.starts_with(&format!("{b}_")) || b.starts_with(&format!("{a}_")) {
                errors.push(format!(
                    "MCP server names `{a}` and `{b}` share an underscore-prefix relationship \
                     that produces ambiguous wire-form tool names at replay time. \
                     Rename one server."
                ));
            }
        }
    }
}

pub(super) fn validate_nodes(pipeline: &Pipeline, errors: &mut Vec<String>) {
    for node in &pipeline.nodes {
        let id = &node.id;
        if !is_valid_node_id(id) {
            errors.push(format!(
                "node id `{id}` must match [A-Za-z_][A-Za-z0-9_]* so templates can write `nodes.<id>`"
            ));
        }
        if node.timeout == Some(0) {
            errors.push(format!("node `{id}`: `timeout` must be greater than zero"));
        }
        match &node.kind {
            NodeKind::Agent(agent) if !pipeline.agents.contains_key(&agent.agent) => {
                errors.push(format!(
                    "node `{id}` references undeclared agent `{}`",
                    agent.agent
                ));
            },
            NodeKind::Shell(shell) if shell.command.trim().is_empty() => {
                errors.push(format!("node `{id}`: shell `command` is empty"));
            },
            _ => {},
        }
    }
}

fn is_valid_node_id(id: &str) -> bool {
    let mut chars = id.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

pub(super) fn validate_agents(pipeline: &Pipeline, errors: &mut Vec<String>) {
    for (name, config) in &pipeline.agents {
        validate_budgets(name, config, errors);
        validate_roots_have_no_parent_dir(name, config, errors);
        validate_bash_confinement(name, config, errors);
        for tool in &config.allowed_tools {
            validate_tool(name, config, tool, pipeline, errors);
        }
    }
}

fn validate_budgets(name: &str, config: &AgentConfig, errors: &mut Vec<String>) {
    let policy = &config.policy;
    if policy.max_iterations == 0 {
        errors.push(format!("agent `{name}`: max_iterations must be > 0"));
    }
    if policy.max_total_tokens == 0 {
        errors.push(format!("agent `{name}`: max_total_tokens must be > 0"));
    }
    if policy.max_tool_calls == 0 && !config.allowed_tools.is_empty() {
        errors.push(format!(
            "agent `{name}`: max_tool_calls is 0 but agent has allowed_tools"
        ));
    }
}

// A `..` component survives the lexical `starts_with` compose-down check
// (`/work/../etc` starts with `/work`), so it is refused outright.
fn validate_roots_have_no_parent_dir(name: &str, config: &AgentConfig, errors: &mut Vec<String>) {
    for root in &config.policy.roots {
        if root.components().any(|c| c == Component::ParentDir) {
            errors.push(format!(
                "agent `{name}`: root `{}` contains `..`, which would escape the jail",
                root.display()
            ));
        }
    }
}

fn validate_bash_confinement(name: &str, config: &AgentConfig, errors: &mut Vec<String>) {
    let policy = &config.policy;
    let confinement = match (policy.allowed_domains.is_empty(), policy.roots.is_empty()) {
        (false, false) => "`allowed_domains` and `roots`",
        (false, true) => "`allowed_domains`",
        (true, false) => "`roots`",
        (true, true) => return,
    };
    if !policy.unconfined_shell && config.allowed_tools.iter().any(|t| t == "Bash") {
        errors.push(format!(
            "agent `{name}` lists `Bash` but declares {confinement}; a shell can reach \
             any host and path, so the confinement would not hold. Drop `Bash` or set \
             `unconfined_shell: true` to accept that",
        ));
    }
}

fn validate_tool(
    agent: &str,
    config: &AgentConfig,
    tool: &str,
    pipeline: &Pipeline,
    errors: &mut Vec<String>,
) {
    if let Some(effect) = builtin_effect(tool) {
        let policy = &config.policy;
        // Silent runtime denials become explicit validation failures.
        if effect.requires_mutations() && !policy.allow_mutations {
            errors.push(format!(
                "agent `{agent}` allowed_tools: `{tool}` requires mutations but allow_mutations is false"
            ));
        }
        if effect.requires_network() && !policy.allow_network {
            errors.push(format!(
                "agent `{agent}` allowed_tools: `{tool}` requires network but allow_network is false"
            ));
        }
        if effect.requires_context_writes() && !policy.allow_context_writes {
            errors.push(format!(
                "agent `{agent}` allowed_tools: `{tool}` requires context writes but allow_context_writes is false"
            ));
        }
    } else if let Some(child_name) = tool.strip_prefix("subagent.") {
        match pipeline.agents.get(child_name) {
            Some(child) => validate_compose_down(agent, config, child_name, child, errors),
            None => errors.push(format!(
                "agent `{agent}` allowed_tools: `{tool}` references undeclared agent `{child_name}`"
            )),
        }
    } else if let Some(rest) = tool.strip_prefix("mcp.") {
        match rest.split_once('.') {
            Some((server, _)) if !pipeline.mcp_servers.contains_key(server) => {
                errors.push(format!(
                    "agent `{agent}` allowed_tools: `{tool}` references undeclared MCP server `{server}`"
                ));
            },
            Some(_) => {},
            None => errors.push(format!(
                "agent `{agent}` allowed_tools: `{tool}` is not a valid mcp tool reference \
                 (expected `mcp.<server>.<tool>` or `mcp.<server>.*`)"
            )),
        }
    } else {
        errors.push(format!(
            "agent `{agent}` allowed_tools: `{tool}` is not a known builtin, subagent reference, or mcp tool"
        ));
    }
}

/// A child may not be more permissive than its parent.
fn validate_compose_down(
    agent: &str,
    parent: &AgentConfig,
    child_name: &str,
    child: &AgentConfig,
    errors: &mut Vec<String>,
) {
    let (pp, cp) = (&parent.policy, &child.policy);
    for (flag, parent_on, child_on) in [
        ("allow_mutations", pp.allow_mutations, cp.allow_mutations),
        ("allow_network", pp.allow_network, cp.allow_network),
        (
            "allow_context_writes",
            pp.allow_context_writes,
            cp.allow_context_writes,
        ),
    ] {
        if !parent_on && child_on {
            errors.push(format!(
                "agent `{agent}` ({flag}=false) cannot delegate to `{child_name}` ({flag}=true)"
            ));
        }
    }
    // An unjailed parent does not constrain its children.
    if !pp.roots.is_empty() {
        for child_root in &cp.roots {
            if !pp.roots.iter().any(|root| child_root.starts_with(root)) {
                errors.push(format!(
                    "agent `{agent}` cannot delegate to `{child_name}`: subagent root `{}` \
                     is not a subdirectory of any parent root",
                    child_root.display(),
                ));
            }
        }
    }
    // Entering the child spends one unit of the parent's recursion budget.
    if cp.max_subagent_depth.saturating_add(1) > pp.max_subagent_depth {
        errors.push(format!(
            "agent `{agent}` (max_subagent_depth={}) cannot delegate to `{child_name}` \
             (max_subagent_depth={}); child must be at most parent - 1",
            pp.max_subagent_depth, cp.max_subagent_depth,
        ));
    }
}

/// Warnings that do not fail validation.
#[must_use]
pub fn warnings(pipeline: &Pipeline) -> Vec<String> {
    // Domain lists under `allow_network=false` never apply until a later
    // flag flip, so surface them as a possible mistake.
    pipeline
        .agents
        .iter()
        .filter(|(_, c)| {
            !c.policy.allow_network
                && (!c.policy.allowed_domains.is_empty() || !c.policy.blocked_domains.is_empty())
        })
        .map(|(name, _)| {
            format!(
                "agent `{name}`: allowed_domains/blocked_domains is set but \
                 allow_network=false — the domain list will never apply"
            )
        })
        .collect()
}

/// Compile every template string once and check `nodes.<id>` reads
/// against each node's transitive ancestors.
pub(super) fn validate_templates(pipeline: &Pipeline, graph: &DepGraph, errors: &mut Vec<String>) {
    let mut checker = TemplateChecker {
        engine: TemplateEngine::new(),
        errors,
    };

    // `vars` and MCP configs render before any node has run.
    for (key, value) in &pipeline.vars {
        checker.check_value(&format!("vars.{key}"), value, None);
    }
    for (name, server) in &pipeline.mcp_servers {
        if let Ok(value) = serde_json::to_value(server) {
            checker.check_value(&format!("mcp_servers.{name}"), &value, None);
        }
    }

    for (idx, node) in pipeline.nodes.iter().enumerate() {
        let scope = Scope {
            node: &node.id,
            declared: graph.by_id.keys().map(String::as_str).collect(),
            ancestors: ancestors(&pipeline.nodes, graph, idx),
        };
        checker.check_node(node, &scope);
        if let NodeKind::Agent(agent_node) = &node.kind
            && let Some(config) = pipeline.agents.get(&agent_node.agent)
        {
            checker.check_agent_config(&agent_node.agent, config, &scope);
        }
    }
}

/// Ids of every node reachable by following `needs:` from node `idx`.
fn ancestors<'a>(nodes: &'a [Node], graph: &DepGraph, idx: usize) -> BTreeSet<&'a str> {
    let mut seen = BTreeSet::new();
    let mut stack = vec![idx];
    while let Some(cur) = stack.pop() {
        for dep in &nodes[cur].needs {
            if seen.insert(dep.as_str()) {
                stack.push(graph.by_id[dep]);
            }
        }
    }
    seen
}

struct Scope<'a> {
    node: &'a str,
    declared: BTreeSet<&'a str>,
    ancestors: BTreeSet<&'a str>,
}

struct TemplateChecker<'e> {
    engine: TemplateEngine,
    errors: &'e mut Vec<String>,
}

impl TemplateChecker<'_> {
    fn check_node(&mut self, node: &Node, scope: &Scope<'_>) {
        let id = &node.id;
        match &node.kind {
            NodeKind::Shell(shell) => {
                self.check(&format!("node `{id}` command"), &shell.command, Some(scope));
                for (i, arg) in shell.args.iter().enumerate() {
                    self.check(&format!("node `{id}` args[{i}]"), arg, Some(scope));
                }
                if let Some(stdin) = &shell.stdin {
                    self.check(&format!("node `{id}` stdin"), stdin, Some(scope));
                }
                for (key, value) in &shell.env {
                    self.check(&format!("node `{id}` env.{key}"), value, Some(scope));
                }
            },
            NodeKind::Agent(agent) => {
                self.check(
                    &format!("node `{id}` initial_prompt"),
                    &agent.initial_prompt,
                    Some(scope),
                );
            },
        }
    }

    // Agent config strings render per node, so they are checked against
    // the ancestors of each node that uses the agent.
    fn check_agent_config(&mut self, name: &str, config: &AgentConfig, scope: &Scope<'_>) {
        let used_by = scope.node;
        if let Some(system) = &config.system {
            self.check(
                &format!("agent `{name}` system (used by node `{used_by}`)"),
                system,
                Some(scope),
            );
        }
        for (i, root) in config.policy.roots.iter().enumerate() {
            self.check(
                &format!("agent `{name}` roots[{i}] (used by node `{used_by}`)"),
                &root.to_string_lossy(),
                Some(scope),
            );
        }
    }

    fn check_value(&mut self, label: &str, value: &Value, scope: Option<&Scope<'_>>) {
        match value {
            Value::String(s) => self.check(label, s, scope),
            Value::Array(items) => {
                for (i, item) in items.iter().enumerate() {
                    self.check_value(&format!("{label}[{i}]"), item, scope);
                }
            },
            Value::Object(map) => {
                for (key, item) in map {
                    self.check_value(&format!("{label}.{key}"), item, scope);
                }
            },
            _ => {},
        }
    }

    fn check(&mut self, label: &str, source: &str, scope: Option<&Scope<'_>>) {
        if !looks_templated(source) {
            return;
        }
        let refs = match self.engine.referenced_nodes(label, source) {
            Ok(refs) => refs,
            Err(PipelineError::Template { source, .. }) => {
                // The error's own Display names the cache hash, not the field.
                let detail = source.detail().unwrap_or_default();
                let line = source
                    .line()
                    .map_or(String::new(), |l| format!(" (line {l})"));
                self.errors.push(format!(
                    "{label}: invalid template: {}: {detail}{line}",
                    source.kind()
                ));
                return;
            },
            Err(PipelineError::Validation(msg)) => {
                self.errors.push(msg);
                return;
            },
            Err(other) => {
                self.errors.push(format!("{label}: {other}"));
                return;
            },
        };
        for referenced in refs {
            let problem = match scope {
                None => "`nodes.*` is not available here, nothing has run yet".to_string(),
                Some(s) if !s.declared.contains(referenced.as_str()) => {
                    format!("`nodes.{referenced}` names no declared node")
                },
                Some(s) if !s.ancestors.contains(referenced.as_str()) => format!(
                    "`nodes.{referenced}` is not an upstream dependency of node `{}`; \
                     add it to `needs:`",
                    s.node
                ),
                Some(_) => continue,
            };
            self.errors.push(format!("{label}: {problem}"));
        }
    }
}
