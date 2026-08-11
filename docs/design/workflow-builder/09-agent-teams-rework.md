# 09 — Agent-teams rework: Claude Code becomes the execution engine

Status: draft for alignment
Date: 2026-08-11
Supersedes: the execution half of 04-kvdag-and-execution.md and 07-phase3-plan.md.
Replaces: 08-phase4-plan.md (branch `docs/phase4-plan`) — that plan extended the
custom engine and is retired by this document.

## 0. The decision

Karvex stops executing workflows itself. A workflow run becomes **one Claude
Code team-lead session in a karvex pane**, orchestrating **Claude Code agent
teams** — the lead spawns teammates through karvex's existing tmux shim, so
every node of the run is a real, interactive `claude` session in a real karvex
pane. Steering a node is clicking its pane and typing. Steering the run is
clicking the lead's pane and typing.

Karvex's job shrinks to what it is uniquely positioned to do:

- **define** — author, version, and store workflow definitions (kept as-is)
- **launch** — render a definition + args into a lead prompt and spawn the lead
- **observe** — project Claude Code's own team/task state into run records,
  events, and the DAG view
- **save** — persist run history, summaries, and a snapshot of what the team
  actually did
- **resume** — bring a stopped run back with `claude --resume`
- **reproduce loosely** — rerun the same definition version with the same args;
  the lead re-plans, which is the point

Everything that decided *how execution proceeds* — scheduling, dependency
resolution, node claiming, retry policy, output-schema validation, corrective
prompts, growth guardrails, the watchdog, the summariser lifecycle — is Claude
Code's job now, via its shared task list and the lead's judgment.

## 1. Why this works today (evidence, all verified live on this machine)

Claude Code 2.1.226 is installed; agent teams (experimental,
`CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS=1`) and cross-session messaging
(≥ 2.1.224) are both available. Docs:
`code.claude.com/docs/en/agent-teams`, `/docs/en/cross-session-messaging`.

**Teams already spawn into karvex panes.** Claude Code's split-pane teammate
mode drives tmux; karvex ships a tmux-compat shim (`src/cli/tmux_compat.rs`,
installed as a `tmux` symlink by `src/platform/tmux_shim.rs`, env injected by
`src/pane.rs::apply_tmux_compat_env`). A live team config captured 2026-08-11
from a Claude session running inside karvex:

```json
{
  "name": "session-213aa9bf",
  "leadSessionId": "213aa9bf-2652-45ca-ac73-1cf00b493ef3",
  "members": [
    { "name": "team-lead",   "agentType": "team-lead", "tmuxPaneId": "leader",  "backendType": "in-process" },
    { "name": "explore-shim", "agentType": "Explore",  "tmuxPaneId": "w3:p1P",
      "model": "opus", "backendType": "tmux", "isActive": true, "cwd": "/home/karan/code/karvex" }
  ]
}
```

`tmuxPaneId` **is a karvex pane id** — karvex's own identifiers come back to it
through Claude Code's team state. The subagent-name fix
(`src/agent_session_registry.rs`) already reads this file.

**The shared task list is a dependency graph.** One JSON file per task at
`~/.claude/tasks/<team-name>/<n>.json`:

```json
{ "id": "1", "subject": "1. Workspaces/tabs/panes CRUD + IDs",
  "description": "create/list/rename/...", "owner": "verify-core",
  "status": "completed", "blocks": [], "blockedBy": [] }
```

`blockedBy` is edge structure; `owner` is the claiming teammate; status is
`pending | in_progress | completed`. Claude Code does claiming (file-locked),
unblocking, and reassignment. A karvex workflow node maps 1:1.

**Sessions are discoverable and messageable.** Each session registers
`~/.claude/sessions/<pid>.json`:

```json
{ "pid": 3581893, "sessionId": "a73f777a-…", "cwd": "/home/karan/code/karvex",
  "kind": "interactive", "messagingSocketPath": "/run/user/1000/cc-socks/3581893.sock",
  "name": "karvex-fd", "status": "busy" }
```

