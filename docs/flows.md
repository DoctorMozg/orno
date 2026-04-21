# Flows

Mermaid diagrams describing what's wired today in `crates/orno-core/src/**` and
`crates/orno-cli/src/**`. This is a snapshot of what the live code does.
Seams are in place and the execution path (Engine → walker → NodeRegistry →
NodeExecutor) is wired end-to-end for `shell` nodes; the agent loop is still
a stub pending LLM transport work. ADR targets live in `docs/arch.md` and the
ADRs; this document only describes what the code does.

Every diagram below points at specific file paths. When code moves, update
the diagram in the same commit.

## Crate topology

Two crates, one-way dependency (ADR 0001). `orno-cli` is the only place
`clap` / `tokio::main` / `tracing-subscriber` are wired.

```mermaid
graph LR
    CLI["orno-cli (bin: orno)"]
    CORE["orno-core (lib)"]
    CLI -->|"uses pipeline, events, execution"| CORE
    CLI -->|clap, clap_complete| CLAP[(clap)]
    CLI -->|tokio::main| TOKIO[(tokio)]
    CLI -->|JSON log subscriber| TRACING[(tracing-subscriber)]
    CORE -->|serde, serde_yaml_ng| SERDE[(serde)]
    CORE -->|schemars| SCHEMARS[(schemars)]
    CORE -->|MiniJinja| JINJA[(minijinja)]
    CORE -->|thiserror, async_trait| THISERROR[(thiserror / async_trait)]
    CORE -->|tracing spans only| TRACING_LIB[(tracing)]
```

## Module dependency graph — orno-core

Internal modules declared in `crates/orno-core/src/lib.rs:7-15`. Arrows are
`use` edges seen in the code.

```mermaid
graph TD
    lib[lib.rs]
    error[error]
    config[config]
    events[events]
    events_sink[events::sink]
    llm[llm]
    budget[budget]
    node[node]
    node_agent["node::agent"]
    node_shell["node::shell"]
    node_reg["node::registry"]
    pipeline[pipeline]
    pipe_schema["pipeline::schema"]
    pipe_load["pipeline::load"]
    pipe_tpl["pipeline::template"]
    exec[execution]
    exec_walker["execution::walker"]
    exec_context["execution::context"]
    exec_sched["execution::scheduler"]
    telemetry[telemetry]

    lib --> error
    lib --> config
    lib --> events
    lib --> llm
    lib --> budget
    lib --> node
    lib --> pipeline
    lib --> exec
    lib --> telemetry

    events --> events_sink

    llm --> error
    budget --> error
    budget --> llm

    node --> error
    node --> node_agent
    node --> node_shell
    node --> node_reg
    node_agent --> error
    node_shell --> error

    pipeline --> pipe_schema
    pipeline --> pipe_load
    pipeline --> pipe_tpl
    pipe_load --> pipe_schema
    pipe_load --> error
    pipe_tpl --> error

    exec --> exec_walker
    exec --> exec_context
    exec --> exec_sched
    exec_walker --> pipeline
    exec_walker --> events
    exec_walker --> error
    exec_sched --> events
    exec_sched --> pipeline
    exec_sched --> error
    exec_sched --> exec_walker
    exec_sched --> exec_context
    exec_sched --> node
    exec_sched --> node_reg
```

`exec_sched` drives the walker and resolves each ready node through
`NodeRegistry`, then merges per-node `Context` snapshots before template
rendering.

## CLI command dispatch

`crates/orno-cli/src/main.rs:10-21` parses `Cli` and branches into one of
four handlers. Only `run` reaches `orno-core`'s execution engine; the other
three touch just the loader or schema emitter.

