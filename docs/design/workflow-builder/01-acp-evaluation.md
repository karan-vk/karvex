# Recommendation: `@agentclientprotocol/claude-agent-acp` as karvex's Claude Code integration layer

Required by `herdr-workflow-builder-prompt.md` §"References to study first", item 6.

Evidence base: the full adapter source (`src/acp-agent.ts`, 8,094 lines; ~20k
lines of vitest), `@agentclientprotocol/sdk` 1.3.0, `@anthropic-ai/claude-agent-sdk`
0.3.220, the Rust `agent-client-protocol` 2.0.0 crate, and three live end-to-end
probes against the authenticated `claude` CLI 2.1.222 with captured wire logs.
Claims marked **[measured]** were observed on this machine, not read in docs.

---

## Verdict

# ADOPT-PARTIALLY — narrowed

Concretely, three separable calls:

| Aspect of ACP | Call | When |
|---|---|---|
| ACP's **data model / event vocabulary** | **Adopt** as karvex's internal names for node lifecycle, tool activity, and usage | Phase 1, costs nothing |
| ACP's **tier design** (`session/set_config_option` `{model, effort}`) | **Adopt the design**, implement over the CLI flags | Phase 2 |
| ACP as the **integration/communication layer** with Claude Code | **Reject** | — |
| ACP as an **optional headless executor** for unwatched nodes and karvex's own internal agents, behind a cargo feature | **Defer**, revisit no earlier than Phase 4 | Phase 4+, off by default |

ACP is not on the critical path for Phases 1–3. It is not a dependency of this
design. Nothing in Phases 1–3 becomes harder if ACP is never adopted, and
nothing becomes impossible.

---

## 1. What the adapter is

A Node ≥ 22 process that translates ACP (JSON-RPC 2.0, NDJSON over stdio) into
`@anthropic-ai/claude-agent-sdk` `query()` calls, which spawn the native `claude`
binary **headless**:

```
karvex ⇄ NDJSON JSON-RPC ⇄ node dist/index.js ⇄ claude-agent-sdk ⇄ claude (headless) ⇄ API
```

It is a shim written by Zed Industries, not a protocol Claude Code implements.
One `query()` per ACP session; a single persistent consumer drains the SDK
stream and settles each turn's deferred.

The most important design fact is `_meta.claudeCode.options`: a full pass-through
of the SDK `Options` object. `hooks`, `mcpServers`, `disallowedTools`, `tools`
are merged; `cwd`, `includePartialMessages`, `allowDangerouslySkipPermissions`,
`permissionMode`, `canUseTool`, `executable` are overridden and not
client-controllable.

Two consequences that shape the recommendation:

1. You cannot supply your own permission callback. Permissions arrive only as
   `session/request_permission`. Fine, arguably good.
2. **You cannot register SDK hooks over the wire.** `Options.hooks` entries are
   JS callbacks marshalled as `hookCallbackIds`; functions do not survive JSON.
   `TeammateIdle` / `TaskCreated` / `SubagentStop` / `Stop` hooks are reachable
   **only through `settings.json`** — which is exactly the channel karvex already
   owns (`src/integration/claude_settings.rs`,
   `src/integration/assets/claude/karvex-agent-state.sh`,
   `KARVEX_INTEGRATION_VERSION = 7`).

## 2. What ACP genuinely offers

- 13 typed `session/update` variants (`tool_call`, `tool_call_update`,
  `agent_message_chunk`, `agent_thought_chunk`, `plan`, `usage_update`, …).
- `_session/steering` — injects into the *running* turn at SDK priority `now`,
  returning `injected | startedNewTurn | promptRequired`. A genuinely good
  steering primitive, but it addresses **the session**, not a teammate.
- **[measured]** `session/new` returns four config options, including
  `model ∈ {default, opus[1m], claude-fable-5[1m], sonnet, haiku}` and
  `effort ∈ {default, low, medium, high, xhigh, max}`. This is the single
  nicest thing ACP offers this project and maps one-to-one onto spec Feature 2.
- `session/fork | resume | list | close` — natural fits for the history/restore
  features.
- Deterministic, testable without PTYs.

## 3. Why it is rejected as *the* integration layer

### 3.1 The engine the spec names is not reachable through ACP **[measured]**

The spec says the builder is "powered by Claude Code's **Agent Teams**". I tested
whether teams form under ACP rather than reasoning about it. With
`CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS=1` forwarded through
`_meta.claudeCode.options.env` (and also set globally in this machine's
`~/.claude/settings.json`, which the adapter loads via
`settingSources: ["user","project","local"]`), after spawning two named workers:

