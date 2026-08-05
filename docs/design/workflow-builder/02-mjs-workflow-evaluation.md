# Recommendation: Claude Code's `.mjs` workflow mechanism — replicate, or not?

Required by `herdr-workflow-builder-prompt.md` §"References to study first", item 7.

Evidence base: two real generated workflow scripts (`rename-karvex.mjs`, 570
lines; the `workflow-builder-phase0` script, ~32 KB), their persisted run records
(`~/.claude/projects/<project>/<session>/workflows/wf_*.json`), their append-only
`journal.jsonl` files, and a resume-after-kill cycle observed live in flight.
The `Workflow` tool's own reply text is quoted verbatim because every claim below
traces back to it.

---

## Verdict

# HYBRID, DECLARATIVE-PRIMARY

- **A kvdag definition is declarative data** — nodes and typed edges — stored and
  immutably versioned in SurrealDB and interpreted by a Rust scheduler. This is
  the source of truth: the thing the TUI renders, the thing a human edits, the
  thing checkpoints attach to, the thing self-improvement diffs.
- **Dynamism comes from two narrow, typed primitives**, not from scripting:
  typed edge conditions, and a bounded `expand` **proposal** a node may emit
  (`00-overview.md` D12). Neither is Turing-complete and neither is user-supplied
  code.
- **No JavaScript engine is embedded in karvex.** Reproducing the `.mjs` DSL as
  the primary representation is rejected.
- **Three specific ideas from the `.mjs` mechanism are adopted verbatim**
  (§4): schema-forced structured output per node, background execution with
  completion notification plus a live progress view, and an append-only run
  journal — the last one re-keyed from *positional* to *topological*.

---

## 1. What the mechanism actually is

It is a **Claude Code CLI feature**, not a user-authored file format. An
orchestrating agent `Write`s a `.mjs` file and calls `Workflow({ scriptPath })`.
The tool replies:

> "Workflow launched in background. Task ID: wcxeb0y3d … Transcript dir:
> `.../subagents/workflows/wf_e04e8de0-cc9` … Script file:
> `.../workflows/scripts/workflow-builder-phase0-wf_e04e8de0-cc9.js` (Edit this
> file with Write/Edit and re-invoke Workflow with `{scriptPath: ...}` to iterate
> without resending the script.) Run ID: wf_e04e8de0-cc9. To resume after editing
> the script: `Workflow({scriptPath, resumeFromRunId: "wf_e04e8de0-cc9"})` —
> completed agents return cached results (cached results may themselves be empty
> — inspect journal.jsonl before assuming there is something to recover). You
> will be notified when it completes. Use /workflows to watch live progress."

The runtime is an **embedded JS engine inside the CLI binary** — abort stack
traces point at `/$bunfs/root/src/entrypoints/cli.js`, a Bun-compiled
single-file executable. It does not shell out to Node.

### Script shape

```js
export const meta = {
  name: 'rename-herdr-to-karvex',
  description: '...',
  phases: [{ title: 'Mechanical', detail: '...', model: 'sonnet' }, ...],
}
// then arbitrary top-level imperative JS, ending in `return <finalResult>`
```

`meta.phases` is descriptive metadata only; the real sequence is whatever
`phase('Mechanical')` calls happen at runtime.

### Orchestration hooks (observed in both real scripts)

- **`agent(prompt, opts)`** — spawns one subagent, returns a Promise for its
  *structured* result. `opts` carries `label`, `phase`, `model` (`'sonnet'` /
  `'opus'`), `effort` (`'high'` on 9 of 10 phase-0 researchers), and critically
  **`schema`**: a JSON Schema (`type:'object', additionalProperties:false,
  required:[...]`) that forces the subagent's answer through schema-validated
  structured output. Real schemas seen: `REPORT_SCHEMA {status, report}`,
  `BUILD_SCHEMA {buildGreen, testsGreen, report, remainingFailures}`,
  `FINDINGS_SCHEMA {summary, findings[]}`, `SUMMARY_SCHEMA {summary,
  key_findings, recommendations, risks, report_path}`. This is what lets the
  parent do real control flow (`if (build?.buildGreen && build?.testsGreen) break`)
  instead of scraping prose.
- **`parallel(fns)`** — array of thunks, `Promise.all`-style fan-out. Observed at
  widths 4, 4, and 10.
- **`phase(title)`**, **`log(message)`** — bookkeeping for the `/workflows` UI and
  the persisted run record.
- **`pipeline(...)`** — named in the SDK types but **not observed** in either real
  script; both express sequential composition with plain `await`/`while`.

Everything else is ordinary JavaScript: template-string prompt construction from
a shared `RULES` contract constant, `while` retry loops, `.filter(Boolean)`,
`.flatMap()`, and a final `return {...}` that becomes the run's `result`.

### The determinism constraint