```mermaid
flowchart LR
    start([argv]) --> parse["Cli::parse<br/>cli.rs:13"]
    parse --> sw{Command}
    sw -->|Run| run["commands::run::run<br/>run.rs:17"]
    sw -->|Validate| val["commands::validate::run<br/>validate.rs:6"]
    sw -->|Schema| sch["commands::schema::run<br/>schema.rs:3"]
    sw -->|Completions| cmp["commands::completions::run<br/>completions.rs:7"]

    run --> load1["pipeline::load::load_from_path"]
    run --> eng["Engine::new + Engine::run"]
    run --> ndjson["println! EventEnvelope NDJSON → stdout"]

    val --> load2["pipeline::load::load_from_path"]
    val --> okline["println! 'ok: version=N nodes=M' → stdout"]

    sch --> schstr["orno_core::pipeline_json_schema_string"]
    sch --> schout["println! JSON → stdout"]

    cmp --> clapgen["clap_complete::generate → stdout"]
```

Stream discipline (`main.rs:26-33`): stdout carries user-consumable output
(NDJSON envelopes, schema, completions). Stderr carries `tracing` JSON via
the subscriber installed in `init_tracing`.

## Pipeline load and validation

`pipeline::load::load_from_path` (`pipeline/load.rs:10-18`) is the single
entry point from the CLI. Validation catches the two constraints serde
cannot: non-empty nodes, unique ids, and known deps.

```mermaid
flowchart TD
    path[["path: &Path"]]
    path --> read["std::fs::read (load.rs:11)"]
    read -->|ok| parse["serde_yaml_ng::from_slice (load.rs:15)"]
    read -->|err| io["PipelineError::Io"]
    parse -->|ok| pipeline[/"Pipeline (version, vars, nodes)"/]
    parse -->|err| perr["PipelineError::Parse"]
    pipeline --> val["validate (load.rs:21)"]

    subgraph validate_checks
        v1{"nodes.is_empty?"}
        v2{"duplicate id?"}
        v3{"every needs in id set?"}
    end

    val --> v1
    v1 -->|yes| verr1["Validation: pipeline has no nodes"]
    v1 -->|no| v2
    v2 -->|yes| verr2["Validation: duplicate node id"]
    v2 -->|no| v3
    v3 -->|no| verr3["Validation: node depends on unknown"]
    v3 -->|yes| okout[["Ok(Pipeline)"]]
```

## `orno run` — end-to-end data flow

`commands::run::run` composes the pieces. The `Engine` records lifecycle
events into an `InMemorySink`; after the engine returns, the CLI drains
the sink and prints each envelope as NDJSON. `Engine::run` drives a
`DagWalker`: `next_ready` hands out nodes whose `needs:` have completed,
each is rendered against its per-node `Context` and dispatched through
`NodeRegistry::get(kind_str).execute(id, req)`, the result feeds back via
`walker.complete(id, ok)`, and on failure the walker returns the
transitively-dependent node ids to emit as `NodeSkipped` (see ADR 0021).

```mermaid
sequenceDiagram
    actor User
    participant CLI as orno-cli commands run
    participant Load as pipeline load
    participant Reg as NodeRegistry
    participant Tpl as TemplateEngine
    participant Sink as InMemorySink
    participant Eng as Engine
    participant Walker as DagWalker
    participant Exec as NodeExecutor

    User->>CLI: orno run hello.yaml
    CLI->>Load: load_from_path(path)
    Load-->>CLI: Pipeline
    CLI->>Reg: register shell + agent
    CLI->>Tpl: TemplateEngine::new
    CLI->>Sink: Arc new InMemorySink
    CLI->>Eng: Engine::new(sink, registry, templates)
    CLI->>Eng: run(run_id, pipeline)
    Eng->>Sink: record RunStarted
    Eng->>Walker: DagWalker::new(pipeline)
    loop while next_ready
        Walker-->>Eng: Some(&Node)
        Eng->>Tpl: render_request with per-node Context
        Tpl-->>Eng: NodeRequest
        Eng->>Sink: record NodeStarted
        Eng->>Reg: get(kind_str)
        Reg-->>Eng: Arc NodeExecutor
        Eng->>Exec: execute(id, req)
        Exec-->>Eng: NodeResponse or NodeError
        Eng->>Sink: record NodeFinished(ok)
        Eng->>Walker: complete(id, ok)
        Walker-->>Eng: Vec of skipped (id, reason)
        loop for each skipped
            Eng->>Sink: record NodeSkipped
        end
    end
    Eng->>Sink: record RunFinished(ok)
    Eng-->>CLI: Ok
    CLI->>Sink: snapshot
    Sink-->>CLI: Vec of EventEnvelope
    loop for each envelope
        CLI->>User: println JSON (NDJSON)
    end
```

