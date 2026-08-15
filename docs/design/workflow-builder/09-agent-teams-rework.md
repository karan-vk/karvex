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
   makes this rare. **Superseded by §3.1a**, which makes binding an assertion
   the lead's own `SessionStart` hook makes and keeps this rule as the
   documented fallback — with the missing ceiling and a bind deadline.

The lead is interactive on purpose: permission prompts, plan approvals, and
steering all happen in its pane, which is the interaction model Karan asked
for ("I just have to click on the pane").

### 3.1a Identity: the run's sessions say who they are

*Added after §3.1 was implemented, and it supersedes §3.1 step 4's inference
for every lead whose hook fires. Verified live against Claude Code 2.1.232 and
2.1.233 on 2026-08-15.*

Step 4 binds a run to a team by **inference**: a `createdAt` inside a slack
window plus a matching lead cwd. The rework audit found two defects in that
rule and both were real. The freshness window had a floor and no ceiling, so a
session the user starts by hand in the same directory half an hour later stayed
eligible forever; and a run whose team never appeared polled forever, stayed
`running`, and then wedged the single-live-run guard for every later run on
that server.

Claude Code offers a better channel, and karvex now uses it.

**What upstream exports, verified live.** Every session exports
`CLAUDE_CODE_MESSAGING_SOCKET` and `CLAUDE_CODE_MESSAGING_TOKEN` to its hooks
*before any hook runs*, `SessionStart` included, alongside
`CLAUDE_CODE_SESSION_ID` and `CLAUDE_PROJECT_DIR`. A `SessionStart` hook
receives `{"session_id", "transcript_path", "cwd", "hook_event_name",
"source"}` on stdin. Hook entries from a `--settings` payload are **added** to
the hooks in the user's own settings rather than replacing them — the probe's
hook ran as `sessionstart-hook-3.sh`, third of three, and the user's bundled
karvex agent-state hook kept working in the same pane. And the team a session
leads is named `session-` plus the first eight characters of its session id
(session `51ea857f-…` produced `~/.claude/teams/session-51ea857f/`).

**So karvex asks.** `workflow.run` writes a run-scoped `--settings` file into
the run directory carrying three keys — `teammateMode: "tmux"`,
`crossSessionInbound: "accept"` (§3.5a), and a `SessionStart` hook that runs
`kvx workflow run report-session --run <run id>`. The verb reads the payload on
stdin and the endpoint out of its own environment and posts
`workflow.run.report_session`. Binding becomes an **assertion the session makes
about itself**, checked against two identifiers karvex minted: the run id, baked
into the hook command, and `KARVEX_PANE_ID`, which only karvex sets.

Three consequences worth stating plainly:

