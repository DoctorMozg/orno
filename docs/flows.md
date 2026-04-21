# Flows

Mermaid diagrams describing what's wired today in `crates/orno-core/src/**` and
`crates/orno-cli/src/**`. This is a snapshot of the skeleton — seams are in
place, but most execution paths are deliberate stubs. ADR targets live in
`docs/arch.md` and the ADRs; this document only describes what the code does.

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
    node_llm["node::llm"]
    node_shell["node::shell"]
    node_reg["node::registry"]
    pipeline[pipeline]
    pipe_schema["pipeline::schema"]
    pipe_load["pipeline::load"]
    pipe_tpl["pipeline::template"]
    exec[execution]
    exec_dag["execution::dag"]
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
    node --> node_llm
    node --> node_shell
    node --> node_reg
    node_llm --> llm
    node_llm --> error
    node_shell --> error

    pipeline --> pipe_schema
    pipeline --> pipe_load
    pipeline --> pipe_tpl
    pipe_load --> pipe_schema
    pipe_load --> error
    pipe_tpl --> error

    exec --> exec_dag
    exec --> exec_sched
    exec_dag --> pipeline
    exec_dag --> error
    exec_sched --> events
    exec_sched --> pipeline
    exec_sched --> error
    exec_sched --> exec_dag
```

Note the gap: `exec_sched` does **not** depend on `node` or `node::registry`
yet. Node kinds and their executors are unreachable from the live
scheduler — they exist as pre-built seams.

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
the sink and prints each envelope as NDJSON. Node execution itself is not
wired — the scheduler emits synthetic `NodeStarted`/`NodeFinished{ok:true}`
for every node in source order (`execution/scheduler.rs:42-66`).

```mermaid
sequenceDiagram
    actor User
    participant CLI as orno-cli commands run
    participant Load as pipeline load
    participant Sink as InMemorySink
    participant Eng as Engine
    participant Plan as dag plan

    User->>CLI: orno run hello.yaml
    CLI->>Load: load_from_path(path)
    Load-->>CLI: Pipeline
    CLI->>Sink: Arc new InMemorySink
    CLI->>Eng: Engine new (sink clone)
    CLI->>Eng: run(run_id, pipeline)
    Eng->>Sink: record RunStarted
    Eng->>Plan: plan(pipeline)
    Plan-->>Eng: Vec of node_id (source order)
    loop for each node_id
        Eng->>Sink: record NodeStarted
        Note right of Eng: Skeleton — no executor dispatch yet
        Eng->>Sink: record NodeFinished ok=true
    end
    Eng->>Sink: record RunFinished ok=true
    Eng-->>CLI: Ok
    CLI->>Sink: snapshot
    Sink-->>CLI: Vec of EventEnvelope
    loop for each envelope
        CLI->>User: println JSON to stdout (NDJSON)
    end
```

What is **not** wired yet (reflected in the diagram's `Note`): the scheduler
never calls `NodeRegistry::get`, `NodeExecutor::execute`, or any
`LlmTransport`. Budget enforcement, MCP, subagents, and the agent loop are
all absent from this path.

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
    class LlmExecutor {
        -transport
    }
    class ShellExecutor
    NodeExecutor <|.. LlmExecutor
    NodeExecutor <|.. ShellExecutor
    LlmExecutor ..> LlmTransport

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
        +run(run_id, pipeline)
    }
    Engine ..> EventSink
```

Concrete implementor locations:

- `DummyTransport` — `crates/orno-core/src/llm/mod.rs:50`
- `LlmExecutor` — `crates/orno-core/src/node/llm.rs:23`, composes `Arc<dyn LlmTransport>`
- `ShellExecutor` — `crates/orno-core/src/node/shell.rs:13`
- `InMemorySink` — `crates/orno-core/src/events/sink.rs:37`
- `NoopEnforcer` — `crates/orno-core/src/budget/mod.rs:25`
- `Engine` — `crates/orno-core/src/execution/scheduler.rs:11`

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
    class LlmNode {
        provider String
        model String
        prompt String
        temperature Option~f32~
        max_tokens Option~u32~
    }
    class ShellNode {
        command String
        args Vec~String~
    }
    class ExternalNode {
        command String
        args Vec~String~
    }
    Pipeline "1" o-- "*" Node
    Node --> NodeKind
    NodeKind <|-- LlmNode
    NodeKind <|-- ShellNode
    NodeKind <|-- ExternalNode
```

`NodeKind` is serialized with `#[serde(tag = "kind", rename_all =
"snake_case")]`, so the YAML discriminator is `kind: llm|shell|external`.
`Node` flattens its `NodeKind` variant via `#[serde(flatten)]`
(`schema.rs:27`).

Drift note: ADR 0009 collapses `llm` into `agent`. The current skeleton
still ships `NodeKind::Llm`. When the migration lands, this diagram and
`schemas/pipeline.schema.json` move together.

## Data model — event envelope

`crates/orno-core/src/events/mod.rs`. `schema_version` on the envelope and
`#[non_exhaustive]` on `Event` are the forward-compatibility hinges.

```mermaid
classDiagram
    class EventEnvelope {
        schema_version u32
        seq u64
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
    Event <|-- BudgetExceeded
    Event <|-- RunFinished
```

`Event` is serialized with `#[serde(tag = "type", rename_all =
"snake_case")]` (`events/mod.rs:22`), so lifecycle events on the wire
carry `"type": "run_started" | "node_started" | ...`. `CURRENT_SCHEMA_VERSION
= 1` is the value written into every envelope today.

Emission today: the engine emits `RunStarted`, `NodeStarted`/`NodeFinished`
pairs, and `RunFinished`. `BudgetExceeded` has no emitter yet — it exists
for the budget seam to wire into.

## NodeExecutor dispatch (designed but not wired)

`LlmExecutor::execute` (`node/llm.rs:24-58`) already delegates through
`LlmTransport`. `NodeRegistry` (`node/registry.rs`) already keys executors
by kind. They are simply not called from `Engine::run`. This is the shape
the dispatch will take once the scheduler is wired:

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
    alt NodeRequest Llm
        Ex->>Tx: complete(LlmRequest)
        Tx-->>Ex: LlmResponse
        Ex-->>Eng: NodeResponse (content, usage)
    else NodeRequest Shell
        Ex-->>Eng: NodeError NotImplemented
    else NodeRequest External
        Note over Ex: not implemented yet
    end
    Eng->>Sink: record NodeFinished
```

Mapping gap to close when wiring: `pipeline::schema::NodeKind` (YAML-side)
and `node::NodeRequest` (executor-side) are parallel enums today with no
conversion between them. The scheduler will need a translator — either a
`From<&Node> for NodeRequest` impl or an explicit builder — before the
dispatch above can run.

## Template rendering

`pipeline::template::TemplateEngine` (`pipeline/template.rs`) wraps
MiniJinja with `auto_escape` forced to `None`. It is constructed nowhere
in the live call path yet; it exists for the agent loop to render
`prompt:` and other user-supplied templates with pipeline `vars` as
context.

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