```
NEW TEAM DIRS: []
TEAM CONFIG EXISTS: false   (~/.claude/teams/session-1d5c59bd/config.json)
TASKS DIR EXISTS: false     (~/.claude/tasks/session-1d5c59bd/)
```

Controls: headless `claude -p` also creates no team dir (22 before, 22 after);
the **interactive** session running concurrently on the same machine *did* create
`~/.claude/teams/session-a73f777a/config.json` with `members[]`,
`leadAgentId: "team-lead@…"`, `backendType: "in-process"`.

So team formation is bound to the interactive CLI session path. ACP delivers the
teams **substrate** — named background workers via the `Agent` tool, `SendMessage`,
`TaskCreate/Get/List/Update/Stop/Output`, nested subagent transcripts — but not the
teams **product feature** (team config, per-agent mailboxes, file-locked shared
task list, `TeammateIdle`/`TaskCreated`/`TaskCompleted` hooks, direct teammate
addressing, plan approval).

Adopting ACP *as the layer* would mean adopting it for a capability it does not
carry.

### 3.2 The DAG's load-bearing telemetry is not standard ACP anyway

Node *creation* and *labels* are standard-ACP-visible: **[measured]** the `Agent`
tool's `rawInput` accretes over successive `tool_call_update`s via streamed
partial JSON, reaching `{description, prompt, subagent_type, name:"alpha", model:"haiku"}`.

Node **lifecycle** is not:

- **[measured]** the `Agent` tool's ACP `tool_call` flips to `status:"completed"`
  the instant the background spawn returns ("Async agent launched successfully"),
  **10 wire messages before** the worker's actual `task_notification`.
- **[measured]** the worker's later re-activation via `SendMessage` produces
  **no `Agent` tool_call at all**.
- Live roster, real completion, per-node `{total_tokens, tool_uses, duration_ms}`,
  and per-node `output_file` checkpoints arrive **only** on `_claude/sdkMessage`
  (`task_started` / `task_progress` / `task_updated` / `task_notification` /
  `background_tasks_changed`) — an explicitly private, vendor-specific escape
  hatch of a pre-1.0 adapter shipping ~10 releases/month with no semver promise
  on `_meta.claudeCode.*`.

ACP-only therefore buys a *standard* protocol for the easy half and a *private
Claude-specific extension* for the hard half. That is coupling, not portability.

Related: **[measured]** parent-edge attribution is documented best-effort
(adapter comment at `:561-566`), and 2 of 8 observed tool-call ids appeared both
with and without `parentToolUseId`. A first-seen-wins DAG builder mis-parents
nodes.

### 3.3 It costs karvex its product identity in exchange for ~20% of a signal it already has

Under ACP the `claude` process is headless — there is no terminal UI to show.
karvex would have to reimplement, in ratatui, the entire Claude Code transcript
surface: streaming markdown, thinking blocks, tool cards, diff/edit review,
plan and TODO panels, permission modals, elicitation forms, embedded terminal
output. That is the bulk of an editor-grade ACP client, wildly out of proportion
to Phase 1, and it deletes the property the whole spec is built on: *the user can
type into the real agent*. No `/rewind`, no `/context`, no `/compact`.

It would also make `src/detect/` dead for those panes and introduce a second,
conflicting state authority — directly against the "Detection is decoupled"
principle and the agent-status arbitration karvex already implements.

Meanwhile karvex already owns a structured side-channel covering most of the
same ground, at zero marginal cost:

| Need | karvex today |
|---|---|
| Structured Claude lifecycle | `settings.json` hooks installed by `src/integration/claude_settings.rs` calling back over `karvex.sock` |
| Session identity + **full transcript path** | the same hook reports `session_id` and `transcript_path` |
| Screen state | `src/detect/` manifests → `AgentState::{Idle,Working,Blocked,Unknown}` |
| Drive an agent | `agent.prompt` (verifies the live foreground process still matches the expected agent before writing, and handles the Enter-submit race), `agent.send_keys`, `pane.send_input` |
| Observe | `events.subscribe` → `pane.agent_status_changed`, `pane.output_matched` |
| Isolation | `src/worktree.rs` |

And this design goes further (D6 in `00-overview.md`): karvex assigns the Claude
session id itself with `--session-id <uuid>`, so the transcript path is known
*before* the process starts. Tailing that JSONL gives a complete, structured
record of every message and tool call — the same substance ACP streams, over a
channel that is a stable published CLI contract rather than a private `_meta`
extension.

### 3.4 Distribution cost

