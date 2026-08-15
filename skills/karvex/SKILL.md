---
name: karvex
description: "Control Karvex, a terminal multiplexer for coding agents. Use only when the user explicitly mentions Karvex or asks to use Karvex to inspect or control panes, tabs, workspaces, commands, or another agent. Do not use merely because a task could benefit from a background terminal, delegation, or parallel work. Requires KARVEX_ENV=1."
---

# Karvex

Karvex organizes terminals into workspaces, tabs, and panes, recognizes coding agents running inside panes, and exposes the current session through the `kvx` CLI.

Before issuing any control command, verify that this agent is running inside a Karvex-managed pane:

```bash
test "${KARVEX_ENV:-}" = 1
```

If the check fails, say that you are not running inside Karvex and stop. Do not inspect or control the focused Karvex session from outside Karvex.

When the check passes, the `kvx` binary in `PATH` talks to the current session. Use it to inspect neighboring work, create terminal layout, start agents and commands, read output, and wait for state changes.

## Learn the current CLI

The installed binary is the authority for command syntax. Start with:

```bash
kvx --help
```

Then print the relevant command group by running the group without a subcommand:

```bash
kvx agent
kvx pane
kvx workspace
kvx tab
kvx worktree
kvx terminal
kvx notification
kvx integration
kvx session
```

Do not run bare `kvx` for discovery; it launches or attaches the TUI. Do not probe a mutating nested command by omitting arguments. Commands such as `kvx workspace create` are valid with defaults and will execute.

Most control commands return JSON. Read identifiers and state from those responses instead of predicting them.

## Understand layout, panes, and agents

Choose the primitive that matches the job:

- Workspace, tab, and pane topology organize terminal locations.
- Pane commands control raw terminals, shells, tests, servers, input, and output.
- Agent commands control the recognized coding agent currently occupying a pane.

A pane exists whether or not it contains an agent. `agent start` requires an existing available shell pane and never creates, splits, or moves layout. Use pane commands for ordinary processes. Use agent commands when Karvex must validate agent identity or interpret `idle`, `working`, `blocked`, `done`, and `unknown` lifecycle states.

Agent commands accept either a unique live agent name or the pane ID currently hosting that agent. They do not accept terminal IDs or bare agent-kind labels. Names must match `[a-z][a-z0-9_-]{0,31}` and be unique among live agents. A name follows the current pane occupant and is cleared when that agent exits, is released, or is replaced.

`idle` means the agent is ready for input and its tab has been seen in the focused Karvex UI. `done` is the same underlying idle state after unseen background work finishes. Focusing the tab or targeting the pane or agent with a focus command marks it seen. CLI reads do not mark it seen. `blocked` means Karvex recognized an approval or question UI. `unknown` means an agent is present but Karvex cannot classify it confidently; it does not prove completion.

## Use IDs and caller context

Public IDs are opaque stable handles:

- workspace: `w1`
- tab: `w1:t1`
- pane: `w1:p1`

Closed tab and pane IDs are not reused. A pane moved into another workspace receives a new workspace-qualified pane ID. After `pane move`, continue with `.result.move_result.pane.pane_id` or the live agent name. The old value is reported as `.result.move_result.previous_pane_id`; only the moved process's inherited caller context keeps resolving that old ID, so do not use it as a general agent target.

Karvex injects the caller's context into each managed pane:

```bash
printf '%s\n' "$KARVEX_WORKSPACE_ID" "$KARVEX_TAB_ID" "$KARVEX_PANE_ID"
```

Prefer `--current` when a pane command should target the calling pane. Omitting a target may use the UI-focused pane, which can belong to the user or another client.

Discover live state with:

```bash
kvx workspace list
kvx tab list --workspace "$KARVEX_WORKSPACE_ID"
kvx pane current --current
kvx pane list --workspace "$KARVEX_WORKSPACE_ID"
kvx agent list
```

Creation responses expose the IDs to use next. `workspace create` returns `.result.workspace`, `.result.tab`, and `.result.root_pane`. `tab create` returns `.result.tab` and `.result.root_pane`. `pane split` returns the new pane as `.result.pane`.

## Start and coordinate an agent

Default to a sibling pane in the current tab and the current working directory. Do not create a workspace, tab, worktree, or different cwd unless the user explicitly requests that topology or location.

Honor a direction requested by the user. Otherwise inspect the caller pane:

```bash
kvx pane layout --pane "$KARVEX_PANE_ID"
```

Split a wide pane to the right and a narrow or tall pane down. Avoid repeated same-direction splits that create unusably narrow columns or short rows. Keep the user's focus in the calling pane and explicitly preserve the caller's working directory:

```bash
kvx pane split --current --direction right --cwd "$PWD" --no-focus
```