What remains stubbed: the `AgentExecutor` still returns `NotImplemented`;
`LlmTransport`, budget enforcement, MCP, and subagents are absent from the
path. Shell nodes dispatch through the real `ShellExecutor`; agent nodes
compile and dispatch but fail at execute time until Phase 4 lands.

## Seams and implementors

Traits and their concrete implementors in the current tree. Dashed
`implements` edges are placeholder/default impls that exist only to keep
the seam live.

```mermaid
classDiagram
    class LlmTransport {
        <<trait>>
        +complete(req) LlmResponse
    }
    class DummyTransport
    LlmTransport <|.. DummyTransport

    class NodeExecutor {
        <<trait>>
        +execute(id, req) NodeResponse
    }
    class AgentExecutor
    class ShellExecutor
    NodeExecutor <|.. AgentExecutor
    NodeExecutor <|.. ShellExecutor

    class NodeRegistry {
        -map
        +register(kind, executor)
        +get(kind) Option
    }
    NodeRegistry o-- NodeExecutor

    class EventSink {
        <<trait>>
        +record(envelope)
    }
    class InMemorySink {
        -events
        +snapshot() Vec
    }
    EventSink <|.. InMemorySink

    class BudgetEnforcer {
        <<trait>>
        +preflight(req)
        +record_usage(prompt, completion)
    }
    class NoopEnforcer
    BudgetEnforcer <|.. NoopEnforcer

    class Engine {
        -sink
        -registry
        -templates
        +new(sink, registry, templates)
        +run(run_id, pipeline)
    }
    Engine ..> EventSink
    Engine ..> NodeRegistry
```

Concrete implementor locations:

- `DummyTransport` — `crates/orno-core/src/llm/dummy.rs:11`
- `AgentExecutor` — `crates/orno-core/src/node/agent.rs:11` (stateless stub; the real loop composes `LlmTransport` + tool handlers per ADRs 0005, 0008)
- `ShellExecutor` — `crates/orno-core/src/node/shell.rs:16` (real `tokio::process::Command` dispatch; ADR 0013 effects-declaration deferred)
- `InMemorySink` — `crates/orno-core/src/events/in_memory_sink.rs:13`
- `NoopEnforcer` — `crates/orno-core/src/budget/mod.rs:22` (no-op; stays alongside the trait per the Rust-idioms rule in `CLAUDE.md`)
- `Engine` — `crates/orno-core/src/execution/scheduler.rs:24`

Seams that ADRs 0005–0008 call for but are **not yet in the code**: `Agent`,
`ToolHandler`, `McpClient`. Do not add these without a working consumer.

## Error hierarchy

`crates/orno-core/src/error.rs`. `CoreError` re-exported from `lib.rs:17`
is the top-level facade; CLI dispatch boundaries use `anyhow::Result` and
rely on `#[from]` / `#[source]` chaining for readable diagnostics.

```mermaid
classDiagram
    class CoreError {
        <<enum>>
        Pipeline
        Node
        Llm
    }
    class PipelineError {
        <<enum>>
        Io(path, source)
        Parse(source)
        Validation(msg)
        Template(name, source)
    }
    class NodeError {
        <<enum>>
        UnknownKind(id, kind)
        NotImplemented(id)
        Execution(id, source)
    }
    class LlmError {
        <<enum>>
        NotImplemented()
        Rejected(msg)
    }
    CoreError --> PipelineError
    CoreError --> NodeError
    CoreError --> LlmError
```