karvex ships four static Rust binaries plus Homebrew and Nix. The `claude` binary
is itself self-contained (verified: `/home/karan/.local/share/claude/versions/2.1.222`
is an `ELF 64-bit LSB executable`). ACP mode additionally requires a **Node ≥ 22
runtime plus an npm package** on the user's machine. Making ACP mandatory puts a
Node toolchain on karvex's install path. Making it optional does not — which is
why the deferred adoption is feature-gated, not conditional at runtime only.

Rust-side, `agent-client-protocol` 2.0.0 adds **+26 crates**, which is the
smol/`async-io` reactor stack — i.e. a **second async runtime** alongside karvex's
deliberately minimal `tokio` feature set. Runtime-agnostic in practice, but an
extra reactor thread and a second set of executor semantics. MSRV 1.88 /
edition 2024 (compatible with the pinned 1.96.1 toolchain).

### 3.5 The literal "hybrid" is impossible

For the record, because it will be proposed again: you **cannot** attach ACP as a
side-channel to an already-running interactive `claude` in a pane. The adapter
*owns and spawns* the CLI subprocess's stdio (`src/acp-agent.ts:5745`,
`pathToClaudeCodeExecutable` at `:5653`), and the CLI exposes no ACP listener —
`claude --help` has no ACP subcommand or flag; the native binary is not an ACP
server. A Claude Code session is either an interactive PTY session or an
SDK/ACP session.

A hybrid *does* exist — it is what this design adopts — but its side-channel is
karvex's own hooks + deterministic transcript + `kvx` CLI self-reporting, not ACP.

---

## 4. What is adopted, precisely

### 4.1 The vocabulary (Phase 1, free)

karvex's internal and API names for node activity follow ACP's shape, so a future
full-ACP front-end is cheap and the names stay neutral per the runtime/client
boundary guardrail:

| ACP concept | karvex name |
|---|---|
| `task_id` (stable per worker identity, **[measured]** stable across re-activations while `tool_use_id` changed) | `run_node.id` — node identity, not invocation identity |
| `parentToolUseId` | `spawned_by` relation between `run_node`s |
| `tool_call` / `tool_call_update` | `run_event { kind: tool_activity }` |
| `agent_message_chunk` / `agent_thought_chunk` | `run_event { kind: output }` |
| `plan` / `plan_update` | `run_event { kind: plan }` |
| `usage_update`, `task_progress.usage` | `run_node.usage { total_tokens, tool_uses, duration_ms }` |
| `task_notification.output_file` | `node_checkpoint.artifact_path` |
| `background_tasks_changed` | the engine's live `RunGraph` roster |

### 4.2 The tier design (Phase 2)

**[measured]** ACP's live option sets are exactly the spec's tier axes. karvex
implements the same two-axis model over the verified CLI flags on `claude`
2.1.222:

- `--model <alias>` — accepts `fable`, `opus`, `sonnet` (or a full model name)
- `--effort <level>` — accepts `low`, `medium`, `high`, `xhigh`, `max`

Mapping table in `04-kvdag-and-execution.md` §7.

### 4.3 The deferred optional executor (Phase 4+, off by default)

If adopted later: cargo feature `acp` + config flag, using `agent-client-protocol`
2.x to spawn `claude-agent-acp` when present, degrading silently to PTY when Node
or the adapter is absent. Used only for nodes that do **not** need a watchable
pane — cheap fan-out and karvex's own internal agents (end-of-run summariser,
the Feature 4 1:1 interviewer, the workflow compiler) — which get a compact
synthetic node in the DAG view instead of a pane.

If it is ever built, two rules are non-negotiable:

1. Always enable `_meta.claudeCode.emitRawSDKMessages` filtered to
   `[{type:"system"}]`, and drive node lifecycle off
   `task_started`/`task_progress`/`task_updated`/`task_notification`/
   `background_tasks_changed` — **never** off `session/prompt` resolution or the
   `Agent` tool's ACP status (§3.2, and the adapter's own ~250 lines of
   accepted-residual comments tied to #825, #864, #866, #773, #680, #886, #880).
2. Build parent edges by **merging across updates**, never first-seen-wins (§3.2).

---

## 5. Re-evaluate if any of these become true

- Anthropic ships a native ACP mode in the `claude` binary (removes the Node
  runtime dependency and most of §3.3/§3.4).
- Team infrastructure forms under SDK/ACP sessions (removes §3.1).
- ACP standardises a subagent/teammate node kind with real lifecycle and
  per-node usage (removes §3.2).
- The adapter reaches 1.0 with a documented stability policy for
  `_meta.claudeCode.*` and `_claude/sdkMessage`.

Until then: PTY panes are the load-bearing execution surface, the hooks +
deterministic transcript + `kvx` self-report triple is the structured channel,
and ACP contributes a vocabulary and a tier design.
