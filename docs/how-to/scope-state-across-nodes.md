# How to scope state across nodes

Say an agent produces structured output and a downstream node needs specific fields from it, without re-parsing the agent's free-form reply. The `SetState` builtin tool plus the `nodes.<id>.state.<key>` template namespace cover that.

## When to use this

Use `SetState` when:

- The agent produces several structured fields and a downstream node needs them by name.
- You want the assistant's free-form reply for humans *and* clean fields for tooling.
- You don't want a downstream node running regex over the agent's prose to extract a value.

Don't use `SetState` when:

- The downstream node only needs the agent's reply text. `nodes.<id>.output` already provides it.
- You're tempted to hand off large blobs (file contents, JSON dumps). `SetState` is for small structured fields, not arbitrary payloads, and there's a per-node size cap.

## The shape

```yaml
agents:
  triager:
    allowed_tools:
      - SetState                    # the tool
    policy:
      allow_context_writes: true    # the gate
      # ... other policy fields

nodes:
  - id: triage
    kind: agent
    agent: triager
    # agent calls SetState with key="severity", value="high"
    # agent calls SetState with key="category", value="performance"

  - id: write_summary
    kind: shell
    needs: [triage]
    command: sh
    args:
      - "-c"
      - "echo 'Severity: {{ nodes.triage.state.severity }}' > out.md"
```

Three pieces:

1. The agent has `SetState` in its `allowed_tools`.
2. The agent's `policy.allow_context_writes` is `true`. This is the opt-in gate.
3. Downstream nodes read state via `{{ nodes.<id>.state.<key> }}`.

## Why a separate gate?

`SetState` has its own effect class (`ContextSelf`) and its own gate (`allow_context_writes`). It is **not** subject to `allow_mutations`. Writing to a per-node state slot isn't a filesystem mutation, it's a structured handoff, so an agent can publish state to its own slot without being authorized to touch any file.

That means a read-only agent can still publish structured findings:

```yaml
agents:
  reviewer:
    allowed_tools: [Read, SetState]
    policy:
      allow_mutations: false        # cannot Edit/Write/Bash
      allow_network: false          # cannot WebFetch
      allow_context_writes: true    # CAN call SetState
```

## Step 1: Teach the agent its method

System prompts that prescribe a sequence tend to work better than open-ended ones. This one comes from `examples/scoped-state/pipeline.yaml`:

```yaml
agents:
  triager:
    system: |
      You triage bug reports into structured fields. You have exactly
      one tool — `SetState` — and three fields to publish.

      Method (follow in order):
        1. Call SetState with key="severity" and a JSON string value
           from the set {"low", "medium", "high", "critical"}.
        2. Call SetState with key="category" and a JSON string value
           from {"performance", "correctness", "ui", "infra", "other"}.
        3. Call SetState with key="next_action" and a JSON string
           value — one imperative sentence describing the immediate
           next step.
        4. Reply with a one-sentence summary of the triage. Do not
           call any more tools after the reply.
```

The prompt does three things worth copying:

- It names the tool, so the model doesn't guess the surface.
- It numbers the steps, so the model emits one tool call per turn.
- It constrains the value space, so the downstream consumer doesn't have to handle "Crit" vs. "critical" vs. "URGENT".

## Step 2: Read state in a downstream node

```yaml
nodes:
  - id: triage
    kind: agent
    agent: triager
    initial_prompt: |
      Bug description:
      {{ vars.bug_description }}

      Follow the method in your system prompt.

  - id: write_summary
    kind: shell
    needs: [triage]
    command: sh
    args:
      - "-c"
      - "mkdir -p out && cat > out/triage.md"
    stdin: |
      # Triage summary

      - Severity:    {{ nodes.triage.state.severity }}
      - Category:    {{ nodes.triage.state.category }}
      - Next action: {{ nodes.triage.state.next_action }}

      Agent summary: {{ nodes.triage.output }}
```