Neither real script (600 combined lines) contains `Date.now()`, `Math.random()`,
or any other wall-clock/random value anywhere, including inside prompt strings.
All identity comes from static text plus prior `agent()` results. This is
*required* by the resume mechanism (§2): non-determinism inside a prompt changes
the cache key and permanently misses the cache.

## 2. Resumability: a journaled **prefix** cache

Each run persists `workflows/<runId>.json` (script text verbatim, `status`,
`result`, `phases`, `workflowProgress`, `logs[]`, `totalTokens`,
`totalToolCalls`, `durationMs`) and an append-only
`subagents/workflows/<runId>/journal.jsonl` of exactly two event types:

```json
{"type":"started","key":"v2:<64-hex>","agentId":"aa0b59d09b547d6cf"}
{"type":"result","key":"v2:<64-hex>","agentId":"aa0b59d09b547d6cf","result":{…}}
```

`key` is a content hash of the call's identity (prompt + model + schema, most
likely + label/phase). On `resumeFromRunId` the runtime **re-executes the script
from the top**; at each `agent()` call it hashes and looks for a matching
`"result"` line. A hit replays instantly with zero spawn and zero tokens.

**Observed live, not inferred.** `wf_e04e8de0-cc9` was killed
(`"status":"killed","error":"Error: Workflow aborted…"`) with **10 `"started"`
lines and zero `"result"` lines** — every parallel researcher died before
finishing, exactly the caveat the tool warns about. The resume therefore had to
relaunch all 10, appending a second batch of 10 `"started"` lines with new
`agentId`s to the same journal. Contrast `wf_b3a90444-ef3` (completed,
`durationMs: 9298395` ≈ 2.6 h, `agentCount: 14`), whose journal shows clean
`started` → `result` pairs, including a `FINDINGS_SCHEMA` result carrying 5 typed
finding objects with `file`/`line`/`rule`/`problem`/`fix`/`severity` fields that
a later `agent()` consumed programmatically via `JSON.stringify(allFindings)`.

**The critical property:** the cache is keyed by *call identity at a script
position*, not by *node identity in a graph*. It can answer "has this exact call
already run?" It cannot answer "restore node X's output and re-run node Y", which
is precisely what spec Feature 3 requires.

---

## 3. Requirement-by-requirement scoring

| Spec requirement | `.mjs` imperative script | Declarative DAG data |
|---|---|---|
| **Visual, editable, steerable DAG view** (Feature 1) | The graph exists only as an execution *trace* once agents have launched. `if (build?.buildGreen)`, `while (attempt < 3)`, `parallel(SURFACES.map(...))` mean the shape is a side effect of running arbitrary JS. You cannot open a not-yet-run script and get a meaningful picture. | Nodes/edges *are* the artifact. Rendering, hit-testing, and mid-run mutation are direct operations on data the renderer already holds — which is what `compute_view()`/`render()` purity expects: geometry over data, never over live script state. **Declarative wins.** |
| **Immutable kvdag versioning** (Feature 4) | Script text can be stored as an immutable blob, but diffs are *textual diffs of imperative code*, not semantic ("node X's prompt changed", "edge Y→Z added"). Self-improvement must reason about and mutate specific nodes, not rewrite control flow. | Nodes/edges map onto records + `RELATE` edges; a new version is a new immutable record set sharing `node_key`s where unchanged, trivially diffable per node. **Declarative wins.** |
| **Dynamic runtime growth** (Feature 1/Phase 2) | Exactly what it is built for: `parallel()` over dynamically-computed arrays, `agent()` inside loops, arbitrary recursion. **This is the one axis where the script model genuinely wins.** | A rigid pre-declared graph cannot do this natively. Requires an explicit expansion primitive — new engine work, not free. **Script wins; mitigated by the hybrid escape hatch.** |
| **Per-node / per-subgraph checkpoint restore** (Feature 3) | Whole-script content-hash **prefix** cache. Replay-or-relaunch only. There is no "restart from node X with fresh inputs while keeping node Y's cached output"; the *topology* is not addressable, only the *call*, which only exists once you have executed up to it. | Node ids are stable, addressable, and independent of control-flow position by construction. Restore is "load node N's last checkpoint as its result" — a plain read. **Declarative wins.** |

Score: the script model satisfies **1 of 4** hard requirements, at the cost of the
other three.

## 3.1 Two engineering reasons independent of the table

1. **karvex's execution primitive is a PTY pane running a real `claude`, not an
   in-process Task/subagent.** Faithfully reproducing `.mjs` means embedding a JS
   engine (Boa / QuickJS / deno_core) inside the karvex binary and
   reimplementing Claude Code's `agent()`/journal/task machinery on top of it,
   purely so that `agent()` can call out to pane I/O underneath. That is a large,
   unmotivated dependency, a second execution model, and an
   arbitrary-code-execution surface inside a TUI.