karvex already consumes this registry for pane naming. `messagingSocketPath`
is the programmatic steer/interrogate channel; the session's own inbound
controls still apply.

**Resume exists.** `claude --resume <lead-session-id>` restores the lead's
conversation. The task directory persists across resume (keyed by team name,
which is derived from the session id: `session-` + first 8 chars). Split-pane
(tmux-backend) teammates are independent sessions and can each be resumed the
same way.

**Two lifecycle traps, both handled in this plan:**

1. The team **config** dir (`~/.claude/teams/<team>/`) is deleted when the lead
   session ends → karvex snapshots it into the run record while the run lives.
2. In-process teammates don't survive `/resume` → karvex always launches the
   lead in split-pane (tmux) mode so teammates are panes, and the resume prompt
   tells the lead its old teammates are gone and to respawn as needed.

## 2. What is kept, what is removed

From the module map (explore pass, 2026-08-11; line counts approximate):

### Kept — definition, storage, presentation (~19.5k lines)

| Area | Files | Fate |
|---|---|---|
| Definitions & model | `workflow/definition.rs`, `workflow/model.rs` (data half), `workflow/tier.rs` | kept; tiers become prompt hints (§3.2) |
| Store | `workflow/store/*`, `app/workflow_store.rs`, migrations | kept; schema gains run-binding fields (§3.4) |
| Wire schema | `api/schema/workflows.rs` | kept, trimmed (§3.6) |
| UI | `ui/workflow_dag.rs`, `workflow/layout.rs`, `ui/workflow_launch.rs`, `ui/workflow_runs.rs` + input halves | kept; DAG re-pointed at the projection (§3.5) |
| CLI | `cli/workflow.rs` | kept, trimmed |
| Run history | `app/workflow_history.rs` | kept; old runs stay readable forever |
| tmux shim | `cli/tmux_compat.rs`, `platform/tmux_shim.rs` | kept — it is now the execution substrate |

### Removed — custom execution (~16k lines)

| Area | Files | Replaced by |
|---|---|---|
| Scheduler, ready set, edge settling | `engine/schedule.rs`, `engine/graph.rs` | Claude Code task list (`blockedBy`) |
| Output validation + corrective prompt | `engine/complete.rs` | the lead's judgment |
| Growth guardrails | `engine/expand.rs` | the lead creates tasks freely; karvex only records |
| Watchdog | `engine/watchdog.rs` | lead + user steering; karvex's per-pane agent detection still shows stuck panes |
| Summariser lifecycle | epilogue machinery in `engine/mod.rs` | a final "write the run summary" task + `kvx workflow run finish` self-report (§3.3) |
| Node contract | `binding/spawn.rs` task.md / output_schema.json / result.json / node env | the lead's spawn prompts |
| Node claiming, retry policy, 20s tick, effect pump | `app/workflow.rs` engine half | Claude Code |
| Engine facade | `engine/mod.rs` | — |

`binding/interrogate.rs` shrinks to "resume this session id in a pane".
`binding/observe.rs` is replaced by the projection watcher (§3.4).

Old run records remain readable: the historical snapshot path
(`app/workflow_history.rs`) never fed the engine and keeps working. We do not
migrate or reinterpret pre-rework runs.

## 3. The new architecture

### 3.1 Launch: `workflow.run` spawns a lead

`workflow.run` (API, CLI, and launcher modal — all already speak the same
path) now:

1. Resolves the definition version + args exactly as today.
2. Renders a **lead prompt** (§3.2) into a run directory
   (`KARVEX_WORKFLOW_RUNS_DIR/<run-id>/lead-prompt.md`).
