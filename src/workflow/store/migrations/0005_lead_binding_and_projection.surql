-- The agent-teams rework: a run stops being something karvex executes and
-- becomes **one Claude Code team-lead session** karvex launches and observes
-- (`docs/design/workflow-builder/09-agent-teams-rework.md` §0, §3.4, §3.7).
-- Three things had nowhere to live: the run's binding to that lead session,
-- the task facts projected out of Claude Code's shared task list, and the
-- member snapshots taken from the team config before Claude Code deletes it.
--
-- Twenty-three statements: the four numbered schema changes below, plus the
-- one backfill `UPDATE` item (2) needs. A `DEFAULT` is applied at write time,
-- not at read time (the `0002`/`0003`/`0004` precedent), so every pre-rework
-- `run_node` row would otherwise decode with the three new non-option columns
-- absent. The three are backfilled by a single `UPDATE` with three `SET`
-- clauses — one pass over `run_node` rather than three, which is the shape
-- `0003` used for exactly this reason.

-- (1) The lead binding (§3.1 step 4, §3.7 "Save"). A run is now identified
-- outwardly by the `claude` session it spawned: `lead_session_id` is what
-- `claude --resume` takes, `team_name` is what addresses
-- `~/.claude/tasks/<team>/` and `~/.claude/teams/<team>/`, and the pane and
-- terminal ids are karvex's own public handles on the lead's pane so the DAG
-- view's "focus the lead" affordance survives a server restart.
-- `lead_prompt_version` records which revision of the render contract (§3.2)
-- produced the prompt this lead was launched with, so a resume renders the
-- same contract rather than whatever karvex ships today.
--
-- All five are `option` and none is backfilled on purpose: a pre-rework run
-- was executed by karvex's own engine and never had a lead, so "no binding"
-- is the truthful reading of those rows rather than a value to invent (§2,
-- "we do not migrate or reinterpret pre-rework runs").
DEFINE FIELD lead_session_id     ON workflow_run TYPE option<string>;
DEFINE FIELD team_name           ON workflow_run TYPE option<string>;
DEFINE FIELD lead_pane_id        ON workflow_run TYPE option<string>;
DEFINE FIELD lead_terminal_id    ON workflow_run TYPE option<string>;
DEFINE FIELD lead_prompt_version ON workflow_run TYPE option<int>;

-- (2) The projected task facts (§3.4). One Claude Code task maps 1:1 onto a
-- `run_node` row, but three of its facts had no column:
--
--   * `task_id` — the id of the file under `~/.claude/tasks/<team>/`, e.g.
--     `"7"`. `option` because a planned node exists from `create_run` onward
--     and only acquires an id once the lead actually creates its task; a node
--     the lead never created keeps NONE, which is exactly the fact the DAG
--     view needs to show it as unstarted.
--   * `subject` — the observed task subject, stored verbatim rather than
--     re-derived from the definition. The lead is allowed to split, merge, and
--     reword (§3.2, "what loose means"), so the definition's label and the
--     subject the team actually worked under are two different facts and both
--     are worth keeping. `label` stays the authored/kvdag name.
--   * `owner` — the claiming teammate's name. Claim, reassignment, and
--     unblocking are Claude Code's now, so this is observed, never decided
--     here. Empty string means unclaimed, which is a real state in the source
--     data and not a missing value.
--   * `emergent` — true for a task the definition never planned. First-classed
--     rather than dropped: the drift *is* the record, and §3.7's `workflow
--     capture` promotes it back into a definition later.
DEFINE FIELD task_id  ON run_node TYPE option<string>;
DEFINE FIELD subject  ON run_node TYPE string DEFAULT "";
DEFINE FIELD owner    ON run_node TYPE string DEFAULT "";
DEFINE FIELD emergent ON run_node TYPE bool   DEFAULT false;

UPDATE run_node SET
    subject  = IF subject  = NONE THEN ""    ELSE subject  END,
    owner    = IF owner    = NONE THEN ""    ELSE owner    END,
    emergent = IF emergent = NONE THEN false ELSE emergent END;

-- (3) `run_member` — the snapshot of `~/.claude/teams/<team>/config.json`.
--
-- This table exists for one reason: **Claude Code deletes that directory when
-- the lead session ends** (§1, trap 1; §4's risk row). While the run is live
-- the file is authoritative and karvex only reads it; the instant the lead
-- exits it is gone, and these rows are the only durable record that the run
-- ever had teammates at all. Every later surface depends on them — the run
-- browser's per-node owner, §3.7's "resume an individual teammate's session",
-- and the sidebar's teammate names for a run that has already finished.
--
-- Snapshot semantics, not a journal: one row per `(run, name)`, rewritten in
-- place on every observed change, with `first_seen_at`/`last_seen_at`
-- bracketing the window the member was visible in. A teammate that
-- disappears from the config keeps its row and stops advancing
-- `last_seen_at`, so a member is never silently erased by the file that
-- erased it.
DEFINE TABLE run_member SCHEMAFULL;
DEFINE FIELD run          ON run_member TYPE record<workflow_run>;
-- the teammate name Claude Code addresses it by, and §3.2's naming rule asks
-- the lead to make match the node it owns
DEFINE FIELD name         ON run_member TYPE string;
DEFINE FIELD agent_type   ON run_member TYPE string DEFAULT "";
DEFINE FIELD model        ON run_member TYPE string DEFAULT "";
-- karvex's own public pane id, handed back through Claude Code's team state:
-- the team config's `tmuxPaneId` is a karvex pane id, because the teammate was
-- spawned through karvex's tmux shim (§1). `option` — the lead itself is
-- `in-process` and has no pane of its own here.
DEFINE FIELD pane_id      ON run_member TYPE option<string>;
-- `tmux` for a split-pane teammate, `in-process` for the lead; kept because it
-- is what decides whether a member is separately resumable (§3.7)
DEFINE FIELD backend_type ON run_member TYPE string DEFAULT "";
DEFINE FIELD is_active    ON run_member TYPE bool DEFAULT false;
DEFINE FIELD cwd          ON run_member TYPE option<string>;
DEFINE FIELD first_seen_at ON run_member TYPE datetime;
DEFINE FIELD last_seen_at  ON run_member TYPE datetime;
-- both timestamps are bound explicitly by the writer from the observation's
-- own stamp; neither carries a `DEFAULT time::now()`, which is the second
-- clock migrations `0002` and `0004` exist to keep out of this schema
DEFINE INDEX run_member_name ON run_member FIELDS run, name UNIQUE;

-- (4) `run_event.kind` gains `member` and `task`: the projection journals what
-- it observed, so a finished run can still explain when a teammate appeared
-- and when a task changed hands, long after both source directories are gone.
-- The existing list is copied verbatim and appended to — the assertion is the
-- schema's own record of every kind that has ever shipped.
DEFINE FIELD OVERWRITE kind ON run_event TYPE string
       ASSERT $value IN ["run_started","run_finished","node_created","node_started",
                         "node_status","node_output","tool_activity","plan",
                         "usage","message_delivered","steer","interrupt",
                         "expand_proposed","expand_accepted","expand_rejected",
                         "growth_limited","watchdog","checkpoint","succession",
                         "error","summary","member","task"];