Replace `right` with `down` when appropriate. Read the new pane ID from `.result.pane.pane_id`.

An available shell pane must be at its interactive prompt, with the shell itself in the foreground and no foreground command, editor, or agent running. Start a supported agent in that pane with a useful unique name:

```bash
kvx agent start reviewer --kind codex --pane <returned-pane-id>
```

Use the kind requested by the user. Run `kvx agent` to inspect the installed kind list and options. Pass native agent arguments only after `--`:

```bash
kvx agent start reviewer --kind codex --pane <returned-pane-id> -- <agent-args...>
```

`agent start` returns only after Karvex detects the expected agent in the same pane and considers it ready for interactive input. It defaults to a 30-second startup timeout.

Submit work through the agent surface:

```bash
kvx agent prompt reviewer "Review the current diff and report only actionable findings." --wait --timeout 120000
```

`agent prompt` atomically submits text and encoded Enter while honoring the pane's live bracketed-paste mode. For normal agent work, `--wait` is enough: it waits for the first settled `idle`, `done`, or `blocked` state. Do not repeat those defaults with `--until`.

A prompt sent from a non-working state must produce an observed lifecycle change within five seconds. Otherwise Karvex returns `agent_prompt_stalled` instead of waiting indefinitely. This wait tracks lifecycle state, not an individual turn; if the agent is already working, completion of the active turn may satisfy it.

Use `--until` only for a state-specific workflow, such as waiting for an already-running agent to request input:

```bash
kvx agent wait reviewer --until blocked --timeout 120000
```

Without `--until`, standalone `agent wait` uses the same settled-state defaults as `agent prompt --wait`.

Use logical keys for interactive agent UI controls:

```bash
kvx agent send-keys reviewer esc
kvx agent send-keys reviewer ctrl+c
```

Karvex validates all keys before writing any bytes. Read the result through the resolved agent:

```bash
kvx agent get reviewer
kvx agent read reviewer --source recent-unwrapped --lines 120
```

If a wait fails or returns `blocked`, inspect `agent get` and `agent read` before deciding what input to send. Use the pane surface only when raw terminal control is intentional.

## Run an ordinary command in another pane

Create a sibling pane with the same geometry rule, preserve the caller's working directory, and keep user focus unchanged:

```bash
kvx pane split --current --direction right --cwd "$PWD" --no-focus
```

Read the new pane ID from `.result.pane.pane_id`, then run and inspect the command:

```bash
kvx pane run <returned-pane-id> "just test"
kvx pane wait-output <returned-pane-id> --match "test result" --timeout 120000
kvx pane read <returned-pane-id> --source recent-unwrapped --lines 120
```

`pane run` atomically sends command text and Enter. `pane wait-output` searches the selected snapshot immediately, so output that already exists can match. Use `--match <text>` for a literal substring or `--regex <pattern>` for a Rust regular expression. Omitting `--timeout` allows an indefinite wait.

Use the read source that matches the task:

- `visible`: the currently rendered viewport.
- `recent`: recent rendered output, including soft wraps.
- `recent-unwrapped`: recent output with soft wraps joined; prefer it for logs and transcripts.
- `detection`: the plain-text bottom-buffer snapshot used for agent detection.

Use `--format ansi` when colors and terminal styling are evidence. Otherwise use text.

`--lines` asks Karvex for more rows from the pane's available screen and host scrollback. If increasing it does not reveal more of a completed response, the pane is probably running the agent on the terminal's alternate screen. Rows that leave the alternate screen do not enter Karvex's host scrollback, so a larger line count cannot recover them.

After that failed read, ask the agent to write its complete response as Markdown in a temporary directory and reply only with the file path, then read the file directly. Use this only as a fallback; do not request file output in the initial prompt.

## Claude Code Agent Teams teammate panes

When Claude Code's Agent Teams feature runs inside a Karvex pane **in a split-pane teammate mode**, Karvex makes itself Claude's tmux backend automatically: it exports a tmux-compatible session identity into the pane and puts a `tmux` shim on `PATH` that Claude's own code talks to. The practical effect is that each teammate Claude spawns shows up as an ordinary Karvex pane in the same tab, not as a nested terminal session — the leader pane keeps focus, and teammate panes are visible and controllable the same way any other pane is.

Two conditions must both hold, and neither is the default:

```bash
# 1. Agent teams are experimental and off unless this is set.
CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS=1
# 2. Teammate mode must resolve to a split-pane backend.
claude --teammate-mode tmux          # or: --settings '{"teammateMode":"tmux"}'
```