3. Spawns a pane in the run's workspace (same placement rule as today:
   `ActiveRun.workspace_id`, split of the focused pane) running interactive
   `claude` with:
   - `CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS=1`
   - teammate mode forced to tmux. Confirmed against the 2.1.226 bundle:
     `--teammate-mode tmux` is a real hidden flag (choices
     `auto|tmux|iterm2|in-process`) but only applies when the teams env flag
     is set, and the **default is `in-process` even inside tmux** — the spawn
     path short-circuits before backend detection — so forcing is mandatory,
     not defensive. `--settings '{"teammateMode":"tmux"}'` feeds the same
     snapshot.
   - `KARVEX_WORKFLOW_RUN_ID` in the env, so the lead's `kvx` calls
     self-identify
   - NO positional prompt. Verified live (twice, correct argv): a positional
     prompt never reaches an interactive `claude` in a fresh pane — the same
     lost-seed failure karvex already knew for node panes. The lead spawns
     bare and is seeded exactly once via pane input (`agent.prompt`) after its
     team config appears.
   - a trust preflight before any of this: `claude`'s folder-trust dialog
     both blocks an untrusted cwd AND discards the initial prompt, leaving a
     healthy-looking lead with no plan. `workflow.run` reads
     `~/.claude.json` → `projects.<cwd>.hasTrustDialogAccepted` and refuses
     with the exact remedy; an unreadable config answers "trusted" rather
     than blocking every run.
4. Binds the run to its team. Verified live: `--session-id` does NOT
   determine the lead session id or team name, and `~/.claude/sessions/<pid>.json`
   is not reliably written for these launches — so binding reads the team
   config itself, which appears at lead startup carrying `leadSessionId` and
   `createdAt`. Two tiers: (1) match an unbound team whose `createdAt` falls
   within a slack window of spawn time and whose leader member's `cwd` equals
   the lead pane's cwd; (2) prefer, *within that fresh set*, a team whose
   members carry `backendType: "tmux"` pane ids belonging to this run's own
   workspace. Freshness is mandatory, not just tier-1: karvex pane ids are
   per-server, so a stale team config from a dead server can name pane ids
   that happen to exist in this server too — the pane rule alone once bound a
   run to a months-old unrelated team (found live, regression-pinned). Bind
   `lead_session_id` + `team_name` once, never re-derive. Run status:
   `running`. Known limitation: two runs launched into the same cwd within
   the slack window can race for the tier-1 match; the single-live-run guard
   makes this rare.

The lead is interactive on purpose: permission prompts, plan approvals, and
steering all happen in its pane, which is the interaction model Karan asked
for ("I just have to click on the pane").

### 3.2 The render contract (definition → lead prompt)

The rendered prompt is the *entire* influence karvex has on execution, so it is
a versioned, tested template. It contains:

- The workflow name, run id, and interpolated args.
- **The node list as a task plan**: "create these tasks with exactly these
  subjects and these `blockedBy` relationships" — one task per node, subject
  prefixed with the node id so the projection can match tasks back to
  definition nodes by prefix, loosely.
- **Teammate guidance from tiers**: `workflow/tier.rs` output becomes prose —
  suggested teammate count, model per node demand ("use sonnet for the
  mechanical nodes, opus for the design node"). Hints, not enforcement.
- **Naming rule**: name each teammate after the node it owns, so team-config
  `tmuxPaneId` entries label panes with node names (the sidebar already
  renders these).
- **The finish rule**: when all tasks are complete, write a run summary and
  call `kvx workflow run finish --summary-file <path>` (the lead's pane has
  `kvx` on PATH and `KARVEX_WORKFLOW_RUN_ID` exported). This replaces the
  entire summariser subsystem with one CLI call.
- What "loose" means: the lead may split, merge, add, or skip tasks when the
  work demands it — karvex records what actually happened rather than
  enforcing the plan.

### 3.3 Finish and failure

- **Normal finish**: the lead calls `kvx workflow run finish` with a summary →
  run `succeeded`, summary stored via the existing `run_summary` table and
  `workflow.run.summarized` event.
- **Lead exits without finishing** (pane closed, crash, shutdown): karvex
  notices the session vanish → run recorded terminal with the last-known
  task/member snapshot, resumable (§3.7). Until the Phase D protocol bump
  this reuses the existing `failed` wire status carrying a structured
  `failure = {"kind": "lead_exited", "resumable": true, …}` payload rather
  than minting a new wire enum variant mid-phase; Phase D gives it a real
  status name.
- **Cancel**: `workflow run cancel` sends the lead a shutdown message over its
  messaging socket and, failing that, closes the panes. No task-level kill
  choreography — teammates belong to the lead.
- There is no engine-judged `Failed`. If the lead reports failure in its
  summary, the run records that; the truth lives in the transcript, one click
  away.

### 3.4 Projection: Claude Code state → karvex run records

A small watcher (server-side, feature-neutral name: **run projection**) polls
the run's two source directories while the run is live (2s cadence is plenty;
these are tiny local JSON files):