Each `{{ nodes.triage.state.<key> }}` render is the **exact JSON value** the agent handed to `SetState`, not a regex over its final assistant message. If the agent set `severity` to the string `"high"`, the rendered value is `high`. If it set `severity` to a JSON object `{"score": 8}`, the rendered value is that object's JSON serialization.

The agent's free-form reply is still available as `{{ nodes.triage.output }}`, alongside the structured fields.

## Step 3: Validate before running

```bash
orno validate pipeline.yaml
orno plan pipeline.yaml
```

Validation checks that:

- `SetState` is in the agent's `allowed_tools`.
- `allow_context_writes: true` is set on the agent's policy.
- Every `{{ nodes.<id>.state.<key> }}` reference points at an upstream node (DAG ordering).

It does **not** check that the agent will actually publish every key you reference. Only a run will tell you that.

## Constraints on keys and values

- **Keys** must match `^[A-Za-z_][A-Za-z0-9_]*`: letters, digits and underscores, and no leading digit. `severity_score` is fine. `severity-score` and `1st_check` are not.
- **Values** are JSON. Strings, numbers, booleans, arrays and objects are all valid.
- **Size cap**: state has a per-node total size cap. A write that would exceed it returns `StateTooLarge` to the model as a tool denial, and the loop continues. The model can recover by writing smaller values.
- **Multiple writes to the same key**: the latest write wins. Every call is captured as a `tool_call_recorded` event, so the intermediate values stay in the event log, but downstream templates see only the last one.

## Recipe: bug triage with structured handoff

The full `examples/scoped-state/pipeline.yaml` is the canonical recipe for this pattern. To run it:

```bash
export OPENROUTER_API_KEY=sk-or-v1-...
orno run examples/scoped-state/pipeline.yaml
cat target/orno-scoped-state/triage-summary.md
```

Output:

```markdown
# Triage summary

- Severity:    high
- Category:    performance
- Next action: Profile the CSV export endpoint with a 50k-row dataset to identify the blocking call.

Agent summary: This is a high-severity performance issue affecting CSV export with large datasets, requiring immediate profiling.
```

The agent had no filesystem tool, only `SetState`. The downstream shell node persisted the result, and state is what made the handoff structured.

## Recipe: multi-field handoff to multiple consumers

```yaml
agents:
  classifier:
    allowed_tools: [Read, SetState]
    policy:
      allow_mutations: false
      allow_context_writes: true

nodes:
  - id: classify
    kind: agent
    agent: classifier

  - id: alert_security
    kind: shell
    needs: [classify]
    command: notify-security
    args: ["--severity", "{{ nodes.classify.state.severity }}"]

  - id: file_ticket
    kind: shell
    needs: [classify]
    command: ticket-create
    args: ["--category", "{{ nodes.classify.state.category }}",
           "--owner",    "{{ nodes.classify.state.suggested_owner }}"]
```

Both downstream nodes consume specific fields. Neither depends on the other (no `needs:` between them), and the engine runs nodes one at a time. A downstream node that wants the agent's free-form summary still uses `nodes.classify.output`.

## Failure modes

| Failure                                              | What happens                                                            |
| ---------------------------------------------------- | ----------------------------------------------------------------------- |
| Agent calls `SetState` without `allow_context_writes` | Tool denied; loop continues; model gets the denial as a tool result.    |
| Agent uses a malformed key                           | `SetState` returns an error; loop continues.                            |
| Agent's total state exceeds the size cap             | `StateTooLarge` denial; the offending write is rejected; loop continues. |
| Downstream template references a key the agent didn't set | Template render fails; `node_finished.failure: TemplateRenderFailed`. |

The third row has a detail worth knowing: a write that would push the total over the cap is rejected and **rolled back**, so the agent's state is whatever it was before the failed write.

## See also

- [Tools › `SetState`](../reference/tools.md#setstate): full argument schema.
- [Pipeline YAML › `policy.allow_context_writes`](../reference/pipeline-yaml.md#policy-semantics): the gate.
- [`examples/scoped-state`](../../examples/scoped-state/): runnable example.
