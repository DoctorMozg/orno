#!/usr/bin/env bash
#
# End-to-end record/replay walkthrough for the MCP-over-HTTP demo.
#
# Scenario: a single agent (`doc_summarizer` in mcp-http-demo.yaml)
# fetches the modelcontextprotocol/python-sdk documentation through a
# public, no-auth GitMCP HTTP server and writes a one-paragraph
# summary. We exercise the same pipeline three ways so the loop
# closes — live -> recorded -> replayed:
#
#   1) Live run     — real OpenRouter LLM call + real GitMCP HTTP
#                     traffic. Costs API tokens.
#   2) Record run   — same live behavior, plus a bundle file capturing
#                     every LLM request/response and MCP exchange.
#                     Safe to commit (GitMCP needs no Authorization
#                     header, and orno's per-run Redactor strips the
#                     OpenRouter key per ADR 0020).
#   3) Replay run   — drives the agent loop entirely from the bundle.
#                     No network, no LLM cost, no OPENROUTER_API_KEY
#                     needed. Output should match phase 2 modulo
#                     `run_id` (new ULID per run) and timestamps.
#
# Requires: cargo on PATH; OPENROUTER_API_KEY in $REPO_ROOT/.env.secrets
# (only for phases 1 and 2).
#
# Usage: bash examples/mcp-http-record-replay.sh

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

PIPELINE="examples/mcp-http-demo.yaml"
BUNDLE_DIR="examples/bundles"
BUNDLE="$BUNDLE_DIR/mcp-http-demo.ndjson"
SECRETS=".env.secrets"

if [[ -t 1 ]]; then
  BOLD=$'\033[1m'; CYAN=$'\033[36m'; RED=$'\033[31m'; RESET=$'\033[0m'
else
  BOLD=''; CYAN=''; RED=''; RESET=''
fi

banner() {
  printf '\n%s%s== %s ==%s\n\n' "$BOLD" "$CYAN" "$1" "$RESET"
}

if [[ ! -f "$SECRETS" ]]; then
  printf '%serror: %s not found.%s Phases 1 and 2 need an OPENROUTER_API_KEY.\n' \
    "$RED" "$SECRETS" "$RESET" >&2
  printf 'Create it with:\n    echo "OPENROUTER_API_KEY=sk-or-..." > %s\n' "$SECRETS" >&2
  exit 1
fi

mkdir -p "$BUNDLE_DIR"

banner "Phase 1 / 3 — Live run (real LLM + real MCP)"
echo "Streaming NDJSON events on stdout, tracing JSON on stderr."
echo "Ends with run_finished ok:true and a grounded content_excerpt."
cargo run --quiet -p orno-cli -- run "$PIPELINE" --secrets-file "$SECRETS"

banner "Phase 2 / 3 — Record run (live + bundle capture)"
echo "Same execution, but every LLM call and MCP exchange is captured to"
echo "$BUNDLE so the run can be replayed offline."
# Drop any prior bundle so a partial write from a prior failure cannot
# silently merge with the new run.
rm -f "$BUNDLE"
cargo run --quiet -p orno-cli -- run "$PIPELINE" \
  --secrets-file "$SECRETS" \
  --record-bundle "$BUNDLE"
printf '\nBundle written: %s (%s lines, %s bytes)\n' \
  "$BUNDLE" \
  "$(wc -l <"$BUNDLE" | tr -d ' ')" \
  "$(wc -c <"$BUNDLE" | tr -d ' ')"

banner "Phase 3 / 3 — Replay (no LLM, no network)"
echo "No OPENROUTER_API_KEY required here. The bundle drives the agent loop"
echo "deterministically; events should match phase 2 modulo run_id and timestamps."
cargo run --quiet -p orno-cli -- replay "$BUNDLE"

banner "Done"
echo "Bundle artifact:    $BUNDLE"
echo "Re-run replay only: cargo run -p orno-cli -- replay $BUNDLE"
echo "Re-run this script: bash examples/mcp-http-record-replay.sh"