Claude Code's teammate-mode default is `in-process` (it was `auto` before Claude Code 2.1.179). In-process teammates run inside the leader's own session and **never become Karvex panes**, and being inside tmux does not change that — the spawn path short-circuits before backend detection. So a launch that does not force a split-pane mode will produce a working team with no teammate panes to read or steer. Verify from Claude's own team config rather than from the flag being accepted, and check the first *teammate* — the leader is always `in-process` with `tmuxPaneId: "leader"`:

```bash
python3 - <<'PY'
import glob, json
for f in sorted(glob.glob("$HOME/.claude/teams/*/config.json")):
    for m in json.load(open(f))["members"]:
        print(m["name"], m.get("backendType"), m.get("tmuxPaneId"))
PY
```

A member reading `tmux` with a `wN:pN` pane id confirms the integration is live.

Treat a teammate pane as a normal Claude Code pane for every read and inspect operation:

```bash
kvx pane list --workspace "$KARVEX_WORKSPACE_ID"
kvx agent list
kvx agent get <teammate-pane-or-name>
kvx agent read <teammate-pane-or-name> --source recent-unwrapped --lines 120
```

Do not assume a teammate pane's existence, title, or lifecycle state — read it from `pane.list`/`agent.list` output the same way you would for any other agent pane. A teammate's pane closes on its own when Claude reaps it (the team ends, or the teammate is killed from the leader); do not close a teammate pane yourself unless the user explicitly asks, for the same reason you would not close any other agent's pane out from under it.

Two environment variables control whether this integration is active for the current session; check them before assuming teammate panes will appear:

```bash
env | grep -E '^KARVEX_NO_(TMUX_COMPAT|AUTO_INTEGRATION)='
# KARVEX_NO_TMUX_COMPAT set (any value, including empty): panes get no tmux
#   identity and no shim install, so teammates never become Karvex panes.
# KARVEX_NO_AUTO_INTEGRATION set (any value): the Claude Code hook is not
#   auto-installed on server start, so teammate panes won't report lifecycle
#   state until the hook is installed by hand.
```

Both are presence-based, so `grep`ping for the variable name is the reliable check — a `-0`/`-false` value still opts out.

If `KARVEX_NO_TMUX_COMPAT` is set, Claude Code falls back to its own non-tmux backends and teammates will not appear as Karvex panes at all — do not spend time debugging a "missing" teammate pane under that condition; report the opt-out to the user instead. The same applies to an in-process teammate mode: absent teammate panes are far more often one of these two opt-outs than a Karvex fault.

## Run a multi-agent workflow

A Karvex **workflow** is a stored, versioned plan: named nodes, their dependencies, and per-node model demands. A **run** of it is one interactive Claude Code **team lead** session in a Karvex pane, driving Agent Teams through the same tmux shim described above. `workflow run` launches that lead, hands it the plan as a rendered prompt, and then only watches: Karvex defines the plan, launches the lead, observes what the team does, and saves the result — it never executes a node itself. The lead creates the shared task list, spawns teammates, and decides retries and completion. Every node that runs is a real Claude session in a real Karvex pane.

Use a workflow when the user wants a repeatable multi-step plan they can rerun, save, and inspect later. For one-off parallel work, split panes and `agent start` directly — a workflow is heavier and stores history.

Inspect and author:

```bash
kvx workflow list
kvx workflow show <name>
kvx workflow create --file <definition.toml>
```

`workflow create` is all-or-nothing: an invalid definition stores nothing. Two rules the validator enforces that are easy to get wrong — a node `key` becomes the prefix of its task subject, and every edge `port` must appear as `{{port}}` in the downstream node's `prompt_template`, or the edge's data has nowhere to land.

Start a run from a pane in the workspace the run should live in:

```bash
kvx workflow run start <name> --arg key=value
kvx workflow run show <run-id>
kvx workflow run list <name>
kvx workflow run cancel <run-id>
```

`run start` preflights two things and refuses rather than launching a lead that cannot work: the installed `claude` must support agent teams, and Claude Code must already trust the run's working directory. An untrusted directory does not merely prompt — Claude's folder-trust dialog **discards the lead's initial prompt**, leaving a healthy session with no plan. If the preflight refuses on trust, have the user open `claude` in that directory once and accept the dialog.

`run show` is the projection of the team's own state, not Karvex's opinion:

```
nodes:
  bugs      succeeded   Find correctness bugs
  style     succeeded   Review style and clarity
  verdict   running     Write the review verdict
  .task/4   pending     finish: write the run summary
members:
  team-lead (idle) — in-process
  bugs (idle, opus) — pane w1:p4
  style (idle, sonnet) — pane w1:p5
```

Planned nodes are matched back to the definition by the `node-id:` prefix on the task subject. Tasks the lead invented appear as **emergent** nodes under `.task/` — that is expected, not an error. `members` maps each teammate to the pane it occupies.