- `~/.claude/tasks/<team>/` → node records: subject, owner, status,
  blockedBy. Projected into `run_node`-shaped records and `workflow.node.*`
  events (matched to definition nodes by subject prefix; unmatched tasks are
  recorded as *emergent* nodes — the loose part, first-classed). Live-verified
  shape notes: tasks also carry `activeForm`, and `owner` is **absent** on
  unclaimed tasks (optional at the parse layer, never defaulted-empty);
  members additionally carry `agentId`, `color`, `joinedAt`, `prompt`,
  `planModeRequired`, `subscriptions`.
- `~/.claude/teams/<team>/config.json` → member records: teammate name,
  agent type, model, **pane id**, active flag. Snapshotted on every change
  into the run record, because Claude Code deletes this directory at session
  end.

Node "status" in the DAG view becomes two-layered: task status
(pending/in_progress/completed) from the projection, live agent state
(working/idle/needs-input) from karvex's existing per-pane detection — which
is per-pane and needs no workflow-specific code.

Boundary note (CLAUDE.md guardrail): the projection is a shared runtime fact
and lives on the server behind the JSON API; nothing here is TUI-private.

### 3.5 UI: same surfaces, honest content

- **Launcher** (`Ctrl+B F`): unchanged — definitions, args, tier hints.
- **DAG view** (`Ctrl+B B`): renders the projection. Planned nodes appear
  immediately (from the definition); tasks light them up as the lead creates
  and teammates claim them; emergent tasks appear as new nodes. Selecting a
  node whose owner has a pane focuses **that pane** — this is the primary
  steer/interrogate affordance now. The footer verbs collapse to:
  `↵ focus pane · hjkl move · esc`. A `m message` verb (send into a member's
  session over its messaging socket) is deferred to Phase E: the
  session-registry file it depends on is empirically not written for our lead
  launches, and a verb that silently no-ops is worse than no verb. Phase E
  owns the session-identity plumbing for resume and gets messaging with it.
- **Run history** (`Ctrl+B Shift+B`): unchanged shape; a historical run's node
  detail shows owner, timing, and offers *interrogate* = resume that member's
  session id in a pane (§3.7).
- **Sidebar**: teammate names/colors already work via the team-config reader
  and the accent-token path in the shim.

### 3.6 API/CLI surface after the trim

Kept (re-pointed): `workflow.{create,get,list,run}`, `workflow.version.*`,
`workflow.run.{get,list,cancel}`, `workflow.summary.*`, run/node events.
Added: `workflow.run.finish` (lead self-report), member/emergent-node fields
on run records.
Removed (corrected against the tree in Phase D): Methods
`workflow.node.{report,expand,restart,steer,interrupt,interrogate}`, the
`workflow.node.output_checkpoint` *event* (never a Method), and CLI verbs
`node {complete,report,expand,restart,steer,interrupt,interrogate}`
(`node complete` was a CLI alias dispatching `node.report`). `node.get`
stays, projection-backed. Interrogate is a clean absence, not a stub —
Phase E's interrogate (resume a member's session id from the snapshot) is a
different mechanism, and a seam shaped like the old one would be a
wrong-shaped hole. `include_prior_summaries` stays and is wired into the
lead prompt (render contract v2) — the run dir's `context/prior-runs.md` is
told to the lead instead of being silently written and never mentioned.
`PROTOCOL_VERSION` (`src/protocol/wire.rs`): bump once per the published-
protocol rule — protocol 19 is what the installed preview build speaks; check
against published channels at implementation time.