`CoreError` conversions are all `#[from]` (`error.rs:8-16`). Struct-variant
fields (`Io`, `Template`, `UnknownKind`, `NotImplemented`, `Execution`) are
rendered above as method-style `(…)` for mermaid compatibility; the Rust
code uses named-field structs.

Conversion conventions (per `CLAUDE.md` dependency discipline): `#[from]`
only where the variant carries no extra context; otherwise `#[source]`
with a struct variant. `LlmError` has no `#[from]` today because every
call site wraps it with node id context in `NodeError::Execution`.

## Data model — pipeline schema

`crates/orno-core/src/pipeline/schema.rs`. This is the serde-facing YAML
shape consumed by `load_from_path`. `NodeKind` is `#[non_exhaustive]` and
discriminated by `kind:` in YAML.

```mermaid
classDiagram
    class Pipeline {
        version u32
        vars BTreeMap~String,Value~
        agents BTreeMap~String,AgentConfig~
        mcp_servers BTreeMap~String,McpServerConfig~
        nodes Vec~Node~
    }
    class Node {
        id String
        kind NodeKind
        needs Vec~String~
    }
    class NodeKind {
        <<enum>>
    }
    class AgentNode {
        agent String
        initial_prompt String
    }
    class ShellNode {
        command String
        args Vec~String~
    }
    class AgentConfig {
        model String
        provider String
        system Option~String~
        allowed_tools Vec~String~
        policy AgentPolicy
    }
    class AgentPolicy {
        max_iterations u32
        max_total_tokens u64
        max_tool_calls u32
        max_subagent_depth u32
        allow_mutations bool
        allow_network bool
        allowed_domains Vec~String~
        blocked_domains Vec~String~
        on_parse_error OnParseError
    }
    class McpServerConfig {
        <<enum>>
    }
    class McpStdioConfig {
        command Vec~String~
        env BTreeMap~String,String~
    }
    class McpHttpConfig {
        url String
        auth Option~McpAuthConfig~
        headers BTreeMap~String,String~
    }
    Pipeline "1" o-- "*" Node
    Pipeline "1" o-- "*" AgentConfig
    Pipeline "1" o-- "*" McpServerConfig
    Node --> NodeKind
    NodeKind <|-- AgentNode
    NodeKind <|-- ShellNode
    AgentConfig --> AgentPolicy
    McpServerConfig <|-- McpStdioConfig
    McpServerConfig <|-- McpHttpConfig
```

`NodeKind` is serialized with `#[serde(tag = "kind", rename_all =
"snake_case")]`, so the YAML discriminator is `kind: agent|shell` (ADR
0009 collapsed `llm` into `agent`; ADR 0017 §3 removed the former
`external` variant entirely). `Node` flattens its `NodeKind` variant via
`#[serde(flatten)]` (`schema.rs:40`). `McpServerConfig` uses the same
internal-tag pattern on `transport:` for `stdio` vs `http`.

## Data model — event envelope

`crates/orno-core/src/events/mod.rs`. `schema_version` on the envelope and
`#[non_exhaustive]` on `Event` are the forward-compatibility hinges.
`timestamp` is a human-readable RFC 3339 UTC correlator (ADR 0018) that
lets stdout event lines and stderr tracing lines be joined on wall
clock without a decoder.