### Steer a running workflow

Read and steer a node exactly like any other agent pane — the pane *is* the interface:

```bash
kvx agent read <teammate-pane> --source recent-unwrapped --lines 120
kvx agent prompt <teammate-pane> "Also check the error paths." --wait
```

Steer the **lead's** pane for anything about the run as a whole: reprioritising, spawning more teammates, or abandoning a node. Find it in `run show` (`lead_pane_id` in JSON output).

Two conditions occur often enough to check for before reporting a run stuck:

- **A permission prompt in the lead's pane blocks the whole run.** Teammate permission requests bubble up to the lead, and the lead waits. `kvx agent read <lead-pane>` shows the dialog; the user answers it, or you do only if the user has authorised that action. Pre-approving the run's expected tools avoids this.
- **A teammate finishes its work but leaves its task `in_progress`.** This is a known Claude Code agent-teams limitation, not a Karvex fault. The node's pane state (`idle`) is the truth; the task file lags. Tell the lead to confirm the result and mark the task complete.

If you remember `kvx workflow node steer`, `interrupt`, `restart`, `complete`, `expand`, or `interrogate` from an older Karvex, stop retrying them: each still parses, but the server now refuses every one of them, naming the pane affordance above instead of executing anything — the per-node engine that used to serve them is gone. `kvx workflow node show` is the one node verb that still answers with real, live data; use it to read a node's state, and use the pane itself to act on it.

### Message a run session

Reach a named session in the run directly, without hunting for its pane:

```bash
kvx workflow run message --to <name> --text "Also check the error paths."
```

`--to` takes the team-roster name (the lead is `team-lead`); pass `--text-file <path>` instead of `--text` for anything long. Called from the lead's own pane, `run message` needs no `--run` — same as `run finish` below. Delivery has two channels: if Karvex has already captured that session's messaging-socket endpoint from its own startup self-report, it hands the message over Claude Code's own peer-inbox socket, subject to that session's own inbound controls; otherwise it falls back to typing the text straight into that session's pane, the same as `agent prompt` would. A teammate's messaging token only ever exists in that teammate's own process environment, so Karvex cannot recover it after the fact — once this Karvex server restarts, the run it was tracking is no longer resident in memory at all, and `run message` refuses outright rather than silently degrading to pane input.

### Watchdog messages

Karvex separately watches for a task a teammate's own task list still marks `in_progress` while that teammate's pane has actually sat idle — Claude Code's own docs note teammates sometimes finish work and never mark the task complete, so a status by itself is never proof of anything. When the disagreement holds for long enough, Karvex says so directly in-session: a message framed `[karvex · watchdog]`, first nudging the idle owner, then re-prompting it more specifically if nothing moved, and — if a teammate still never answers — telling the **lead** instead, with what Karvex measured and the lead's own options (message the teammate, reassign the task, or respawn it; Karvex itself can do none of those three, only watch and report). A lead whose own pane goes idle gets the same nudge/re-prompt pair about the run as a whole, with no third rung, since escalating the lead to itself would be a message to nobody. Recognise the frame as the runtime talking, not the user, and act inside the affected session rather than replying to Karvex on its behalf.

### Self-improvement review (not live yet)

The workflow protocol already has review-cycle methods for turning accepted findings from a past run into a new workflow version, but there is no `kvx workflow review` command yet and no cycle you can start: every one of those methods still answers with a refusal today, because the orchestration that creates and drives a cycle has not shipped. If a user asks for a review cycle, or you remember one from a newer Karvex, say plainly that it is not available on this build rather than guessing at a command.

### Finish

The lead's pane self-identifies: Karvex exports `KARVEX_WORKFLOW_RUN_ID` into it, so `kvx workflow run finish --summary-file <path>` needs no run id when called from that pane. The lead closes its own run this way; Karvex never decides a run is done. Until that call, a run with every task complete is still `running`. The stored summary is then readable with:

```bash
kvx workflow summary list
kvx workflow summary show <run-id>
```

Do not call `run finish` on the user's behalf from outside the lead's pane unless the user asks you to force-close a run whose lead has died.

## Safety and coordination rules

- Use `--no-focus` for background work unless the user asked to switch context.
- Use `--current`, an explicit pane ID, or a unique agent name. Do not rely on another client's focused pane.
- Parse IDs from JSON responses. Do not derive them from sidebar order or examples.
- Do not close workspaces, tabs, panes, or sessions you did not create unless the user explicitly asked.
- Never run `kvx server stop` from an active session unless the user explicitly intends to stop the server and its pane processes.
- Never kill the main Karvex process. Use named test sessions for experiments that need an isolated server.
- CLI server errors are JSON on stderr with exit status 1. CLI syntax errors exit with status 2.
