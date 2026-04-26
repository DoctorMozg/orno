# `hello` — minimum viable pipeline

The smallest pipeline that exercises a real agent loop. One agent node, `max_iterations: 1`, no tools, no MCP, no network.

## Surface exercised

- `kind: agent` with a single iteration.
- `vars.*` template substitution (`{{ vars.target }}`).
- Strict policy with every effect denied.
- `OPENROUTER_API_KEY` discovery from `.env.secrets` or process env.

## Inputs

None required.

## Run live

```bash
export OPENROUTER_API_KEY=sk-or-v1-...
cargo run -p orno-cli -- run examples/hello/pipeline.yaml
```

## Run without a key

The dummy transport returns a deterministic canned response, so `--replay-tape` and validation tests can exercise the pipeline offline:

```bash
ORNO_TEST_LLM_TRANSPORT=dummy \
  cargo run -p orno-cli -- run examples/hello/pipeline.yaml
```

## Inspect without spending tokens

```bash
cargo run -p orno-cli -- plan examples/hello/pipeline.yaml
cargo run -p orno-cli -- validate examples/hello/pipeline.yaml
```