```mermaid
classDiagram
    class EventEnvelope {
        schema_version u32
        seq u64
        timestamp OffsetDateTime
        event Event
    }
    class Event {
        <<enum>>
    }
    class RunStarted {
        run_id String
    }
    class NodeStarted {
        run_id String
        node_id String
    }
    class NodeFinished {
        run_id String
        node_id String
        ok bool
    }
    class NodeSkipped {
        run_id String
        node_id String
        reason SkipReason
    }
    class SkipReason {
        <<enum>>
        DependencyFailed(upstream)
    }
    class BudgetExceeded {
        run_id String
        reason String
    }
    class RunFinished {
        run_id String
        ok bool
    }
    EventEnvelope --> Event
    Event <|-- RunStarted
    Event <|-- NodeStarted
    Event <|-- NodeFinished
    Event <|-- NodeSkipped
    Event <|-- BudgetExceeded
    Event <|-- RunFinished
    NodeSkipped --> SkipReason
```

`Event` is serialized with `#[serde(tag = "type", rename_all =
"snake_case")]` (`events/mod.rs:50`), so lifecycle events on the wire
carry `"type": "run_started" | "node_started" | ...`. `CURRENT_SCHEMA_VERSION
= 1` is the value written into every envelope today. `timestamp` is
serialized via `time::serde::rfc3339` — wire form is a JSON string like
`"2026-04-21T18:31:54.387860Z"`. `EventEnvelope::new(seq, event)` is
the single construction site; no scheduler path builds an envelope by
hand.

Emission today: the engine emits `RunStarted`, `NodeStarted`/`NodeFinished`
pairs, and `RunFinished`. `NodeSkipped` is emitted for every transitively-
dependent node of a failed node; `upstream` names the originating failure,
not the direct parent (ADR 0021). `BudgetExceeded` has no emitter yet — it
exists for the budget seam to wire into.

## NodeExecutor dispatch — live path

`Engine::run` calls `NodeRegistry::get(kind_str).execute(id, req)` for each
ready node (`execution/scheduler.rs`). `ShellExecutor::execute` runs a
subprocess via `tokio::process::Command`; `AgentExecutor::execute` still
returns `NotImplemented` pending the Phase 4 agent loop.

```mermaid
sequenceDiagram
    participant Eng as Engine
    participant Reg as NodeRegistry
    participant Ex as NodeExecutor
    participant Tx as LlmTransport
    participant Sink as EventSink

    Eng->>Sink: record NodeStarted
    Eng->>Reg: get(kind_str)
    Reg-->>Eng: Some executor
    Eng->>Ex: execute(id, NodeRequest)
    alt NodeRequest Agent
        Note over Ex: AgentExecutor returns NotImplemented today; the ADR 0005 loop lands in Phase 4
        Ex->>Tx: complete(LlmRequest) per iteration
        Tx-->>Ex: LlmResponse
        Ex-->>Eng: NodeResponse (final assistant msg, usage)
    else NodeRequest Shell
        Note over Ex: ShellExecutor spawns subprocess (ADR 0013)
        Ex-->>Eng: NodeResponse (stdout, stderr, exit_code)
    end
    Eng->>Sink: record NodeFinished
```

Kind translation lives in `node::mod.rs` (`from_kind`, `kind_str`,
`render_request`); the scheduler delegates to those helpers without
matching on `NodeKind` itself.

## Template rendering

`pipeline::template::TemplateEngine` (`pipeline/template.rs`) wraps
MiniJinja with `auto_escape` forced to `None`. The CLI constructs it in
`commands::run::run` and passes it to `Engine::new`; the scheduler
renders each node's `NodeRequest` through `node::render_request` against
the per-node `Context` snapshot before dispatching. `vars`, `env`, and
`nodes.<id>.output` are the in-scope namespaces (yaml-spec.md). Agent
`prompt:` templates will consume the same engine once the Phase 4 agent
loop lands.

```mermaid
flowchart LR
    src[["template source<br/>(e.g. prompt: in YAML)"]]
    ctx[["context<br/>(vars + node outputs)"]]
    env[(MiniJinja Environment<br/>auto_escape=None)]
    src --> render["TemplateEngine::render"]
    ctx --> render
    env --> render
    render -->|ok| out[[rendered String]]
    render -->|err| terr["PipelineError::Template { name, source }"]
```
