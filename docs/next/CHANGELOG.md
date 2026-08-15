# Changelog

## Unreleased

### Added

- Karvex watches a workflow run for stuck teammates again, and this time it says so out loud instead of restarting anything. Every `workflow.watchdog_tick_secs` (default 20s) each `in_progress` task — and the lead itself — is classified from what Karvex's own screen detection reports about the owner's pane: a pane that is working, or that moved since the last sample, resets; a pane that has been idle while its task still says `in_progress` is the disagreement worth acting on, since Claude Code documents that teammates sometimes finish work and never mark the task completed. After `workflow.stuck_threshold` no-progress samples Karvex nudges the teammate, then re-prompts it with the exact lag, then tells the *lead* — the only actor that can reassign or respawn — and finally surfaces the node as `attention: stuck` and stops talking. Nothing is ever killed and the projected task status is never overwritten; `attention` is Karvex's own column beside it, re-evaluated every sample, so a node that starts moving again clears it. Messages are framed `[karvex · watchdog]` and go out on the same inbox-socket-or-pane channels `kvx workflow run message` uses, and a rung that reached neither channel is journalled as undelivered and retried rather than counted as said — a nudge nobody received must never look like a nudge that was ignored. A task waiting on a human or on an unfinished task is surfaced (`needs_input`) and never nudged, a blocked lead surfaces as `lead_blocked` for every member, a task with no owner Karvex can see skips to the lead and surfaces as `unbound`, and a badly overrun authored `timeout_ms` surfaces as `budget_exceeded` without walking the ladder at all. `workflow.stuck_threshold` and `workflow.drift_threshold` — published, defaulted, and read by nothing since the execution engine was removed — are what these two windows are, under their original names and defaults. `workflow.watchdog_enabled = false` turns off classification, messages, samples, and writes together.
- `attention` on a run node is now populated on `workflow.run.get`, `workflow.node.get`, and the `workflow.node.watchdog` event. The field shipped with protocol 20 reading `None` on every node because nothing wrote the column; the watchdog is that writer, and the reader was hardcoded to `None` beside it.
- `kvx workflow review start <run_id>` closes the loop the watchdog bullet above opens: a finished run can review itself. Karvex revives each interviewed teammate's Claude Code session with a forked resume (`claude --resume <session id> --fork-session`), spawned into its own pane on a brand-new session id and transcript that leaves the original completely untouched, puts its own measured record to it — the tasks it owned, how they went, any watchdog nudges it actually received, quoted verbatim — and asks it to answer for itself. Attribution is enforced, not requested: a finding is a resumed teammate's own account only when its interview was actually planned resumed, genuinely answered, and Karvex holds a provenance row linking the two; every other path — no session id was ever captured, the interview stalled on a permission prompt, ran out of time, or lost its pane before answering — degrades to `evidence_only`, and every surface that shows a finding (`kvx workflow review show`, the TUI overlay, the compiled version's own `change_summary`) carries that distinction plainly rather than flattening every finding into one undifferentiated list. Every member is a candidate, ranked most-troubled-first by what Karvex measured, capped at `workflow.review_max_interviews` (default 6); anyone beyond the cap is skipped outright, not degraded. A synthesis pane proposes one finding per definition node — a `prompt`/`structural` level, a `keep`/`improve`/`replace` verdict, a rationale, and its evidence — and `kvx workflow review apply <run_id> (--accept <node_key>... | --decline-all)` decides: a declined finding leaves no trace, and accepted findings compile together or not at all, held to the same authoring standard a hand-written definition passes, into one new **immutable** version tagged `origin: self_improvement` and parented at the run's own version — the version the run actually executed is never edited. `kvx workflow review show|answer|report` round out the CLI (`answer`/`report` are what the interview/synthesis panes themselves run, not something typed by hand), and `V` in the DAG view — or `keys.open_workflow_review` (default `prefix+shift+v`) from anywhere — opens a review overlay with a passive header ask (`review available`/`reviewing…`/`review ready`), a per-finding accept toggle, and a two-step apply/decline confirm.
- `kvx workflow run message [--run <run_id>] --to <name> (--text <text> | --text-file <path>) [--priority now|next|later]` and the DAG view's `m` key finally get their own line here: both send a message to a run's own Claude Code sessions (a member name, or `team-lead`) over the same cross-session-messaging-or-typed-into-the-pane channel the watchdog uses, journalled the same honest way. Neither is new work — the channel has existed since the identity/messaging binding landed — but nothing had described it as a user-facing command until now.

### Changed

- Bumped the client/server protocol version to 20. The engine removal above already made six `workflow.node.*` methods answer a documented refusal instead of what protocol 19 promised, and protocol 19 is published in both the stable and preview channels, so the version was owed independent of anything new. The same bump also lands additive wire surface for an upcoming anti-stuck watchdog and self-improvement review cycle — `workflow.review.{start,get,apply,answer,report}`, `workflow.node.watchdog` and `workflow.review.{started,ready,closed}` events, `attention` on run nodes, and `session_id`/`last_state` on run members — every method and field of which answers honestly today: the five review methods are stubs that answer `workflow_review_not_found` rather than `not_implemented`, and the new fields read `None` because nothing writes them yet. `run_node.watchdog_interventions` also stops being hardcoded to `0` on the durable run projection and instead reads what was actually journalled, even though nothing journals a nonzero value before the watchdog itself ships.
- A workflow run is now one Claude Code team-lead session in a Herdr pane, not a graph Herdr executes itself. `kvx workflow run start` renders a definition version into a lead prompt, spawns an interactive `claude` with agent teams forced on and split-pane teammates forced on, and then only watches: the lead creates Claude Code's own shared task list, spawns teammates as their own Herdr panes, and decides scheduling, retries, reassignment, and completion through its own judgment rather than through anything Herdr enforces. A small server-side projection reads the team's task and member files every two seconds and turns them into the run's records — tasks are matched back to definition nodes by a `<node-id>:` subject prefix, and anything the lead created that matches no node is recorded as an *emergent* node rather than dropped, since the lead is free to split, merge, or add work the plan didn't ask for. `kvx workflow run finish` (the lead's own end-of-run self-report) replaces the entire old summariser subsystem; there is no engine-judged verdict any more; a run's status becomes `succeeded` the moment `finish` is called regardless of what actually happened, and the truth — what happened, and whether it worked — lives in the summary's `outcome` word and text. This removes ~16k lines of custom scheduler, node contract, and growth-guardrail machinery (`workflow/engine/**`, `binding/{observe,interrogate,spawn}`'s node-contract half, `app/workflow.rs`'s engine half) that predates agent teams becoming a viable execution substrate; none of it has a like-for-like replacement, because none of it is Herdr's job any more.
- `kvx workflow node steer|interrupt|restart|complete|expand|interrogate` and the matching `workflow.node.*` methods now answer `workflow_node_verb_retired` instead of doing anything: a node is a real teammate's own pane now, so steering it is opening that pane and typing, and steering the run as a whole is opening the lead's pane the same way. The CLI still parses all six verbs so a script that calls one gets a message naming the pane-based replacement instead of a bare parse error; `workflow.node.get`/`kvx workflow node show` are unaffected and read the same projection `run show` does. `workflow.node.expand`'s growth guardrails (`expand_max`, `max_depth`, `max_nodes`) and `output_schema`-validated completion go with it — a document that still authors `output_schema`, `is_template`, `expand_allow`, `expand_max`, `timeout_ms`, `isolation`, `max_attempts`, or `runner = "command"` on a node still loads and still authors, but none of it reaches the lead or changes what a run does.

### Fixed

- `workflows.mdx` (and its Japanese and Simplified Chinese translations) now describes the shipped agent-teams model — what launches a lead, how the DAG view and run browser project Claude Code's own task and team state, how you steer a run by opening a pane, and how a run finishes — instead of the removed custom execution engine's node contract, `node complete`, schema-validated completion, and growth guardrails. It also now says plainly that `workflow.retention_runs` is a published config key with nothing left to call the pruning code that implements it (the old engine's end-of-run epilogue was its only caller), and that v1 runs are attended by design — a lead's permission prompts and plan approvals happen in its own pane, and there is no unattended mode.
- `kvx workflow run start --restore-from <run_id>` and the run browser's `r` restore action no longer silently discard what they resolve. `app/api/workflows.rs` used to resolve a restore request into node checkpoints and seed assignments and then drop them on the floor (`let _ = (&assignments, &seeds, &context_runs, &restore_from_run);`), so a "restore" quietly started an ordinary fresh run while still looking like a restore — prior-run summaries reached the lead either way, which is exactly what made the gap easy to miss. The resolved selection now renders into the lead prompt under "Restored from a previous run", naming what actually carried forward per task (summary, checkpoint result, artifacts); a selector that could not be restored — no checkpoint, a truncated payload, or a changed prompt/schema without `--restore-allow-changed` — is named in the restore report the CLI prints and is simply planned fresh, rather than the request either failing silently or the lead having to guess.
- Claude subagents now show their own name instead of a hex session id. Session names were resolved only from Claude Code's per-process session registry (`<claude dir>/sessions/*.json`), which Claude Code writes for interactive sessions alone, so a team of subagents rendered as a row of indistinguishable short session ids. Karvex now also reads the team registry (`<claude dir>/teams/*/config.json`), where a subagent's name is recorded against the Karvex pane it occupies, and falls back to it when the session registry has nothing for a session. The session registry still wins where it has a name. Only members that are still marked active and that occupy a real pane are believed, and only from a team config that has been written since this Karvex came up — public pane ids are handed out by the running server, so a config older than it cannot be describing its panes, whatever flags a hard-killed host left behind. Where two teams both claim a pane, the more recently changed one wins. The name reaches `agent_session.name` on `pane.get`, `pane.list`, `agent.get`, and `agent.list` exactly as an interactive session's does, and an unresolved session still falls back to a short session id.
- `kvx workflow create` no longer burns the name when the definition is rejected. The graph validators — cycles, duplicate node keys, edges naming a node that does not exist — used to run only after the workflow row had already been written, so a rejected create left an empty, version-less workflow squatting the name with no `workflow delete` to clear it, and the retry failed with a raw SurrealDB index message. The whole definition is now validated before anything is written, so a rejected create leaves no trace; a genuine name collision is refused with a new `workflow_name_taken` code and a message meant for a human; and a version-less row left behind by an earlier release is adopted by the next create under that name, so names already burned by 0.12.0 become usable again. A `workflow.version.create` that fails validation likewise no longer rewrites the workflow's description or default tier to describe a revision that was never written.

## [0.12.1] - 2026-08-15

### Fixed

- Claude Agent Teams teammates now reliably spawn as Karvex panes on Linux and macOS instead of quietly falling back to in-process teammates. Karvex prepends its `tmux` shim directory to every managed pane's `PATH`, but the pane's own shell startup gets the last word: `path_helper` on macOS, `brew shellenv`, `fish_add_path`, mise, asdf, nix and a plain `export PATH="...:$PATH"` all re-order `PATH` afterwards, and measurably demote that entry — on a stock fish setup the directory Karvex passed as `PATH[0]` came out at index 9. Any real `tmux` in a directory that jumped ahead (Homebrew's prefix is the common one) then won the lookup, Claude Code ran it against Karvex's socket, and the failure was invisible because Claude Code falls back to in-process teammates on a backend error. When another `tmux` is on `PATH` at all, Karvex now also installs the shim next to its own binary — the `PATH` directory that makes `kvx` runnable by name, found by canonicalizing `<path entry>/kvx` against the running executable so symlinked Homebrew, Nix and `~/.local/bin` installs all recognise themselves. When nothing could shadow the shim it still writes nothing outside its own data directory. That copy is never placed in a package manager's prefix, never replaces a file or link Karvex does not own, never creates a directory, and is never written by a binary that is not installed on `PATH` by name, so a `cargo nextest` run or a `cargo run` build cannot touch a developer's `~/.local/bin`. An existing one is repointed at the current binary on every install pass, so an upgrade cannot leave a dangling `tmux` behind. When the competing `tmux` comes first anyway — or Karvex lives in a package manager's prefix — Karvex installs nothing and logs one warning naming it.
- The tmux-compat shim no longer answers commands aimed at a real tmux server. It resolved its socket from `$KARVEX_SOCKET_PATH` first and otherwise trusted `$TMUX` outright, so a `tmux` invocation made *inside a real tmux session* — including one started inside a Karvex pane, where `$KARVEX_SOCKET_PATH` is still inherited — could be serviced out of Karvex's own pane tree instead of reaching the tmux it named. `$TMUX` is now the authority on which multiplexer a process is inside, and a socket it names is only serviced when it is demonstrably Karvex's own; everything else passes through to the real `tmux`. `$KARVEX_SOCKET_PATH` on its own still resolves when `$TMUX` is unset.
- A pane's `PATH` is now split and rejoined with the platform's own separator rather than a hard-coded `:`, and a shim directory the pane's shell demoted is moved back to the front instead of having a second copy stacked on top of it.

## [0.12.0] - 2026-08-10

### Added

- Every finished run — succeeded or failed — now gets an end-of-run summary, written by a real summariser node you can watch. The summariser is an epilogue rather than a graph node: it starts only once the run's terminal status is decided, so `workflow.run.finished` fires exactly when it always did and a new `workflow.run.summarized` event follows when the summary lands. It runs under the reserved instance path `.summary`, is visible in the DAG view (the run's status line reads `· summarising…`, then `· summary failed` if it gave up), and is excluded from `nodes_total`/`nodes_done`, which count the run's declared work. Every failure mode — output that fails its schema twice, a spawn failure, a dead pane, a cancelled run — converges on giving up: journalled, notified once, pane closed, and the run's own status untouched. Read summaries back with `kvx workflow summary show <run_id>` and `kvx workflow summary list [<name|id>] [--limit N]`, or `workflow.summary.get`/`workflow.summary.list` on the JSON API. Turn the summariser off with `workflow.summary_enabled = false`, or bind it to a command of your own with `KARVEX_WORKFLOW_SUMMARY_COMMAND` (a JSON array of argv strings; an unparseable value disables summaries for that server and says so, rather than silently running `claude`).
- A new run of a workflow now starts with what its recent runs left behind. The most recent summaries — `workflow.history_context_runs`, default 3 — are written to `<run dir>/context/prior-runs.md`, and every node's `task.md` gets a two-line `## Prior runs` section pointing at it, so a node that does not need history pays two lines of prompt instead of several thousand characters. A run with no history renders its `task.md` byte-identically to before. Opt out per run with `kvx workflow run start --no-prior-summaries` or `include_prior_summaries: false`; which runs were offered is recorded on the run as `context_runs`.
- A run can now start with some of its nodes already done, seeded from a past run's checkpoints: `kvx workflow run start <name|id> --restore-from <run_id> [--restore <selector>]... [--restore-allow-changed]`. A bare `--restore-from` restores every restorable node. A restored node is `restored` — it carries the source run's result, fires its outbound edges exactly like a succeeded node so downstream nodes receive that payload, and never gets a pane, since nothing runs; its timestamps are the restore instant and its provenance (source run, node key, checkpoint) is on `kvx workflow node show`, the JSON API, and the DAG view. Restorability is decided per node by comparing the source and target versions' `prompt_template` and `output_schema`, so restoring into an edited workflow re-runs only what actually changed. Starting with `--restore-from` is a successful start carrying a report — each node restored, or skipped as `definition_changed` (bypassable with `--restore-allow-changed`), `no_checkpoint`, or `payload_truncated` — while a selector naming no node in the target version is refused with `workflow_restore_unknown_selector` and creates no run at all.
- A past node's Claude session can now be interrogated: `kvx workflow node interrogate <run_id> <path> [--reconstructed] [--note <text>]`, or `i` on a node in the DAG view. Karvex forks the node's session (`claude --resume <session> --fork-session`) into a new pane in the directory the node ran in, so the source transcript is never modified and the same node can be interrogated again later; the pane holds no node token, so an interrogation can never report, complete, or expand on the original node's behalf. When the session cannot be revived — a `command` node that never had one, a transcript that is gone, a working directory that no longer exists — the answer is `workflow_transcript_unavailable` naming which, and no pane is created. `--reconstructed` (`Shift+I` in the DAG view) then seeds a fresh session from the node's stored checkpoint in a pane labelled `reconstructed`, offered explicitly rather than substituted. Interrogations are not run nodes: they never enter the graph or the counters, and they render in their own lane below it.
- `prefix+shift+B` opens a run browser in the TUI: every run this server knows about, newest first, across every workflow, with a detail strip for the selected run and its summary. `Enter` opens a run in the DAG view — live if it is still running, read-only from history otherwise — and `r` restores it into a new run behind a confirmation. Pruned runs appear too, dimmed and tagged, with the fixed line `history pruned — restore and interrogation unavailable`. Rebind it with `keys.open_workflow_runs`.
- Run history is now retained per workflow (`workflow.retention_runs`, default 50). Older runs are pruned whole — the run, its nodes, its checkpoints, and its journal — while their summaries are kept forever, so a pruned run still appears in `kvx workflow summary list` (flagged `run_pruned`) and in the run browser, and `kvx workflow run show` and restore refuse it with `workflow_run_pruned` whose message points at `kvx workflow summary show`. Pruning runs after a run closes and its summary resolves, never on a read path.
- `workflow.run.list` can now list across every workflow (`workflow_id` is optional), and `WorkflowRunInfo` carries `workflow_name`, `context_runs`, and `restore_from_run`, so a cross-workflow run list can label its rows without one extra call per row. `WorkflowRunNodeInfo` gains `transcript_path` and `restored_from`, and three new event kinds — `workflow.run.summarized`, `workflow.interrogation.started`, `workflow.interrogation.ended` — are subscribable. All of it is additive; the protocol version is unchanged.
- The sidebar's agent rows now show the agent's own name for its current session, so several Claude sessions running in one workspace no longer render as identical rows. Karvex resolves the name from Claude Code's live session registry (`<claude dir>/sessions/*.json`, honouring `$CLAUDE_CONFIG_DIR`), re-reading it every five seconds so renames and auto-naming keep up, and only when some pane actually reports a session id. A session with no name yet falls back to a short session id, and panes with no agent session are unchanged. The value is a new `session` sidebar token, added to the default agent rows as `["agent", "session"]`, so it can be moved, styled, or dropped like any other token. `pane.get`/`pane.list` expose the same name as an optional `name` field on `agent_session`.

### Changed

- `maxLength` is now enforced when a node's `result.json` is validated against its `output_schema`. The validated subset was `type`, `required`, `properties`, and `items`, so a `maxLength` a workflow author declared was accepted and ignored. A result whose string is longer than its declared `maxLength` now gets the same treatment as any other schema failure — one corrective re-prompt, then `needs_attention` — where earlier releases let it through. A node that declares a `maxLength` it did not mean should drop it from the schema.
- Every `kvx workflow` command now takes `--json`, including `run cancel`, `run list`, `node steer`, `node interrupt`, `node restart`, and `workflow list`, which previously did not parse the flag at all even though the docs described it as available throughout. Under `--json` a refusal prints the raw error envelope instead of the humanized message, and the exit code is unchanged either way. `node complete` is deliberately excluded: it is the node-side reporting verb, whose contract is its exit code and its environment.

### Fixed

- A run left non-terminal by a server restart is no longer reported as still running forever. Runs found `pending`, `running`, or `paused` when the workflow store opens are now marked `failed` with the reason `interrupted`, and their unfinished nodes `cancelled`, so run history tells the truth about a run whose panes died with the server — and the run browser's restore is exactly the recovery it offers for one.
- A truncated expansion now reports the ceiling it actually hit. A rejection caused by a node's own `expand_max` carried no limit value, so both the live report and the journal-derived one said `limit_value: 0` instead of the node's configured maximum.
- A run's journal entries are now stamped by the engine that produced them rather than by the store when the queued write is applied, the same fix `workflow_run.started_at` got in 0.10.x. Every timestamp derived from the journal — growth limits today, run summaries and restore provenance now — previously drifted from the live value by however long the write queue was, without bound under a backlog.
- A server started with a `KARVEX_SOCKET_PATH` override no longer shares session state with the default session. Its state directory now follows its own socket — `session.json`, `session-history.json`, the log files and the tmux shim directory all live in the directory holding that socket — which is where the default and named sessions already keep theirs. Previously an overridden server fell back to the default config directory, so it restored the default session's workspaces into itself at boot (respawning that session's real agent processes) and overwrote the same `session.json` on every save, including the final one on `kvx server stop`; concurrent servers also interleaved into one shared `karvex-server.log`. The default single-server workflow is unchanged, as is `--session <name>`, and a pane's inherited `KARVEX_SOCKET_PATH` still resolves to its own server's directory. Automation that sets a custom socket and wants its old state should move `session.json` next to that socket.

## [0.11.0] - 2026-08-09

### Added

- Managed panes now export a tmux-compatible identity (`TMUX`, `TMUX_PANE`) and get Karvex's own `tmux` shim prepended to `PATH`, so tools that detect tmux by its presence — notably Claude Code's Agent Teams mode — work inside a Karvex pane. The shim lives at `<data_dir>/shims/tmux`, symlinked to Karvex's own binary, and is mirrored into `~/.local/bin/tmux` on macOS when that directory already exists, so it wins over a Homebrew `tmux` on `PATH`; shim installation gates the export, so if it fails, or on Windows, pane env is left unchanged. Set `KARVEX_NO_TMUX_COMPAT=1` to opt out; the opt-out is checked before the install runs, so it also prevents the shim from being created in the first place. There is no `kvx uninstall`; removing the shim means deleting `<data_dir>/shims/tmux` and, on macOS, `~/.local/bin/tmux` if it points at Karvex.
- The `tmux` shim translates the narrow command surface Claude Code's teammate backend uses onto Karvex's own pane API, so each teammate Claude spawns becomes a native Karvex pane instead of a nested tmux session: `display-message` answers `#{pane_id}`/`#{window_id}` and the `#{client_control_mode}`/`#{client_termtype}` startup probes, `list-panes` enumerates a tab's panes leader-first in creation order, `split-window` creates the pane (converting tmux's "size the new pane" percentage into Karvex's "share kept by the existing pane" ratio, and honouring `-d` by leaving focus with the leader), `respawn-pane` waits for the new pane's shell to settle and then submits the teammate command into it, `select-pane -T` renames the pane, `kill-pane` closes it, and `send-keys` types into it; `set-option`, `select-layout`, `resize-pane` and `show-options` are accepted so Claude's styling and rebalance calls succeed. Anything outside that surface — a named-socket `tmux -L` invocation, a `-S` socket that is not this session's, or plain interactive `tmux` — is passed through to a real `tmux` found later on `PATH`, so existing tmux use keeps working. The shim talks only to the socket it resolves from `KARVEX_SOCKET_PATH` or `TMUX`, never to the default session, and bounds every request at 1500ms so a stopped server surfaces as a fast, plain `no server running` rather than a hang; `tmux -V` keeps succeeding even with no server, so Claude's availability check still passes.
- Karvex now installs, or refreshes, the Claude Code hook integration automatically on server start whenever it is missing or outdated, so agent tracking for Claude panes works out of the box without running `kvx integration install claude` by hand. Set `KARVEX_NO_AUTO_INTEGRATION=1` to opt out.
- Claude Code Agent Teams teammate accent colours — set through tmux `set-option` (`window-style`/`pane-border-style`/`pane-active-border-style`) — are now reported through `pane.report_metadata` as an `agent_accent` token and tint the teammate's name in the sidebar's agent panel.

### Fixed

- A Claude Code Agent Teams teammate pane is now recognized as a Claude pane. Claude's native installer keeps version-pinned binaries at `<data dir>/claude/versions/<version>` and puts a `claude` symlink to the active one on `PATH`, so a hand-started Claude is identified by its `claude` argv[0] — but teammates are launched by that absolute versioned path, giving them a bare version string like `2.1.226` as their process name. Teammate panes consequently reported no agent at all: no lifecycle state, no entry in `kvx agent list`, and no row in the sidebar's agent panel, even though their hook was reporting a session correctly the whole time. Karvex now recognizes the `claude/versions/<version>` install layout specifically; a numeric process name anywhere else is still never treated as an agent.
- Terminal passthrough sequences (`\ePtmux;…\e\\`) are now unwrapped at the top of a pane's inbound byte stream instead of being dropped, so OSC 52 clipboard writes and OSC 11/OSC 4/XTGETTCAP colour and capability queries made from inside a tmux-aware app (neovim, fzf, yazi, lazygit, tmux-aware prompts) keep working once a pane exports `TMUX`. Unrelated DCS strings still pass through byte-identical. This is a prerequisite for the tmux-compat work above, not an independent feature.

## [0.10.2] - 2026-08-09

### Fixed
- The `events.subscribe` stream now delivers every subscribed event for a connection in one global order instead of one event per subscribed type per 100ms poll pass. A per-type cursor previously let `workflow.run.finished` drain ahead of the `workflow.node.*` events it summarised whenever a backlog built up (an 11-event `node_created` backlog took over three seconds to catch up to a one-event `run_finished` type); a single cursor over the event hub's own sequence now walks the backlog in the order it actually happened, so a node's `created` always precedes its own `updated`/checkpoint events, and a run's `finished` always follows its nodes' events.
- `workflow.run.updated` now fires when a run's node set grows, coalesced into one event per batch of newly materialised nodes and always ordered before that batch's `run.finished`. It previously fired only from `pause`/`resume`, so a subscriber tracking `nodes_total` had no way to see a run grow.
- `growth_limited` survives a server restart. `workflow.run.get`, `workflow.node.get`, and `workflow.run.list` read it back from the run's own journalled `growth_limited` events instead of always reporting `null` once the run was reloaded from the store, even though the live engine had reported it correctly the whole time.
- `parent_path` survives a server restart the same way: a run node's spawn provenance is resolved from its persisted `parent` column instead of being dropped on the durable read path.
- A node stuck in `needs_attention` — a failed spawn, an unrecoverable schema failure, a missing kvdag definition, or sustained idle with no result — now carries a blocker reason and a resume condition naming the concrete next command, on `kvx workflow run show`, `kvx workflow node show`, and the JSON API. The reason and resume condition were only ever recorded for `Succession::Blocked`; `needs_attention` set the node's status without ever setting its succession, so the fields these commands already knew how to render always came back empty. A node that recovers by the route its own resume condition names — steer it until it writes a valid `result.json` — sheds the blocker when it succeeds, rather than carrying a stale `resume:` line and a `succession` of `blocked` for work that is already done.
- `kvx workflow run cancel` on a run that has already closed is now refused with `workflow_run_closed`, the same guard `steer`, `interrupt`, and `restart` already use, instead of answering `ok` with a `workflow_run_cancelled` envelope for a run whose status never actually changed.
- The growth clock in `kvx workflow run show`'s `growth:` line and `kvx workflow node show`'s `growth_limited:` line now names `UTC` explicitly, matching every other timestamp `kvx` prints, instead of a bare `HH:MM` that reads as local time on whatever terminal it lands in.
- `kvx workflow run show` now prints a `limits:` line reporting the run's enforced `max_nodes`/`max_depth` ceilings. They were previously `--json`-only, contradicting `workflows.mdx`'s claim that "what the run enforces is what `kvx workflow run show` and the JSON API report."
- `kvx …` piped into a reader that closes early (`kvx workflow run show <id> | head`) now exits quietly on the broken pipe instead of panicking with a backtrace. The print-and-exit subcommands now restore the default POSIX `SIGPIPE` disposition Rust replaces with `SIG_IGN`; a piped command now exits `141` rather than `0` or a panic — the correct behavior for a CLI, but worth knowing if a script checked the previous exit code. `kvx server`, the TUI, `kvx client`, and the interactive attaches (`kvx agent attach`, `kvx terminal attach`, `kvx terminal session …`) deliberately keep `SIG_IGN`, because they need a closed pipe back as an `io::Error` they can report rather than dying mid-write with the terminal still in raw mode.
- The TUI workflow launcher's workflow list now moves on `j`/`k` as well as the arrow keys, matching the DAG view. Every non-control character key was previously routed to the argument text input and silently swallowed when focus was on the list instead.
- The Japanese and Simplified Chinese `workflows.mdx` translations now describe the fan-in design that actually shipped in 0.10.1 — one file per contributor at `inputs/<port>/<source>.json`, an ordered index at `inputs/<port>.json`, and one attributed block per contribution in the rendered `{{slot}}` — instead of the pre-0.10.1 "renders as a JSON array" description, and now include the `--input` override-precedence sentence and the `--label` paragraph that were missing from both locales.

## [0.10.1] - 2026-08-08

### Fixed
- `kvx workflow node expand --input KEY=VALUE` is no longer validated and then discarded: the override now reaches the child's rendered `task.md`, matching what `workflows.mdx` and the v0.10.0 changelog already claimed.
- `kvx workflow node expand --label <text>` — a required flag — is no longer discarded either. An expansion child is now named by the label its proposing node gave it on every surface that names a node: its `task.md` title, its pane title, its `claude --name`, and its box in the DAG view. The label was resolved per kvdag key instead, so every child cut from one template wore that template's own label (a fan-out of `Worker`, `Worker`, `Worker`, …) and a node could not tell its own children apart.
- A downstream node's inbound port that inherits edges from more than one expansion child no longer collapses to one last-writer-wins payload. A port with two or more settled inbound edges now writes one file per contributor at `inputs/<port>/<source>.json`, keeps the ordered index of them (in child-creation order) at `inputs/<port>.json`, and renders its `{{slot}}` as one attributed block per contribution, so a fan-in node receives every child's result and knows whose is whose instead of silently keeping only the most recently settled one. A port with exactly one settled edge is unaffected, on disk and in the prompt.
- The DAG view's `hjkl`/arrow-key navigation can now reach nodes an expansion proposal created. `Down`/`Up` now rank a node's graph successors/predecessors by rendered row distance before horizontal distance, so the row an expansion appends is adjacent again instead of being skipped in favor of a node one graph layer further on — `Enter` and `s` were previously unreachable for exactly the nodes dynamic growth creates.
- The DAG view's detail strip now carries the selected node's growth limit in full. A node that proposed into a guardrail carried the limit only in its box's fixed title row, which elided it to a fragment, while the strip below — the one band wide enough to render it whole — showed path, status, model, usage, and result and said nothing about the limit at all, so the guarantee that a refused proposal is never silent rendered without conveying which guardrail was hit or how much of the proposal survived. The notice now sits beside the node's path and status there, without changing the overlay's fixed detail height.
- A run node's label is now readable over the JSON API rather than only inside the TUI. `workflow.run.get` and `workflow.node.get` carry a `label` for every node — an expansion child's accepted `--label`, or the authored kvdag label for a static node — and `kvx workflow run show` and `kvx workflow node show` print it. Every node of a generation shares its template's `node_key`, so without this the CLI named a whole fan-out `worker` while the DAG view named its children apart.
- An `expand_allow` authoring failure — a key naming a node that does not exist, or one that is not a template — now reports the `workflow_invalid_definition` error code, the same one a definition error caught earlier in validation already uses, instead of the generic `workflow_store_error` a genuine store failure also uses.
- `prefix+shift+F` with no run executing is now documented as falling through to the workflow launcher, rather than reporting a dead end — its actual, deliberate behavior since the launcher shipped, which `workflows.mdx` had not caught up to.
- `kvx workflow run show`'s `growth:` line is now documented to match what it actually prints: the run's current growth state and which guardrail was last hit, without the requested/accepted counts for that rejection. Those counts are documented where they actually live — `kvx workflow node show`'s `growth_limited:` line and the DAG view's banner and per-node notice.
- `ui.toast.delivery`'s config reference now says what it does and does not gate for workflow notices. It gates the popup form only: under the default `off`, a growth limit is still reported by the DAG view's run banner and per-node notice, the `workflow.growth.limited` event, and `kvx workflow run show`/`kvx workflow node show`, so a stock config loses the popup and never the report.
- A run reaching `succeeded` is now documented as leaving its nodes' panes open, rather than silently saying nothing about it: they stay open for inspection, and only `run cancel` (the whole run) or `node restart` (one node) tear a node's pane down.

## [0.10.0] - 2026-08-08

### Added
- Workflow runs can now grow their own graph while executing. A node marked `is_template = true` is never scheduled directly; a node that declares `expand_allow = [...]` and `expand_max = N` can ask for instances of those templates with `kvx workflow node expand <run_id> <path> --template <key> --label <text> [--input KEY=VALUE]... [--count N] [--json]`, authenticated by the same `KARVEX_WORKFLOW_NODE_TOKEN` `kvx workflow node complete` reads, so a node can only propose on its own behalf. A node proposes and Karvex decides: every proposal is judged against the proposing node's `expand_allow` and `expand_max`, then the run's `max_depth` and `max_nodes`. Children are created as `<parent>/<template>/<n>`, numbered from 1 per parent and template, and start only once their parent has settled. `expand_max` defaults to `0`, so expansion is opt-in per node and counts cumulatively across every proposal that node makes. `--input KEY=VALUE` overrides one `{{slot}}` of the template's `prompt_template`; a key naming no declared slot is refused rather than quietly ignored. `expand_allow` is validated at authoring time, so a key naming a node that does not exist, or one that is not a template, is rejected by `workflow.create`.
- An accepted expansion child inherits a copy of its parent's outbound edges, so the fan-in node downstream of a proposing node waits for the whole generation instead of for the parent alone. Draw the fan-in edge from the proposing node; a template's own edges are dropped when the run graph is built, and a node reachable only through a template is still rejected at authoring time as unreachable.
- A refused or truncated proposal is never silent. A proposal for four children with room for two now creates two and reports the shortfall on every surface that describes the run: a new `workflow.growth.limited` JSON API event (proposing node, template, guardrail, guardrail value, requested/accepted counts), `growth_limited` on `workflow.run.get` and `workflow.node.get`, a `growth_limited:` line in `kvx workflow run show` and `kvx workflow node show`, a banner and per-node notice in the DAG view, and a notice toast where notices are enabled. The `node expand` response carries the same verdict back to the node that asked, and a rejected proposal is a successful command rather than an error — only a bad run, path, or token, or a run that has already closed, exits non-zero.
- A run's growth ceilings are now enforced, persisted, and reported from one place, where a Phase 1 run executed exactly the nodes its document declared and left `max_depth` and `max_nodes` inert. `max_nodes` (default `24`) counts every node the run has materialised regardless of how it ended, so a failed or skipped child does not refund budget and a node cannot fan out forever by failing; `max_depth` (default `3`) counts generations of expansion rather than graph layers, so every node written in the document is depth 0. `--tier medium` caps a run at 24 nodes and `--tier low` at 12, narrowing the document's `max_nodes` and never raising it. `workflow.run.get` and `kvx workflow run show` report `max_depth`, `max_nodes`, and `nodes_live`, so a run's banner cannot contradict its own record.
- `prefix+f` opens a workflow launcher in the TUI: the saved workflows, one input line per required argument of the selected one, and a tier row seeded from that workflow's own `default_tier`. `Enter` starts the run, `Esc` closes it, and every row is clickable. Rebind it with `keys.open_workflow_launcher`. Starting a run no longer requires leaving Karvex for the CLI.
- A workflow document's `default_tier` is now documented and consumed end to end: it seeds the launcher's tier row and is what `kvx workflow run start` falls back to when `--tier` is omitted.
- The `auto` tier now resolves each node from that node's own recorded history rather than from an empty record, using the two facts a run can honestly report — whether the node succeeded on its first pass, and how many times its result failed its output schema.
- Each run node's model and effort are now resolved once at run start and written verbatim onto the node together with the reason string behind them, instead of being resolved separately by the live engine and by the store's read path. `kvx workflow node show` and `workflow.node.get` explain an `auto` assignment rather than only stating it.

### Fixed
- A steer, an interrupt, or an expand proposal handed to a run that has already closed is now refused, the way `kvx workflow node restart` already was. A closed run never settles again, so all four verbs now share one guard that names the run's status and points at `kvx workflow run start`; the guard runs before the node lookup, so a closed run is reported as closed rather than as a path problem.
- A node whose pane is closed out from under it — by `pane.close`, by the TUI's close-pane binding, or by closing the tab or workspace it lived in — no longer stays `running` forever. Direct closes are observed immediately and bulk closes are reconciled on the run's existing live tick.
- A workflow notice no longer displaces the one before it. Notices queue behind the one on screen and are shown in turn as it expires, so a per-node blocker raised while the run-level notice is up is still seen. The queue is bounded at eight, dropping the oldest waiting notice rather than the newest.
- A finished run no longer reports two different start times depending on whether it is read from the live engine or from the store. The engine stamps `started_at` once and the database no longer mints a competing value, so a run restored after a restart keeps the instant it actually started.
- `kvx workflow show --json` now describes the same workflow the default output does. `workflow.get` carries a `detail` projection with the head version's nodes, edges, args, and version chain, so the machine-readable envelope no longer omits the sets the human rendering prints.
- `kvx workflow show` now reports the head version's `description` and `default_tier` instead of the values the workflow was first created with. `kvx workflow update` writes the authored document's metadata onto the workflow it heads, so `workflow.get` can no longer report v1's description beside `head_version: 2`.

## [0.9.4] - 2026-08-08

### Added
- `kvx workflow` now appears in `kvx --help`'s Usage and Common commands, and in `cli-reference.mdx` (all three locales), so the feature is discoverable without already knowing `workflows.mdx` exists.
- `kvx workflow show` now renders a workflow's summary, version history (with formatted timestamps instead of raw epoch milliseconds), and — for the head version — its nodes (key, label, runner, demand), edges (from, to, kind, port), and declared args, in human-readable text by default. Pass `--json` for the previous machine-readable envelope.
- `kvx workflow create` and `kvx workflow update` now print human-readable output by default (including local definition errors like an invalid TOML/JSON document, rendered with real newlines instead of `\n`-escaped text inside a JSON envelope); pass `--json` for the previous machine-readable envelope.
- `kvx workflow run show`'s default output now names the node responsible when a run is `paused` or a node needs attention, alongside that node's blocker reason and resume condition, and lists every node's path and status.
- `kvx workflow node show`'s default output now includes the node's blocker (reason and resume condition) when one is recorded.
- The workflow DAG view is now a bindable action. `prefix+shift+F` opens the DAG view for the run this server is executing, and `keys.open_workflow_dag` rebinds it; pressing it with no run reports that instead of opening an empty overlay. The view was previously reachable only by picking **workflow dag** from the sidebar launcher, so it was absent from the keybind table and could not be bound in `config.toml` at all.
- A workflow run now announces itself. Each node entering `needs_attention` raises a notice carrying that node's blocker reason, and the run raises one when it succeeds, fails, is cancelled, or pauses — naming the node it is paused on — so a run that needs a human no longer waits silently behind whatever pane you were looking at.
- Each node's pane is now titled with its workflow and its node label (`ux-dag-probe · Typecheck`, trimmed to fit the pane header). A `runner = "command"` node emits no title of its own, so its pane was previously an anonymous rectangle — which is exactly what `Enter` from the DAG view drops you into.

### Fixed
- A workflow definition document that omits `output_schema` on a node or `kind` on an edge — both of which `workflows.mdx` never documented as required — now gets a sensible default (`output_schema` defaults to `{}`, `kind` defaults to `sequence`) instead of a raw serde "missing field" error that masked every other authoring validator (cycle, dangling edge, duplicate key, missing command). Both fields are now documented in `workflows.mdx` as defaulting when omitted.
- An edge declaring `kind = "conditional"` with no `condition`, or a `port` that names no matching `{{slot}}` in the target node's `prompt_template`, is now rejected at authoring time instead of being accepted and silently producing a workflow that never resolves or whose data goes nowhere.
- `kvx workflow run start` now rejects an undeclared `--arg` key before starting the run, listing the workflow's declared args, instead of silently dropping the typo'd argument.
- `kvx workflow update` now reports `(unchanged — this definition matches the current version; no new version was created)` in its human output when a resubmitted definition deduplicates against the workflow's current head version, instead of describing it as a newly created version.
- A duplicate workflow name on `kvx workflow create` now reports a clean "a workflow named ... already exists" message instead of leaking the store's internal `Database index` / record-id error text.
- A node result that fails its `output_schema` now gets exactly one corrective re-prompt, and that re-prompt quotes the schema violations and names `kvx workflow node complete` as the next move; a second failing result flags the node `needs_attention` with those violations recorded, instead of leaving it running against a result nothing ever accepted.
- `kvx workflow node restart` is now refused once the run has closed. Restarting a node of a `succeeded`, `failed`, or `cancelled` run used to put a live process inside a finished run — the run reported `cancelled` while the node it had just restarted reported `running`. The refusal names the run's status and points at `kvx workflow run start`.
- A steer, interrupt, or prompt the runtime could not deliver now leaves a mark. The failure is recorded against the node and surfaces in the `kvx workflow node steer` response itself, in `kvx workflow node show`, at the top of the DAG view's detail strip, and as a notice — instead of the user being left believing the text was delivered.
- Starting a run while another is still in flight now explains the refusal. It names the blocking run and its status, the node the run is stuck on and how many others are stuck with it, says a `paused` run is waiting for a human rather than executing, and gives the two remedies: wait for it, or end it with `kvx workflow run cancel <run>`.
- The DAG view now draws the graph honestly. Node interiors stay clear of edges routed through them, edges whose node was clipped off the frame are dropped instead of leaving a rail and an arrowhead pointing at nothing, the header names the workflow the way its author does (rather than the raw run record id) and reports total nodes plus how many are offscreen, running, failed, and needing attention, `needs_attention` no longer shares a color with `blocked` despite having the opposite remedy, a running node shows a live elapsed time instead of `0s` (`usage.duration_ms` is only written once a node finishes), and the detail strip leads with the node's blocker. The renderer truncates its own lines with an ellipsis, so a narrow terminal can no longer leave a plausible but wrong run id on screen, and footer hints are dropped whole rather than sliced — `esc close` last, since it is the only way out of a full-bleed overlay.
- A run too large for the terminal now says so. The DAG view's empty screen distinguishes "there is no run to show" from "there is a run and this terminal is too small to draw it", instead of reporting a live run as a missing one and advertising navigation for a graph it never drew.
- A single click in the DAG view now selects a node; focusing its pane takes a double click or `Enter`. The only pointer gesture in a mouse-first view used to tear down the view the user was reading, with no way back but the launcher menu.
- A socket path too long for the platform's unix domain socket address (the `sun_path` limit) now fails with an actionable message naming the path, its length, and the `KARVEX_SOCKET_PATH` override, instead of the raw `io::Error` Debug output (`Error: Custom { kind: InvalidInput, ... }`).

## [0.9.3] - 2026-08-08

### Fixed
- Workflow runs read back from the store are now complete. A run reopened after a restart — or inspected with `kvx workflow run show` on a server that did not start it — reports the same progress counts, edge firing state, workspace binding, per-node directories, graph depth, and timestamps/durations as the live run did, instead of a partial projection that lost which branches were taken and where each node's `task.md`, `inputs/`, and `artifacts/` live.
- `kvx workflow show` now lists a workflow's full version history with real metadata. Every version in the chain is returned with its own origin, change summary, and creation timestamp, rather than a single head entry with a hardcoded `0` timestamp.
- Every targeted `kvx workflow` command accepts a workflow name as well as a `workflow:<key>` id. `show`, `update`, `run start`, and `run list` all resolve the same `<name|id>` selector, so a workflow created and listed by name can also be updated and run by that name.
- A node that reports an invalid or missing `result.json` is now flagged `needs_attention` instead of stalling silently. `kvx workflow node complete` always reaches the server, which owns validation and the corrective re-prompt; previously an unreadable result made the CLI exit early and the node stayed `running` forever with nothing on the server ever learning it had tried to finish.
- `kvx workflow node interrupt` now actually interrupts the node's process. Agent nodes receive `Escape` and command nodes receive `ctrl+c` (SIGINT), and an interrupt or steer the runtime could not deliver reports an error rather than falsely reporting success.
- Agent nodes are re-sent their kickoff instructions when Claude's first-run trust dialog swallows the initial prompt, so a node no longer sits at an idle agent that never received its task.
- An agent node's kickoff instructions now reference its `task.md` by absolute path. A node's working directory is the workspace, not its node directory, so the previous relative path named a file the agent could not open.
- Workflow runs now progress correctly on headless servers. The headless loop advances the workflow engine's clock and reconciles managed agents, so detector-driven signals such as sustained idle can fire and `agent.prompt`/`agent.send_keys` no longer answer `agent_not_ready` indefinitely.
- Session startup now fails fast with an actionable message when the derived socket path would exceed the platform's `sockaddr_un.sun_path` limit (108 bytes on Linux, 104 on macOS), naming the path, the limit, and the two ways to shorten it. Previously this surfaced only as a 15-second wait followed by "server did not become ready", with an empty server log.
- `install.sh` installs the binary as `kvx` and honours `KVX_MANIFEST_URL` and `KVX_INSTALL_DIR` overrides (with `HERDR_INSTALL_DIR` still accepted). It previously installed under the old `herdr` name and hardcoded the release manifest URL, so the installed binary did not match the documented `kvx` commands.

## [0.9.2] - 2026-08-07

### Changed
- The workflow subsystem's embedded store moves back to SurrealDB, reinstating the `SurrealValue`-based records and `.surql` migrations behind the unchanged `WorkflowStore` API, in line with the project's plans to build SurrealQL, graph, vector, and live-query features on top of it. Workflow run history written by v0.9.1's `redb` store is not migrated to the reinstated SurrealDB store.
- Release binaries grow accordingly, from roughly 15-19 MB in v0.9.1 to roughly 53 MB, because SurrealDB and its dependencies compile back into the single `kvx-<target>` binary. Each target stays roughly 40% smaller than its v0.9.0 counterpart, and the `[profile.release]` thin-LTO, single-codegen-unit, stripped-symbol build introduced in v0.9.1 is unchanged.

## [0.9.1] - 2026-08-07

### Changed
- Release binaries now build with thin LTO, a single codegen unit, and stripped symbols, cutting each platform's single `kvx-<target>` binary to roughly a fifth of its 0.9.0 size. Workflows remain fully included in that one binary; there is no separate build or asset to install for them.
- The workflow subsystem's embedded store is now `redb` (pure Rust) instead of SurrealDB, behind the unchanged `WorkflowStore` API. Workflow run history written by v0.9.0 is not migrated to the new store.

## [0.9.0] - 2026-08-07

### Added
- Workflows: `kvx workflow create` and `kvx workflow update` save a multi-agent kvdag as an immutable, versioned definition, and `kvx workflow run start` executes it. Every node runs as a real Karvex-managed pane, `sequence`, `data`, and `conditional` edges pass results between nodes, each node's result is validated against its declared `output_schema`, independent nodes run in parallel up to the run's concurrency limit, and a live DAG view shows the run as it executes.
- `kvx workflow run show`, `kvx workflow run cancel`, and the `kvx workflow node` verbs (`show`, `steer`, `interrupt`, `restart`, `complete`) inspect and steer an in-flight run without restarting it.
- Workflow definitions, versions, and runs persist in an embedded SurrealDB store. The subsystem sits behind the default-on `workflow` cargo feature, so `--no-default-features` still builds Karvex without it.
- `theme.custom.sidebar_bg` can now give the desktop sidebar its own background without changing built-in theme defaults.
- Settings and `ui.status_indicators = "symbols"` can now use distinct static shapes for blocked, working, done, idle, and unknown agent states. (#2260)
- The plugin marketplace now discovers valid manifests at repository roots and subdirectories, groups multiple plugins under each repository, and publishes their versions and exact default-branch commits.

### Changed
- Renamed the project from Herdr to Karvex. The CLI, server, and TUI now ship as a single `kvx` binary, the crate is `karvex`, and config, state, socket, and log paths move from the `herdr` directories to `karvex` ones (for example `~/.config/herdr` to `~/.config/karvex`) — copy an existing directory across to keep settings and saved sessions. Bundled agent integration assets are now named `karvex-agent-state.*`.
- The Windows installer installs `kvx.exe` and still recognizes a pre-rename `herdr.exe` install so it can be replaced in place.

### Fixed
- Configs containing the retired Herdr-written `ui.agent_panel_scope` setting no longer report it as an unknown key after upgrades. (#2292)
- Claude Code confirmation prompts using `Enter to confirm · Esc to cancel` now report `blocked` instead of `idle`. (#2268)
- Sidebar agent lists keep scrolling when differently sized clients are attached to the same session. (#2255, thanks @aiworkflowpro)
- `pane send-keys` and `agent send-keys` now preserve Shift when sending `shift+tab`, allowing agent permission modes to be cycled programmatically. (#1561, thanks @keinstn and @tomohisa)
- Pane applications that enable `modifyOtherKeys` with event-type reporting keep receiving key release events. (#2302)
- Host terminals that report mouse input in the default `ESC [ M` encoding instead of SGR are understood again, including reports split across reads. (#2309)
- Closing a focused pane returns focus to the pane the split was opened from instead of the next pane in tree order. (#2266)
- Halfwidth katakana combined with a voiced or semi-voiced mark now renders instead of being blanked. (#2257)
- Navigator search matches named tabs in single-tab workspaces. (#2320)
- `pane query --current` resolves the calling pane from the caller's environment instead of the focused pane. (#2297)
- Pending URL clicks survive host terminal focus loss, so opening a link no longer opens it a second time. (#2290)
- Collapsed workspaces keep their agent status visible, including at two-digit positions. (#2216)

## [0.8.0] - 2026-08-03

### Added
- Added `herdr --skill` to print the agent skill bundled with the running Herdr binary.
- Added `ui.pane_scrollbars = false` to hide terminal pane scrollbars and reclaim their reserved column. (#2167)
- Added `ui.tab_bar_position = "bottom"` to place the desktop tab row below terminal panes. (#2117)
- Added live filtering to the keybind help with `/`, Backspace, and `Ctrl+U`. (#1825, #1832, thanks @corrius)
- Added Windows support for `experimental.switch_ascii_input_source_in_prefix` with Korean IMEs. (#1802, #1823, thanks @joonhwan)
- Added Grok CLI session reporting and native restore with `grok --resume <id>`. (#1800, #1807, thanks @carlesso)
- Added Antigravity CLI session reporting and native restore with `agy --conversation <id>`. (#1011, #1571, #2087, thanks @ludoo)
- Added automatic text history reads for idle alternate-screen agents, with the application viewport restored after collection.
- Added `workspace.move_block`, the `workspace.reordered` event, and atomic worktree-group reordering. (#1694)
- Added a Simplified Chinese README. (#1990, thanks @patrick-xin)

### Changed
- Experimental options are no longer exposed in the Settings TUI and remain available through the config file.
- Agent status indicators now use the same static workspace marks across the sidebar, navigator, and mobile views, eliminating continuous spinner rendering while agents work.
- Hidden pane output no longer triggers unnecessary TUI rendering.
- Windows preview downloads now include Herdr and a modern app-local ConPTY runtime in one archive. (#1533, #1644, #1828)
- Worktree parents and children now stay packed together in the sidebar, including while groups are reordered.
- Public documentation now separates stable, preview, and immutable versioned release snapshots.
- Repository and installation links now use `herdrdev/herdr` after the GitHub organization migration.
- Relicensed Herdr from AGPL-3.0-or-later to Apache-2.0.

### Fixed
- Pane applications now receive semantic light/dark query responses and live Mode 2031 updates when the host appearance changes. (#714)
- Remote attach now falls back to `sh` when the login shell cannot perform path discovery. (#1201)
- PTY output continues to be read while pane input is temporarily blocked. (#1295)
- Worktree CLI help and docs no longer advertise the redundant `--json` flag; worktree commands remain JSON-only and continue accepting the flag for compatibility. (#2171)
- OpenCode 2 preview panes now appear as OpenCode agents and use the existing OpenCode status detection. (#2169)
- Pane text copied through VS Code Remote Tunnels now reaches the viewing machine's clipboard instead of overwriting the remote host clipboard. (#2015)
- Windows agent detection now follows Git Bash-launched agents across emulated `exec` process boundaries. (#2107)
- Detached Windows servers and pane processes now survive logout from the OpenSSH session that started them. (#2008)
- Windows `agent start` now launches agents without native arguments instead of timing out on an invalid empty PowerShell argument list. (#2072)
- Headless servers now resume restored agent sessions without waiting for a TUI client to attach. (#2064)
- Vibe and other Kitty-keyboard pane applications now receive shifted letters and punctuation when they request associated text. (#2020)
- Kitty-keyboard pane applications now receive printable key releases without duplicate text input. (#1746)
- Kitty graphics remain visible during host repaints. (#1628)
- Pane applications now receive correct XTWINOPS terminal and cell-size query responses. (#835)
- WSL clients query the host cell size when the terminal ioctl reports no pixels, keeping graphics sharp instead of using the 8x16 fallback. (#2146, #2160, thanks @WakaTaira)
- Linux runtimes without terminal foreground process groups can opt into child-group agent detection with `HERDR_PROCESS_DETECTION=child-groups`. (#1982)
- Installing the Herdr agent skill with the `skills` CLI no longer copies the entire repository. (#2022)
- Nix builds now include the bundled agent skill required by `herdr --skill`. (#1889, #1890, thanks @olafkfreund)
- Agent prompts now wait briefly after sending text before pressing Enter, preventing prompts from remaining in agent composers without starting a turn. (#1878)
- Empty clipboard writes from pane applications no longer erase existing clipboard contents or show a copied confirmation. (#1893)
- Plain mouse movement no longer triggers continuous full renders while preserving Herdr menu hover and pane application mouse tracking. (#1865)
- Extended-button drags now preserve Herdr hover state while applications receive the drag.
- `ui.copy_on_select = false` now retains drag and double-click word selections without copying; `Ctrl+C`, or `Cmd+C` when the host terminal forwards it, copies and clears the selection. (#1782)
- Pane and agent read responses now report `truncated: true` when older terminal rows were omitted. (#1717)
- Pane applications that query OSC 4 palette colors now inherit the host terminal palette. (#1752)
- Ctrl-clicking a pane URL no longer forwards an unmatched mouse release to alternate-screen applications, preventing duplicate browser tabs. (#1761)
- Known-agent integrations now leave pane ownership to confirmed process exit, so restarting Pi with the same saved session restores lifecycle state even with custom working UI. (#1648, #1792)
- Nested or ephemeral Codex sessions no longer replace the owning pane's resumable session. (#1789, #1927, thanks @Pimpmuckl)
- Pi RPC, JSON, and print processes no longer claim pane lifecycle state intended for Pi TUI sessions. (#2159, thanks @rhjoh)
- Hermes state now comes from screen detection while its plugin reports resumable session identity, avoiding stale lifecycle authority from incomplete hooks.
- OMP integration install, status, and uninstall now respect `PI_CONFIG_DIR` when `PI_CODING_AGENT_DIR` is not set, and installation refuses extension-directory collisions with Pi. (#1696)
- OMP integrations now preserve Windows absolute session paths for native restore. (#2092, thanks @art-wiedzmin)
- Claude integration updates preserve existing settings key order and formatting. (#2066)
- Physical Escape key records on native Windows now bypass raw VT report framing, so pane applications receive Escape immediately and reliably. (#1736)
- Native Windows key presses, grouped repeats, and releases now preserve their physical lifecycle and stay with the pane that received the initial press. (#2077)
- Windows `pane send-keys` and `agent send-keys` now deliver semantic Escape as a complete key tap, preventing a following key from being interpreted as an Alt chord.
- Shift+Enter now reaches native Windows pane applications with its modifier intact. (#1743, #1909, thanks @Pimpmuckl)
- Ctrl+_ input bytes now decode as Ctrl+_ instead of Ctrl+-. (#2164, #2165, thanks @Sertug17)
- Prefix and navigate modes now recognize non-US shifted keybindings while retaining legacy US punctuation support. (#1870)
- Closing a non-focused workspace no longer changes the focused workspace. (#1328, #1877, thanks @yianL)
- A background workspace that closes after its last pane exits no longer moves focus or hides the current workspace. (#1621, #1912, thanks @season179)
- Directional pane focus now keeps Navigate mode active. (#1850, #1993, thanks @we11adam)
- Closing a workspace's last tab through the CLI or API now closes the workspace like the TUI does. (#1760, #1899, thanks @season179)
- Linked worktree workspaces retain their labels during Git metadata refreshes.
- Clients repaint after transient terminal resizes instead of leaving stale or missing rows.
- Repeated workspace Git discovery and foreground-cwd checks no longer block rendering or API handling. (#1838, #2206)
- Relative plugin commands now resolve from the plugin root. (#1949)
- Windows installation preserves inherited `PATH` and related environment variables. (#1947)
- Windows agent process discovery preserves the owning parent agent across wrapper processes. (#1514)
- The Rose Pine `surface_dim` color remains visible when the outer terminal uses a matching theme. (#1946, #2002, thanks @brabli)
- CLI socket commands now report a clear `server_not_running` error instead of a raw I/O error. (#1941, #1963, thanks @season179)
- Non-UTF-8 CLI arguments now produce a usage error instead of panicking. (#2207, thanks @VialFlorian)
- Copy-mode `e` now crosses long soft-wrapped CJK lines when a read window ends on a wide glyph. (#2145, thanks @kiakiraki)
- Clients restore terminal state when they receive SIGHUP or SIGTERM. (#2041, thanks @MattJColes)
- Windows now shows `system` notifications and completes MP3 notification sounds without leaving PowerShell players waiting for a timeout. (#1330)

## [0.7.5] - 2026-07-21

### Breaking Changes
- Installed and linked plugins, including their enabled state, are now global to the current user instead of isolated by Herdr session. Plugins installed only in a named session on Herdr 0.7.3 must be installed or linked again. (#1174)

### Added
- Added a live-agent CLI facade with named `start`, atomic `prompt`, logical `send-keys`, and server-owned `wait` workflows. Agent startup targets an existing pane without changing topology, validates the requested interactive agent kind and strict agent name, and accepts native arguments after `--`.
- Added transient declarative Agent view queries through `agent.view.set/clear`; filtered and sorted views now define sidebar, mobile, mouse, and agent-keybind navigation order.
- Added one-shot plugin `[[startup]]` hooks for restoring plugin-owned state after server startup and live handoff.
- Added per-token foreground, bold, and dim styling to expanded Space and Agent sidebar row layouts.
- Added `ui.sidebar_start_collapsed` to launch Herdr with the sidebar collapsed. (#1463)
- Added `ui.prompt_new_workspace_name` to ask for a workspace name before interactive TUI creation.
- Added macOS support for the `HERDR_AGENT=<agent>` foreground-process hint, allowing agents hidden behind host-visible wrappers such as `nono` to use the named agent's screen manifest. (#679)

### Changed
- Agent commands now accept only a unique live agent name or the pane ID currently hosting that agent. Names are cleared when the occupant exits, is released, or is replaced. The old top-level `wait` commands were replaced by `agent wait` and `pane wait-output`, and `agent send` was replaced by `agent send-keys`.
- The session navigator now uses connected tree glyphs, groups matches by workspace, and automatically selects the first result when a search begins. (#1611)

### Fixed
- CLI requests now return a machine-readable `protocol_mismatch` error when the client and server protocols differ, while recovery commands remain available. (#1435)
- Linux sound notifications now terminate and reap audio players that do not exit, preventing unavailable audio from leaving CPU-bound `mpg123` processes behind. (#1622)
- Oversized bracketed text pastes are now rejected with a client-local notification instead of disconnecting the client. (#1665)
- Agent prompt waits now report `agent_prompt_stalled` after five seconds without an observed state change instead of waiting indefinitely after an ineffective submission.
- `herdr config check` now reports unknown config keys with their full paths instead of treating ignored typos as valid configuration. (#1573)
- Codex panes with customized static terminal titles now fall back to the live working footer instead of remaining idle, while OSC activity remains preferred. (#1563)
- Grok panes now preserve working and blocked state from terminal signals and pinned background-work status instead of falling back to idle mid-turn.
- OpenCode lifecycle reports are now serialized so out-of-order plugin events cannot leave an idle pane marked working. (#1519)
- Kimi question prompts now report blocked until the user answers or dismisses them.
- Pi lifecycle reporting now uses settled events, preventing transient message boundaries from publishing an idle state mid-turn.
- The Pi, OMP, OpenCode, and Kilo Code integrations can now be installed on Windows and report lifecycle state and native session identity through Herdr's named-pipe API. (#1531)
- Named agent prompts now honor live bracketed-paste mode before sending Enter, preserving OpenCode text such as `A != B` instead of triggering shell mode. (#1525)
- New panes, tabs, layouts, and workspaces using `new_cwd = "follow"` now inherit the foreground process-group leader's working directory instead of an unrelated helper process directory. (#1472)
- Cached pane working directories no longer trigger repeated filesystem checks, avoiding slow sidebar rendering on network filesystems such as Ceph. (#1603)
- Windows foreground-process snapshots are now shared across panes, reducing idle CPU use in sessions with many panes. (#1158)
- Terminal diff streams now batch contiguous writes, reducing the visible wave effect while scrolling pane history. (#283)
- A standalone Escape arriving beside another key is now preserved as its own input instead of being combined into a fabricated Alt chord. (#541)
- Pane viewports that were following live output now continue following after a resize.
- Mouse selections now remain visible when `ui.copy_on_select = false` while clipboard writes stay disabled. (#1471)
- Workspace close confirmation now shows the current workspace name instead of a stale or unrelated label. (#1364)
- Plugin command arrays now preserve whitespace-only arguments. (#1594, #1613)
- Plugins can now be installed or linked while no Herdr server is running. (#1670)
- Remote attach now discovers Herdr installed in mise's canonical tool path before offering to install a sidecar binary. (#1201)
- Noninteractive update, plugin, integration, sound, custom-command, and Git subprocesses no longer flash console windows on Windows. (#1468)
- Live handoff now preserves installed plugins and no longer lets the next plugin installation overwrite the existing registry. (#893)
- `herdr agent wait` now returns `agent_not_running` promptly when its target pane closes instead of waiting for the full timeout. (#1439)
- Pane graphics streams now shut down cleanly when a client disconnect races stream teardown.

## [0.7.4] - 2026-07-15

### Added
- Added session-modal popup floating terminal panes for `type = "popup"` custom command keybindings and plugin panes, with optional cell or percentage sizing and no changes to the tiled tab layout. (#1125)
- Added `ui.copy_on_select` to disable automatic clipboard copying after mouse selection while keeping the selection visible.
- Added configurable row layouts for expanded Space and Agent sidebar entries, including built-in display tokens, per-agent overrides, custom metadata tokens, and pane/workspace metadata reporting through the CLI and socket API.
- Added independent `row_gap` settings for expanded Space and Agent sidebar entries.
- Copy mode now supports literal smart-case search with `/` and `?`, repeating with `n` and `N`, match highlighting, and tmux-style cross-line `w`/`b`/`e` word motions. (#1230)
- Added Maki agent support. (#1301, #1302, thanks @tontinton)
- Added a searchable, version-matched configuration reference and a troubleshooting guide covering duplicate terminal key events, modified-arrow shell bindings, updates, remote access, and logs. (#1116, #1370)

### Changed
- Expanded Space and Agent sidebar entries now use a packed layout by default; set the corresponding `row_gap` to `1` to restore the previous spacing.
- Refreshed the bundled Herdr agent skill for current public workspace, tab, and pane ids and the current CLI/API workflow. (#1297)
- Expanded Japanese and Simplified Chinese CLI documentation with shell completion setup and API schema usage. (#1151)

### Fixed
- Collapsed Agent sidebar rows now follow the same ordering and click targets as the expanded panel, and their shortcut numbers are assigned by visible list position instead of repeating across workspaces. (#1168, #1344)
- Shifted indexed bindings such as `prefix+shift+1..9` now match terminals that report the corresponding punctuation characters. (#1184)
- Plugin-driven tab renames now immediately refresh tab-bar geometry and labels. (#1111, #1179, thanks @kovalov)
- New tabs, splits, layouts, and workspaces configured to follow the foreground directory now start from the focused pane's current working directory. (#1245)
- Amp, Codex, and Claude Code detection now recognizes current active-turn UI variants, including reordered Codex title spinners and Claude `/btw` turns. (#1208, #1281, #1366)
- Pi lifecycle state now reanchors after native session replacement, avoiding working panes that remain idle or tied to an abandoned session. (#943, #1189, thanks @dmmulroy)
- OMP lifecycle reports are now retried when startup races drop the first report. (#1310)
- WSL now uses Herdr's drawn cursor by default, matching the native Windows workaround for host cursor flicker. (#930)
- Live handoff now preserves explicit named-session socket paths, waits for slower server shutdowns, and flushes API responses before the old server exits. (#1180, thanks @dvic)
- The Windows installer no longer rewrites an existing config file or creates a duplicate onboarding line during first-run setup. (#1162)
- Config diagnostics now reach CLI-only and attached-client startup paths reliably and clearly identify fallback configuration behavior.
- Detached custom command children are now reaped after exit instead of accumulating zombie processes. (#1360)
- Renamed single tabs now remain visible in the Agents sidebar instead of losing their tab label. (#1369)
- Documentation search results are now scoped to the active locale and stable or preview channel.
- Horizontal wheel and trackpad events now reach pane applications that enable mouse reporting. (#1349)
- Copy mode `$` and End now stop at the final visible character on the row instead of jumping to the pane edge. (#1405)
- Split SGR mouse reports are now reassembled across input reads, and a preceding standalone Escape is preserved instead of being swallowed or leaked as mouse bytes. (#1334, #1382)
- Linux foreground-process discovery now stays within Herdr pane process trees instead of scanning unrelated host processes, reducing CPU use on busy multi-user systems. (#1399)
- Single-codepoint emoji chosen from the Windows emoji picker now reach panes when WezTerm's kitty keyboard support sends them as CSI-u events with associated text. (#1404)
- Outer-terminal focus gained and lost reports now reach the focused pane when its application enables focus reporting, restoring Neovim file autoreload and other focus-aware terminal behavior. (#1337)
- Native Windows servers now detach from the terminal console that launched them, so closing WezTerm, Windows Terminal, or another host terminal no longer stops persistent pane processes. (#1329)
- Windows API clients now remain connected while waiting for initial named-pipe request bytes, so `status server`, `api snapshot`, and other socket commands no longer intermittently fail with BrokenPipe. (#1279)
- `herdr --remote` now installs remote helper binaries without routing the binary stream through a multiline `/bin/sh -c` command, fixing installs for non-POSIX login shells such as xonsh. (#1203, thanks @nhumrich)

## [0.7.3] - 2026-07-08

### Fixed
- The session navigator now keeps the active search query when leaving and re-entering search focus, and its footer now shows shortcuts for the current input mode. (#1115, #1140, thanks @liby)
- Re-focusing an already-focused done agent or pane through the socket API now marks it seen instead of leaving stale done status in API responses.
- Windows foreground-process detection now ignores cyclic process-parent snapshots instead of growing memory until the server aborts. (#1083)
- Terminal redraws now hide the cursor inside synchronized output, reducing focused-pane cursor flicker during active redraws. (#967)
- Headless render streams no longer scan visible plain-text URLs during rendering, reducing redraw work while preserving OSC 8 hyperlink metadata.
- The workspace picker once again honors navigate-mode workspace up/down keys, including custom bindings, after `prefix+w`. (#1149)

## [0.7.2] - 2026-07-07

### Added
- Added MastraCode integration support with lifecycle state reports and native thread restore. (#337, #788, thanks @wardpeet)
- Added `ui.sidebar_collapsed_mode = "hidden"` to make a collapsed sidebar use zero width while keeping the existing compact rail as the default. (#842)
- Added `herdr completion <shell>` / `herdr completions <shell>` to generate shell completion scripts for bash, elvish, fish, PowerShell, and zsh. (#435)
- Added `session.snapshot` to bootstrap client runtime state in one socket API response before subscribing to events.
- Added `herdr api schema` to inspect the bundled socket API schema, with `--json` for the full JSON Schema document and `--output PATH` for file output.
- Added `layout.updated` socket events so protocol clients can keep tab layout snapshots current after pane split, resize, swap, move, zoom, and layout mutations.
- Added pane scroll metrics to pane socket API responses and `pane.scroll_changed` subscriptions for clients that need to show when a pane is scrolled back.
- Added `herdr terminal session observe` for read-only live ANSI terminal streams that bridge processes can consume as newline-delimited JSON.
- Added `herdr terminal session control` for bridge processes that need live ANSI frames plus input, resize, scroll, release, and takeover authority.
- Added `ui.hide_tab_bar_when_single_tab` to hide the tab row when a workspace has one tab. (#448)
- Added Japanese and Simplified Chinese website docs.

### Changed
- The mobile switcher now starts from an agents-first summary and renders worktrees as a tree, making narrow terminals easier to scan.
- macOS prefix input-source switching now runs on the foreground client, so non-Latin input sources are restored reliably after prefix mode. (#774, #1016, thanks @ppggff)
- Nix packaging now uses `xcbuild` instead of custom Apple SDK wrappers for Darwin builds. (#995, thanks @arunoruto)

### Fixed
- Windows clients now send shifted punctuation such as `!`, `?`, and `:` as literal text to Kitty-keyboard-mode pane apps, fixing Kiro CLI TUI prompts while preserving modified key chords. (#1066, #1105)
- Alt-Shift letter chords are now preserved instead of being collapsed into plain uppercase input. (#1088)
- Antigravity background-task waits are now detected even when the UI does not show a `/tasks` hint. (#755)
- `herdr --remote` now prints clean remote attach failures and SSH authentication guidance instead of Rust Debug-formatted I/O errors when SSH authentication is denied. (#1034)
- `herdr server stop` now stops Windows named-pipe servers instead of failing with `named pipes do not support I/O timeouts`. (#1113)
- `herdr server stop` now waits until both server sockets are unreachable before returning, avoiding an immediate first-start failure when restarting right after replacing the binary.
- macOS `herdr --remote` clients now bridge Finder-dropped image files to the remote pane instead of forwarding the local file path as typed text. (#828)
- Grok Build agent detection now tracks the current Grok Build UI: panes report working while responses, tools, and subagents run, and blocked on permission prompts and question dialogs, instead of falling back to idle mid-turn. (#1017, #1055, thanks @TonyxSun)
- GitHub Copilot CLI detection now recognizes the newer Esc interrupt prompt as working. (#1119, #1120, thanks @LaneBirmingham)
- Unix local Herdr clients no longer treat empty bracketed paste as a clipboard-image bridge; `herdr --remote` keeps using it for local-desktop image paste over SSH. (#986)
- Custom command keybindings now run through `cmd.exe /d /c` on Windows instead of `/bin/sh`, so `type = "pane"` and `type = "shell"` bindings can launch native Windows commands. (#1041)
- Plain PageUp/PageDown now reach primary-screen pager apps such as `less -X` and Git diff when they enter application cursor mode, while shell transcripts still use Herdr pane scrollback. (#953)
- Copy mode now supports Ctrl-page navigation, keeps the Herdr prefix key available while copying, and restores the copy context correctly after prefix commands. (#681, #885, #1092, thanks @reobin)
- `prefix+e` scrollback editor panes now open on Windows without trying to run `/bin/sh`; Windows uses `VISUAL`, then `EDITOR`, then `notepad.exe` as the fallback editor. (#914)
- `herdr pane split --current` now resolves to the calling Herdr pane instead of the UI-focused pane when run inside a pane. (#902)
- Native Windows clients running inside Alacritty now preserve mouse reports and `ctrl+j` input instead of leaking mouse escape sequences into panes. `shift+enter` remains dependent on whether the outer terminal reports it as a distinct modified Enter key. (#792)
- Windows clients now preserve bracketed paste, Backspace, modifier-only keys, host cursor drawing, native clipboard copies, recent pane reads, and wait connections across the native input path. (#670, #795, #907, #920, #930, #962, #963, #1067)
- New tabs and workspaces now follow the focused pane's current directory more reliably, including PowerShell panes that report cwd through prompt shell integration on Windows. (#912, #919)
- Pi and OMP integration state now survives internal session reloads, recovers after resumed sessions such as `omp -c`, and reports Ask/tool approval waits as blocked instead of leaving the pane working or stuck on the previous session. (#800, #879, #984, thanks @dmmulroy)
- Pi state socket reports are now retried, reducing stale sidebar state when the report races server startup. (#1049)
- OpenCode now reports subagent permission prompts as blocked and handles object-form `session.status` events. (#838, thanks @soar)
- Remote attach now discovers compatible Homebrew, mise, and Nix profile installs before offering to install a sidecar binary to `~/.local/bin/herdr`. (#840)
- `herdr --remote` sessions now keep the remote server in its own login-independent session and preserve compatible running servers after helper binary updates, so network drops should disconnect only the client instead of killing remote panes.
- `herdr --remote` now reuses one OpenSSH connection across setup probes, installs, server checks, and the final bridge when `[remote].manage_ssh_config` is enabled, so password-based hosts prompt once instead of once per setup command. (#888)
- Foreground agent session reports can now replace stale saved session references, so resumed panes do not stay tied to an older agent session. (#943)
- Kitty graphics panes now repaint streaming image updates reliably and delete replaced host images instead of leaking them. (#947, #948, thanks @DevSrSouza)
- Pane apps that query OSC 12 cursor color now receive a response. (#806)
- ANSI undercurl styles now render in panes. (#895)
- CJK pane border labels, compact keybinding help ranges, and active auto-named tabs now measure by display width, avoiding broken alignment and unreadable labels. (#799, #810, #817, #829)
- Ctrl+/ is now encoded as Ctrl+_, matching terminal expectations for pane apps. (#847)
- PowerShell panes now stay alive after agent Ctrl+C. (#860)
- SGR mouse reports no longer leak into pane input after host-side handling. (#939)
- Wrapped pane links now preserve their target instead of being truncated across soft-wrapped lines. (#1098)
- Linux foreground process-group scans are cached, reducing idle CPU in large sessions. (#936)
- Session autosaves now run off the main loop, reducing UI stalls in busy sessions.
- Worktree removal now focuses the parent workspace after closing the worktree workspace. (#1004)
- Closing a tab from the context menu now exits the menu cleanly. (#945)
- Copy feedback now stays visible above retained pane updates. (#555)
- Windows ARM64 installer fallback now works when the normal checksum path is unavailable. (#897)

## [0.7.1] - 2026-06-24

### Added
- Added `[update].version_check` and `[update].manifest_check` so background Herdr version checks and remote agent-detection manifest checks can be disabled independently. Manual `herdr update` and bundled/local detection manifests still work when the background checks are disabled. (#677)
- Added `HERDR_AGENT=<agent>` as a Linux foreground-process hint for agents hidden behind wrappers such as VMs, Bubblewrap, or `fence`, allowing Herdr to use the named agent's screen manifest when `/proc` cannot expose the real command. (#679)
- Added `ui.pane_borders` and `ui.pane_gaps` to make split pane dividers and spacing configurable. (#271)

### Changed
- Removed the Agents panel workspace/all filter. The panel now always shows all agents, defaults to grouped-by-space ordering, and can switch to priority ordering with `ui.agent_panel_sort = "priority"`. (#318)
- User keybindings now displace conflicting built-in defaults during config load, so overriding a default binding no longer leaves both actions attached to the same key. (#747)
- Worktree creation now checks out an existing local branch when the requested branch already exists instead of failing by trying to create it again. (#729)
- Worktree operations started through the socket API and plugin/UI flows now defer long-running Git work until the app runtime can drive it, keeping clients responsive and preserving plugin lifecycle events for worktree-created panes. (#657, #662, #686)
- OMP, OpenCode, Pi, Devin, and other official hook integrations now scope lifecycle and session reports to the intended root agent process more reliably, reducing stale or cross-process session adoption after restarts, nested commands, and new sessions. (#614, #712, #719, #765)

### Fixed
- Windows Terminal multiline text paste now reaches pane apps as one bracketed paste, so OMP, Pi, and similar prompts no longer submit each pasted line separately. Plain Esc, Shift+Enter, mouse, focus, resize, and Unicode paste handling are preserved on the Windows client path. (#670)
- Local Herdr clients no longer treat raw `Ctrl+V` as a clipboard-image paste trigger, so pane apps such as Vim and Neovim receive block-visual `Ctrl+V` even when the desktop clipboard contains an image. `herdr --remote` keeps `keys.remote_image_paste = "ctrl+v"` by default. (#647)
- Herdr now refreshes cached host terminal colors when terminals report a light/dark color-scheme change, so pane apps that query OSC 10/11 no longer need detach/attach to see updated default colors. Opt-in `[theme].auto_switch` can also switch Herdr's own UI between configured `dark_name` and `light_name` themes. (#675)
- Full-lifecycle hook agents can now recover when an old release/report sequence belongs to a previous agent generation. Herdr keeps process-exit validation active under lifecycle authority and re-anchors hook sequence guards after fresh session references or proven process exits. (#684)
- OMP now reports a native session reference, so an OMP pane reappears in the Agents panel after exiting and rerunning `omp` in the same pane, and Herdr can resume it with `omp --resume=<session>`. Previously the released lifecycle hook stayed suppressed until a server restart. (#614)
- Host terminal color query (OSC 10/11) replies that arrive split at their escape introducer no longer leak as text like `11;rgb:...` into the focused pane, most visible when launching agents that probe terminal colors on startup. (#549)
- Long CJK Git branch names in the sidebar now truncate by display width instead of overflowing or cutting at the wrong cell boundary. (#644)
- Temporary pane commands launched from API flows no longer steal focus from the previously focused pane after they finish. (#658)
- Root agent session restore now ignores child process reports that would otherwise overwrite the saved session for the owning pane. (#712)
- Kitty file-transfer media queries are now answered, allowing pane apps that rely on kitty graphics file support to detect image/file media capability correctly. (#732)
- Idle or slow clients no longer block server writes to other clients while the blocked client is waiting for output. (#726)
- GitHub Copilot CLI `ask_user` accept prompts are now detected as blocked so the Agents panel shows that the pane is waiting for input. (#725)
- Pane reads now skip wide-character spacer cells, avoiding duplicated or malformed output around double-width characters. (#698)
- Split pane border intersections now use the active pane color consistently. (#742, thanks @cullendotdev)
- The Windows installer checksum fallback no longer depends on `Get-FileHash`, improving compatibility with constrained PowerShell environments. (#751)
- Pi launched through npm wrappers on Windows is now detected as Pi instead of a generic wrapped process. (#754)
- Windows builds now force the system ConPTY path through a vendored `portable-pty` patch, avoiding the bundled-path startup failure seen in affected Windows environments. (#761)
- Key release events that fall back to encoded input no longer double-send text into pane apps. (#769)
- Remote clients now allow a longer initial handshake, improving `herdr --remote` startup over high-latency links. (#753)

## [0.7.0] - 2026-06-15

### Added
- Added local plugin v1 support with `plugin.link/list/unlink/enable/disable`, manifest-declared actions, event hooks, managed plugin panes, link handlers, command logs, keybinding integration, and authoring docs under Preview docs.
- Added `herdr plugin install <owner>/<repo>[/subdir...]`, `plugin uninstall`, source metadata in `plugin.list`, offline registry fallback, and a human-readable default `plugin list` with `--json` for scripts.
- Added `herdr plugin config-dir <id>` and automatic plugin config/state directory creation so plugin setup docs can point users at a stable config path.
- Added Devin CLI automatic detection plus `herdr integration install devin` hooks that report session ids for restore with `devin --resume <id>`. Devin state remains screen-detected because Devin hooks do not cover every permission cancellation and user interrupt transition. (#606, #622, thanks @minatoaquaMK2)
- Added supporting plugin host APIs for `pane.current`, `pane.process_info`, `client.window_title.set/clear`, `layout.export/apply`, plugin pane placement, plugin invocation context/env injection, and plugin pane ownership across `pane.move`.
- Added `pane.move` and `herdr pane move` to relocate a running pane into another tab, a new tab, or a new workspace without restarting its terminal process. (#299)
- Tabs containing a zoomed pane are now marked in the tab bar so the zoom state is visible from other tabs.

### Changed
- Bumped the client/server protocol version to 14 for `pane.move` compatibility. (#299)
- Public workspace, tab, and pane ids are now short stable handles such as `w1`, `w1:t1`, and `w1:p1`; closed tab and pane ids no longer retarget later resources. (#569)

### Fixed
- `pane.send_keys` and `pane.send_input.keys` now accept Herdr key-combo strings such as `ctrl+h`, `ctrl+j`, `ctrl+k`, and `ctrl+l`. (#613, thanks @dmmulroy)
- Config startup and reload now warn about unknown top-level table sections, including a `[toast]` hint that points to `[ui.toast]`, instead of silently ignoring them.
- Claude Code session restore now accepts real `/clear`, `/resume`, and compacted session identity changes while still ignoring nested `claude -p` startup sessions that inherit the pane environment. (#620)
- Auto-named tab labels now stay compact after closing, moving, or creating tabs while public tab ids remain stable.
- F1-F4 key presses sent as `ESC[11~` through `ESC[14~` now reach pane apps instead of being dropped. (#574)
- Numeric keypad keys sent through the kitty keyboard protocol now enter their digits and operators instead of being dropped. (#570)
- Pane resize keybindings now shrink panes again instead of only being able to grow them. (#562)
- Windows pane cursor rendering is now stable instead of showing a misplaced or flickering cursor. (#556)
- Tab identity is now preserved across restored sessions.
- Idle panes now poll their PTY less frequently, reducing CPU use while sessions are inactive.
- Captured pane URL clicks, including plugin link handlers, now use Ctrl-click on macOS too because captured terminal mouse reports do not expose Cmd-click separately from plain click. (#307)

## [0.6.10] - 2026-06-11

This is a hotfix release for v0.6.9. See the v0.6.9 notes for the full feature release.

### Fixed
- Lifecycle-authority agent integrations such as Pi and OpenCode no longer trigger a repeated detection reset loop that could flood logs, drive high CPU, and make the UI lag or stop responding. (#560, #565, thanks @dzevs)

## [0.6.9] - 2026-06-10

### Fixed
- Copy mode page scrolling now stops at the same top and bottom boundaries as normal pane scrolling instead of overshooting or getting stuck near the edges. (#459, #460, thanks @reobin)
- Clipboard-copy feedback no longer stays visible after the related selection state has gone stale. (#443)
- The session navigator now uses live workspace labels, so renamed workspaces and cwd-derived labels stay current while navigating. (#377)
- Hermes Agent integration installs now preserve flat plugin-list settings instead of rewriting them into nested lists. (#479)
- Host-terminal focus redraws now stay pending until the client can send them, so panes refresh after focus returns even when redraw delivery was briefly busy.
- Numeric keypad keys that send VT100 application-keypad escape sequences now enter their digits and operators instead of being dropped. (#493)
- Codex panes now stay marked working when the live status header uses reasoning-summary text such as `Investigating code output` instead of the literal `Working` label. (#501)
- Codex blocker detection now ignores stale prompt text outside the live prompt region, reducing false blocked states from old scrollback.
- Native pane URL clicks now use Cmd-click on macOS and Ctrl-click on other platforms. (#307)
- Worktree open, create, and remove actions now work from bare repositories instead of assuming a normal checkout. (#497)
- Pane mouse handling no longer sends empty PTY writes for mouse events that produce no terminal input. (#496)
- Pane output now renders flag emoji and other multi-codepoint grapheme clusters as complete symbols instead of blank cells. (#243)
- Starting Herdr with no restored workspaces, or closing the last workspace, now opens a default workspace instead of leaving the client on an empty screen where direct keybindings such as `cmd+n` were shown but ignored. (#366)
- Resizing restored panes no longer aborts the server when libghostty-vt reflows a terminal whose pre-resize cursor row is past the new height. (#465)
- Full-screen TUIs such as Neovim now receive resize-generated terminal responses after Herdr internal pane resizes, so grown panes redraw without waiting for extra input. (#471)
- Nested agent session reports from child terminals no longer overwrite the owning pane's restored agent session id. (#511)
- Headless servers now avoid repeated scrollback rendering work for inactive panes, reducing CPU in large sessions. (#512)
- Mouse-click handling now respects `ui.prompt_new_tab_name`, so mouse-created tabs follow the same naming prompt setting as keyboard-created tabs. (#521, thanks @imrajyavardhan12)
- Pasting now works in modal text inputs, including rename prompts, command prompts, and worktree dialogs. (#302)
- Linux clipboard image reads now validate image payloads before accepting them, preventing malformed clipboard data from reaching pane image paste flows. (#534)

### Added
- Added remote auto-updates for agent detection manifests, with per-agent validation, local override precedence, `herdr server agent-manifests` diagnostics, and explain output showing remote manifest status.
- Added `herdr server update-agent-manifests` to fetch remote agent detection manifests immediately, reload the running server, and print the updated manifest status.
- Added `herdr agent explain` to show the manifest source, matched rule, evaluated matcher and region evidence, visible evidence flags, skipped-update reason, and idle fallback reason for live panes or saved screen fixtures.
- Added `herdr integration install kimi` for Kimi Code CLI hooks that report lifecycle state and session ids through Herdr's socket API. When native agent session restore is enabled, Herdr can resume Kimi panes with `kimi --session <id>`. (#431, #463, thanks @wbxl2000)
- Added `herdr integration install droid` for Factory Droid hooks that report session ids through Herdr's socket API. When native agent session restore is enabled, Herdr can resume Droid panes with `droid --resume <id>`.
- Added `herdr integration install kilo` for Kilo Code CLI plugins that report lifecycle state and session ids through Herdr's socket API. When native agent session restore is enabled, Herdr can resume Kilo panes with `kilo --session <id>`.
- Added `herdr integration install cursor` for Cursor Agent CLI hooks that report session ids through Herdr's socket API. When native agent session restore is enabled, Herdr can resume Cursor panes with `cursor-agent --resume <id>`. (#506, thanks @udirom)
- Added directional pane swap with `prefix+shift+h/j/k/l`, a pane context-menu swap action, pane layout/neighbor/edge/focus/resize socket APIs, matching CLI commands, and optional `pane split --ratio` support. (#330, #421)
- Added `herdr pane zoom` and the `pane.zoom` socket API to toggle, set, or clear tab-local pane zoom from scripts and integrations.
- Added toast ergonomics controls for delayed agent notifications, in-app toast placement, copied-to-clipboard feedback, and the `notification.show` socket API with `herdr notification show` and optional `none`, `done`, or `request` sounds. (#486)

### Changed
- OpenCode installed with the current Herdr plugin now reports lifecycle state directly instead of relying on screen manifest detection. Kimi Code CLI `0.14.0` or newer now reports full lifecycle state through hooks, including interrupts. Droid and Qoder CLI now report native session identity while leaving lifecycle state to screen manifest detection.

## [0.6.8] - 2026-06-04

This is a hotfix release for v0.6.7, prioritizing a server-crash fix for panes that print complex Unicode or emoji output.

### Fixed
- Fixed a Herdr server crash triggered by pane output containing complex Unicode, emoji, or decomposed accent graphemes. Affected sessions could lose running pane processes or crash again after restore if the same saved pane output was replayed. (#453)
- Direct installs managed by mise now update through the mise install path instead of failing to replace the active binary.
- Claude Code panes that are actively thinking or streaming no longer flicker to blocked because of custom status text. (#409)
- Claude Code panes now detect running shell-command status more reliably.
- OpenCode installed through pnpm is now detected as `opencode` instead of being missed because the packaged executable is named `opencode.exe`. (#447)

### Added
- Added opt-in macOS input-source switching during prefix mode with `experimental.switch_ascii_input_source_in_prefix`, so users typing with a non-Latin IME can run prefix commands through an ASCII-capable input source and return to the previous input source when prefix mode ends. (#400, #434, thanks @sf-jin-ku)

## [0.6.7] - 2026-06-03

### Added
- Added a compact collapse control to the expanded sidebar so mouse users can collapse and expand the sidebar from visible controls. (#278, #291, thanks @turgaybulut)
- Added an opt-in preview update channel with `herdr channel set preview`, `[update].channel`, automated preview manifests, and GitHub prerelease publishing for users who want fixes before stable releases as Herdr transitions toward less frequent, more stable releases.
- Added a remote SSH bridge keepalive fallback. `herdr --remote` now generates a temporary SSH config that includes the user's SSH config first, then adds `ServerAliveInterval` and `ServerAliveCountMax` only when the user has not already configured keepalives. Set `[remote].manage_ssh_config = false` to disable this. (#354, #355, thanks @SunskyXH)
- Added `ui.right_click_passthrough_modifier` so a configured modifier such as `ctrl` can forward right-click hold and drag gestures to mouse-reporting pane apps while normal right-click still opens Herdr's pane menu. (#148)
- Added Kilo Code CLI automatic detection for idle, working, and blocked terminal states. (#270)
- Added `herdr integration install copilot` for GitHub Copilot CLI hooks that report native session ids through Herdr's socket API. Copilot state still comes from Herdr's screen detection because Copilot hooks do not provide complete lifecycle coverage. When native agent session restore is enabled, Herdr can resume Copilot panes with `copilot --resume=<id>`. (#232, #386, thanks @LaneBirmingham)

### Changed
- Native agent session restore is now enabled by default for supported panes with current official integrations. Set `[session] resume_agents_on_restore = false` to disable it.
- Claude Code, Codex, GitHub Copilot CLI, Droid, Kimi Code CLI, and Qoder CLI integrations now report session identity only. Native state for those agents comes from Herdr's screen detection, while Pi, OMP, OpenCode, Kilo Code CLI, Hermes Agent, and custom socket integrations can still report state.

### Fixed
- Large long-running sessions no longer hit the frame-streaming crash fixed by the vendored libghostty-vt update. (#276)
- Copy mode now preserves linewise selection after `shift+v` while moving the cursor. (#360, #389, thanks @reobin)
- Leaving copy mode now restores the previous scroll position, or returns to the bottom when copy mode started at the bottom. (#398, #410, thanks @reobin)
- Git branch labels now resolve correctly in repositories that use Git's reftable ref format instead of showing `.invalid`. (#384, #423, thanks @LaneBirmingham)
- The official Nix flake now builds on macOS by providing Darwin SDK discovery helpers and Darwin cctools to the vendored libghostty-vt build. (#405, #407, thanks @DeevsDeevs)
- Commands launched after `--`, such as `herdr agent start ... -- opencode --session <id>`, now preserve child argv flags instead of parsing them as Herdr flags. (#383)
- Pane apps that request any-motion mouse tracking now receive hover/move events, making Textual-style TUI mouse interaction more reliable inside Herdr. (#419)
- Claude Code background-agent wait text in scrollback no longer keeps an idle pane marked working after the background agent has completed.
- Claude Code and Codex transcript or expanded-detail viewers no longer publish a false idle state while the pane is still showing active agent status.
- Claude Code question prompts that use the arrow-glyph selector are now detected as blocked.
- Kiro sub-agent tool approval prompts are now detected as blocked instead of working. (#388)
- Shift-letter prefix bindings such as `prefix+shift+n` now work in legacy SSH terminal sessions that send uppercase letters without separate Shift metadata. (#312)
- Idle panes now avoid repeated full foreground-process scans, reducing idle CPU on sessions with many panes. (#439)
- Restored native agent sessions now resume across background workspaces and tabs after the first client provides terminal context instead of waiting until each pane is focused.
- Pane input no longer waits behind the PTY actor's idle read poll, restoring responsive typing at quiet shell prompts. (#379)
- Pane apps that query OSC 4 ANSI palette colors now receive the active terminal palette response, so OpenCode and similar TUIs can enable system-theme behavior inside Herdr. (#387)
- Pane apps that query terminal capabilities with XTGETTCAP now receive supported capability responses, improving feature detection in Neovim and similar terminal apps. (#393)
- Pane text selection now derives its highlight colors from the host terminal or active Herdr palette instead of forcing the theme's blue accent. (#298)
- `herdr channel set preview` and `herdr channel set stable` now update direct installs from the selected channel immediately, reject preview on Homebrew and Nix installs before changing config, and show package-manager guidance for managed installs.
- Plain `herdr update` and remote binary replacement now ask before stopping running sessions, avoid protocol-heavy prompt text, and leave the current install untouched when the user chooses not to stop active pane processes. Explicit `--handoff` update flows try live handoff without a second handoff prompt.
- Remote bootstrap now uses the remote shell only for PATH discovery and runs internal probes through `/bin/sh`, so `herdr --remote` can detect existing installs when the remote login shell is fish. (#396)

## [0.6.6] - 2026-05-31

### Added
- Custom command keybindings now accept an optional `description` field to provide user-defined descriptions shown in the keybind help panel instead of the default `'custom command'` label. (#362)

### Fixed
- The OpenCode integration no longer treats `session.created` or `session.updated` plugin events as idle signals, so active sessions stay marked working until OpenCode reports `session.status` or `session.idle`. (#351)
- New interactive panes now use login-shell startup on macOS by default so Homebrew and other login PATH setup is available, with `terminal.shell_mode = "non_login"` as an opt-out. (#350)
- Claude Code panes no longer stay blocked after stale permission-prompt reports when the visible screen has returned to idle or working state. (#349)
- Codex panes no longer stay working because stale `esc to interrupt` text remains above a visible idle prompt, and visible approval-review work is now preserved as working. (#352)
- Sidebar Git status refresh now deduplicates workspaces from the same checkout and reuses cached ahead/behind results when refs have not changed, reducing idle CPU from repeated `git` polling. (#353)
- Update prompts, toasts, and docs now distinguish installing a new binary from stopping or reattaching a running Herdr session to use it.
- Large restored sessions no longer leave restored or newly split panes without shells after startup, and live handoff keeps PTY ownership bounded to one master fd per pane. (#357)
- Pane shutdown no longer warns that a pane is still alive after the direct child has already exited and been reaped. (#338)
- Closing the last pane or tab in a parent worktree workspace now shows the existing confirmation before closing the whole worktree group. (#369)

## [0.6.5] - 2026-05-29

### Added
- Added pane copy mode at `prefix+[` with keyboard navigation, visual selection, and clipboard yank support. (#231)
- Added `foreground_cwd` to pane and agent API/CLI responses so integrations can inspect the active foreground process directory without changing the existing pane/workspace `cwd` semantics. (#345)
- Added read-only `agent_session` metadata to pane and agent API/CLI responses when official integrations report native session references.

### Fixed
- Live handoff now preserves terminal state when transferring supported running panes to a replacement server.
- WSL clipboard writes now prefer OSC 52 before WSLg clipboard tools, so mouse selection and double-click copy populate Windows clipboard history in Windows Terminal. (#333)
- Incomplete host terminal OSC default-color replies no longer get misread as Alt-key input and forwarded into panes, preventing interactive prompts such as `gh auth login --web` from aborting on split `ESC ]` input. (#279, #306, #344)
- Workspace rename prompts and background notifications now use live cwd-derived workspace labels instead of stale session labels. (#332)
- `herdr session stop` no longer fails on zero-duration socket timeouts when the stop deadline is nearly exhausted.
- Update preview instructions now wrap long package-manager commands instead of truncating the shell command suffix.
- Restored native agent resume panes now fall back to a shell when the resumed agent exits instead of closing the whole pane.

## [0.6.4] - 2026-05-27

### Fixed
- Fixed macOS server startup with large restored sessions by raising the server file descriptor soft limit, preventing new panes from failing with `dup of fd N failed` or `Too many open files` around 40 live panes. (#327)

This is a hotfix for v0.6.3. See the v0.6.3 notes for the full feature release.

## [0.6.3] - 2026-05-27

### Added
- Added native agent session restore behind `[session] resume_agents_on_restore`, allowing supported Pi, Claude Code, Codex, OpenCode, and Hermes panes with current official integrations to restart into their previous agent conversation after a Herdr server restart. (#233)
- Added opt-in pane screen history across full server restarts with `[experimental] pane_history = true` and Settings > Experiments > pane screen history. (#217, #248, thanks @icedac)
- Added a session navigator at `prefix+g` with a searchable workspace/tab/pane tree, agent state filters, mouse switching, and keyboard navigation. (#157)
- Added configurable navigate-mode movement bindings for workspace and pane navigation keys. (#193)
- Added a configurable `last_pane` keybinding action for tmux-style back-and-forth navigation to the last focused pane across workspaces and tabs. It is unset by default. (#287)
- Added scrollback support to direct agent terminal attaches. Mouse wheel and plain PageUp/PageDown now scroll the attached terminal viewport, while terminal apps that request mouse or alternate-scroll input still receive those events. The client/server protocol is now version 11.
- Added `ui.redraw_on_focus_gained` to keep the existing full redraw on outer-terminal focus gain by default while allowing users to opt out of the visible refresh. (#282)
- Added `ui.mobile_width_threshold` to configure the terminal width at which Herdr switches to the mobile single-column layout. (#317)
- Added `--handoff` for `herdr update` and `herdr --remote` to opt into live server handoff for supported running servers. Plain update and remote attach use the normal restart/stop flow by default.
- Added `pane.report_metadata` and `herdr pane report-metadata` so user hooks can customize pane titles, displayed agent names, compact status labels, and visible state labels without taking over integration-owned lifecycle or session state. (#36)
- Added tmux-style double-click token copy in panes, with temporary copy feedback and mouse passthrough preserved for terminal apps that request mouse input. (#142, #296, thanks @babymastodon)
- Added Ctrl-click URL opening inside panes for OSC 8 hyperlinks and visible `http://` or `https://` URLs when the host terminal sends the modified click to Herdr. (#307)
- Added Qoder CLI detection, terminal state heuristics, and `herdr integration install qodercli` hook support. (#308, #309, thanks @wayneleelwc)

### Fixed
- Remote bootstrap now downloads exact-version release assets for Homebrew and Nix clients instead of copying package-manager-managed local binaries into `~/.local/bin/herdr`.
- `website/latest.json` now stores asset URLs for archived releases under `releases[version].assets`, so remote bootstrap can fetch the current client version even when Homebrew and the top-level latest release are temporarily out of sync.
- App and server event queues no longer stall under load, improving delivery of pane and agent state updates. (#265)
- Agent status subscriptions now deliver already-matching states and event-hub notifications reliably for waits and automation. (#288, #295)
- Codex background terminal waits are detected more reliably, and idle agent checking uses less CPU. (#300)
- Split OSC 10/11 host color replies are buffered correctly, so terminal apps still receive host foreground/background color responses when replies arrive in chunks. (#306, #310)
- `herdr session stop` is more reliable when the server closes the socket early or stops without sending a full response.
- The OpenCode integration now releases pane ownership on plugin dispose, preventing stale integration state after OpenCode exits. (#314)
- Linux sound alerts no longer fall back to `aplay` for mp3 files, preventing static noise on systems without `paplay`. Herdr now tries mp3-capable players such as `pw-play`, `ffplay`, `mpg123`, and `mpv` instead. (#290)

## [0.6.2] - 2026-05-23

### Added
- Added optional Nix flake support for building, running, installing, and developing Herdr with Nix. (#208, #221, #264)
- Added `terminal.new_cwd` to choose whether new panes, tabs, and workspaces follow the source pane/workspace, start in `$HOME`, use Herdr's process directory, or use a fixed path.
- Added `herdr integration install omp` for OMP's `.omp` extension directory. The extension reports OMP pane state through Herdr's socket API without relying on native `omp` process detection.
- Added CLI and socket API support for Git worktrees with `herdr worktree list/create/open/remove`, optional worktree provenance on workspace responses, and client/server protocol version 10.

### Fixed
- GitHub Copilot CLI sessions now use tested terminal heuristics for approval prompts, freeform input, plan review, and thinking states in the Agents panel. (#232, #256, thanks @LaneBirmingham)
- Kiro approval prompts are now detected as blocked in the Agents panel. (#255)
- Workspace labels now follow the live pane working directory after directory changes.
- Remote clients using local keybindings no longer show stale server keybinding warnings from the remote host.

## [0.6.1] - 2026-05-22

### Added
- Added `ui.mouse_scroll_lines` to configure how many pane scrollback lines each mouse wheel notch scrolls. The default remains 3. (#236)
- Added `--remote-keybindings local|server` for `herdr --remote`. Remote attach now uses the launching client's local keybindings by default without copying config files to the remote host; use `--remote-keybindings server` to keep the remote server's keybindings. The client/server protocol is now version 9.
- Added `experimental.reveal_hidden_cursor_for_cjk_ime = false` (opt-in), `experimental.cjk_ime_agents = []` (optional allow-list), and `experimental.cjk_ime_cursor_shape = "steady_block"` to expose the focused pane's cursor anchor to the outer terminal even when the pane requested `?25l`, restoring macOS IME candidate-window tracking for TUIs that paint their own cursor (Claude Code, pi, codex). When `cjk_ime_agents` is non-empty, the reveal applies only to focused panes whose detected agent matches one of the listed names. When the pane reports no cursor position, the anchor falls back to the pane's top-left so a stable IME hint is always available. Trade-off when enabled: an extra hardware cursor may appear in the outer terminal for apps that hide the cursor without painting a replacement. (#149, thanks @ChihGodlee)
- Added explicit sidebar Git worktree groups plus native worktree creation, existing checkout open, and safe checkout cleanup flows, configured by `[worktrees].directory`, `keys.new_worktree`, optional `keys.open_worktree`, and optional `keys.remove_worktree`. (#137)
- Added named-session reattach and stop command hints so detach and update guidance point back to the active session. (#199, thanks @Golden-Pigeon)

### Fixed
- Pane apps that query OSC 10/11 default foreground/background colors now receive the host terminal colors, so OpenCode and similar TUIs can detect light terminal themes inside Herdr. (#253)
- Codex Plan mode question prompts now override stale integration `working` reports when the visible terminal UI is clearly waiting for an answer, stale hook authority is cleared when foreground process detection sees Codex exit back to the shell, and Claude Code cancellations now recover from stale hook `working` reports when the idle prompt returns. (#249)
- Keybinding parsing now accepts non-ASCII printable keys such as `ö`, `é`, and `ğ`, including UTF-8 Alt chords. (#247)
- Kimi Code CLI sessions now use structural terminal detection for approval prompts and live thinking/tool status, improving working and blocked state reporting in the Agents panel. (#215)
- Antigravity CLI (`agy`) sessions are now detected, and their terminal UI now reports working and blocked states in the Agents panel. (#207)
- Cursor Agent sessions launched as `cursor-agent` or symlink aliases such as `agent` are now detected, and their terminal UI now reports working and blocked states in the Agents panel. (#225)
- Agent detection now ignores runtime argument strings when identifying foreground processes, reducing false positives from helper commands and wrapped processes. (#238)
- In-app notifications now stay below interactive floating overlays, so dialogs and menus remain readable and clickable while a toast is visible. (#228)
- `herdr --remote` now offers to restart the remote server after installing or replacing a remote binary, or when the running server version differs, even if the client/server protocol is still compatible.

## [0.6.0] - 2026-05-20

### Added
- Added keybinding v2 with explicit `prefix+...` syntax, array bindings per action, configurable prefix-mode pane focus, tab switching, and direct modified chords for users who opt in. (#154, #201, #202, #219)
- Added `herdr config reset-keys` to back up `config.toml` and remove custom keybindings so built-in v2 defaults apply on restart or config reload. (#154)
- Added an integrations tab in settings and first-run onboarding so users can install recommended agent integrations from inside Herdr.
- Added update badges on the sidebar menu, settings menu item, and integrations settings tab when installed integrations are outdated.
- Added `terminal.default_shell` to choose the executable used for new interactive panes. When unset, Herdr still falls back to `$SHELL`, then `/bin/sh`. (#196)
- Added native Kiro CLI detection with idle and working state heuristics. (#185)

### Fixed
- Keybinding conflict warnings now stay visible and show one readable yellow row per conflicting binding.
- Update prompts that need to stop a running server now default Enter to yes and show `[Y/n]`.
- Pending release notes no longer open automatically on startup; the latest notes remain available from the menu.
- Running `herdr server` directly now prints socket and log paths and explains that normal TUI users should run `herdr`.
- Kitty graphics virtual Unicode placeholders now render image placements instead of leaving placeholder cells behind. (#136)
- Clipboard image reads are now capped to Herdr's image payload limit, preventing oversized local clipboard images from being read into memory.
- The install script now reads Herdr's public latest-release manifest, so fresh installs use the same binary URLs as `herdr update`.
- The Claude Code integration no longer lets subagent completion hooks report durable `working`, preventing delayed recap or subagent completion events from reviving an idle pane. (#198)
- Remote clients now bridge local clipboard images into the remote pane by staging them as temporary image files and pasting the remote path, so Claude Code image paste works over `herdr --remote`. (#205)

### Breaking Changes
- Removed the separate `keys.quit` binding. Use `keys.detach`, which detaches in server mode and exits in `--no-session` mode. The default detach binding is now `prefix+q`.
- Keybindings now use explicit trigger syntax: `prefix+c` means prefix mode, while `ctrl+alt+c` is direct. Bare printable direct bindings such as `new_tab = "c"` are rejected with diagnostics because they intercept normal typing. The default keymap now gives tmux-style tab actions to `prefix+c`, `prefix+n`/`prefix+p`, and `prefix+1..9`, uses `prefix+w` for workspace navigation, and moves pane focus to `prefix+h/j/k/l`. (#154)
- The client/server protocol is now version 8. Stop and restart any running v0.5.12 server before attaching with this release.

## [0.5.12] - 2026-05-19

### Fixed
- The Claude Code integration no longer reports successful or failed post-tool hooks as `working`, and installing the updated integration removes Herdr's deprecated post-tool hook entries from existing Claude settings. (#198)
- The Codex integration now reports native `PermissionRequest` hooks as `blocked`, so permission prompts no longer stay pinned as `working` after a tool-use hook. (#198)
- Workspace and tab rename prompts now handle Backspace, Ctrl+Backspace, Alt+Backspace, Cmd+Backspace, Ctrl+H, Ctrl+W, and Ctrl+U as editing shortcuts instead of inserting stray characters or clearing unexpectedly. (#204)

## [0.5.11] - 2026-05-19

### Added
- Added the `terminal` built-in theme, which uses the host terminal's ANSI palette for Herdr UI colors. (#140, #146, thanks @babymastodon)
- Added Hermes Agent foreground-process detection with basic idle, working, and blocked heuristics. (#144)
- Added a Hermes Agent plugin integration for direct state reporting. (#144)
- Added `ui.sidebar_min_width` and `ui.sidebar_max_width` to configure the sidebar's expanded resize bounds. Defaults remain 18 and 36 columns; existing configs are unchanged. (#132, #135, thanks @ChihGodlee)

### Fixed
- Running the internal `herdr client` command from inside Herdr now respects the nested-launch guard, and the command is no longer advertised in root help. (#187)
- The Herdr agent skill now refuses to claim pane ownership unless it is running inside Herdr. (#152)
- Terminal-style docs code blocks now keep their copy button in the top-right corner. (#190)
- The sidebar `new` workspace button now aligns with the sidebar's left padding. (#189)
- Herdr now preserves `session.json` symlinks when saving persistent session state. (#139, #147, thanks @cloudmanic)
- Alt+Backspace is now preserved when forwarded into panes. (#155, #165)
- Directional pane focus now works while a tab is zoomed. (#151, #167)
- Agent detection now prefers the foreground process group leader, reducing false matches from child helper processes. (#161, #172)
- Remote attach now uses a matching `herdr` already available on the remote `PATH` before installing a new copy. (#170)
- Modified Enter input such as Shift+Enter is now preserved in supported terminals. (#168)
- Sidebar agent entries now show user-assigned agent names when available. (#145)

### Breaking Changes
- The client/server protocol is now version 7. Stop and restart any running v0.5.10 server before attaching with this release.

## [0.5.10] - 2026-05-17

### Added
- Added indexed keybind families under `[keys.indexed]` for jumping directly to workspace, tab, or visible agent positions 1-9.
- Added hook-owned custom agent status labels, so integrations can show short visual states like `indexing` without changing semantic agent status.
- Added terminal-backed agent commands and socket API methods for listing, reading, sending to, renaming, focusing, waiting on, attaching to, and starting agent terminals.
- Added direct terminal attach with `herdr agent attach <target>` and `herdr terminal attach <terminal_id>`.
- Added `ui.prompt_new_tab_name = false` for creating new tabs immediately with generated names instead of opening the rename dialog. (#123)
- Added optional `keys.edit_scrollback` to open the focused pane's retained scrollback in `$EDITOR` inside a temporary zoomed pane. (#122)

### Changed
- Renamed the focused pane fullscreen keybinding to `keys.zoom`; `keys.fullscreen` remains supported as a legacy alias.

### Fixed
- Grok Build is now detected as `grok`, with basic working, blocked, and idle state detection. Conflicting known-agent hook labels are ignored once native foreground-process detection identifies a different known agent. (#133)
- Terminal cursor shapes now forward through attached clients. (#116)
- Herdr now redraws immediately when the outer terminal regains focus.
- GitHub Copilot is now correctly detected when its process name is `copilot`. (#118)
- Integration installs now respect `PI_CODING_AGENT_DIR`, `CLAUDE_CONFIG_DIR`, and `CODEX_HOME` when choosing Pi, Claude Code, and Codex config paths. (#121)
- Split pane resize hit areas no longer overlap the first content column or row, making text selection work from the start of right and bottom panes. (#120)
- Dragging text selections near pane edges now autoscrolls into scrollback, and selection state now clears correctly when switching workspaces, tabs, or panes. (#128, #129, thanks @leeeanh)
- Zoomed panes now keep their border visible in tabs that contain multiple panes. (#115)

## [0.5.9] - 2026-05-15

### Added
- Added experimental Kitty graphics rendering for local panes and attached clients behind `experimental.kitty_graphics`, including support for larger graphics frames.
- Added `ui.toast.delivery = "system"` for OS-level background notifications, using `notify-send` on Linux and `terminal-notifier` or `osascript` on macOS.
- Added light variants for Catppuccin, Tokyo Night, Gruvbox, One, Solarized, Kanagawa, and Rosé Pine themes.
- Added `ui.mouse_capture = false` for tmux-style mouse behavior, letting the terminal handle normal clicks while still forwarding mouse input to pane apps that request it.

### Changed
- Moved experimental settings into `[experimental]`.

### Fixed
- PageUp and PageDown now scroll Herdr pane scrollback for normal panes while still forwarding keys to full-screen or mouse-reporting apps.
- Enhanced tilde key sequences now parse correctly, improving compatibility with terminals that emit them.
- `herdr integration install codex` now enables the current Codex `[features] hooks = true` flag and migrates the deprecated top-level `codex_hooks` flag.

### Breaking Changes
- `advanced.allow_nested` has moved to `experimental.allow_nested`; update configs that allow nested Herdr launches.
- The client/server protocol is now version 5. Stop and restart any running v0.5.8 server before attaching with this release.

## [0.5.8] - 2026-05-12

### Added
- Added manual pane labels through `herdr pane rename`, the `pane.rename` socket API, an optional `keys.rename_pane` binding, and the right-click pane menu.
- Added `ui.show_agent_labels_on_pane_borders`, which can show detected or reported agent names in split pane borders when no manual pane label is set.
- Added `herdr integration status [--outdated-only]` so installed agent integrations can be checked for legacy or outdated versions.
- Added an optional `keys.open_notification_target` binding for jumping to the pane behind the current notification.
- Added optional `keys.previous_agent` and `keys.next_agent` bindings for cycling through sidebar agent entries.

### Changed
- Scrolling over the tab bar now switches tabs directly, including overflowing tab bars.

### Fixed
- Indexed terminal palette colors now render correctly for 256-color terminal apps.
- Hook-based agent integrations now reject stale out-of-order reports and base notifications on effective agent state, reducing duplicate or stuck state changes.
- Background tabs now resize when the outer terminal size changes, preventing stale pane dimensions when switching back to them.
- Client shutdown now drains queued control messages more reliably.
- Pane cursors are now hidden while scrolled back, and omitted while the mobile switcher is open.
- Mobile agent switcher entries now include tab context, making agents easier to identify on narrow terminals.
- macOS foreground job detection now uses process groups, improving agent state tracking for foreground commands.
- Remote SSH no longer fails before connecting when macOS temporary bridge socket paths exceed Unix socket length limits. (#103, thanks @moonsphere)
- Nix-wrapped agent commands are now detected by their underlying agent entrypoint.
- Pane renames made through the socket API now rerender immediately.

## [0.5.7] - 2026-05-10

### Added
- Added ANSI-formatted pane reads to the CLI and socket API with `herdr pane read --format ansi` / `--ansi`, preserving colors and styles for visible and recent pane output.

### Changed
- The agents panel now highlights the currently focused agent entry, matching the active workspace styling. (#84, thanks @soomtong)

### Fixed
- Git branch and ahead/behind refreshes now run off the main loop, preventing slow Git status checks from freezing the UI.
- Update and startup flows now detect incompatible running servers earlier and give clear stop/restart guidance instead of trying to attach with a mismatched client/server protocol.
- `herdr update` now downloads and prepares the new binary before stopping a running server, reducing the chance of interrupting an active session when download or install preparation fails.

## [0.5.6] - 2026-05-09

### Added
- Added the `vesper` built-in theme. (#71, thanks @nexxeln)
- Added `herdr --remote <ssh-target>`, so you can use Herdr as a thin client for remote servers without SSHing in first. Herdr connects over SSH, bootstraps a matching remote `herdr` binary when needed, starts the remote server automatically, and streams an efficient terminal view back to your local terminal.

### Changed
- Updated the bundled `libghostty-vt` engine and removed the custom Linux C++ runtime link workaround from static builds.
- CLI workspace, tab, and pane creation now preserve the current focus by default; pass `--focus` to switch to the newly created item.

### Fixed
- OSC 8 hyperlinks emitted inside panes now remain clickable after Herdr renders them, including titled markdown-style links.
- Agent panel scope now defaults to `all` and is saved to config when changed, so choosing `current` or `all` survives session resets and upgrades.
- Native agent hook state now clears when the detected native agent exits, preventing stale hook-reported status from sticking to a pane.
- Clicking an in-app agent toast now jumps to the relevant pane and clears the toast after focus.

## [0.5.5] - 2026-05-06

### Added
- Added a mobile layout for narrow terminals, making it practical to SSH into your machine and run herdr from your phone.

### Fixed
- Non-ASCII terminal input is no longer dropped when UTF-8 characters arrive split across multiple reads.
- Native agent detection now clears agents after their foreground process exits and control returns to the shell, preventing stale agent status in the sidebar.
- Pane contents no longer shift horizontally when scrollback appears, keeping the scrollbar gutter stable.

## [0.5.4] - 2026-05-03

### Fixed
- Visible active-tab panes that finish while the outer terminal is unfocused are now marked as seen when you return to herdr, preventing stale done/attention indicators.
- IME candidate windows and mobile SSH cursor tracking now stay anchored to the focused pane during client redraws, including apps that hide the cursor, instead of drifting to sidebar or repaint positions.

## [0.5.3] - 2026-04-30

### Added
- Added named persistent sessions, so you can keep separate herdr environments for different projects or contexts while sharing the same global config. See the docs for the full session CLI. (#57, thanks @fbettag)
- Added `herdr status`, `herdr status server`, and `herdr status client` to inspect the local client, running server, protocol compatibility, socket path, and whether a restart is needed.

### Changed
- Focused panes can now still alert you through terminal notifications when the herdr terminal window is unfocused, so active work does not go quiet just because you switched to another app.

### Fixed
- Dragging pane split borders now works when the app inside the pane has mouse reporting enabled, including Claude Code no-flicker mode. (#61, thanks @EYH0602)
- Pressing the prefix key twice now forwards a literal prefix key into the focused pane in client mode again.
- `herdr integration install` and `herdr integration uninstall` now work without requiring a running herdr server.
- Pane PTYs now keep their last attached size while detached, preventing detached output from being resized or rewrapped to fallback dimensions.

## [0.5.2] - 2026-04-27

### Added
- Config can now be reloaded in the running app/server from the global menu or with `herdr server reload-config`, applying safe live settings without restarting the persistent server.

### Fixed
- Persistent server startup now surfaces config diagnostics in attached clients instead of silently hiding parse or validation errors.
- Pane backgrounds now stay transparent when the host terminal background color is unknown, while explicit terminal cell backgrounds still render correctly.
- Persistent-session toast and sound notifications now target the foreground attached client instead of firing across every connected client.
- Claude Code subagent hook events no longer make the parent Claude pane look idle or released when a subagent finishes, and permissioned tool-call completion keeps the pane in the correct working state.

## [0.5.1] - 2026-04-25

### Added
- Toast notifications can now be delivered through the outer terminal as desktop notifications. Configure this with `ui.toast.delivery = "terminal"`; see the [configuration docs](https://herdr.dev/docs/configuration/) for details.
- Herdr now writes separate capped support logs for app, client, and server modes, making persistent-session issue reports easier to diagnose without unbounded log growth.
- The bundled opencode plugin now reports question prompts as blocked while waiting for user input, then returns to working or idle when answered or dismissed. Question prompts are also detected by the default terminal-screen heuristics. (#51, thanks @mspiegel31)

### Changed
- Routine API request traces now log at debug level by default, making normal support logs smaller and easier to read while preserving detailed traces when debug logging is enabled.

### Fixed
- Pasted text and other reverse-video terminal content now stays readable when pane backgrounds are transparent. (#45, thanks @EYH0602)
- Panes now advertise a stable `TERM=xterm-256color` and `COLORTERM=truecolor` by default, improving redraw and cursor behavior in shells and remote sessions.
- Pane scrollbars once again reserve their own rightmost column instead of overlaying terminal content in persistent session mode.
- Terminal-delivered toast notifications now use the server-approved delivery decision in persistent session mode, so attaching clients do not incorrectly suppress them.
- In-app toast delivery now stays inside herdr instead of also forwarding a terminal/desktop notification.

## [0.5.0] - 2026-04-21

### Breaking Changes Please Read
- herdr now defaults to a persistent server/client session model. running `herdr` starts or reattaches to a background session server instead of launching the old single-process UI.
- quitting the UI in default mode now detaches the current client and leaves the shared session running. use `herdr server stop` to stop the background server explicitly.
- the old monolithic behavior is still available as an escape hatch with `herdr --no-session`.

### Added
- Persistent sessions are now the default product behavior. You can detach and reattach without stopping pane processes.
- Added the thin client and headless server as first-class product components, including auto-detect launch, explicit `herdr client`, and `herdr server stop`.
- Sessions now restore cleanly after full restart, preserving workspaces, tabs, panes, and running process state.
- Multi-client attach is now supported. Multiple clients can connect to the same shared session.

### Changed
- In persistence mode, in-app quit actions now detach the current client by default instead of shutting down the whole background server.
- The current persistence model is a shared session view across attached clients. It is not yet full tmux-style per-client independent navigation.
- Restored sessions now land in terminal mode, while fresh sessions still start in navigate mode.

## [0.4.11] - 2026-04-16

### Breaking Changes Please Read
- The update flow changes in `0.4.11`. Herdr no longer installs updates silently in the background. Starting with this release, herdr only checks for updates and shows them in the UI. To install a new release, quit herdr and then run `herdr update` manually in your shell.
- This prepares the upcoming `0.5.0` persistence release. Herdr is moving from the old single-binary update model toward a persistent server/client session model, so your workspace can keep running while clients attach, detach, and reconnect.
- The reason for this change is upgrade safety. Herdr needs to stop the old running process cleanly before the new client/server model takes over, so manual update avoids mixed-version states during the transition.

### Added
- Hook-reported agent state can now use custom agent labels, so integrations are no longer limited to herdr’s built-in agent names. Custom labels now flow through pane/workspace UI and the socket API anywhere agent names are shown.

## [0.4.10] - 2026-04-14

### Added
- Prefix mode now supports custom command keybindings via `[[keys.command]]`, so you can launch detached shell helpers or open temporary overlay panes from inside herdr using the active workspace, tab, pane, and cwd context.
- Pressing the prefix key twice now forwards a literal prefix keystroke into the focused pane, which makes nested tools and terminal apps that use the same prefix easier to control.

### Fixed
- App-level key handling now normalizes enhanced keyboard reporting consistently, so shifted bindings and text like `?` and uppercase characters work correctly in navigate mode and text-entry UI.
- Ctrl+letter input is now encoded correctly when pane apps enable kitty keyboard mode, improving compatibility with terminal programs that expect CSI-u style key reporting.
- The collapsed sidebar now keeps the active workspace visibly highlighted even while you stay in terminal mode.
- Droid Mission Control screens are now treated as idle instead of active work, reducing false busy-state detection.

## [0.4.9] - 2026-04-13

### Fixed
- Droid's primary-screen redraws no longer erase pane scrollback inside herdr, while normal scrollback-clear behavior is preserved elsewhere.
- `q` is now dedicated to quitting in navigate mode instead of also acting as a generic cancel key in modals and overlays, reducing accidental quits.
- Tab bar scrolling is tighter: the scroll-right button and new-tab button now sit directly adjacent to the last visible tab without a gap, and manual scroll no longer overscrolls past the last tab.

## [0.4.8] - 2026-04-12

### Added
- Themes can now set `panel_bg = "reset"` to let herdr’s panel chrome inherit the host terminal background instead of painting an opaque panel fill. This also accepts the aliases `default`, `none`, and `transparent`.
- Ghostty-backed panes now preserve the host terminal’s default background when it matches the outer terminal theme, so terminal window transparency can show through pane content instead of being repainted as an opaque color.

### Fixed
- Clipboard writes now prefer native platform clipboard tools (`pbcopy`, `wl-copy`, `xclip`, or `xsel`) before falling back to OSC 52, which makes copy operations from panes more reliable across terminal setups.

## [0.4.7] - 2026-04-10

### Added
- The tab bar now handles large tab sets better: you can scroll overflowing tabs with the mouse controls or wheel, and reorder tabs by dragging them.
- `workspace create` and `tab create` now return the created root pane in their JSON response, so automation can act on the new pane immediately without an extra lookup.

### Fixed
- Background panes that start idle no longer show up as `done` or trigger finished-state attention until they have actually transitioned from working or blocked to idle.
- Left-click now focuses panes and right-click now opens the pane context menu even when the inner TUI has mouse reporting enabled, fixing apps like Claude Code. (#25, thanks @othavioquiliao)
- OSC 52 clipboard writes from apps running inside panes now reach the host clipboard correctly, including copy requests emitted by child processes inside the pane.
- `pane close` now removes only the targeted tab when other tabs still exist in the workspace, instead of closing the whole workspace.
- Amp approval prompts are now detected more reliably as blocked, including tool-call, command, and file edit/create approval screens.

### Breaking Changes
- Socket API clients that match `result.type` exactly need to handle `workspace_created` and `tab_created` for `workspace.create` and `tab.create`; these calls no longer return `workspace_info` and `tab_info`.

## [0.4.6] - 2026-04-09

### Fixed
- Agent state detection is now more reliable when panes are scrolled back, when Codex is running in narrow panes, and when Claude opens slash-command or settings menus, reducing false blocked or idle states.
- Mouse-driven terminal text selection now autoscrolls into pane scrollback and clears cleanly after copy, so selecting beyond the visible viewport works as expected.
- Pane terminal colors now return to the outer terminal theme after fullscreen TUIs exit, fixing cases like Droid leaving stale background colors behind. This restore path now also works correctly on macOS.

## [0.4.5] - 2026-04-09

### Added
- `herdr workspace create` and `herdr tab create` now support `--label`, so scripts and agents can name new workspaces and tabs immediately instead of creating them first and renaming them afterward.
- The global menu now includes a manual **reload keybinds** action, so you can apply `config.toml` keybinding changes without restarting herdr.
- The socket API and CLI now expose a `done` agent status, including `herdr wait agent-status --status done`, so automation can distinguish finished agent runs from panes that are merely idle.

### Changed
- Session state is now saved automatically with a debounce while you work, so recent workspace, tab, pane, and sidebar changes are preserved more reliably even if herdr exits unexpectedly.

### Fixed
- Only the focused pane now owns the terminal cursor, which removes stray cursor blocks from unfocused panes.
- In-app **What's New** / release notes now render inline code spans and fenced code blocks correctly.
- Default numbered tabs now stay auto-named when you keep or rename them back to their numeric label, so generated tab numbering stays compact and predictable.

## [0.4.4] - 2026-04-08

### Changed
- The expanded sidebar can now be split into resizable workspace and agent sections with a draggable divider, and that section sizing is preserved across restarts.

### Fixed
- IME input now works properly for Chinese and other UTF-8 input methods in pane terminals, so candidate selection no longer falls back to typing raw digit keys. (#9, thanks @Edmund-a7)
- `herdr pane run ...` now uses the bracketed-paste-aware input path, improving compatibility with shells and terminal apps that expect pasted command text to arrive atomically.
- The local socket API is more robust and secure: its Unix socket is now restricted to the current user, and long-running output waits and subscriptions stop cleanly on disconnect or shutdown instead of hanging indefinitely.

## [0.4.3] - 2026-04-07

### Fixed
- Update checks and in-app **What's New** release notes no longer depend on GitHub’s release API, which avoids the transient 403 failures from the previous update path.
- `herdr pane run ...` now submits the full command atomically in one request, fixing cases where scripted commands did not reliably execute because the final Enter was sent separately.
- Bare line-feed input is now preserved in raw terminal input instead of being normalized to Enter, fixing Linux terminal cases where inputs like Shift+Enter or Ctrl+J could be interpreted incorrectly.

## [0.4.2] - 2026-04-07

### Added
- The expanded sidebar agent panel can now switch between the current workspace and all workspaces, so you can scan and jump to agents across the whole session.
- The collapsed sidebar now shows compact per-pane agent indicators, so you can keep an eye on agent activity without reopening the full sidebar.

### Changed
- The sidebar now handles larger workspace sets more cleanly: the workspace section has headers, its own scrolling, better-aligned drag/drop slots, and manual width changes persist across restarts. Double-clicking the divider resets it to the configured default width.
- Pane scrollback is now configured with `advanced.scrollback_limit_bytes`, matching Ghostty's byte-based scrollback limit. Set it to `0` to disable pane scrollback entirely. The old `advanced.scrollback_lines` key is still accepted as an alias, but it now uses the same byte-based value.
- Linux release binaries now ship with libghostty SIMD enabled again without reintroducing the musl startup issue, restoring the optimized Linux build path.

### Fixed
- Typing in pane terminals on macOS is responsive again after the Ghostty migration, by keeping a persistent per-pane Ghostty key encoder instead of rebuilding it on every keypress.
- The collapsed sidebar expand toggle works again.
- Creating a new tab now waits until you confirm the dialog, so cancelling the new-tab flow no longer leaves behind an unwanted tab.
- Copying selected pane text now uses Ghostty's native selection extraction, which preserves wrapped text and wide characters more accurately.
- Session restore is more tolerant of older and current snapshot formats, including pre-tab session files.

## [0.4.1] - 2026-04-06

### Fixed
- Fixed Linux release binaries crashing on startup.

## [0.4.0] - 2026-04-05

### Major Changes
- Herdr now uses a Ghostty-backed terminal engine as its pane runtime.
- The legacy vt100 pane backend has been removed, making Ghostty the single terminal backend going forward.

### UX and Interaction
- Workspaces can now be reordered by dragging them in the sidebar.
- Notification sounds now support custom mp3 file overrides, with either one shared file or separate files for finished vs needs-attention alerts.

### API and Integration
- Workspace API ids are now stable, making socket and CLI automation more predictable across workspace changes and restores.

### Packaging and Runtime
- macOS builds now statically link the vendored `libghostty-vt`, preserving the single-binary install and update flow.

## [0.3.2] - 2026-04-03

### Changed
- The global launcher now surfaces update-related actions more clearly: when release notes are available you can open **What's New**, and when an update has been downloaded you can **quit to apply update** directly from the menu.
- Release notes are now retained as the latest available notes after you dismiss the startup modal, so you can reopen them later from the UI instead of only seeing them once.

### Fixed
- Fixed held-key repeat in terminal panes on macOS terminals that send explicit repeat events through the enhanced keyboard protocol, restoring continuous backspace, character, and arrow-key repeat without letting modal close/confirm key repeats leak into the shell.

## [0.3.1] - 2026-04-03

### Added
- New tabs now open directly into the rename flow, with the default tab name prefilled and replaced on first type so you can name tabs as you create them.

### Changed
- Polished modal layout and spacing across onboarding, settings, keybind help, and release notes so overlays feel more consistent and their content/actions line up more cleanly.
- Debug builds now use separate runtime/config paths from normal releases, which avoids local development sessions colliding with your main herdr install.

### Fixed
- Starting a second herdr instance against an active socket now fails fast with a clear error instead of clobbering the running session.
- Fixed pane and agent state updates being dropped under internal event queue pressure, which could leave a pane showing stale status after work finished.
- Fixed onboarding modal sizing and click targets, and corrected release-notes scroll calculations when a scrollbar is present.

## [0.3.0] - 2026-04-03

### Major Changes
- Added tabs within workspaces, so a single workspace can now hold multiple terminal tab contexts with their own pane layouts.
- Added first-class tab support to the local socket API and CLI wrappers, including `herdr tab ...` commands and tab ids like `1:2` alongside workspace-scoped pane ids.
- Added built-in direct integrations for pi, claude code, codex, and opencode, plus authoritative hook-driven state reporting so supported agents can report semantic state directly instead of relying only on screen heuristics.
- Added a post-update release-notes screen so herdr can explain what changed after an update is installed.

### UX and Controls
- Added optional direct pane-focus keybindings for terminal mode, so you can switch panes with modifier shortcuts like `alt+h` or `alt+right` without entering navigate mode first.
- Reworked keybind discoverability so the in-app keybind help now shows all supported actions, including optional bindings that are currently unset.
- Keybind help now uses a centered scrollable modal with mouse and keyboard scrolling, matching the release-notes interaction model more closely.
- Popups and action-button interactions now use more consistent modal geometry and button semantics across the UI.
- Polished the sidebar agent section so it focuses on detected agents only and uses clearer two-line agent cards with more breathing room.

### Behavior Fixes
- Hook-driven agent state updates now stay correct in tabbed workspaces.
- Modifier-only keypresses no longer leak into panes as stray input.
- Multi-tab agent labels now include tab names when that extra context matters.
- Workspace identity now follows the first tab's root pane again instead of stale creation-time cwd.
- Background notification suppression is now tab-aware rather than workspace-wide, so background tabs in the current workspace can still alert correctly.

### Documentation
- Updated the README, configuration guide, integrations guide, skill, and socket API docs to reflect tabs, direct integrations, unset optional keybindings, direct terminal-mode navigation examples, workspace-scoped pane ids, and the current workspace identity/sidebar model.

## [0.2.4] - 2026-04-01

### Fixed
- Fixed a macOS-only startup misdetection where pi could briefly appear as codex in the sidebar because process environment entries were being parsed as command-line arguments.

## [0.2.3] - 2026-03-31

### Changed
- Mouse wheel handling now follows the tmux/Ghostty model more closely: fullscreen apps receive wheel input when they own scrolling, while herdr keeps host scrollback for panes that are behaving like a normal terminal transcript.
- Pane scrollbars now only appear when herdr has real host scrollback for that pane, instead of implying a host-managed scroll position for app-owned scrolling.

### Fixed
- Fixed Codex and pi panes becoming unscrollable in herdr by preserving recoverable host history for top-anchored normal-screen output, without relying on alternate-screen scrollback retention.
- Fixed pane wheel routing so apps using mouse reporting or alternate-scroll behavior can receive scroll input directly instead of having herdr always intercept it.

## [0.2.2] - 2026-03-31

### Fixed
- Fixed pane scrollbars so they reserve their own lane instead of drawing over terminal content, which makes scrolling and scrollbar dragging behave more cleanly in narrow panes.
- Fixed alternate-screen scrollback handling so full-screen terminal apps can preserve recoverable history inside herdr panes instead of losing rows that scroll off.
- Fixed Codex in herdr panes losing transcript/history while running in alternate screen, so past output remains scrollable instead of disappearing as the session grows.
- Hid the rendered terminal cursor while a pane is scrolled back, avoiding stray cursor blocks appearing in the wrong place during history navigation.

## [0.2.1] - 2026-03-31

### Added
- Herdr now checks for updates at startup and periodically while it stays open, so long-running sessions can still discover new releases without a restart cycle.
- Added a lightweight bottom-right toast when an update has been downloaded and is ready, with a simple restart-to-use-it flow.

### Changed
- Rendering is now driven more directly by app events instead of relying as much on polling, which makes the UI feel snappier and cuts unnecessary redraw work.

### Fixed
- Restored smooth fast spinner animation for working agents.
- Closing a pane or workspace now reliably terminates the processes running inside that pane session instead of leaving shells or child processes behind.
- Fixed bracketed paste handling so incomplete paste sequences are preserved across read timeouts instead of being dropped or misread.

## [0.2.0] - 2026-03-30

### Added
- Added a local Unix socket API for controlling running herdr sessions, including workspace and pane management, pane reads, text/key input, pane splitting, and output waits.
- Added event subscriptions over the socket API for workspace and pane lifecycle events, pane output matches, and agent state changes.
- Added CLI wrappers on top of the socket API with `herdr workspace ...`, `herdr pane ...`, and `herdr wait ...`, using compact public ids for scripting and agent orchestration.
- Added a settings popup with mouse support for changing themes, sound alerts, and toast notifications from inside herdr.
- Added 9 built-in themes: catppuccin, tokyo night, dracula, nord, gruvbox, one dark, solarized, kanagawa, and rosé pine.
- Added interactive pane scrollbars, manual sidebar resizing, and upstream git ahead/behind indicators in the workspace sidebar.

### Changed
- Redesigned the sidebar into a two-section layout that separates workspace-level triage from per-agent detail, making it easier to supervise multiple agents in parallel.
- Agent state names exposed in the UI and integration surfaces now use `working` and `blocked`.
- Herdr now blocks nested launches by default when started inside a herdr-managed pane; set `advanced.allow_nested = true` to opt back in.

### Fixed
- Improved terminal keyboard protocol parsing and input forwarding across terminal variants, including better handling for shifted printable keys.
- Fixed Ghostty on macOS misparsing some arrow-key and modifier/enhanced key sequences.
- Refined sidebar rollups and pane ordering so workspace status and agent lists stay more stable and predictable.

### Documentation
- Refreshed the README, socket API reference, and reusable agent skill docs to better explain herdr's agent multiplexer model and integration surface.

## [0.1.2] - 2026-03-28

### Added
- Added first-run onboarding flow that lets you choose notification preferences (sound and toast) on startup.
- Added optional visual toast notifications in the top-right corner for background workspace events (completion and attention-needed alerts).
- Added configurable keybindings for all navigate mode actions: new workspace, rename workspace, close workspace, resize mode, and toggle sidebar. See the [configuration docs](https://herdr.dev/docs/configuration/) for the full key reference.
- Added configuration validation with startup diagnostics. Invalid key combinations or duplicate bindings now fall back to safe defaults with a visible warning.

### Changed
- **Breaking:** Default prefix key changed from `ctrl+s` to `ctrl+b` to avoid common terminal flow control conflicts.
- Workspaces now derive their identity from the repository or folder of their root pane, updating automatically as you navigate. Custom names act as overrides rather than static labels.
- Sidebar now shows workspace numbers again in expanded view.
- Refined sidebar presentation with consistent marker/name/state ordering and comma-separated agent summaries.
- Keybinding parser now accepts special keys (`enter`, `esc`, `tab`, `backspace`, `space`) and function keys (`f1`–`f12`).

### Documentation
- Split configuration reference into dedicated configuration docs with full keybinding documentation and config diagnostics explanation.

## [0.1.1] - 2026-03-28

### Added
- Added optional sound notifications for agent state changes, including a completion chime when background work finishes and an alert when an agent needs input.
- Added per-agent sound overrides under `[ui.sound.agents]`, so you can mute or enable notifications by agent instead of using one global setting. Droid notifications are muted by default.

### Changed
- Request alerts now play even when the agent is in the active workspace, while completion sounds remain limited to background workspaces.

### Fixed
- Improved foreground job detection on Linux and macOS so herdr can recognize agents that run through wrapper processes or generic runtimes, including cases like Codex running under `node`.
- Made Claude Code state detection more stable by handling more spinner variants and smoothing short busy/idle flicker during screen updates.

## [0.1.0] - 2026-03-27

### Added
- Initial release.