2. **It reintroduces the exact opacity the design exists to remove.** A hidden
   imperative script is opaque by construction; a declarative node graph is
   inspectable by construction — it is *the same data the scheduler used to
   decide what to run next*, which is also the data the TUI renders. Deer-workflow
   (the reference builder) confirms the failure mode: its "workflow" is an
   ordinary async TS module with no declared graph at all, and consequently its
   TUI is a passive phases-checklist + log tail — no DAG visualisation exists in
   that codebase, because there is no graph to visualise.

---

## 4. What is adopted from the `.mjs` mechanism

Three ideas, carried over deliberately:

### 4.1 Schema-forced structured output per node — adopted 1:1
Every `agent()` call's `schema` option is exactly a kvdag node's
**output contract**. Each node declares a JSON Schema; its result must validate
against it before the node can complete (`00-overview.md` D7); downstream nodes
and edge conditions branch on typed fields the way `build?.buildGreen` did. This
is what makes typed edge conditions possible *without* scripting, and it is the
single most transferable artifact from the whole mechanism.

### 4.2 Background execution + notification + live progress view — adopted
`Workflow()`'s "launched in background … you will be notified … use `/workflows`
to watch live progress" is precisely the UX karvex wants, substituting the in-TUI
DAG view for `/workflows` and karvex's event bus + notification path for the
tool's completion callback. A `workflow.run` call returns immediately with a run
id; the run proceeds in the background; the DAG view is the live progress
surface.

### 4.3 Append-only run journal — adopted, but **re-keyed**
The journal idea is right; the key is wrong for this use case.

| | `.mjs` | karvex |
|---|---|---|
| Journal | `journal.jsonl`, append-only | `run_event` table, append-only, keyed `(run, seq)` with an optional `run_node` reference |
| Restore index | *the journal itself* — one content hash serves both roles | a **separate** `node_checkpoint` table, keyed `(kvdag_version, node_key, kind)` plus `(run_node, seq)` — topological |
| Key | `v2:<sha-of-call-identity>` (prompt + model + schema) | journal: `(run, seq)`; checkpoints: `(kvdag_version, node_key, kind)` |
| Answers | "has this exact call run before?" | journal: "replay this run exactly"; checkpoints: "which nodes have a checkpointed result at this kvdag version?" |
| Enables | whole-script prefix resume | per-node and per-subgraph restore, run replay, DAG projection, self-improvement evidence |

Strictly more capable: the topological key can express everything the positional
key can, plus the two restore modes the positional key structurally cannot.

Also adopted, from the same family of good ideas:
- **The shared-contract prompt pattern** (`RULES`, one ~80-line constant
  interpolated into every phase's prompt so all agents share one source of
  truth) becomes a kvdag-level `contract` field prepended to every node's system
  prompt — one place to state invariants for the whole workflow.
- **The determinism discipline.** karvex's node prompts are assembled from
  static templates plus upstream checkpoint summaries only — never wall clock,
  never randomness — so a re-run of the same version with the same inputs
  produces the same node identities and can hit the same checkpoints.

### 4.4 Explicitly not adopted

| Not adopted | Why |
|---|---|
| Embedded JS runtime | §3.1 |
| `meta.phases` as the structure | Descriptive-only in the original; karvex's phases are real node groupings with edges |
| `resumeFromRunId` prefix caching | Superseded by topological checkpoint restore (§4.3) |
| `pipeline()` | Not observed in any real script; sequential edges cover it |
| One-level nesting caps (deer-workflow's `MAX_NESTED_WORKFLOW_DEPTH = 1`) | That is a stateless-CLI scoping decision, not a technical ceiling; karvex bounds depth with configurable guardrails instead |

---

## 5. The escape hatch, precisely bounded

The "hybrid" part is small on purpose. Exactly two constructs give the graph the
dynamism `.mjs` got from raw JS, and neither can execute user code:

1. **Typed edge conditions.** An edge may carry a predicate over the source
   node's schema-validated output: field presence, equality, comparison,
   `in`/`not in`, boolean composition. Evaluated in Rust; total; no loops; no
   function calls. This covers `if (build?.buildGreen) break` and the
   bounded-retry `while (attempt < 3)` (which becomes a node-level retry policy,
   not a script loop).
2. **The `expand` proposal.** A node's output may include
   `expand: [{ template, label, inputs }]`. The engine validates each proposal
   against the kvdag's declared templates and the run's `max_depth`/`max_nodes`
   guardrails, then commits the accepted ones as real nodes with real panes, or
   rejects them with a surfaced `workflow.growth.limited` event. This covers
   `parallel(SURFACES.map(...))` and dynamic fan-out width, and — unlike the
   `.mjs` mechanism — it also covers *teammate-spawned teammates*, which Claude
   Code's own team feature cannot do at all.

Everything else the two real scripts did — `.filter(Boolean)`, JSON
stringification into the next prompt, log narration — is either input assembly
(handled declaratively by edge payload selection) or bookkeeping (handled by the
run journal).