### 3.7 Save, resume, reproduce, capture

- **Save**: run row = definition version + args + lead session id + team name
  + member snapshots + task snapshots + summary. All in the existing store;
  append-only discipline unchanged.
- **Resume** (`workflow run resume <run>`): spawn a pane running
  `claude --resume <lead_session_id>` with the same env. Tasks are still on
  disk under the team name. The resume prompt (rendered by karvex) tells the
  lead: "your teammates from before are gone; the task list is intact; respawn
  what you need and continue." Members' own session ids are in the snapshot,
  so the lead (or the user) can also resume an individual teammate's session.
- **Reproduce loosely** (`workflow run start` on the same version): fresh lead,
  same rendered prompt, new team. Definition versioning already gives this.
- **Capture** (the genuinely new feature, last phase): after a run, the final
  task snapshot *is* an observed graph — subjects, ownership, blockedBy,
  emergent nodes. `workflow capture <run>` proposes a new definition version
  from it, so a run that drifted from the plan can be promoted into the plan.

## 4. Risks and mitigations

| Risk | Mitigation |
|---|---|
| Agent teams are experimental; flag or file formats may change | The two file formats are trivial JSON; projection is isolated in one module with fixture tests; the feature already changed once (TeamCreate removal) without breaking these files |
| Task status lags (documented limitation) | Two-layer status: pane agent detection shows the live truth even when task files lag |
| Team config dir deleted at session end | Snapshot on every observed change, not at the end |
| In-process teammates don't resume | Always force split-pane mode; assert it post-spawn from the team config (`backendType: "tmux"`) |
| One team per session / no nested teams | Exactly matches the model: one run = one lead session. A node cannot be a sub-team — documented limitation, revisit if upstream lifts it |
| Lead permission prompts block unattended runs | v1 runs are attended by design (panes, clicking). Unattended hardening (permission modes, `--settings`) is a later knob |
| Shim gaps (`capture-pane`, `new-window`, layout) | Not used by the teams flow — proven by live teams running in karvex today; keep a shim conformance test pinned to the serviced subset |
| Claude Code version drift on user machines | `workflow.run` preflights `claude --version` ≥ 2.1.224 and the teams flag, and fails with a clear message |
| Folder-trust prompt blocks an unattended lead (`--dangerously-skip-permissions` does NOT skip it) and discards the initial prompt even once answered | Implemented: `workflow.run` preflights `~/.claude.json`'s `projects.<cwd>.hasTrustDialogAccepted` and refuses with the exact remedy; unreadable config counts as trusted |

## 5. Delivery phases

Each phase leaves the tree green (`just check`); demolition comes *after* the
new path works, so at no point is there no working run path.

- **A — Lead-run binding**: render contract, lead spawn, run-row binding
  fields, `run finish` self-report, preflight. New code beside the old engine.
- **B — Projection**: tasks/teams watchers, member snapshots, emergent nodes,
  events. A run started via the new path is fully visible in `kvx workflow
  run show` and the events stream.
- **C — UI/CLI re-point**: DAG + run history read the projection; focus-pane
  affordance; launcher unchanged; CLI trim.
- **D — Demolition**: delete the engine, node contract, pump, obsolete API
  ops; protocol bump; test-suite reshape (engine tests go; render/projection
  fixture tests stay).
- **E — Resume, reproduce, capture**: `run resume`, definition-from-run
  capture, docs.

A/B are one coupled change (the binding writes what the projection reads) and
ship together on one branch based on `integration/oct-fixes` (it contains the
team-config reader and the four engine fixes the old path still needs until D).