* **The run id rides in the command, not the environment.** Claude Code
  forwards the *value* of `--settings` to the teammates it spawns (the 2.1.232
  bundle's teammate argv builder re-emits `--settings <value>`), but a
  teammate's pane is created by karvex's tmux shim and carries karvex's base
  environment, not the lead's — so `KARVEX_WORKFLOW_RUN_ID` does not reach a
  teammate and the settings file is what makes a teammate's report land on the
  right run.
* **A file, not inline JSON**, for the same reason: a path forwards cleanly
  through the teammate's respawn argv where a multi-line JSON blob does not.
* **It is a `kvx` verb, not a shipped shell asset.** `assets/claude/…` exists
  because those hooks are *installed* into user settings and must keep working
  across upgrades, which is what the `KARVEX_INTEGRATION_VERSION` migration
  rule governs. This hook is written fresh into the run directory on every
  launch, so there is no installed copy to migrate, no version to bump, and no
  second PowerShell implementation to keep in step.

**The binding rule, in order.** `workflow::binding::identity::decide_binding`:

1. **Asserted** — the lead's own report. The team name follows from the session
   id by the documented derivation, so no team config need exist on disk yet.
2. **Inferred** — §3.1 step 4's two tiers, kept verbatim as the fallback for a
   lead whose hook never fired (hooks disabled, a `claude` older than the
   payload, or a server the hook could not reach), now with the audit's missing
   **ceiling**: a team's `createdAt` must sit within `TEAM_MATCH_SLACK_MS`
   before and `TEAM_MATCH_CEILING_MS` after the spawn instant.
3. **Expired** — past `BIND_DEADLINE` (120 s, and the ceiling is tied to it so
   there is exactly one window to reason about) the run is closed `failed` with
   a `lead_unbound` reason that names the wait and the likely cause. A run that
   cannot bind must not stay `running`: that is what wedges every later run.

**Teammates report themselves the same way**, because they inherit the same
`--settings`. Their reports are keyed by pane id, which is exactly the join the
team config already publishes (`member.tmuxPaneId == KARVEX_PANE_ID`,
`src/pane.rs`), so a teammate's *name* still comes from the team config and only
its endpoint comes from the hook. A report carrying an `agent_id` is an
in-process subagent's and is never an identity assertion.

**What is recorded, and when.** The pane karvex launched is durable from the
instant it exists (`StoreWrite::RunLeadPane`) — karvex launched that lead and
does not have to infer it, and without that write every surface treats the
unbound window as an engine-era run. The session id and team name are written at
binding (`StoreWrite::RunLeadBinding`). The messaging endpoints are deliberately
**not** persisted: a socket path stops being true when its process exits, so a
stored one would be a durable record of something that has stopped being true.

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
  `↵ focus pane · m message owner · hjkl move · esc`. The `m message` verb
  (send into a session's inbox over its messaging socket) was deferred here to
  Phase E, because the session-registry file it depends on is empirically not
  written for our lead launches and a verb that silently no-ops is worse than
  no verb. **Superseded by §3.5a**: the identity plumbing arrived early through
  §3.1a's run-scoped hook, which hands karvex the endpoint without the registry
  — so the verb is back, and it refuses out loud when it cannot deliver.
- **Run history** (`Ctrl+B Shift+B`): unchanged shape; a historical run's node
  detail shows owner, timing, and offers *interrogate* = resume that member's
  session id in a pane (§3.7).
- **Sidebar**: teammate names/colors already work via the team-config reader
  and the accent-token path in the shim.

### 3.5a Messaging: `m` comes back, on Claude Code's own channel

*Added after §3.5 deferred it. It supersedes that deferral. Verified live
against Claude Code 2.1.232 on 2026-08-15.*

§3.5 deferred a `m message` verb because "the session-registry file it depends
on is empirically not written for our lead launches, and a verb that silently
no-ops is worse than no verb". The second half of that sentence still governs
everything here. The first half was answered from a different direction: karvex
no longer needs the registry, because §3.1a's hook hands it the endpoint
directly — which is the *only* way a teammate is ever reachable, since teammates
never register in `~/.claude/sessions/` at all and their
`CLAUDE_CODE_MESSAGING_TOKEN` therefore exists nowhere but their own hook
environment.

**The channel.** Each session binds a per-session Unix socket and exports its
path; the protocol is newline-delimited JSON, and Claude Code's own startup log
prints the recipe. Verified end to end: `{"type":"auth","token":…}` then
`{"type":"user","message":{"role":"user","content":…}}` into a live session's
socket started a turn there, and the receiving Claude acted on it — the
transcript shows it wrapped as *"Another Claude session sent a message: …"*.
Three details are load-bearing:

* **Every frame carries `session_id`.** A frame whose id is not the receiving
  session's is dropped (tested with a bogus uuid: never delivered), so a socket
  path recycled to a different process fails closed instead of steering a
  stranger.
* **The auth frame is optional on Linux and required on Windows.** karvex
  always sends it when it holds a token and never invents one, because a bad
  auth frame is a reason to drop the connection where auth is required.
* **karvex is the lead's parent, not its child**, so Claude Code's own-child
  check never fires for it and a karvex message is an ordinary peer message,
  subject to the receiver's inbound controls. The run's `--settings` therefore
  sets `crossSessionInbound: "accept"`, upstream's documented knob for exactly
  this case, so delivery does not depend on the lead's permission mode.

**The surface.** `workflow.run.message` takes a run id, a target named by the
same roster `workflow.run.get` publishes (`team-lead` is the lead), the text,
and Claude Code's own `now|next|later` priority. It answers a receipt naming the
**channel** that carried it. The CLI is `kvx workflow run message`; the DAG's
`m` composes one for the session that owns the selected node, falling back to
the lead for an unclaimed task, which is who would assign it anyway.

**Two channels, never conflated.** `inbox_socket` is the peer channel above.
`pane_input` is karvex typing into the session's pane, which is
indistinguishable from the user typing and is what remains when karvex holds no
endpoint — a session whose feature flag had not resolved when its hook ran, or
any session of a run that outlived a karvex restart. They are not equivalent, so
every delivery is journalled with the channel that carried it
(`RunEventKind::MessageDelivered`); after a restart that journal is the only
record of how a run was steered.

**Honesty rules, which are the point of the verb existing at all.**

* A refusal is always a named error, never a silent no-op:
  `workflow_run_message_refused` carrying the reason — no live run, nothing has
  identified itself yet, no session by that name (with the roster that does
  exist), that session reported no socket, or this machine cannot message at
  all.
* Only two facts refuse before trying: a `claude` older than 2.1.224, and
  native Windows, where upstream offers no cross-session messaging.
* A **kill-switch variable is a warning, never a gate.** The four documented
  variables (`CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC`, `DISABLE_TELEMETRY`,
  `DO_NOT_TRACK`, `DISABLE_GROWTHBOOK`) only disable messaging on an account
  whose feature flags have never been fetched. Probed live on an account with
  them cached, `DISABLE_TELEMETRY=1 claude` changed nothing — peer address
  still shown, `/list-agents` still working, socket and token still exported.
  So karvex reports `supported: true` with
  `reason: "kill_switch_suspected"` and tries anyway. It says what it saw and
  refuses nothing over it, and the authoritative answer is post-launch and
  evidence-based: whether the session reported an inbox socket.
* The receipt says *handed over*, not *delivered*. Upstream's inbound controls
  decide between delivered, held, and refused after the write, and that verdict
  travels only to another session's reply address, which karvex does not have.

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
