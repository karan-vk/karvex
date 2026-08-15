-- The agent-teams rework, continued: teammate session identity so a
-- finished run's review cycle can still resume the sessions that ran it
-- (`phase4-retarget-plan.md` §3.3, S1), and karvex's own opinion about a
-- stuck node, kept structurally separate from the projected task status
-- (§3.6, D-10).
--
-- Five statements, all additive `option` columns: nothing here needs a
-- backfill `UPDATE`, unlike `0003`/`0005`. Those migrations backfilled
-- because they added non-option columns with a `DEFAULT` that only applies
-- at write time (`0002`'s precedent); every column this migration adds is
-- `option`, so a pre-`0006` row simply reads back `NONE` for it, the same way
-- `0005`'s `lead_session_id`/`team_name` do today.
--
-- Nothing else is needed: `run_node.watchdog_interventions`,
-- `run_event.kind IN ["watchdog","member","task"]`, and every review table
-- (`review_cycle`, `review_finding`, `interrogation`) already exist since
-- `0001_init.surql` and `0005_lead_binding_and_projection.surql` — they have
-- simply never had a writer until this packet (`phase4-retarget-plan.md` P7).

-- (1) `run_member` gains the teammate identity the projection must persist on
-- every observation, because Claude Code's team config never carries a
-- session id and the whole config directory disappears when the lead session
-- ends (§3.3, spike S1). All four are `option`: a member whose hook never
-- fired, or a snapshot taken before this column existed, keeps `NONE` rather
-- than a guessed value — "unknown" must stay distinguishable from "empty".
DEFINE FIELD session_id      ON run_member TYPE option<string>;
DEFINE FIELD transcript_path ON run_member TYPE option<string>;
-- The last observed pane agent state (free text, not a closed vocabulary —
-- it mirrors whatever karvex's own detection reported) and when it was last
-- seen to change, so a finished run can still say how long a teammate sat
-- idle while its task stayed `in_progress`.
DEFINE FIELD last_state      ON run_member TYPE option<string>;
DEFINE FIELD last_state_at   ON run_member TYPE option<datetime>;

-- (2) `run_node.attention` — karvex's own opinion that a node needs
-- intervention, never the projected `run_node.status` itself (D-10: status is
-- Claude Code's fact, attention is karvex's judgment, and overwriting the
-- former with the latter would corrupt the projection). `option`, and
-- re-evaluated on every watchdog tick rather than only ever escalating:
-- `NONE` clears a prior attention once the node is seen moving again.
--
-- The ASSERT uses the same `$value == NONE OR $value IN [...]` idiom
-- `run_node.evidence`/`run_node.succession` already use (`0001_init.surql`),
-- because SurrealDB skips an `option<T>` field's ASSERT entirely when the
-- value is `NONE` — an unconditional `$value IN [...]` would silently never
-- fire for the column's own default state.
DEFINE FIELD attention ON run_node TYPE option<string>
       ASSERT $value == NONE OR $value IN
       ["stuck","budget_exceeded","needs_input","lead_blocked","unbound"];
