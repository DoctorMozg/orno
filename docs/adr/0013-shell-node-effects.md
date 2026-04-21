# ADR 0013 — Shell node effects declaration

- Status: proposed; extends ADR 0005 to non-agent nodes
- Date: 2026-04-21

## Context

ADR 0005 fixes five strictness dimensions on `agent` nodes, enforced
by an `AgentPolicy` at the executor boundary. `ShellNode` (ADR 0004,
amended by ADR 0009) ships without the equivalent policy:
`ShellNode { command, args }` has no declared effects surface.

A CI runner holding cloud credentials — the intended deployment
target — running a YAML pipeline with `kind: shell` plus a `curl`
invocation is, today, pre-approved RCE with ambient network,
filesystem, and environment access. The strictness claim ADR 0005
makes on agent nodes does not extend to the most obvious node kind;
for an operator writing CI policy, this is the same as having no
strictness.

The brainstorm on 2026-04-21 surfaced this as the highest blast-radius
gap in the current v0.1 surface (lens-devops's "Shell Nodes Are the
Undeclared Blast Radius," lens-engineer concur, lens-cto concur).

## Decision

`ShellNode` gains a mandatory `effects:` block, enforced at the
executor boundary (parse accepts the struct; executor rejects before
spawn):

```yaml
- kind: shell
  command: curl
  args: ["-sSL", "https://api.example.com/foo"]
  effects:
    network: true                          # bool, required
    fs: read-only                          # read-only | read-write | none, required
    env_passthrough: [CI, GITHUB_TOKEN]    # list, required (may be empty)
    allowed_domains: ["api.example.com"]   # optional
    blocked_domains: []                    # optional
```

1. **Mandatory.** Missing `effects:` is a hard
   `PipelineError::ShellEffectsMissing` validation failure. No
   implicit defaults — the absence is always intentional on the
   operator side, never a forgetting-the-field silent pass-through.
2. **Domain semantics mirror ADR 0005.** Blocklist wins on overlap.
   Violations emit the same `DomainBlocked` event; no new variant.
3. **Env passthrough is an explicit allowlist.** All environment
   variables not listed are stripped from the child process. `PATH`
   and `HOME` are injected by the executor from pipeline config, not
   passed through from the orno parent environment.
4. **`ShellPolicy`** in `orno-core::node::shell` aggregates the
   block — parallel to `AgentPolicy` from ADR 0005. The executor
   builds it once from the pipeline struct and validates before
   spawn.
5. **v0.1 enforcement posture.** Declaration is mandatory and fails
   hard. Observation of actual process operations (verifying
   declared effects match real syscalls) is **deferred** — v0.1
   ships the declaration surface only. Kernel-level sandboxing
   (nsjail, landlock, unshare) is a follow-up ADR. Observed-
   violation enforcement follows the ladder in ADR 0016.

## Consequences

- CI operators can deny pipelines by declared effects without
  reading individual shell commands. Policy scales.
- Strictness is a property of the v0.1 node-kind set as a whole, not
  just `agent`. ADR 0005's claim becomes defensible.
- Pre-v0.1 example pipelines with `kind: shell` require updates to
  add `effects:` blocks — acceptable breaking change before v0.1.0
  tag; post-tag changes go through a deprecation window.
- The event enum grows by one variant
  (`ShellEffectsViolation { declared, observed }`) —
  append-only per ADR 0003, reserved for the v0.2 observation path.
- The declared-vs-observed split is explicitly two-phase: v0.1 ships
  declared-only, v0.2+ ships observation tooling. ADR 0016 specifies
  the per-version defaults.

## Amendments

Extends ADR 0005 — strictness now covers shell nodes, not only agent
nodes. Does not amend ADR 0004 or 0009: the node-kind set stays
`agent, shell, external`.
